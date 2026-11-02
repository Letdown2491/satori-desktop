//! The profile service — port of `src/data/profile-cache.ts` semantics:
//! persistent cache (0600 profiles.json, 10k cap), 12h TTL with background
//! revalidation, debounced coalesced batch fetching (chunked kind-0
//! queries), and eager warming of the follow list at bootstrap.
//!
//! The reactive part lives in the UI: cards render with whatever is cached
//! and PATCH themselves when `ensure_profiles` resolves late arrivals.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nostr::prelude::*;
use tokio::sync::mpsc;

use crate::manager::Profile;
use crate::pool::Pool;

/// Revalidate cached profiles older than this.
const TTL: u64 = 12 * 3600;
/// Fetch chunk (relays choke on huge author lists).
const CHUNK: usize = 100;
/// Debounce window: calls inside it share one chunked fetch.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);
const CAP: usize = 10_000;

#[derive(Clone)]
struct Entry {
    profile: Profile,
    at: u64,
}

struct Waiter {
    wants: Vec<PublicKey>,
    tx: mpsc::UnboundedSender<Vec<Profile>>,
}

pub struct ProfileService {
    cache: Mutex<HashMap<String, Entry>>,
    calls: Mutex<Vec<Arc<Mutex<Vec<Waiter>>>>>,
    pool: Arc<Pool>,
    disk: PathBuf,
    dirty: Arc<AtomicBool>,
}

impl ProfileService {
    pub fn new(pool: Arc<Pool>) -> Arc<Self> {
        let disk = crate::config::state_file(crate::config::files::PROFILES);
        let service = Arc::new(Self {
            cache: Mutex::new(Self::load_disk(&disk)),
            calls: Mutex::new(Vec::new()),
            pool,
            disk,
            dirty: Arc::new(AtomicBool::new(false)),
        });
        service.spawn_debouncer();
        service
    }

