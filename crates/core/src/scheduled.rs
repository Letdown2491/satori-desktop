//! Scheduled posts — port of `src/data/scheduled.ts` (webapp final state,
//! commit `a4b09e8`): a note SIGNED at compose, held on disk, broadcast by
//! a periodic sweep at its time. `signed.created_at` == the scheduled
//! time, broadcast AT that moment so it reads as ≈now and relays don't
//! reject future-dating.
//!
//! Rules ported verbatim:
//! - **Cap**: `MAX_PER_USER` (100) entries per author — the webapp bounded
//!   a shared multi-user store; here it bounds the file and the sweep's
//!   in-memory copy against a runaway. At cap, `hold` returns None and the
//!   composer surfaces the error.
//! - **Sweep**: every 30s (the manager owns the loop), broadcast entries
//!   with `scheduled_at <= now`, remove on success, keep on failure and
//!   retry next tick, GIVE UP after `GIVE_UP_SECS` (1h) past the scheduled
//!   time. Never overlapping sweeps (the `sweeping` guard).
//! - **Restart-safe**: the sweep re-reads due entries from the store each
//!   tick — no per-post timers, nothing in memory only. A post due while
//!   the app was closed goes out at the next tick after launch.
//! - **Corrupt-entry screen**: an entry whose stored pubkey doesn't match
//!   its signed event is skipped (the signed event is self-authenticating;
//!   don't waste a broadcast on a mismatch).
//!
//! Divergence from the webapp (documented, deliberate): the browser held a
//! nip07 key only at compose, so it persisted per-post `writeTargets` for
//! the *daemon* to broadcast with. The desktop app IS the daemon — it
//! holds the signing session and the NIP-65 write set, so targets resolve
//! at publish time through the SAME funnel as normal posting
//! (`Core::commit`: LMDB save → outbox write-ahead → publish → dequeue).
//! Nothing per-post is stored beyond author, signed event, scheduled time.
//!
//! CRASH DISCIPLINE — publish-before-mark (at-least-once), like the
//! outbox queue's write-ahead: `hold` flushes SYNCHRONOUSLY (the entry
//! must survive a crash-before-first-attempt), and the entry is removed
//! from the store only AFTER ≥1 relay accepted. A crash at any point
//! before that leaves the entry on disk; the next start's sweep
//! re-publishes it. Relays dedupe identical event ids, so the worst case
//! is a duplicate the relays collapse — never a silently LOST post.
//! Mark-before-publish was rejected: it turns a crash into a vanished
//! scheduled post, which is strictly worse. Removals ride the debounced
//! flusher (10s, the hints.rs/outbox_queue.rs cadence) — a crash before
//! those flush at worst re-publishes, same dedupe argument.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use nostr::prelude::*;

use crate::config;

/// Per-author cap (scheduled.ts MAX_PER_USER = 100).
pub const MAX_PER_USER: usize = 100;
/// Stop retrying a post that keeps failing after 1h past its time
/// (scheduled.ts GIVE_UP_SECS).
pub const GIVE_UP_SECS: u64 = 3600;
/// Flush cadence for removals (hints.rs / outbox_queue.rs share it).
const FLUSH_SECS: u64 = 10;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScheduledPost {
    pub token: String,
    /// Author (== event.pubkey; the mismatch screen skips such entries).
    pub pubkey: String,
    /// Fully-signed event; created_at == scheduled_at.
    pub event: Event,
    /// Unix SECONDS to broadcast at.
    pub scheduled_at: u64,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Stored {
    #[serde(default)]
    items: Vec<ScheduledPost>,
}

struct Book {
    map: HashMap<String, ScheduledPost>,
}

static BOOK: OnceLock<Mutex<Book>> = OnceLock::new();
static DIRTY: AtomicBool = AtomicBool::new(false);
/// The webapp's `sweeping` guard: never overlap a slow sweep.
static SWEEPING: AtomicBool = AtomicBool::new(false);
/// Token serial (drafts.ts newDraftId shape: cheap and unique is enough).
static TOKEN_SEQ: AtomicU64 = AtomicU64::new(0);

fn book() -> &'static Mutex<Book> {
    BOOK.get_or_init(|| {
        let items = load_disk();
        if !items.is_empty() {
            eprintln!("[core] scheduled: loaded {} pending post(s)", items.len());
        }
        spawn_flusher();
        Mutex::new(Book {
            map: items.into_iter().map(|p| (p.token.clone(), p)).collect(),
        })
    })
}

