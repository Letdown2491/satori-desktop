//! Event store — nostr-lmdb (LMDB) at `config::events_dir()`.
//!
//! The projection source of truth (DESIGN.md §1.2): views read from here and
//! from live subscriptions, never from widget state. The nostr-sdk client is
//! built against this database, so every event received from relays is saved
//! automatically; this module adds the query surface the UI needs.

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Arc;

use nostr_database::error::Error as DbError;
use nostr::prelude::*;
use nostr_lmdb::prelude::*;

use crate::config;

type NostrResult<T> = Result<T, DbError>;

/// The content-kind catalog (content-prefs.ts:40-55): label + the kinds it
/// groups. All default visible; prefs.json `content_hidden` hides.
pub const CATALOG: [(&str, &[u16]); 12] = [
    ("Notes", &[1]),
    ("Reposts", &[6]),
    ("Pictures", &[20]),
    ("Videos", &[21, 22]),
    ("Podcasts", &[54]),
    ("Polls", &[1068]),
    ("Articles", &[30023]),
    ("Listings", &[30402]),
    ("Repos", &[30617]),
    ("Wikis", &[30818]),
    ("Calendar", &[31922, 31923]),
    ("Highlights", &[9802]),
];

/// The kinds currently visible (prefs.json `content_hidden: [kind]`).
pub fn visible_kinds() -> Vec<Kind> {
    let hidden: Vec<u16> = std::fs::read_to_string(super::config::state_file(
        super::config::files::PREFS,
    ))
    .ok()
    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
    .and_then(|v| {
        v.get("content_hidden")
            .and_then(|h| serde_json::from_value(h.clone()).ok())
    })
    .unwrap_or_default();
    FEED_KINDS
        .iter()
        .filter(|k| !hidden.contains(&u16::from(**k)))
        .copied()
        .collect()
}

/// The content kinds the following feed carries (content-prefs.ts catalog's
/// default surface).
pub const FEED_KINDS: [Kind; 14] = [
    Kind::TextNote,       // 1
    Kind::Repost,         // 6
    Kind::Custom(20),     // picture (NIP-68)
    Kind::Custom(21),     // video (NIP-71)
    Kind::Custom(22),     // short video
    Kind::Custom(54),     // podcast episode (NIP-F4)
    Kind::Custom(1068),   // poll (NIP-88)
    Kind::Custom(30023),  // article (NIP-23)
    Kind::Custom(30402),  // classified (NIP-99)
    Kind::Custom(30617),  // repo (NIP-34)
    Kind::Custom(30818),  // wiki (NIP-54)
    Kind::Custom(31922),  // calendar date (NIP-52)
    Kind::Custom(31923),  // calendar time
    Kind::Custom(9802),   // highlight (NIP-84)
];

pub struct Store {
    db: Arc<NostrLmdb>,
}

impl Store {
    pub async fn open() -> NostrResult<Self> {
        std::fs::create_dir_all(config::events_dir())
            .map_err(|e| DbError::other(e.to_string()))?;
        let db = NostrLmdb::open(config::events_dir()).await?;
        Ok(Self { db: Arc::new(db) })
    }

    pub(crate) fn database(&self) -> Arc<NostrLmdb> {
        Arc::clone(&self.db)
    }

    /// Save an event explicitly (own publishes, unmetered fetches).
    pub async fn save(&self, event: &Event) -> NostrResult<()> {
        self.db.save_event(event).await.map(|_| ())
    }

    /// One page of the following feed: notes by authors, newest first,
    /// strictly older than `until` when given.
    pub async fn feed_page(
        &self,
        authors: &[PublicKey],
        until: Option<u64>,
        limit: usize,
    ) -> NostrResult<Vec<Event>> {
        let mut filter = Filter::new()
            .kinds([Kind::TextNote, Kind::Repost])
            .authors(authors.to_vec())
            .limit(limit);
        if let Some(ts) = until {
            filter = filter.until(Timestamp::from(ts));
        }
        let mut events: Vec<Event> = self.query(filter).await?.into_iter().collect();
        // BTreeSet orders by id; the feed needs created_at order. Machine
        // notes drop here — every feed path crosses this chokepoint.
        events.retain(|e| !is_machine_note(&e.content));
        events.sort_by_key(|e| std::cmp::Reverse(e.created_at.as_secs()));
        Ok(events)
    }

    /// Replies to a note: kind 1s whose `e` tags reference it (NIP-10).
    pub async fn replies(&self, id: EventId) -> NostrResult<Vec<Event>> {
        let filter = Filter::new().kind(Kind::TextNote).event(id);
        let mut events: Vec<Event> = self.query(filter).await?.into_iter().collect();
        events.retain(|e| !is_machine_note(&e.content));
        events.sort_by_key(|e| e.created_at.as_secs());
        Ok(events)
    }

    /// Latest kind-0 metadata for each of the given authors, from the cache.
    pub async fn profiles(&self, authors: &[PublicKey]) -> NostrResult<Vec<Event>> {
        let filter = Filter::new().kind(Kind::Metadata).authors(authors.to_vec());
        Ok(self.query(filter).await?.into_iter().collect())
    }

    /// Latest event of one kind by one author (contact list, relay list, ...).
    pub async fn latest_by_kind(
        &self,
        author: PublicKey,
        kind: Kind,
    ) -> NostrResult<Option<Event>> {
        let filter = Filter::new().kind(kind).author(author).limit(1);
        Ok(self.query(filter).await?.into_iter().next())
    }

    /// The user's own reactions (engagement set source).
    pub async fn own_reactions(&self, author: PublicKey) -> NostrResult<Vec<Event>> {
        let filter = Filter::new().kind(Kind::Reaction).author(author).limit(5000);
        Ok(self.query(filter).await?.into_iter().collect())
    }

    /// Fetch one event by id (repost target resolution).
    pub async fn event_by_id(&self, id: EventId) -> NostrResult<Option<Event>> {
        self.db.event_by_id(&id).await
    }

    /// Local tombstone (own deletions — NIP-09).
    pub async fn delete_event(&self, id: EventId) -> NostrResult<()> {
        self.db.delete(Filter::new().id(id)).await
    }

    async fn query(&self, filter: Filter) -> NostrResult<BTreeSet<Event>> {
        self.db.query(filter).await
    }
}

/// Machine notes (filters.ts:104-109): JSON-as-content events are bot
/// droppings, not human speech — dropped at the chokepoint like tombstones.
pub fn is_machine_note(content: &str) -> bool {
    let t = content.trim_start();
    t.starts_with("{\"content\"")
        || serde_json::from_str::<serde_json::Value>(t)
            .ok()
            .map(|v| {
                v.is_object()
                    && v.get("content").map(|c| c.is_string()).unwrap_or(false)
                    && v.get("kind").is_some()
                    && v.get("tags").is_some()
            })
            .unwrap_or(false)
}

pub fn parse_key(hex: &str) -> Option<PublicKey> {
    PublicKey::from_str(hex).ok()
}
