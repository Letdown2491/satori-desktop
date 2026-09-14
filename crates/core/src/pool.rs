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
use std::time::Duration;

use nostr::prelude::*;
use nostr_sdk::error::Error as SdkError;
use nostr_sdk::prelude::*;

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

pub struct Pool {
    client: Client,
    pub store: Store,
}

impl Pool {
    pub async fn open() -> Result<Self, SdkError> {
        let store = Store::open().await?;
        // Throwaway pool keys: NIP-42 auth uses these; signing never does.
        let client = ClientBuilder::new().database(store.database()).build();
        Ok(Self { client, store })
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