    fn load_disk(path: &PathBuf) -> HashMap<String, Entry> {
        #[derive(serde::Deserialize)]
        struct Raw {
            pubkey: String,
            name: String,
            picture: String,
            about: String,
            banner: String,
            // Added with the profile-page rework; old cache files lack them
            // (serde defaults keep the round-trip tolerant).
            #[serde(default)]
            nip05: String,
            #[serde(default)]
            lud: String,
            at: u64,
        }
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<Raw>>(&s).ok())
            .map(|raws| {
                raws.into_iter()
                    .map(|r| {
                        (
                            r.pubkey.clone(),
                            Entry {
                                profile: Profile {
                                    pubkey: r.pubkey,
                                    name: r.name,
                                    picture: r.picture,
                                    about: r.about,
                                    banner: r.banner,
                                    nip05: r.nip05,
                                    lud: r.lud,
                                },
                                at: r.at,
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn save_disk(&self, cache: &HashMap<String, Entry>) {
        #[derive(serde::Serialize)]
        struct Raw {
            pubkey: String,
            name: String,
            picture: String,
            about: String,
            banner: String,
            nip05: String,
            lud: String,
            at: u64,
        }
        // LRU-ish: newest entries win the cap.
        let mut all: Vec<&Entry> = cache.values().collect();
        all.sort_by_key(|e| std::cmp::Reverse(e.at));
        let raws: Vec<Raw> = all
            .iter()
            .take(CAP)
            .map(|e| Raw {
                pubkey: e.profile.pubkey.clone(),
                name: e.profile.name.clone(),
                picture: e.profile.picture.clone(),
                about: e.profile.about.clone(),
                banner: e.profile.banner.clone(),
                nip05: e.profile.nip05.clone(),
                lud: e.profile.lud.clone(),
                at: e.at,
            })
            .collect();
        if let Ok(body) = serde_json::to_string(&raws) {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            if let Some(parent) = self.disk.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&self.disk)
                .and_then(|mut f| f.write_all(body.as_bytes()));
        }
        self.dirty.store(false, Ordering::Relaxed);
    }

    pub fn get(&self, pk: &PublicKey) -> Option<Profile> {
        self.cache.lock().unwrap().get(&pk.to_string()).map(|e| e.profile.clone())
    }

    /// Absorb a fetched profile into the cache (+ persist soon).
    pub fn absorb(&self, profile: Profile) {
        let pk = profile.pubkey.clone();
        self.cache
            .lock()
            .unwrap()
            .insert(pk, Entry { profile, at: now_secs() });
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Resolve profiles for pks. Cache hits resolve in the next debounce
    /// tick (~150ms); misses coalesce into one chunked fetch. Stale entries
    /// resolve instantly AND revalidate in the background.
    pub fn ensure_many(&self, pks: Vec<PublicKey>) -> mpsc::UnboundedReceiver<Vec<Profile>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.calls
            .lock()
            .unwrap()
            .push(Arc::new(Mutex::new(vec![Waiter { wants: pks, tx }])));
        rx
    }

    /// Warm + AWAIT (bootstrap: the first page must paint resolved).
    pub async fn warm_await(&self, pks: Vec<PublicKey>) {
        let missing: Vec<PublicKey> = self.missing_or_stale(pks);
        if missing.is_empty() {
            return;
        }
        self.fetch_and_absorb(missing).await;
    }

    /// Warm in the background (fire-and-forget): fetch + absorb, nobody
    /// waits — the reactive UI patches as profiles land.
    pub fn warm(self: &Arc<Self>, pks: Vec<PublicKey>) {
        let missing = self.missing_or_stale(pks);
        if missing.is_empty() {
            return;
        }
        let service = Arc::clone(self);
        tokio::spawn(async move {
            service.fetch_and_absorb(missing).await;
        });
    }

    fn missing_or_stale(&self, pks: Vec<PublicKey>) -> Vec<PublicKey> {
        pks.into_iter()
            .filter(|pk| {
                self.cache
                    .lock()
                    .unwrap()
                    .get(&pk.to_string())
                    .map(|e| now_secs() - e.at > TTL)
                    .unwrap_or(true)
            })
            .collect()
    }

    /// Stale entries revalidate silently in the background.
    fn revalidate_stale(self: &Arc<Self>) {
        let stale: Vec<PublicKey> = {
            let cache = self.cache.lock().unwrap();
            cache
                .values()
                .filter(|e| now_secs() - e.at > TTL)
                .filter_map(|e| PublicKey::from_hex(&e.profile.pubkey).ok())
                .collect()
        };
        if !stale.is_empty() {
            let service = Arc::clone(self);
            tokio::spawn(async move {
                service.fetch_and_absorb(stale).await;
            });
        }
    }

    fn spawn_debouncer(self: &Arc<Self>) {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(DEBOUNCE).await;

                // Flush cache to disk when dirty.
                if service.dirty.load(Ordering::Relaxed) {
                    let cache = service.cache.lock().unwrap().clone();
                    service.save_disk(&cache);
                }

                service.revalidate_stale();

                // Take pending calls → union misses → one chunked fetch.
                let calls: Vec<Arc<Mutex<Vec<Waiter>>>> = {
                    let mut c = service.calls.lock().unwrap();
                    std::mem::take(&mut *c)
                };
                if calls.is_empty() {
                    continue;
                }
                let mut missing: Vec<PublicKey> = Vec::new();
                for call in &calls {
                    for w in call.lock().unwrap().iter() {
                        for pk in &w.wants {
                            if service.get(pk).is_none() && !missing.contains(pk) {
                                missing.push(*pk);
                            }
                        }
                    }
                }
                if !missing.is_empty() {
                    service.fetch_and_absorb(missing).await;
                }
                // Resolve every call with cache state (fresh ∪ still-missing
                // as None).
                for call in calls {
                    let waiters: Vec<Waiter> = {
                        let mut w = call.lock().unwrap();
                        std::mem::take(&mut *w)
                    };
                    for w in waiters {
                        let results: Vec<Profile> = w
                            .wants
                            .iter()
                            .filter_map(|pk| service.get(pk))
                            .collect();
                        let _ = w.tx.send(results);
                    }
                }
            }
        });
    }

    async fn fetch_and_absorb(&self, pks: Vec<PublicKey>) {
        for chunk in pks.chunks(CHUNK) {
            let filter = Filter::new()
                .kind(Kind::Metadata)
                .authors(chunk.to_vec())
                .limit(CHUNK);
            for ev in self.pool.get(filter).await.unwrap_or_default() {
                if let Some(p) = Profile::from_event(&ev) {
                    self.absorb(p);
                }
            }
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
