//! Relay pool — port of `src/data/pool.ts` + `src/nostr/nip65.ts` semantics,
//! slice 1 (v1): a nostr-sdk client over the LMDB store.
//!
//! v1 routing (deliberately simple, TODO(port) marks the full port):
//! - READ: indexers (the bootstrap relays from nip65.ts:9-15) + own NIP-65
//!   read relays. Feed queries pass all followed authors in one filter.
//! - WRITE: own NIP-65 write relays, falling back to indexers.
//! - TODO(port): quality-weighted set-cover routing, adaptive budgets, EWMA
//!   latency profiles, read-dead detection, seen-on memory (M2+).
//! - TODO(port): NIP-09 tombstones + NIP-40 expiry at the chokepoint (M2).

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use nostr::prelude::*;
use nostr_sdk::authenticator::Authenticator;
use nostr_sdk::error::Error as SdkError;
use nostr_sdk::prelude::*;

use crate::signer::Signer;
use crate::store::Store;

/// Bootstrap indexer relays (nip65.ts:9-15).
pub const INDEXERS: [&str; 5] = [
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://relay.coracle.social",
    "wss://nos.social",
    "wss://purplepag.es",
];

/// Per-query deadline (pool.ts 'page' budget).
const READ_BUDGET: Duration = Duration::from_secs(4);

/// The session identity the pool answers NIP-42 AUTH challenges with.
///
/// NIP-78 (Sep 2026): relays SHOULD serve kinds 78/30078 only to the
/// authenticated owner, so a challenge must be answered AS THE OWNER's
/// key. A throwaway identity would authenticate and still gain nothing.
/// The identity is attached at session activation and cleared at logout;
/// a challenge arriving with no session fails, as it always has.
#[derive(Clone, Default)]
struct PoolIdentity(Arc<RwLock<Option<Signer>>>);

impl PoolIdentity {
    fn set(&self, signer: Signer) {
        *self.0.write().unwrap() = Some(signer);
    }

    fn clear(&self) {
        *self.0.write().unwrap() = None;
    }

    fn get(&self) -> Option<Signer> {
        self.0.read().unwrap().clone()
    }
}

/// The pool's NIP-42 authenticator over the live session identity. The
/// client captures the authenticator Arc at build time and hands the SAME
/// Arc to every relay added later, so the identity inside the lock can
/// change per session (or vanish) without touching the client.
struct PoolAuthenticator(PoolIdentity);

impl std::fmt::Debug for PoolAuthenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The identity (keys) is not Debug by design; never print it.
        f.write_str("PoolAuthenticator")
    }
}

impl Authenticator for PoolAuthenticator {
    // The SDK's `BoxedFuture` alias is crate-private; this is the same
    // type spelled out (Pin<Box<dyn Future + Send>>).
    fn make_auth_event<'a>(
        &'a self,
        relay_url: &'a RelayUrl,
        challenge: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Event, SdkError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(signer) = self.0.get() else {
                return Err(SdkError::other(
                    "no session; not answering AUTH challenge",
                ));
            };
            // NIP-42: kind 22242, ["relay", url], ["challenge", challenge],
            // empty content, signed by the session key. A bunker signer
            // round-trips each challenge through the bunker app (the
            // owner-approved design); local keys sign in-process.
            let pk = signer
                .public_key()
                .await
                .map_err(|e| SdkError::other(e.to_string()))?;
            let unsigned = ClientAuthentication::new(challenge, relay_url.clone())
                .finalize_unsigned(pk);
            signer
                .sign_event(unsigned)
                .await
                .map_err(|e| SdkError::other(e.to_string()))
        })
    }
}

pub struct Pool {
    client: Client,
    pub store: Store,
    identity: PoolIdentity,
}

impl Pool {
    /// Direct client access for pool management (relay add/remove).
    pub fn client(&self) -> &Client {
        &self.client
    }

    pub async fn open() -> Result<Self, SdkError> {
        let store = Store::open().await?;
        // No keys live in the client: NIP-42 AUTH challenges are answered
        // with the session identity (set_identity, below) — the owner's
        // key, the only one strict relays honor for kinds 78/30078
        // (NIP-78, Sep 2026). Signing never happens here; nothing else
        // reads the identity.
        let identity = PoolIdentity::default();
        let client = ClientBuilder::new()
            .database(store.database())
            .authenticator(PoolAuthenticator(identity.clone()))
            .build();
        Ok(Self {
            client,
            store,
            identity,
        })
    }

    /// Attach the session identity: NIP-42 AUTH challenges are answered as
    /// this key. Replaces any previous identity (a session switch); a
    /// bunker identity round-trips each challenge through the bunker app.
    pub fn set_identity(&self, signer: Signer) {
        self.identity.set(signer);
    }

    /// Session gone: challenges stop being answered (never as a stale key).
    pub fn clear_identity(&self) {
        self.identity.clear();
    }

    /// Connect the read set. Write relays join at publish time.
    pub async fn connect_reads(&self, read_relays: Vec<String>) -> Result<(), SdkError> {
        let mut urls: Vec<String> = INDEXERS.iter().map(|s| s.to_string()).collect();
        urls.extend(read_relays);
        for url in urls {
            // Bad relay = skip; read-scoped capability.
            let _ = self
                .client
                .add_relay(url)
                .capabilities(RelayCapabilities::READ)
                .await;
        }
        self.client.connect().await;
        Ok(())
    }

    /// Deadline-not-completeness read (pool.ts 'page' mode).
    pub async fn get(&self, filter: Filter) -> Result<BTreeSet<Event>, SdkError> {
        self.client.fetch_events(filter).timeout(READ_BUDGET).await
    }