/// The persisted store, parsed fresh — the restart seam: a file written by
/// a previous run loads here, and its due entries publish on the next
/// sweep. Unparseable = start empty, LOUD (the outbox queue's discipline).
pub(crate) fn load_disk() -> Vec<ScheduledPost> {
    match std::fs::read_to_string(config::state_file(config::files::SCHEDULED)) {
        Err(_) => Vec::new(), // no file yet = nothing ever scheduled
        Ok(body) => match serde_json::from_str::<Stored>(&body) {
            Ok(s) => s.items,
            Err(e) => {
                let msg = e.to_string().replace('\n', " ");
                eprintln!(
                    "[core] scheduled: file UNPARSEABLE, starting empty ({})",
                    &msg[..msg.len().min(160)]
                );
                Vec::new()
            }
        },
    }
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Hold a freshly-signed post for the sweep with a generated token — the
/// one seam every compose path shares (holdScheduled). Returns None at
/// cap (the caller surfaces SCHEDULE_FULL). Flushes SYNCHRONOUSLY: the
/// write-ahead durability point.
pub fn hold(me: &PublicKey, event: Event, scheduled_at: u64) -> Option<String> {
    let mut held = book().lock().unwrap();
    let mine = held
        .map
        .values()
        .filter(|p| p.pubkey == me.to_string())
        .count();
    if mine >= MAX_PER_USER {
        return None;
    }
    let n = TOKEN_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let token = format!("s{nanos:08x}{n:04x}");
    held.map.insert(
        token.clone(),
        ScheduledPost {
            token: token.clone(),
            pubkey: me.to_string(),
            event,
            scheduled_at,
        },
    );
    drop(held);
    write_disk();
    Some(token)
}

/// The author's pending posts, soonest first (listScheduled).
pub fn list(me: &PublicKey) -> Vec<ScheduledPost> {
    let mut mine: Vec<ScheduledPost> = book()
        .lock()
        .unwrap()
        .map
        .values()
        .filter(|p| p.pubkey == me.to_string())
        .cloned()
        .collect();
    mine.sort_by_key(|p| p.scheduled_at);
    mine
}

/// Cancel: only the author's own token goes (cancelScheduled). Flushed
/// SYNCHRONOUSLY — a cancel the user saw confirmed must not resurrect
/// after a restart.
pub fn cancel(me: &PublicKey, token: &str) -> bool {
    let mut held = book().lock().unwrap();
    let gone = if held
        .map
        .get(token)
        .is_some_and(|p| p.pubkey == me.to_string())
    {
        held.map.remove(token).is_some()
    } else {
        false
    };
    drop(held);
    if gone {
        write_disk();
    }
    gone
}

/// Due entries whose stored pubkey matches the signed event (the corrupt-
/// store screen); due = scheduled_at <= now.
fn due(now: u64) -> Vec<ScheduledPost> {
    book()
        .lock()
        .unwrap()
        .map
        .values()
        .filter(|p| p.scheduled_at <= now && p.event.pubkey.to_string() == p.pubkey)
        .cloned()
        .collect()
}

/// Remove published / given-up tokens in ONE read+write for the whole
/// sweep (dropMany) — debounced, since a crash before the flush at worst
/// re-publishes (relays dedupe by id).
fn remove(tokens: &[String]) {
    let mut held = book().lock().unwrap();
    let mut changed = false;
    for t in tokens {
        if held.map.remove(t).is_some() {
            changed = true;
        }
    }
    drop(held);
    if changed {
        DIRTY.store(true, Ordering::Relaxed);
    }
}

/// Give-up decision (scheduled.ts: `now - scheduledAt > GIVE_UP_SECS`).
pub fn give_up(now: u64, scheduled_at: u64) -> bool {
    now.saturating_sub(scheduled_at) > GIVE_UP_SECS
}

/// Claim this tick's due entries (the `SWEEPING` single-flight: None when
/// a sweep is already in flight — never overlap a slow sweep). The caller
/// publishes them and hands the settled tokens to `finish_sweep`.
pub fn take_due(now: u64) -> Option<Vec<ScheduledPost>> {
    if SWEEPING.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(due(now))
}

/// Settle the sweep: remove the published / given-up tokens in ONE
/// read+write for the whole tick (dropMany) — debounced, since a crash
/// before the flush at worst re-publishes (relays dedupe by id) — and
/// release the single-flight.
pub fn finish_sweep(tokens: &[String]) {
    remove(tokens);
    SWEEPING.store(false, Ordering::SeqCst);
}

/// The whole sweep, synchronous (the manager runs its tick on the async
/// runtime via take_due/finish_sweep instead; this is the same discipline
/// in one call — tests and any future sync host use it).
pub fn sweep(now: u64, publish: impl Fn(&Event) -> bool) -> usize {
    let Some(due) = take_due(now) else {
        return 0; // never overlap a slow sweep
    };
    let mut remove: Vec<String> = Vec::new();
    let mut published = 0;
    for p in due {
        if publish(&p.event) {
            remove.push(p.token.clone());
            published += 1;
        } else if give_up(now, p.scheduled_at) {
            eprintln!("[core] scheduled: giving up on {}", p.token);
            remove.push(p.token.clone());
        }
        // else: keep it and retry on the next sweep (relay may be down)
    }
    finish_sweep(&remove);
    published
}

fn write_disk() {
    let mut snap: Vec<ScheduledPost> = {
        let held = book().lock().unwrap();
        held.map.values().cloned().collect()
    };
    snap.sort_by_key(|p| p.scheduled_at);
    let body = serde_json::to_string(&Stored { items: snap }).unwrap_or_default();
    let path = config::state_file(config::files::SCHEDULED);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = path.with_extension(format!("tmp-{}-{nanos}", std::process::id()));
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
    else {
        return;
    };
    use std::io::Write as _;
    if f.write_all(body.as_bytes()).is_ok() {
        let synced = f.sync_all().is_ok();
        drop(f);
        if synced {
            let _ = std::fs::rename(&tmp, &path);
            DIRTY.store(false, Ordering::Relaxed);
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The flusher thread (hints.rs / outbox_queue.rs discipline).
fn spawn_flusher() {
    std::thread::Builder::new()
        .name("scheduled-flush".into())
        .spawn(|| {
            loop {
                std::thread::park_timeout(std::time::Duration::from_secs(FLUSH_SECS));
                if DIRTY.swap(false, Ordering::Relaxed) {
                    write_disk();
                }
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;

    fn note(keys: &Keys, content: &str, at: u64) -> Event {
        keys.sign_event(UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from_secs(at),
            Kind::TextNote,
            vec![],
            content,
        ))
        .unwrap()
    }

    fn pk() -> PublicKey {
        Keys::generate().public_key()
    }

    /// The book is a process-global static (the outbox_queue.rs pattern):
    /// each test starts from an empty map so leftovers from an earlier
    /// test (its fills/cancels) can't leak into this one's sweep counts.
    #[cfg(test)]
    fn clear() {
        book().lock().unwrap().map.clear();
    }

    /// Scheduling holds, lists soonest-first, respects the per-author
    /// cap, and cancels only the owner's own token.
    #[test]
    fn hold_list_cancel_cap() {
        let _env = EnvGuard::new("sched-basic");
        clear();
        let a = Keys::generate();
        let b = pk();
        let t0 = hold(&a.public_key(), note(&a, "one", 100), 100).unwrap();
        let t1 = hold(&a.public_key(), note(&a, "two", 200), 200).unwrap();
        hold(&b, note(&Keys::generate(), "other", 150), 150).unwrap();
        let mine = list(&a.public_key());
        assert_eq!(mine.len(), 2);
        assert_eq!(mine[0].event.content, "one"); // soonest first
        assert_eq!(mine[1].token, t1);
        // Another author can't cancel (or even see) a's token.
        assert!(!cancel(&b, &t0));
        assert!(list(&b).iter().all(|p| p.token != t0));
        assert!(cancel(&a.public_key(), &t0));
        assert_eq!(list(&a.public_key()).len(), 1);
        // Cap: fill a's slots, the next hold is refused.
        for _ in 1..MAX_PER_USER {
            hold(&a.public_key(), note(&a, "fill", 300), 300).unwrap();
        }
        assert!(hold(&a.public_key(), note(&a, "over", 300), 300).is_none());
        // b unaffected.
        assert!(hold(&b, note(&Keys::generate(), "fine", 300), 300).is_some());
    }

    /// The sweep publishes due entries through the injected publish path
    /// and removes them; not-due entries wait; failures retry.
    #[test]
    fn sweep_publishes_due() {
        let _env = EnvGuard::new("sched-sweep");
        clear();
        let a = Keys::generate();
        let now = now_secs();
        let past = hold(&a.public_key(), note(&a, "past", now - 10), now - 10).unwrap();
        let future = hold(&a.public_key(), note(&a, "future", now + 999), now + 999).unwrap();
        let published = std::sync::Mutex::new(Vec::new());
        let n = sweep(now, |ev| {
            published.lock().unwrap().push(ev.content.clone());
            true
        });
        assert_eq!(n, 1);
        assert_eq!(*published.lock().unwrap(), vec!["past".to_string()]);
        let rest = list(&a.public_key());
        assert_eq!(
            rest.iter().map(|p| p.token.as_str()).collect::<Vec<_>>(),
            vec![future]
        );
        assert!(rest.iter().all(|p| p.token != past));
        // The corrupt-store screen: a pubkey/event mismatch never publishes.
        // Re-sign as someone else, store under a's pubkey: the signed
        // event self-authenticates, so the entry is skipped, not crashed on.
        let other = Keys::generate();
        let bad = other
            .sign_event(UnsignedEvent::new(
                other.public_key(),
                Timestamp::from_secs(now - 20),
                Kind::TextNote,
                vec![],
                "mismatch",
            ))
            .unwrap();
        book().lock().unwrap().map.insert(
            "corrupt".into(),
            ScheduledPost {
                token: "corrupt".into(),
                pubkey: a.public_key().to_string(),
                event: bad,
                scheduled_at: now - 20,
            },
        );
        assert_eq!(sweep(now, |_| true), 0);
    }

    /// A failed publish retries on later sweeps and is only dropped after
    /// GIVE_UP_SECS past its time.
    #[test]
    fn sweep_failure_retries_then_gives_up() {
        let _env = EnvGuard::new("sched-retry");
        clear();
        let a = Keys::generate();
        let now = now_secs();
        let token = hold(&a.public_key(), note(&a, "flaky", now - 5), now - 5).unwrap();
        // All relays down: kept, retried.
        assert_eq!(sweep(now, |_| false), 0);
        assert!(list(&a.public_key()).iter().any(|p| p.token == token));
        assert_eq!(sweep(now + 30, |_| false), 0);
        assert!(list(&a.public_key()).iter().any(|p| p.token == token));
        // A success at any later tick publishes it.
        assert_eq!(sweep(now + 60, |_| true), 1);
        assert!(list(&a.public_key()).is_empty());
        // Past the give-up horizon, a persistently failing entry is dropped.
        let old = hold(
            &a.public_key(),
            note(&a, "dead", now - GIVE_UP_SECS - 1),
            now - GIVE_UP_SECS - 1,
        )
        .unwrap();
        assert_eq!(sweep(now, |_| false), 0);
        assert!(list(&a.public_key()).iter().all(|p| p.token != old));
    }

    /// Restart catch-up: a file written by a "previous run" (hold flushed
    /// synchronously) parses back with its due entries intact — the next
    /// sweep publishes exactly those.
    #[test]
    fn restart_catch_up_from_disk() {
        let _env = EnvGuard::new("sched-restart");
        clear();
        let a = Keys::generate();
        let now = now_secs();
        let past = hold(&a.public_key(), note(&a, "survivor", now - 1), now - 1).unwrap();
        hold(&a.public_key(), note(&a, "later", now + 5000), now + 5000).unwrap();
        // Simulate the restart: reload the store from disk (the static
        // book survives in-process, but load_disk is what a fresh process
        // seeds its book from — its contents ARE the previous run's state).
        let reloaded = load_disk();
        assert_eq!(reloaded.len(), 2);
        let due_tokens: Vec<&str> = reloaded
            .iter()
            .filter(|p| p.scheduled_at <= now)
            .map(|p| p.token.as_str())
            .collect();
        assert_eq!(due_tokens, vec![past.as_str()]);
        // And the disk file carries the 0600 discipline.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(config::state_file(config::files::SCHEDULED))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// The sweep single-flight: an in-flight sweep is never overlapped.
    #[test]
    fn sweep_never_overlaps() {
        let _env = EnvGuard::new("sched-overlap");
        clear();
        SWEEPING.store(true, Ordering::SeqCst);
        assert_eq!(sweep(now_secs(), |_| true), 0);
        SWEEPING.store(false, Ordering::SeqCst);
    }
}