    /// Own contact list: fresh from relays, cached into the store.
    pub async fn follows(&self, me: PublicKey) -> Result<Vec<PublicKey>, SdkError> {
        let fresh = self
            .get(Filter::new().kind(Kind::ContactList).author(me).limit(1))
            .await?;
        for ev in &fresh {
            let _ = self.store.save(ev).await;
        }
        let cached = self.store.latest_by_kind(me, Kind::ContactList).await?;
        Ok(cached.map(|ev| ev.tags.public_keys().collect()).unwrap_or_default())
    }

    /// Own NIP-65 relay list, split into (read, write).
    pub async fn nip65(&self, me: PublicKey) -> Result<(Vec<String>, Vec<String>), SdkError> {
        let fresh = self
            .get(Filter::new().kind(Kind::RelayList).author(me).limit(1))
            .await?;
        for ev in &fresh {
            let _ = self.store.save(ev).await;
        }
        let cached = self.store.latest_by_kind(me, Kind::RelayList).await?;
        let mut read = Vec::new();
        let mut write = Vec::new();
        if let Some(ev) = cached {
            for tag in ev.tags.clone().to_vec() {
                let parts = tag.as_slice();
                // r <wss-url> [read|write] — absent marker means both.
                if parts.len() >= 2 && parts[0] == "r" && parts[1].starts_with("wss://") {
                    match parts.get(2).map(String::as_str) {
                        Some("read") => read.push(parts[1].clone()),
                        Some("write") => write.push(parts[1].clone()),
                        _ => {
                            read.push(parts[1].clone());
                            write.push(parts[1].clone());
                        }
                    }
                }
            }
        }
        Ok((read, write))
    }

    /// Publish to the write set (own write relays, fallback indexers).
    pub async fn publish(&self, event: &Event, write_relays: &[String]) -> Result<(), SdkError> {
        let targets: Vec<String> = if write_relays.is_empty() {
            INDEXERS.iter().map(|s| s.to_string()).collect()
        } else {
            write_relays.to_vec()
        };
        let _ = self.client.send_event(event).to(targets).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::authenticator::Authenticator as _;

    /// Any 32-byte value; a throwaway test key, never an identity.
    const TEST_HEX: &str = "e1d4a5b33a9d2f0c8b7e6f5a4d3c2b1a0918273645f4e3d2c1b0a99887766554";

    fn auth() -> PoolAuthenticator {
        PoolAuthenticator(PoolIdentity::default())
    }

    /// With a session attached, a challenge yields a valid NIP-42 event:
    /// kind 22242, signed by the identity, relay + challenge tagged,
    /// empty content. Without one (and after clear), nothing is signed.
    #[tokio::test]
    async fn auth_event_follows_nip42_and_needs_an_identity() {
        let url: RelayUrl = "wss://relay.example".parse().unwrap();
        let a = auth();

        // No session: the challenge stays unanswered.
        assert!(a.make_auth_event(&url, "c1").await.is_err());

        let keys = crate::signer::parse_local_secret(TEST_HEX).unwrap();
        a.0.set(Signer::Local(keys.clone()));
        let ev = a.make_auth_event(&url, "c1").await.unwrap();
        assert_eq!(ev.kind, Kind::Authentication);
        assert_eq!(ev.pubkey, keys.public_key());
        assert_eq!(ev.content, "");
        let tags = ev.tags.clone().to_vec();
        let tag_val = |name: &str| {
            tags.iter()
                .find(|t| t.as_slice().first().map(String::as_str) == Some(name))
                .and_then(|t| t.as_slice().get(1).cloned())
        };
        assert_eq!(tag_val("relay").as_deref(), Some("wss://relay.example"));
        assert_eq!(tag_val("challenge").as_deref(), Some("c1"));

        // Logout: the identity is gone, challenges fail again.
        a.0.clear();
        assert!(a.make_auth_event(&url, "c2").await.is_err());
    }

    /// Live-network probe (run: cargo test -p satori-core -- --ignored --nocapture).
    /// Proves: indexers connect → kind:10002 discovery → store roundtrip →
    /// split parsing. The settings pane depends on exactly this chain.
    #[tokio::test]
    #[ignore = "network"]
    async fn nip65_discovery_end_to_end() {
        let pool = Pool::open().await.unwrap();
        pool.connect_reads(Vec::new()).await.ok();

        // Any relay list off the indexers proves discovery.
        let filter = Filter::new().kind(Kind::RelayList).limit(3);
        let events = pool.get(filter).await.expect("fetch");
        println!("discovered {} relay list(s)", events.len());
        assert!(!events.is_empty(), "indexers returned no kind:10002 events");

        let ev = events.iter().next().unwrap();
        let r_tags = ev
            .tags
            .clone()
            .to_vec()
            .iter()
            .filter(|t| t.as_slice().first().map(String::as_str) == Some("r"))
            .count();
        println!("author {} has {} r-tags", ev.pubkey, r_tags);
        assert!(r_tags > 0);

        // Store roundtrip: the exact query nip65() runs.
        pool.store.save(ev).await.unwrap();
        let cached = pool
            .store
            .latest_by_kind(ev.pubkey, Kind::RelayList)
            .await
            .unwrap();
        assert!(cached.is_some(), "latest_by_kind lost the stored relay list");

        let (read, write) = pool.nip65(ev.pubkey).await.unwrap();
        println!("nip65 split: {} read, {} write", read.len(), write.len());
        assert!(!read.is_empty() || !write.is_empty());
    }
}
