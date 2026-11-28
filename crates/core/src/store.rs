//! Event store — nostr-lmdb (LMDB) at `config::events_dir()`.
//!
//! The projection source of truth (DESIGN.md §1.2): views read from here and
//! from live subscriptions, never from widget state. The nostr-sdk client is
//! built against this database, so every event received from relays is saved
//! automatically; this module adds the query surface the UI needs.

use std::collections::{BTreeSet, HashMap};
use std::str::FromStr;
use std::sync::Arc;

use nostr::prelude::*;
use nostr_database::error::Error as DbError;
use nostr_lmdb::prelude::*;

use crate::config;

type NostrResult<T> = Result<T, DbError>;

/// The content-kind catalog (content-prefs.ts:40-55): label + the kinds it
/// groups. All default visible; prefs.json `content_hidden` hides.
pub const CATALOG: [(&str, &[u16]); 14] = [
    ("Notes", &[1]),
    ("Reposts", &[6]),
    ("Pictures", &[20]),
    ("Videos", &[21, 22]),
    ("Voice", &[1222, 1244]),
    ("Podcasts", &[54]),
    ("Polls", &[1068]),
    ("Articles", &[30023]),
    ("Listings", &[30402]),
    ("Repos", &[30617]),
    ("Wikis", &[30818]),
    ("Calendar", &[31922, 31923]),
    ("Highlights", &[9802]),
    ("Goals", &[9041]),
];

/// The kinds currently visible (prefs.json `content_hidden: [kind]`).
pub fn visible_kinds() -> Vec<Kind> {
    let hidden: Vec<u16> =
        std::fs::read_to_string(super::config::state_file(super::config::files::PREFS))
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
pub const FEED_KINDS: [Kind; 17] = [
    Kind::TextNote,      // 1
    Kind::Repost,        // 6
    Kind::Custom(20),    // picture (NIP-68)
    Kind::Custom(21),    // short vertical video (NIP-71)
    Kind::Custom(22),    // video (NIP-71)
    Kind::Custom(1222),  // voice root (NIP-A0)
    Kind::Custom(1244),  // voice reply (NIP-A0)
    Kind::Custom(54),    // podcast episode (NIP-F4)
    Kind::Custom(1068),  // poll (NIP-88)
    Kind::Custom(30023), // article (NIP-23)
    Kind::Custom(30402), // classified (NIP-99)
    Kind::Custom(30617), // repo (NIP-34)
    Kind::Custom(30818), // wiki (NIP-54)
    Kind::Custom(31922), // calendar date (NIP-52)
    Kind::Custom(31923), // calendar time
    Kind::Custom(9802),  // highlight (NIP-84)
    Kind::Custom(9041),  // zap goal (NIP-75)
];

/// The app's LMDB store (nostr-lmdb). `Clone` is a handle clone — the
/// env is one `Arc`; the deferred-vacuum walk takes one so it can read
/// through the live env instead of opening a second one on the file.
#[derive(Clone, Debug)]
pub struct Store {
    db: Arc<NostrLmdb>,
}

impl Store {
    pub async fn open() -> NostrResult<Self> {
        make_store_dir(&config::events_dir()).map_err(|e| DbError::other(e.to_string()))?;
        // A staging dir from a crashed vacuum is debris — unless the
        // state file says a deferred walk is mid-copy (D87): the walk
        // resumes from its cursor instead of restarting from scratch
        // (a session shorter than the copy would never converge). A
        // `complete` state means the boot gate already adopted (the
        // gate runs first); leftover staging there is stale.
        // (events.prev is NOT cleared here: only after the store below
        // OPENS — clearing first let a failed open delete the previous
        // generation, the only recoverable copy (D84 audit).)
        let pending = read_vacuum_state(&config::events_dir())
            .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(str::to_string))
            .as_deref()
            == Some("copying");
        if !pending {
            retire_dir(&config::events_dir().with_extension("vacuum"));
        }
        // The prev delete is conditional (D89 audit): a half-finished
        // swap leaves a just-created live dir beside prev, the only
        // recoverable generation — deleting it unconditionally turns a
        // failed open's fresh store into unrecoverable data loss.
        let had_store = std::fs::metadata(config::events_dir().join("data.mdb"))
            .map(|m| m.len())
            .unwrap_or(0)
            > 0;
        if !had_store {
            // Same half-swap, self-heal: restore the parked generation
            // as live rather than boot onto a fresh dir beside it. The
            // live dir is storeLESS (no data.mdb) but may not be EMPTY —
            // a schema stamp, a half-made staging shell (the audit's
            // double-vacuum trace) — and rename onto a non-empty dir
            // fails with ENOTEMPTY, silently stranding the recovery.
            // Whatever is there without a store is junk: clear it first.
            let prev = config::events_dir().with_extension("prev");
            if prev.join("data.mdb").exists() {
                let _ = std::fs::remove_dir_all(config::events_dir());
                let _ = std::fs::rename(&prev, config::events_dir());
            }
        }
        // The schema backstop (D100): the boot gate runs first in the
        // app and refuses a downgrade there; this repeat check guards
        // every other open path (tests, a future embedder) with the
        // same rule — never open a newer store than this binary speaks.
        // Outdated refuses TOO (five-lens audit Nov 2026): a failed
        // boot-gate migration falls back to Store::open, which must not
        // quietly open the unmigrated store the gate just refused.
        match check_schema(&config::events_dir()) {
            SchemaCheck::Current | SchemaCheck::Fresh => {}
            SchemaCheck::TooNew(version) => {
                return Err(DbError::other(format!(
                    "store schema v{version} is newer than this Satori (v{CODE_SCHEMA}); update Satori, or sign in fresh"
                )));
            }
            SchemaCheck::Outdated(version) => {
                return Err(DbError::other(format!(
                    "store schema v{version} needs a migration; start Satori once (the boot gate migrates), or remove the store to sign in fresh"
                )));
            }
        }
        let db = NostrLmdb::open(config::events_dir()).await?;
        if had_store {
            // The rebuilt store opened: the previous generation is dead now.
            // Retired, not deleted (D122): deleting a 1.9 GB file in 357k
            // extents took 16.6 s here, before the first paint.
            retire_dir(&config::events_dir().with_extension("prev"));
        }
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
        // The whole visible catalog (visible_kinds honors the content prefs).
        self.feed_page_kinds(authors, until, limit, visible_kinds())
            .await
    }

    /// The feed page's look-back windows (D117), narrowest first; `None` is
    /// the unbounded query, the last resort.
    const FEED_WINDOWS: &[Option<u64>] = &[Some(86_400), Some(7 * 86_400), None];

    /// feed_page over an explicit kind set — the articles surface (30023
    /// only) reads through the same chokepoint with a narrower filter.
    pub async fn feed_page_kinds(
        &self,
        authors: &[PublicKey],
        until: Option<u64>,
        limit: usize,
        kinds: Vec<Kind>,
    ) -> NostrResult<Vec<Event>> {
        // D117: windowed first. The authors x kinds index scan reads up
        // to `limit` event bodies PER PAIR before trimming to the newest
        // `limit`: with 258 follows that is thousands of random reads,
        // seconds on a cold page cache. A `since` bound stops each pair at
        // the window edge. When the window holds at least `limit` events
        // the result is exactly the unbounded query's (anything older
        // ranks below them); otherwise the next, wider window runs.
        let base = until.unwrap_or_else(|| Timestamp::now().as_secs());
        let mut events: Vec<Event> = Vec::new();
        for window in Self::FEED_WINDOWS {
            let mut filter = Filter::new()
                .kinds(kinds.clone())
                .authors(authors.to_vec())
                .limit(limit);
            if let Some(ts) = until {
                filter = filter.until(Timestamp::from(ts));
            }
            if let Some(w) = window {
                filter = filter.since(Timestamp::from(base.saturating_sub(*w)));
            }
            events = self.query(filter).await?.into_iter().collect();
            if events.len() >= limit {
                break;
            }
        }
        // BTreeSet orders by id; the feed needs created_at order. Machine
        // notes drop here — every feed path crosses this chokepoint.
        let drop_machines = machine_notes_hidden();
        events.retain(|e| !(drop_machines && is_machine_note(e)));
        events.sort_by_key(|e| std::cmp::Reverse(e.created_at.as_secs()));
        Ok(events)
    }

    /// Replies to a note: kind 1s whose `e` tags reference it (NIP-10).
    pub async fn replies(&self, id: EventId) -> NostrResult<Vec<Event>> {
        let filter = Filter::new().kind(Kind::TextNote).event(id);
        let mut events: Vec<Event> = self.query(filter).await?.into_iter().collect();
        let drop_machines = machine_notes_hidden();
        events.retain(|e| !(drop_machines && is_machine_note(e)));
        events.sort_by_key(|e| e.created_at.as_secs());
        Ok(events)
    }

    /// Replies to ANY of the notes: same shape as `replies`, one index
    /// pass for the whole id set. The notifications cached phase ran one
    /// query per category PER NOTE — up to 300 sequential LMDB
    /// transactions for a 100-note window; these batch it to three.
    pub async fn replies_of(&self, ids: Vec<EventId>) -> NostrResult<Vec<Event>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let filter = Filter::new().kind(Kind::TextNote).events(ids);
        let mut events: Vec<Event> = self.query(filter).await?.into_iter().collect();
        let drop_machines = machine_notes_hidden();
        events.retain(|e| !(drop_machines && is_machine_note(e)));
        events.sort_by_key(|e| e.created_at.as_secs());
        Ok(events)
    }

    /// Kind-5 delete requests from the given authors (NIP-09). Synced
    /// alongside the feed kinds (deletions are rare); the render filter
    /// matches their `e` tags against displayed events.
    pub async fn deletions(&self, authors: &[PublicKey]) -> NostrResult<Vec<Event>> {
        let filter = Filter::new()
            .kind(Kind::EventDeletion)
            .authors(authors.to_vec())
            .limit(1000);
        Ok(self.query(filter).await?.into_iter().collect())
    }

    /// Every kind:5 the store holds (the deletions module's one-time
    /// hydration scan; tombstones persist in the module's own file past
    /// this store's retention).
    pub async fn tombstones(&self) -> NostrResult<Vec<Event>> {
        let filter = Filter::new().kind(Kind::EventDeletion).limit(5000);
        Ok(self.query(filter).await?.into_iter().collect())
    }

    /// NIP-22 comments addressing the event (kind 1111, any `e` tag —
    /// parent or root scope). Oldest first.
    pub async fn comments(&self, id: EventId) -> NostrResult<Vec<Event>> {
        let filter = Filter::new().kind(Kind::Custom(1111)).event(id);
        let mut events: Vec<Event> = self.query(filter).await?.into_iter().collect();
        events.sort_by_key(|e| e.created_at.as_secs());
        Ok(events)
    }

    /// The batched twin: NIP-22 comments addressing ANY of the notes
    /// (kind 1111, any `e` tag — parent or root scope). Oldest first.
    pub async fn comments_of(&self, ids: Vec<EventId>) -> NostrResult<Vec<Event>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let filter = Filter::new().kind(Kind::Custom(1111)).events(ids);
        let mut events: Vec<Event> = self.query(filter).await?.into_iter().collect();
        events.sort_by_key(|e| e.created_at.as_secs());
        Ok(events)
    }

    /// Engagement-not-addressing-a-reply candidates for the notifications
    /// cached phase: reactions (7), zaps (9735), poll votes (1018) whose
    /// `e` tag targets the note. The cached phase was replies+comments
    /// only, so a cold start's instant paint missed every zap/reaction/
    /// vote until the relays answered a beat later.
    pub async fn engagement_notifs(&self, id: EventId) -> NostrResult<Vec<Event>> {
        let filter = Filter::new()
            .kinds([Kind::Reaction, Kind::Custom(9735), Kind::Custom(1018)])
            .event(id);
        Ok(self.query(filter).await?.into_iter().collect())
    }

    /// The batched twin: engagement for ANY of the notes, one pass.
    pub async fn engagement_of(&self, ids: Vec<EventId>) -> NostrResult<Vec<Event>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let filter = Filter::new()
            .kinds([Kind::Reaction, Kind::Custom(9735), Kind::Custom(1018)])
            .events(ids);
        Ok(self.query(filter).await?.into_iter().collect())
    }

    /// The rest of the notifications cached phase (D113): bare mentions
    /// (#p=me: notes, comments, zaps, reactions), reposts of my notes, and
    /// quotes of them (`q` tags). The instant paint read only replies and
    /// engagement on my notes, so a fresh mention (the very thing that lit
    /// the bell, already persisted by the live sub) waited for the relay
    /// round. Three index passes; `limit` bounds the mention pass.
    pub async fn notif_extras(
        &self,
        me: PublicKey,
        ids: Vec<EventId>,
        limit: usize,
    ) -> NostrResult<Vec<Event>> {
        let mut out: Vec<Event> = self
            .query(
                Filter::new()
                    .kinds([
                        Kind::TextNote,
                        Kind::Custom(1111),
                        Kind::Custom(9735),
                        Kind::Reaction,
                    ])
                    .pubkey(me)
                    .limit(limit),
            )
            .await?
            .into_iter()
            .collect();
        if !ids.is_empty() {
            out.extend(
                self.query(
                    Filter::new()
                        .kinds([Kind::Repost, Kind::Custom(16)])
                        .events(ids.clone())
                        .limit(limit),
                )
                .await?,
            );
            let quote_ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
            out.extend(
                self.query(
                    Filter::new()
                        .kinds([Kind::TextNote, Kind::Custom(1111)])
                        .custom_tags(SingleLetterTag::LOWERCASE_Q, quote_ids)
                        .limit(limit),
                )
                .await?,
            );
        }
        let drop_machines = machine_notes_hidden();
        out.retain(|e| !(drop_machines && is_machine_note(e)));
        Ok(out)
    }

    /// Latest kind-0 metadata for each of the given authors, from the cache.
    pub async fn profiles(&self, authors: &[PublicKey]) -> NostrResult<Vec<Event>> {
        let filter = Filter::new().kind(Kind::Metadata).authors(authors.to_vec());
        Ok(self.query(filter).await?.into_iter().collect())
    }

    /// Latest kind-10002 relay list per author, batched: one query per 200
    /// authors instead of one per author. nostr-lmdb applies `limit` per
    /// author-kind cursor pair, so limit(1) here is exactly "newest list
    /// per author", the same semantics the per-author lookup had, minus
    /// the round trips.
    pub async fn relay_lists(&self, authors: &[PublicKey]) -> NostrResult<Vec<Event>> {
        let mut out = Vec::new();
        for chunk in authors.chunks(200) {
            let filter = Filter::new()
                .kind(Kind::RelayList)
                .authors(chunk.to_vec())
                .limit(1);
            out.extend(self.query(filter).await?);
        }
        Ok(out)
    }

    /// Contact lists (kind 3) for a set of authors, newest per author —
    /// the WoT graph's source data (wot.rs rebuilds from these at
    /// bootstrap; same per-author cursor quirk as `relay_lists`).
    pub async fn contact_lists(&self, authors: &[PublicKey]) -> NostrResult<Vec<Event>> {
        let mut out = Vec::new();
        for chunk in authors.chunks(200) {
            let filter = Filter::new()
                .kind(Kind::ContactList)
                .authors(chunk.to_vec())
                .limit(1);
            out.extend(self.query(filter).await?);
        }
        Ok(out)
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

    /// Latest ADDRESSABLE event of one kind by one author with a d tag
    /// (NIP-51 lists: the public bookmarks kind 30001 d="bookmarks").
    pub async fn latest_by_kind_d(
        &self,
        author: PublicKey,
        kind: Kind,
        identifier: &str,
    ) -> NostrResult<Option<Event>> {
        let filter = Filter::new()
            .kind(kind)
            .author(author)
            .identifier(identifier)
            .limit(1);
        Ok(self.query(filter).await?.into_iter().next())
    }

    /// EVERY event of one kind by one author (NIP-51 named lists: one
    /// replaceable event per d — the set of lists, not a latest).
    pub async fn all_by_kind(&self, author: PublicKey, kind: Kind) -> NostrResult<Vec<Event>> {
        let filter = Filter::new().kind(kind).author(author);
        Ok(self.query(filter).await?.into_iter().collect())
    }

    /// The newest kind-1 note across the follows - the refresh 'since'
    /// probe and the quiet-dot poll's answer. The naive probe (a page over
    /// ALL visible kinds) materialized one event per author-kind pair -
    /// up to ~7,000 deserializations for 500 follows on every refresh AND
    /// every poll (audit perf H1: nostr-lmdb applies `limit` per pair).
    /// One kind bounds it at one event per follow; a kind-1-only newest
    /// can only UNDERestimate the feed's true newest, and a smaller
    /// `since` just widens the refetch window (the store dedups what
    /// comes back).
    pub async fn newest_created_at(&self, authors: &[PublicKey]) -> NostrResult<Option<u64>> {
        let page = self
            .feed_page_kinds(authors, None, 1, vec![Kind::TextNote])
            .await?;
        Ok(page.first().map(|e| e.created_at.as_secs()))
    }

    /// Prune (audit memory H-2): every event ever seen was kept forever -
    /// several MB/day, GB-class over a month of live saves. Feed content
    /// older than the retention window is deleted, paged through the kind
    /// index (kind-only queries stop at the batch limit; the per-pair
    /// limit quirk applies only to author+kind queries). Reference lists
    /// (kinds 0/3/10002) keep only the newest per author. LMDB reuses
    /// freed pages for new writes, so the file plateaus rather than
    /// shrinking; there is no compaction pass (the env handle stays
    /// inside nostr-lmdb).
    ///
    /// The window is the owner's lever (D99's Settings row; `retention_days`
    /// in prefs.json): absent = the tested 90-day default; the string
    /// "forever" turns the feed prune off entirely (the owner's archive
    /// choice). Reference-list hygiene runs either way - it is dedup, not
    /// retention. When note pins (kind 10001) gain a view, their targets
    /// join the protect set - the same view-over-the-store contract as
    /// bookmarks (the protect set is the BOOKMARKS' fix, v1.2).
    pub async fn prune(&self, protect: &std::collections::HashSet<EventId>) -> NostrResult<u32> {
        const PRUNE_BATCH: usize = 500;
        let mut deleted = 0u32;
        // Feed content: older than the retention window. Protected ids
        // (the own bookmark set — the notes the Bookmarks tab renders)
        // survive: the tab is a VIEW over these, and a pruned bookmark is
        // an empty tab whenever the indexers have a slow day. A batch that
        // is ENTIRELY protected breaks the page loop (the same query would
        // repeat forever) — the next daily pass picks up past it as newer
        // content ages in.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Some(days) = retention_days() {
            let cutoff = Timestamp::from_secs(now.saturating_sub(days * 86400));
            for kind in FEED_KINDS.iter().chain(std::iter::once(&Kind::Reaction)) {
                loop {
                    let filter = Filter::new().kind(*kind).until(cutoff).limit(PRUNE_BATCH);
                    let batch = self.query(filter).await?;
                    if batch.is_empty() {
                        break;
                    }
                    // One delete per batch (D121 review): per-id deletes
                    // were one write transaction and one disk sync EACH
                    // (1,585 in one session on the maintainer's store).
                    let ids: Vec<EventId> = batch
                        .iter()
                        .map(|ev| ev.id)
                        .filter(|id| !protect.contains(id))
                        .collect();
                    let deleted_here = ids.len();
                    if !ids.is_empty() {
                        self.db.delete(Filter::new().ids(ids)).await?;
                        deleted += deleted_here as u32;
                    }
                    if deleted_here == 0 || batch.len() < PRUNE_BATCH {
                        break;
                    }
                }
            }
        }
        // Reference lists: newest per author only - profiles(), the
        // contact cache and relay_lists() read the newest; the history is
        // dead weight.
        for kind in [Kind::Metadata, Kind::ContactList, Kind::RelayList] {
            let all = self.query(Filter::new().kind(kind)).await?;
            let mut newest: HashMap<PublicKey, Option<(u64, EventId)>> = HashMap::new();
            let mut stale: Vec<EventId> = Vec::new();
            for ev in &all {
                let t = ev.created_at.as_secs();
                let slot = newest.entry(ev.pubkey).or_default();
                match *slot {
                    None => *slot = Some((t, ev.id)),
                    Some((pt, _)) if pt >= t => stale.push(ev.id),
                    Some((_, pid)) => {
                        stale.push(pid);
                        *slot = Some((t, ev.id));
                    }
                }
            }
            for chunk in stale.chunks(PRUNE_BATCH) {
                self.db
                    .delete(Filter::new().ids(chunk.iter().copied()))
                    .await?;
                deleted += chunk.len() as u32;
            }
        }
        Ok(deleted)
    }

    /// The user's own reactions (engagement set source).
    pub async fn own_reactions(&self, author: PublicKey) -> NostrResult<Vec<Event>> {
        let filter = Filter::new()
            .kind(Kind::Reaction)
            .author(author)
            .limit(5000);
        Ok(self.query(filter).await?.into_iter().collect())
    }

    /// The user's own reposts (NIP-18 kinds 6 + 16).
    pub async fn own_reposts(&self, author: PublicKey) -> NostrResult<Vec<Event>> {
        let filter = Filter::new()
            .author(author)
            .kinds([Kind::Repost, Kind::Custom(16)])
            .limit(5000);
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

    /// The store's event count (D99's Settings readout). One index walk
    /// over ids, no materialized events — the H-2 lesson's shape.
    pub async fn event_count(&self) -> NostrResult<u64> {
        self.db.count(Filter::new()).await.map(|c| c as u64)
    }

    async fn query(&self, filter: Filter) -> NostrResult<BTreeSet<Event>> {
        // Slow-read probe (D124): during the boot minute, a store read
        // over 150 ms is logged with its shape (kinds, counts, never
        // content). Outside that window it costs nothing.
        if !crate::diag::booting() {
            return self.db.query(filter).await;
        }
        let summary = format!(
            "kinds={:?} authors={} ids={} tags={:?} since={} limit={:?}",
            filter
                .kinds
                .as_ref()
                .map(|k| k.iter().map(|k| k.as_u16()).collect::<Vec<_>>()),
            filter.authors.as_ref().map_or(0, |a| a.len()),
            filter.ids.as_ref().map_or(0, |a| a.len()),
            filter
                .generic_tags
                .iter()
                .map(|(t, v)| format!("{t}:{}", v.len()))
                .collect::<Vec<_>>(),
            filter.since.is_some(),
            filter.limit,
        );
        let t = std::time::Instant::now();
        let out = self.db.query(filter).await;
        let ms = t.elapsed().as_millis();
        if ms >= 150 {
            crate::boot_mark!(
                "slow store query {ms}ms ({} rows): {summary}",
                out.as_ref().map_or(0, |o| o.len())
            );
        }
        out
    }
}

// --- No copy-on-write on btrfs (D119) ---------------------------------
//
// LMDB rewrites pages in place; on a copy-on-write filesystem every such
// write lands somewhere new. The maintainer's 1.5 GB store was in
// 357,813 extents, and a cold read while writes were committing stalled
// on the extent churn: first feed page 3.0-9.8 s against 0.43-0.99 s
// for the same store with NOCOW. The attribute only applies to files
// created after it is set, so store directories get it at creation and
// an existing copy-on-write store is rewritten once (the deferred
// rebuild, triggered by the boot gate).

/// statfs magic for btrfs.
const BTRFS_MAGIC: i64 = 0x9123_683E;
/// FS_NOCOW_FL (linux/fs.h).
const NOCOW_FL: libc::c_int = 0x0080_0000;
// _IOR('f', 1, long) / _IOW('f', 2, long) on 64-bit Linux.
const FS_IOC_GETFLAGS: u64 = 0x8008_6601;
const FS_IOC_SETFLAGS: u64 = 0x4008_6602;

/// Is `path` on btrfs? (false when statfs fails.)
fn on_btrfs(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: c is a valid NUL-terminated path, st a writable statfs.
    let rc = unsafe { libc::statfs(c.as_ptr(), &mut st) };
    rc == 0 && st.f_type as i64 == BTRFS_MAGIC
}

/// The ioctl numbers above are the asm-generic 64-bit encoding: right on
/// the released architectures (x86_64, aarch64) and the other generic
/// 64-bit ones. Elsewhere (32-bit, powerpc, mips, sparc encode
/// differently) the NOCOW path is compiled out and stores stay as the
/// filesystem makes them.
const NOCOW_SUPPORTED: bool = cfg!(all(
    target_os = "linux",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )
));

/// The inode flags of `path` (file or directory), if readable.
fn inode_flags(path: &std::path::Path) -> Option<libc::c_int> {
    if !NOCOW_SUPPORTED {
        return None;
    }
    use std::os::fd::AsRawFd as _;
    let f = std::fs::File::open(path).ok()?;
    let mut flags: libc::c_int = 0;
    // SAFETY: FS_IOC_GETFLAGS writes one int through the pointer.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_GETFLAGS as libc::Ioctl, &mut flags) };
    (rc == 0).then_some(flags)
}

/// Is `path` marked NOCOW?
fn is_nocow(path: &std::path::Path) -> bool {
    inode_flags(path).is_some_and(|f| f & NOCOW_FL != 0)
}

/// Best effort: mark `dir` NOCOW so files created in it inherit the
/// flag. Silent no-op off btrfs or when the ioctl is refused.
fn mark_nocow(dir: &std::path::Path) {
    use std::os::fd::AsRawFd as _;
    if !NOCOW_SUPPORTED || !on_btrfs(dir) {
        return;
    }
    let Some(flags) = inode_flags(dir) else {
        return;
    };
    if flags & NOCOW_FL != 0 {
        return;
    }
    let Ok(f) = std::fs::File::open(dir) else {
        return;
    };
    let new = flags | NOCOW_FL;
    // SAFETY: FS_IOC_SETFLAGS reads one int through the pointer.
    let _ = unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_SETFLAGS as libc::Ioctl, &new) };
}

/// Create a store directory (live or staging), NOCOW where it can be.
fn make_store_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    mark_nocow(dir);
    Ok(())
}

/// Does this store need the one-time NOCOW rewrite? On btrfs, the live
/// file is copy-on-write, and the flag demonstrably sticks on its
/// directory (so the rewrite will produce a NOCOW file: no loop where
/// the flag can't be set).
fn needs_nocow_rewrite(dir: &std::path::Path) -> bool {
    let file = dir.join("data.mdb");
    if !NOCOW_SUPPORTED || !on_btrfs(dir) || !file.exists() || is_nocow(&file) {
        return false;
    }
    mark_nocow(dir);
    is_nocow(dir)
}

// --- The vacuum (WS3, issue #23) --------------------------------------
//
// LMDB never returns dead pages: deletions and the daily prune free
// records, but data.mdb's mapped extent only ever grows (67MB as of the
// 0.5.0 tag). nostr-lmdb 0.45.3 exposes no compaction hook — the env
// handle is private — so the shape is a REBUILD: copy every surviving
// event into a fresh store directory, verify the counts, then atomically
// swap the directories. The previous store is kept as `events.prev`
// until the next successful boot (Store::open removes it), so a vacuum
// that produced a bad store is recoverable by hand.
//
// Deliberately offline and explicit: `satori-desktop --vacuum` runs the
// rebuild INSTEAD of the app (two writers on one LMDB is corruption),
// prints the report, and exits. The boot gate (`boot_gate`, below) used
// to run the same rebuild inline BEFORE the app opened the store — the
// waited-for trigger fired Nov 19 2026 (296.5 MB -> 482 MB in one week;
// the plateau theory was wrong, the weight is live heavyweight events
// and the extent only ever grows) — and the 0.10.0 test round killed
// that shape: the store doubled AGAIN in a day and the owner sat through
// a multi-minute launch. D87: the gate only STAGES the copy (staging dir
// + state file, instant); the walk runs in the background beside the
// app (spawned at Pool::open, reading through the live store's own
// handle — a second env on the same LMDB is how corruption happens);
// the NEXT boot adopts the finished copy: delta since the copy's
// cutoff, count verification, two renames. The launch cost is bounded
// by one session's writes, not the store's size.

/// What a vacuum moved: the event count and the physical store size
/// before and after (data.mdb's extent; the 32GB map is sparse, the
/// extent is the honest number).
#[derive(Debug, Clone, Copy)]
pub struct VacuumReport {
    pub events: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

pub async fn vacuum() -> Result<VacuumReport, String> {
    // The schema gate (five-lens audit Nov 2026): the CLI door reads,
    // copies, and swaps — it must never swap an Outdated store's
    // pre-migration copy live (the older binary that follows would
    // retroactively stamp it and open it: the silent downgrade D100
    // refuses everywhere else), nor touch a TooNew one.
    match check_schema(&config::events_dir()) {
        SchemaCheck::Outdated(from) => Err(format!(
            "store schema v{from} needs a migration this tool cannot run: start Satori once (the boot gate migrates), then vacuum"
        )),
        SchemaCheck::TooNew(version) => Err(format!(
            "store schema v{version} is newer than this Satori (v{CODE_SCHEMA}); update Satori"
        )),
        _ => {
            // The CLI caller (--vacuum) prints the report and exits — it never
            // opens the store in this process, so the swapped-in handle is
            // dropped here. (A caller that keeps RUNNING must use the returned
            // store: see the heed path-cache note on migrate_with.)
            vacuum_at(&config::events_dir()).await.map(|(r, _)| r)
        }
    }
}

/// The rebuild itself, against explicit paths: `dir` is the live store
/// (`events/`), the staging copy goes to `events.vacuum` beside it, and
/// on success `dir` becomes `events.prev` while the staging copy takes
/// its name. Same-filesystem renames: atomic, no copy.
/// The full copy walk, shared by the vacuum and the schema migration:
/// every event, copied from the source env into the staging env in
/// created_at windows walking BACKWARDS from now (the hot 90 days
/// first, then bookmark-preserved elders). The walk runs to the epoch —
/// an earlier draft stopped after eight consecutive empty windows
/// (~8 months), which silently dropped anything older the prune had
/// preserved (a bookmark past the stop vanished with nothing else in
/// range; D84 audit). The empty-tail queries cost microseconds against
/// the created_at index — ~640 windows to the epoch, capped by
/// [lo == 0] below. The transform applies per event (the vacuum passes
/// identity; the migration re-encodes). Returns events SAVED (drops
/// by the transform are excluded).
async fn walk_copy_all(
    source: &NostrLmdb,
    fresh: &NostrLmdb,
    transform: &(dyn Fn(Event) -> Option<Event> + Send + Sync),
) -> Result<usize, String> {
    const WINDOW: u64 = VACUUM_WINDOW;
    let now = Timestamp::now().as_secs();
    let mut copied = 0usize;
    let mut hi = now;
    loop {
        let lo = hi.saturating_sub(WINDOW - 1);
        // copy_range IS the D89 discipline (five-lens audit Nov 2026):
        // the window pages down to COPY_PAGE chunks before any query
        // materializes, and saves drain in COPY_SAVE_BATCH transactions
        // — the whole window decoded at once, fsynced per event, was
        // the pre-D89 shape on a 1.3 GB store.
        copy_range(source, fresh, lo, hi, transform, &mut copied).await?;
        if lo == 0 {
            break;
        }
        hi = lo - 1;
    }

    // The windows tile [0, now); an event with a created_at AHEAD of the
    // local clock (one relay or client with a skewed time is enough) is
    // counted by the total above but covered by no window, and the verify
    // below would refuse the swap over it - the 0.5.0 vacuum aborted on
    // exactly this, one future-dated event among 25,544. Catch the tail.
    copy_range(
        source,
        fresh,
        now + 1,
        now + 10 * 365 * 24 * 3600,
        transform,
        &mut copied,
    )
    .await?;
    Ok(copied)
}

async fn vacuum_at(dir: &std::path::Path) -> Result<(VacuumReport, Store), String> {
    let staging = dir.with_extension("vacuum"); // events/ -> events.vacuum/
    let prev = dir.with_extension("prev");
    let bytes_of = |d: &std::path::Path| {
        std::fs::metadata(d.join("data.mdb"))
            .map(|m| m.len())
            .unwrap_or(0)
    };
    let bytes_before = bytes_of(dir);

    // Staging from a crashed vacuum is debris: start clean. A stale
    // events.prev goes too, BEFORE the copy (D84 audit): prev is the
    // previous-previous generation by definition (the live store
    // supersedes it, the boot clears it anyway) — keeping it through the
    // copy let three store copies coexist (~3x the extent transiently,
    // and disk-full during the copy is exactly the failure that makes
    // the gated rebuild retry forever).
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| e.to_string())?;
    }
    if prev.exists() {
        std::fs::remove_dir_all(&prev).map_err(|e| e.to_string())?;
    }
    // The explicit vacuum supersedes any deferred-walk bookkeeping: its
    // own run replaces the staging copy wholesale.
    let _ = std::fs::remove_file(dir.with_extension("vacuum.json"));
    make_store_dir(&staging).map_err(|e| e.to_string())?;

    // Both envs open here — the caller's contract is that the app is
    // closed (the --vacuum flag runs instead of it), so nothing else
    // holds the live store.
    let source = NostrLmdb::open(dir).await.map_err(|e| e.to_string())?;
    let fresh = NostrLmdb::open(&staging).await.map_err(|e| e.to_string())?;

    let total = source
        .count(Filter::new())
        .await
        .map_err(|e| e.to_string())?;

    // The full copy walk (identity transform): every event, hot window
    // first, future-dated tail caught. The verify trio below is the
    // honesty gate: nothing lost, nothing gained, nothing wrote mid-run.
    let copied = walk_copy_all(&source, &fresh, &|event| Some(event)).await?;

    // The honesty checks: nothing lost, nothing gained - and if the
    // store moved under us, say so instead of crying copy error. Any
    // abort here leaves the live store untouched; the staging dir is
    // debris for the next run (or boot) to clear.
    let fresh_count = fresh
        .count(Filter::new())
        .await
        .map_err(|e| e.to_string())?;
    if fresh_count != copied {
        // Unreachable in practice: a failed save aborts before this.
        return Err(format!(
            "vacuum aborted: staging holds {fresh_count} events but {copied} were copied"
        ));
    }
    // The source was counted at the start; if it holds a different
    // count now, something wrote mid-run (the app opened during the
    // vacuum and saved live traffic - the second attempt on the live
    // store died exactly there, +2 events). Swapping would drop the
    // newcomers, so refuse and say what to do about it.
    let source_now = source
        .count(Filter::new())
        .await
        .map_err(|e| e.to_string())?;
    if source_now != total {
        return Err(format!(
            "vacuum aborted: the store changed during the run ({total} events at the start, {source_now} now) - Satori was open? Close it and run the vacuum again"
        ));
    }

    // Durability before the swap: the LMDB commits are synced; the
    // directory entries are not until we say so.
    for d in [&staging, dir, staging.parent().unwrap_or(&staging)] {
        std::fs::File::open(d)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
    }
    swap_staged(dir, &staging)?;

    // The stamp rides the swap (see adopt_staged): the live dir never
    // sits unstamped after a generation change.
    write_schema_version(dir, CODE_SCHEMA);
    // The gate measures growth FROM the last rebuild, manual or automatic.
    record_watermark(dir, bytes_of(dir));

    let report = VacuumReport {
        events: total,
        bytes_before,
        bytes_after: bytes_of(dir),
    };
    Ok((
        report,
        Store {
            db: Arc::new(fresh),
        },
    ))
}

/// The swap half of every rebuild: the live store parks as
/// `events.prev`, the staging copy takes its name. Same-filesystem
/// renames: atomic, no copy. The caller synced the directories first.
fn swap_staged(dir: &std::path::Path, staging: &std::path::Path) -> Result<(), String> {
    let prev = dir.with_extension("prev");
    retire_dir(&prev);
    if prev.exists() {
        std::fs::remove_dir_all(&prev).map_err(|e| e.to_string())?;
    }
    std::fs::rename(dir, &prev).map_err(|e| e.to_string())?;
    std::fs::rename(staging, dir).map_err(|e| e.to_string())?;
    if let Some(parent) = dir.parent() {
        let _ = std::fs::File::open(parent).and_then(|f| f.sync_all());
    }
    Ok(())
}

// --- The background copy (D121, replacing D87's walk) ------------------
//
// Store optimization never runs at startup (the owner, D87). In-session,
// a minute after the timeline is up, `background_compact` starts the
// vendored nostr-lmdb's write log and takes LMDB's native compacting
// copy into `events.vacuum/` (NOCOW on btrfs). The next launch's gate
// replays the log into the copy, checks counts and swaps
// (`adopt_journaled`); any doubt discards the copy and the live store
// opens (D120). D87's created_at delta could not see old-dated events
// arriving mid-session, so its adoption failed after real use.
//
// The state file (`events.vacuum.json`, `{state}`: requested | copying |
// complete) is the handoff between the session, the gate and
// `Store::open`'s debris policy.

/// The blocking vacuum's created_at window (30 days; D89 audit).
pub(crate) const VACUUM_WINDOW: u64 = 30 * 24 * 3600;

fn vacuum_state_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.with_extension("vacuum.json")
}

/// The state file's shape: `{state, cutoff, cursor, copied}` — state is
/// `copying` (a walk is pending/resumable) or `complete` (staging is a
/// full copy; the next boot adopts it). `None` = no file (or unparsable,
/// which is treated the same: nothing trusted).
fn read_vacuum_state(dir: &std::path::Path) -> Option<serde_json::Value> {
    std::fs::read_to_string(vacuum_state_path(dir))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

fn write_vacuum_state(dir: &std::path::Path, state: &str, cutoff: u64, cursor: u64, copied: usize) {
    let json = serde_json::json!({
        "state": state,
        "cutoff": cutoff,
        "cursor": cursor,
        "copied": copied,
    });
    if let Ok(json) = serde_json::to_string(&json) {
        let _ = crate::sidecar::try_write_atomic(&vacuum_state_path(dir), json.as_bytes());
    }
}

/// The retention window (D99's Settings lever; `retention_days` in
/// prefs.json). Absent = the tested 90-day default. The string "forever"
/// is None: the feed prune is off (the owner's archive choice). The clamp
/// keeps a hand-edited prefs.json from setting a 0-day churn or a
/// u64-overflow day count.
fn retention_days() -> Option<u64> {
    match std::fs::read_to_string(config::state_file(config::files::PREFS))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("retention_days").cloned())
    {
        Some(serde_json::Value::String(s)) if s == "forever" => None,
        Some(v) => Some(v.as_u64().map(|d| d.clamp(7, 3650)).unwrap_or(90)),
        None => Some(90),
    }
}

/// Schedule the verified store rebuild for the NEXT launch (D99's
/// Settings action; the explicit vacuum cannot run while the app holds
/// the env open). Writes the vacuum handoff state file: the walk spawns
/// beside the app at Pool::open (a missing staging dir starts the copy
/// from the top), the next boot's gate adopts it. Idempotent: a pending
/// or completed staging already counts as scheduled, and the yes-answer
/// is honest either way — the machinery owns the rest.
pub fn schedule_rebuild() -> Result<(), String> {
    let dir = config::events_dir();
    if !dir.join("data.mdb").exists() {
        return Err("no store yet".into());
    }
    if read_vacuum_state(&dir).is_some() {
        return Ok(()); // already asked, copying, or waiting for the handoff
    }
    // D121: the ask is picked up by the next session's background copy
    // (compaction_due), the handoff at the launch after that.
    write_vacuum_state(&dir, "requested", 0, 0, 0);
    Ok(())
}

// --- Schema versioning (D100) -------------------------------------------
//
// The store's biggest file carried no record of who wrote it or in what
// format. This section is the fix: a version stamp inside the store dir,
// checked at every boot, with a migration runner on the vacuum's rails.
//
// The invariant that orders everything: the schema check runs BEFORE the
// vacuum gate at every boot, so an adoption (which re-encodes through the
// SDK and would lose a beside-the-store stamp) only ever handles a
// current-schema store.

/// The store schema THIS binary writes and reads. v1 is retroactive:
/// every store the app ever wrote before versioning existed is v1 —
/// the first open under this machinery stamps such a store in place,
/// and nothing about today's formats changes.
pub const CODE_SCHEMA: u32 = 1;

/// The stamp lives INSIDE the store dir (`events/schema.json`), not
/// beside it: the vacuum's and the migration's swaps rename whole
/// directories, and a beside-the-store stamp would desync from the
/// generation it describes (a `prev` restore would read the new stamp
/// over the old store).
fn schema_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("schema.json")
}

fn read_schema_version(dir: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(schema_path(dir))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("version").and_then(|v| v.as_u64()))
        .map(|v| v as u32)
}

fn write_schema_version(dir: &std::path::Path, version: u32) {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(json) = serde_json::to_string(&serde_json::json!({ "version": version, "at": at })) {
        let _ = crate::sidecar::try_write_atomic(&schema_path(dir), json.as_bytes());
    }
}

/// The boot-time schema verdict. `Fresh`: no store yet (stamped on the
/// way through). `Current`: the stamp matches this binary. `Outdated`:
/// an older store this binary must migrate before opening. `TooNew`: a
/// NEWER store this binary must refuse — opening it would be the silent
/// corruption this exists to prevent.
#[derive(Debug, Clone, PartialEq)]
pub enum SchemaCheck {
    Fresh,
    Current,
    Outdated(u32),
    TooNew(u32),
}

pub fn check_schema(dir: &std::path::Path) -> SchemaCheck {
    check_schema_for(dir, CODE_SCHEMA)
}

/// The schema every store written before versioning existed speaks
/// (D100's retroactive stamp). NOT `CODE_SCHEMA`: an unstamped store is
/// a pre-0.12 store (or one whose stamp was lost), and stamping it with
/// whatever this binary speaks would skip every migration the day v2
/// lands (review, Nov 27 2026).
const UNSTAMPED_SCHEMA: u32 = 1;

/// `check_schema` against an explicit code schema (the tests drive a
/// future v2 binary through it).
fn check_schema_for(dir: &std::path::Path, code: u32) -> SchemaCheck {
    let version = match read_schema_version(dir) {
        Some(v) => v,
        None if dir.join("data.mdb").exists() => {
            // Retroactive stamp: an existing, unstamped store is v1.
            write_schema_version(dir, UNSTAMPED_SCHEMA);
            UNSTAMPED_SCHEMA
        }
        None => {
            // No store yet: it will be born in this binary's schema.
            write_schema_version(dir, code);
            return SchemaCheck::Fresh;
        }
    };
    match version {
        v if v == code => SchemaCheck::Current,
        v if v < code => SchemaCheck::Outdated(v),
        v => SchemaCheck::TooNew(v),
    }
}

/// One migration step: a per-event transform, applied while copying a
/// store into staging. `None` drops the event. Pure and testable — the
/// runner owns the copy, the counts, and the swap.
type MigrationStep = fn(Event) -> Option<Event>;

/// The real steps, in order: `MIGRATION_STEPS[i]` upgrades a v(i+1)
/// store to v(i+2). Empty today — v1 is the only schema, so `Outdated`
/// is unreachable until the first format change appends a step AND
/// bumps CODE_SCHEMA in the same commit.
static MIGRATION_STEPS: &[MigrationStep] = &[];

/// Migrate a store at `dir` from its stamped version to CODE_SCHEMA:
/// the vacuum's walk with the version's transform hooked in — full
/// copy into staging, the count verified against the source, the same
/// two-rename swap, the stamp advanced only after the swap lands. Any
/// abort leaves the live store and its old stamp untouched.
///
/// Returns the OPEN STORE built on the migrated env — never a fresh
/// `Store::open` at `dir`. heed caches envs by path in a process-global
/// registry, and a cache entry only leaves via an env handle we do not
/// hold (nostr-lmdb wraps it), so a reopen after the swap returns the
/// STALE env mapped to the renamed-away inode: the migration would be
/// invisible and the session's writes would land in the parked
/// generation. The env that walked the copy IS the store, now living
/// at `dir`.
async fn migrate_with(
    dir: &std::path::Path,
    from: u32,
    steps: &[MigrationStep],
) -> Result<(VacuumReport, Store), String> {
    if from + steps.len() as u32 != CODE_SCHEMA {
        return Err(format!(
            "no migration path from store schema v{from} to v{CODE_SCHEMA}"
        ));
    }
    let staging = dir.with_extension("migrate");
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| e.to_string())?;
    }
    make_store_dir(&staging).map_err(|e| e.to_string())?;
    let source = NostrLmdb::open(dir).await.map_err(|e| e.to_string())?;
    let fresh = NostrLmdb::open(&staging).await.map_err(|e| e.to_string())?;
    let total = source
        .count(Filter::new())
        .await
        .map_err(|e| e.to_string())?;
    let copied = walk_copy_all(&source, &fresh, &|event| {
        steps.iter().try_fold(event, |e, step| step(e))
    })
    .await?;
    // Nothing lost: the staging count is checked against the walked
    // count, and the source against ITS count at the start (the same
    // mid-run-write refusal the vacuum has; the boot-gate window makes
    // this a formality, but the check is free).
    let fresh_count = fresh
        .count(Filter::new())
        .await
        .map_err(|e| e.to_string())?;
    if fresh_count != copied {
        return Err(format!(
            "migration aborted: staging holds {fresh_count} events but {copied} were copied"
        ));
    }
    let source_now = source
        .count(Filter::new())
        .await
        .map_err(|e| e.to_string())?;
    if source_now != total {
        return Err(format!(
            "migration aborted: the store changed during the run ({total} events at the start, {source_now} now)"
        ));
    }
    for d in [&staging, dir] {
        std::fs::File::open(d)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
    }
    swap_staged(dir, &staging)?;
    write_schema_version(dir, CODE_SCHEMA);
    let report = VacuumReport {
        events: total,
        bytes_before: std::fs::metadata(dir.join("data.mdb"))
            .map(|m| m.len())
            .unwrap_or(0),
        bytes_after: std::fs::metadata(dir.join("data.mdb"))
            .map(|m| m.len())
            .unwrap_or(0),
    };
    Ok((
        report,
        Store {
            db: Arc::new(fresh),
        },
    ))
}

/// The real migration entry (the boot gate's; tests drive
/// `migrate_with` with fake steps).
async fn migrate(dir: &std::path::Path) -> Result<(VacuumReport, Store), String> {
    let from = read_schema_version(dir).unwrap_or(CODE_SCHEMA);
    migrate_with(dir, from, MIGRATION_STEPS).await
}

/// How long the launch may spend replaying a background copy's write log
/// before it gives up and opens the live store (D121).
const HANDOFF_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);

/// The gate's verdict. `Idle` is every ordinary boot; `Deferred` opens
/// the app immediately with the walk running beside it; `Swapped`
/// reports an adopted (or fallback-rebuilt) store; `Migrated` reports a
/// schema migration completed in this boot's exclusive window;
/// `Refused` is the downgrade stop — the store on disk is newer than
/// this binary, and the app must not open it. The swapped and migrated
/// variants carry the OPEN STORE built on the swapped-in env: heed
/// caches envs by path, so the process that swapped MUST adopt the
/// handle instead of reopening the path (the reopen returns the stale,
/// renamed-away env).
#[derive(Debug, Clone)]
pub enum GateAction {
    Idle,
    Deferred,
    Swapped(VacuumReport, Store),
    Migrated(VacuumReport, Store),
    Refused(u32),
}

/// Where retired store directories wait for the background sweep (D122):
/// beside them, so the move is a same-filesystem rename.
fn trash_dir_for(path: &std::path::Path) -> Option<std::path::PathBuf> {
    path.parent().map(|p| p.join("satori-trash"))
}

/// Move a (possibly huge) directory out of the way in O(1) (D122). Never
/// delete store-sized directories on the startup path: on btrfs a 1.9 GB
/// file in 357k extents took 16.6 s to unlink, before the first paint.
/// The rename is instant; [`sweep_trash`] deletes later, in the
/// background. Falls back to a direct delete only if the rename fails.
fn retire_dir(path: &std::path::Path) {
    if !path.exists() {
        return;
    }
    if let Some(trash) = trash_dir_for(path) {
        let _ = std::fs::create_dir_all(&trash);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "dir".into());
        if std::fs::rename(path, trash.join(format!("{name}-{nanos}"))).is_ok() {
            return;
        }
    }
    let _ = std::fs::remove_dir_all(path);
}

/// Delete everything retired so far (D122). Blocking: run it on a
/// blocking thread, after the timeline is up.
pub fn sweep_trash() {
    if let Some(trash) = trash_dir_for(&config::events_dir()) {
        let _ = std::fs::remove_dir_all(trash);
    }
}

/// The write log beside the store (D121): `S <id>` / `D <filter>` / `W`
/// lines appended by the vendored nostr-lmdb while a background copy
/// is pending, replayed into the copy at the next launch.
fn journal_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.with_extension("journal")
}

/// Drop every trace of a background copy: staging dir, state file,
/// write log. Used when a copy is interrupted, superseded or refused.
fn discard_staging(dir: &std::path::Path) {
    retire_dir(&dir.with_extension("vacuum"));
    let _ = std::fs::remove_file(vacuum_state_path(dir));
    let _ = std::fs::remove_file(journal_path(dir));
}

/// Should this session make the background copy (D121)? Nothing pending
/// already, and one of: the Settings "rebuild" ask, a copy-on-write
/// store on btrfs (D119), or growth past the watermark (the size
/// trigger the D87 walk used to own).
pub fn compaction_due() -> bool {
    let dir = config::events_dir();
    let state = read_vacuum_state(&dir)
        .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(str::to_string));
    match state.as_deref() {
        Some("requested") => return true,
        Some(_) => return false, // copying (this session) or complete (next launch)
        None => {}
    }
    let now_bytes = std::fs::metadata(dir.join("data.mdb"))
        .map(|m| m.len())
        .unwrap_or(0);
    if now_bytes == 0 {
        return false;
    }
    if needs_nocow_rewrite(&dir) {
        return true;
    }
    let base = std::fs::read_to_string(watermark_path(&dir))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("bytes").and_then(|b| b.as_u64()));
    base.is_some_and(|base| {
        now_bytes as f64 >= base as f64 * GATE_RATIO && now_bytes.saturating_sub(base) >= GATE_FLOOR
    })
}

/// The background copy (D121), run while the app is in use: start the
/// write log, then LMDB's native compacting copy (a consistent snapshot,
/// free pages dropped) into a fresh staging dir, NOCOW on btrfs. The
/// state file says `copying` until the snapshot is on disk, then
/// `complete`; the next launch's gate replays the log and swaps.
/// Nothing here touches the live file; any failure discards the copy.
pub async fn background_compact(store: &Store) -> Result<u64, String> {
    background_compact_at(store, &config::events_dir()).await
}

async fn background_compact_at(store: &Store, dir: &std::path::Path) -> Result<u64, String> {
    let staging = dir.with_extension("vacuum");
    discard_staging(dir);
    make_store_dir(&staging).map_err(|e| e.to_string())?;
    let now = Timestamp::now().as_secs();
    write_vacuum_state(dir, "copying", now, now, 0);
    let result = async {
        // The log first, then the snapshot: a write committed before the
        // snapshot is already in it, and replaying it again is a no-op.
        store
            .db
            .start_journal(&journal_path(dir))
            .map_err(|e| e.to_string())?;
        store
            .db
            .copy_compact_to(staging.join("data.mdb"))
            .await
            .map_err(|e| e.to_string())?;
        write_schema_version(&staging, CODE_SCHEMA);
        for d in [&staging, &dir.parent().unwrap_or(dir).to_path_buf()] {
            std::fs::File::open(d)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
        }
        Ok::<u64, String>(
            std::fs::metadata(staging.join("data.mdb"))
                .map(|m| m.len())
                .unwrap_or(0),
        )
    }
    .await;
    match result {
        Ok(bytes) => {
            write_vacuum_state(dir, "complete", now, now, 0);
            Ok(bytes)
        }
        Err(e) => {
            store.db.stop_journal();
            discard_staging(dir);
            Err(e)
        }
    }
}

/// The handoff at the next launch (D121), store closed: replay the write
/// log into the snapshot in order (saves copied by id from the live
/// store, deletes re-applied), require equal counts, then swap. A
/// session's worth of writes; seconds at most. Any doubt (a wipe in the
/// log, a torn line, a count mismatch) is an error and the caller
/// discards the copy: the live store always opens.
async fn adopt_journaled(dir: &std::path::Path) -> Result<(VacuumReport, Store), String> {
    let staging = dir.with_extension("vacuum");
    let bytes_of = |d: &std::path::Path| {
        std::fs::metadata(d.join("data.mdb"))
            .map(|m| m.len())
            .unwrap_or(0)
    };
    let bytes_before = bytes_of(dir);
    let log =
        std::fs::read_to_string(journal_path(dir)).map_err(|e| format!("no write log: {e}"))?;
    let source = NostrLmdb::open(dir).await.map_err(|e| e.to_string())?;
    let fresh = NostrLmdb::open(&staging).await.map_err(|e| e.to_string())?;

    // Saves batch until the next delete (order is kept across deletes;
    // within a batch the store's own rules, newest replaceable wins and
    // kind-5 deletes cover their targets, make the order immaterial).
    let mut saves: Vec<EventId> = Vec::new();
    // Runs of id-only deletes merge into one delete (one transaction):
    // deletes of distinct ids commute. Any other line ends the run.
    let mut deletes: Vec<EventId> = Vec::new();
    let mut replayed = 0usize;
    let id_only = |f: &Filter| {
        f.ids.is_some()
            && f.authors.is_none()
            && f.kinds.is_none()
            && f.search.is_none()
            && f.since.is_none()
            && f.until.is_none()
            && f.limit.is_none()
            && f.generic_tags.is_empty()
    };
    async fn flush_deletes(fresh: &NostrLmdb, deletes: &mut Vec<EventId>) -> Result<(), String> {
        for chunk in deletes.chunks(500) {
            fresh
                .delete(Filter::new().ids(chunk.iter().copied()))
                .await
                .map_err(|e| e.to_string())?;
        }
        deletes.clear();
        Ok(())
    }
    async fn flush(
        source: &NostrLmdb,
        fresh: &NostrLmdb,
        saves: &mut Vec<EventId>,
        replayed: &mut usize,
    ) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        saves.retain(|id| seen.insert(*id));
        for chunk in saves.chunks(500) {
            let events = source
                .query(Filter::new().ids(chunk.iter().copied()))
                .await
                .map_err(|e| e.to_string())?;
            // Concurrent submits: the ingester commits them as one batch.
            let results =
                futures_util::future::join_all(events.iter().map(|e| fresh.save_event(e))).await;
            for r in results {
                r.map_err(|e| e.to_string())?;
            }
            *replayed += events.len();
        }
        saves.clear();
        Ok(())
    }
    for line in log.lines() {
        if line.is_empty() {
            continue;
        }
        if line == "W" {
            return Err("the write log saw a wipe".into());
        }
        if let Some(id) = line.strip_prefix("S ") {
            flush_deletes(&fresh, &mut deletes).await?;
            saves.push(EventId::from_hex(id).map_err(|e| format!("bad log line: {e}"))?);
        } else if let Some(json) = line.strip_prefix("D ") {
            flush(&source, &fresh, &mut saves, &mut replayed).await?;
            let filter = Filter::from_json(json).map_err(|e| format!("bad log line: {e}"))?;
            if id_only(&filter) {
                deletes.extend(filter.ids.into_iter().flatten());
            } else {
                flush_deletes(&fresh, &mut deletes).await?;
                fresh.delete(filter).await.map_err(|e| e.to_string())?;
            }
        } else {
            return Err(format!("bad log line: {line:.40}"));
        }
    }
    flush_deletes(&fresh, &mut deletes).await?;
    flush(&source, &fresh, &mut saves, &mut replayed).await?;

    let fresh_count = fresh
        .count(Filter::new())
        .await
        .map_err(|e| e.to_string())?;
    let source_count = source
        .count(Filter::new())
        .await
        .map_err(|e| e.to_string())?;
    if fresh_count != source_count {
        return Err(format!(
            "the copy holds {fresh_count} events after replaying {replayed}, the live store {source_count}"
        ));
    }
    for d in [
        &staging,
        &dir.to_path_buf(),
        &dir.parent().unwrap_or(dir).to_path_buf(),
    ] {
        std::fs::File::open(d)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
    }
    swap_staged(dir, &staging)?;
    write_schema_version(dir, CODE_SCHEMA);
    let _ = std::fs::remove_file(vacuum_state_path(dir));
    let _ = std::fs::remove_file(journal_path(dir));
    record_watermark(dir, bytes_of(dir));
    let report = VacuumReport {
        events: fresh_count,
        bytes_before,
        bytes_after: bytes_of(dir),
    };
    // The swapped-in env IS the store (the heed path-cache rule): the
    // session runs on this handle instead of reopening the path.
    Ok((
        report,
        Store {
            db: Arc::new(fresh),
        },
    ))
}

/// The copy's memory cap (D89 audit): a copy chunk materializes its
/// events as decoded `Event` structs, and the decoded set is 1.5-2x its
/// serialized size. Ranges are split until each query holds at most
/// this many events.
const COPY_PAGE: u64 = 20_000;
/// Save batches: awaited one event at a time, nostr-lmdb's ingester
/// commits (and fsyncs) per event; a concurrent batch drains as one
/// transaction.
const COPY_SAVE_BATCH: usize = 128;

/// Copy every event in [lo, hi] into `fresh` (D89 audit): the range is
/// split recursively until each materialized chunk is under COPY_PAGE,
/// so the walk's RAM is bounded no matter how heavy one window is.
/// Saves are idempotent by id, so overlap and resumes are free. The
/// transform hooks the per-event re-encode (the migration); the
/// vacuum's and adoption's walks pass identity. `copied` counts SAVED
/// events — drops by the transform are excluded (walk_copy_all's
/// honesty check compares against the staging count).
async fn copy_range(
    src: &NostrLmdb,
    fresh: &NostrLmdb,
    lo: u64,
    hi: u64,
    transform: &(dyn Fn(Event) -> Option<Event> + Send + Sync),
    copied: &mut usize,
) -> Result<(), String> {
    let filter = Filter::new()
        .since(Timestamp::from(lo))
        .until(Timestamp::from(hi));
    let n: usize = src.count(filter.clone()).await.map_err(|e| e.to_string())?;
    if n == 0 {
        return Ok(());
    }
    if n as u64 > COPY_PAGE && lo < hi {
        let mid = lo + (hi - lo) / 2;
        Box::pin(copy_range(src, fresh, mid + 1, hi, transform, copied)).await?;
        Box::pin(copy_range(src, fresh, lo, mid, transform, copied)).await?;
        return Ok(());
    }
    let events = src.query(filter).await.map_err(|e| e.to_string())?;
    let mut saved = 0usize;
    for batch in events
        .into_iter()
        .collect::<Vec<_>>()
        .chunks(COPY_SAVE_BATCH)
    {
        let transformed: Vec<Event> = batch.iter().filter_map(|e| transform(e.clone())).collect();
        let saves = transformed.iter().map(|e| fresh.save_event(e));
        for result in futures_util::future::join_all(saves).await {
            result.map_err(|e| e.to_string())?;
        }
        saved += transformed.len();
    }
    *copied += saved;
    Ok(())
}

/// Relative growth (the extent is 1.5x the watermark's)...
const GATE_RATIO: f64 = 1.5;
/// ...AND absolute growth past this: the floor keeps a small store from
/// churning the rebuild path over noise.
const GATE_FLOOR: u64 = 64 * 1024 * 1024;

/// The watermark's path: beside the store (`events/` ->
/// `events.watermark.json`).
fn watermark_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.with_extension("watermark.json")
}

/// Record the gate's landing extent: measured FROM here.
fn record_watermark(dir: &std::path::Path, bytes: u64) {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(json) = serde_json::to_string(&serde_json::json!({ "bytes": bytes, "at": at })) {
        let _ = crate::sidecar::try_write_atomic(&watermark_path(dir), json.as_bytes());
    }
}

/// The rebuild is silent and slow — a GUI launch shows nothing but a
/// frozen icon, and a user who kills and relaunches pays the full copy
/// again (the watermark has not moved). Best-effort desktop notification
/// so a triggered rebuild explains itself (D84 audit). Absent
/// notify-send: a spawn error, ignored.
fn notify_rebuild(message: &str) {
    if cfg!(test) {
        return; // the gate tests run the real thresholds; no desktop spam
    }
    let _ = std::process::Command::new("notify-send")
        .args(["Satori", message])
        .spawn();
}

/// The boot-time gate: decide what this boot owes the store. `Idle` is
/// every ordinary boot; a trigger past the thresholds no longer
/// rebuilds inline (the owner, mid-rebuild: "the store optimization
/// shouldn't run at startup") — it stages the copy and defers the walk
/// to the app's own runtime; a boot that finds a COMPLETED staging copy
/// adopts it (delta + renames, bounded by one session's writes).
pub async fn boot_gate() -> Result<GateAction, String> {
    boot_gate_at(&config::events_dir(), GATE_RATIO, GATE_FLOOR).await
}

async fn boot_gate_at(dir: &std::path::Path, ratio: f64, floor: u64) -> Result<GateAction, String> {
    // Crash insurance first: the swap is two renames, and a crash
    // between them leaves the live store absent with the previous
    // generation parked. Restore it rather than boot onto an empty dir.
    let mut now_bytes = std::fs::metadata(dir.join("data.mdb"))
        .map(|m| m.len())
        .unwrap_or(0);
    if now_bytes == 0 {
        let prev = dir.with_extension("prev");
        if prev.join("data.mdb").exists() {
            std::fs::rename(&prev, dir).map_err(|e| e.to_string())?;
            now_bytes = std::fs::metadata(dir.join("data.mdb"))
                .map(|m| m.len())
                .unwrap_or(0);
        }
    }
    // The schema check (D100), BEFORE the vacuum gate: the adoption
    // path re-encodes through the SDK, so it must only ever see a
    // current-schema store. A downgrade is refused here, before any
    // env opens.
    match check_schema(dir) {
        SchemaCheck::TooNew(version) => return Ok(GateAction::Refused(version)),
        SchemaCheck::Outdated(_) => {
            // A pending vacuum handoff describes a PRE-migration copy:
            // adopting it after the migration would swap the old
            // encoding back in, and the retroactive re-stamp would call
            // the regressed store current (five-lens audit Nov 2026).
            // The migration IS a rebuild — the handoff is void, staging
            // dir and state file both (`copying` walks included).
            let _ = std::fs::remove_file(vacuum_state_path(dir));
            let _ = std::fs::remove_dir_all(dir.with_extension("vacuum"));
            let (report, store) = migrate(dir).await?;
            return Ok(GateAction::Migrated(report, store));
        }
        SchemaCheck::Fresh | SchemaCheck::Current => {}
    }
    if now_bytes == 0 {
        // No store yet (fresh install): nothing to gate.
        return Ok(GateAction::Idle);
    }

    let gate_started = std::time::Instant::now();
    // The background copy's handoff (D121). `complete`: the snapshot is
    // whole; replay the write log and swap. Anything else left behind
    // (an interrupted copy, a D87-era staging dir) is discarded. Never a
    // blocking rebuild here (D120): the live store always opens.
    match read_vacuum_state(dir)
        .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(str::to_string))
        .as_deref()
    {
        // The handoff has a hard budget (D121 review): it runs before the
        // window exists, and the first real one took 44 s (1,585 per-id
        // deletes replayed one transaction each). Past the budget the
        // copy is dropped and the live store opens. The swap itself is
        // synchronous after the last await, so the timeout can only cut
        // the replay, never a half-done rename.
        Some("complete") if journal_path(dir).exists() => {
            match tokio::time::timeout(HANDOFF_BUDGET, adopt_journaled(dir))
                .await
                .unwrap_or_else(|_| {
                    Err(format!(
                        "the handoff passed its {}s budget",
                        HANDOFF_BUDGET.as_secs()
                    ))
                }) {
                Ok((report, store)) => {
                    crate::dlog!(
                        "boot gate: background copy adopted in {} ms, {} events, data.mdb {:.1} MB -> {:.1} MB",
                        gate_started.elapsed().as_millis(),
                        report.events,
                        report.bytes_before as f64 / 1048576.0,
                        report.bytes_after as f64 / 1048576.0
                    );
                    return Ok(GateAction::Swapped(report, store));
                }
                Err(err) => {
                    crate::dlog!("boot gate: background copy refused ({err}); discarding it");
                    // A failed swap leaves the live dir at prev: restore it
                    // before anything opens (D89 audit).
                    if std::fs::metadata(dir.join("data.mdb"))
                        .map(|m| m.len())
                        .unwrap_or(0)
                        == 0
                        && dir.with_extension("prev").join("data.mdb").exists()
                    {
                        let _ = std::fs::remove_dir_all(dir);
                        let _ = std::fs::rename(dir.with_extension("prev"), dir);
                    }
                    discard_staging(dir);
                    record_watermark(dir, now_bytes);
                    return Ok(GateAction::Idle);
                }
            }
        }
        // The Settings ask waits for this session's copy.
        Some("requested") => return Ok(GateAction::Idle),
        Some(other) => {
            crate::dlog!("boot gate: discarding an unfinished store copy (state {other})");
            discard_staging(dir);
        }
        None => {
            // Debris without a state file (a crashed --vacuum).
            if dir.with_extension("vacuum").exists() || journal_path(dir).exists() {
                discard_staging(dir);
            }
        }
    }

    // First observation: the size trigger's baseline (compaction_due
    // reads it during the session).
    if !watermark_path(dir).exists() {
        record_watermark(dir, now_bytes);
    }
    let _ = (ratio, floor);
    Ok(GateAction::Idle)
}

/// Machine notes (filters.ts:104-109): a kind:1 whose ENTIRE content is a
/// bare JSON object/array — an app using kind:1 as a data transport (chess
/// moves, bots), never human discussion. The {/[ lead keeps prose that
/// merely CONTAINS JSON. Built-in default; user can switch it in Settings →
/// Content (machine_notes_hidden in prefs.json, default true).
///
/// Same class, desktop addition: a kind:1 whose entire content is a bare
/// PROFILE ref (npub/nprofile) — client follow-stamps carry no words and
/// rendered as near-blank cards. Event refs (note/nevent) are real quotes
/// and stay; profile refs are mentions, and a mention to nobody is noise.
pub fn is_machine_note(ev: &Event) -> bool {
    if ev.kind != Kind::TextNote {
        return false;
    }
    let t = ev.content.trim();
    if t.len() < 2 {
        return false;
    }
    if t.starts_with('{') || t.starts_with('[') {
        return serde_json::from_str::<serde_json::Value>(t)
            .map(|v| v.is_object() || v.is_array())
            .unwrap_or(false);
    }
    // One whitespace-free profile token, `nostr:` scheme or bare.
    t.chars().all(|c| !c.is_whitespace())
        && Nip19::from_bech32(t.strip_prefix("nostr:").unwrap_or(t))
            .is_ok_and(|n| matches!(n, Nip19::Pubkey(_) | Nip19::Profile(_)))
}

/// The pref, read through the config cache (one metadata syscall between
/// file changes; it used to be a full file parse per page).
pub fn machine_notes_hidden() -> bool {
    config::prefs()
        .and_then(|v| v.get("machine_notes_hidden").and_then(|h| h.as_bool()))
        .unwrap_or(true)
}

pub fn parse_key(hex: &str) -> Option<PublicKey> {
    PublicKey::from_str(hex).ok()
}

#[cfg(test)]
mod tests {
    /// D119: the NOCOW helpers are best effort. Off btrfs (CI tmpfs,
    /// ext4) nothing is marked and no rewrite is ever due; on btrfs a
    /// fresh store dir carries the flag and a file made in it inherits.
    #[test]
    fn nocow_dirs_are_best_effort() {
        let base = std::env::temp_dir().join(format!("satori-nocow-{}", std::process::id()));
        let dir = base.join("events");
        let _ = std::fs::remove_dir_all(&base);
        make_store_dir(&dir).unwrap();
        std::fs::write(dir.join("data.mdb"), b"x").unwrap();
        if NOCOW_SUPPORTED && on_btrfs(&dir) {
            assert!(is_nocow(&dir));
            assert!(is_nocow(&dir.join("data.mdb")));
        } else {
            assert!(!is_nocow(&dir.join("data.mdb")));
        }
        // A NOCOW file (or a non-btrfs one) never asks for the rewrite.
        assert!(!needs_nocow_rewrite(&dir));
        let _ = std::fs::remove_dir_all(&base);
    }

    use super::*;
    use crate::test_env::EnvGuard;

    fn note(author: &Keys, content: &str, ts: u64) -> Event {
        author
            .sign_event(UnsignedEvent::new(
                author.public_key(),
                Timestamp::from(ts),
                Kind::TextNote,
                vec![],
                content,
            ))
            .unwrap()
    }

    /// The vacuum's contract: every event survives into a fresh env,
    /// the counts agree, the swap is real, and the previous generation
    /// parks beside the store until the next boot clears it.
    #[test]
    fn vacuum_rebuilds_and_swaps() {
        let env = EnvGuard::new("store-vacuum");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let store_dir = config::events_dir();
        make_store_dir(&store_dir).unwrap();

        let db = rt.block_on(NostrLmdb::open(&store_dir)).unwrap();
        let keys = Keys::generate();
        let now = Timestamp::now().as_secs();
        // Two events past the 90-day prune window (bookmark-kept elders),
        // one inside it: the copy must span created_at windows. The last
        // one is DATED AHEAD of the clock (a skewed relay sent it): the
        // windows tile [0, now), so the vacuum needs an explicit tail
        // catch or the count-verify refuses the swap (issue #23).
        let events = vec![
            note(&keys, "elder one", now.saturating_sub(200 * 24 * 3600)),
            note(&keys, "elder two", now.saturating_sub(199 * 24 * 3600)),
            note(&keys, "fresh", now.saturating_sub(60)),
            note(&keys, "from a skewed clock", now + 3600),
        ];
        for event in &events {
            rt.block_on(db.save_event(event)).unwrap();
        }
        drop(db);

        let report = rt.block_on(vacuum()).unwrap();
        assert_eq!(report.events, 4);
        assert!(report.bytes_before > 0);
        assert!(report.bytes_after > 0);

        // The swap: the live path opens and holds every event by id.
        let reopened = rt.block_on(NostrLmdb::open(&store_dir)).unwrap();
        assert_eq!(rt.block_on(reopened.count(Filter::new())).unwrap(), 4);
        for event in &events {
            assert!(
                rt.block_on(reopened.event_by_id(&event.id))
                    .unwrap()
                    .is_some()
            );
        }
        drop(reopened);

        // The previous generation parks until Store::open clears it.
        assert!(store_dir.with_extension("prev").exists());
        assert!(!store_dir.with_extension("vacuum").exists());

        // A vacuum over a rebuilt store round-trips too (idempotent).
        let again = rt.block_on(vacuum()).unwrap();
        assert_eq!(again.events, 4);

        // Store::open is the boot door: it must clear events.prev.
        let _store = rt.block_on(Store::open()).unwrap();
        assert!(!store_dir.with_extension("prev").exists());
        drop(_store);
        drop(env);
    }

    /// The D84 regression: the copy walk must reach past empty windows.
    /// A note dated ~400 days ago, with nothing else in its range (the
    /// steady state after the 90-day prune keeps only bookmarked
    /// elders), must survive the rebuild — the old eight-empty-window
    /// stop dropped it silently.
    #[test]
    fn vacuum_reaches_past_empty_windows() {
        let env = EnvGuard::new("store-vacuum-old");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let store_dir = config::events_dir();
        make_store_dir(&store_dir).unwrap();
        let keys = Keys::generate();
        let old = Timestamp::now().as_secs() - 400 * 24 * 3600;
        let ancient = note(&keys, "bookmarked elder", old);
        let recent = note(&keys, "fresh", Timestamp::now().as_secs() - 60);
        let db = rt.block_on(NostrLmdb::open(&store_dir)).unwrap();
        rt.block_on(db.save_event(&ancient)).unwrap();
        rt.block_on(db.save_event(&recent)).unwrap();
        drop(db);

        let report = rt.block_on(vacuum()).unwrap();
        assert_eq!(report.events, 2);

        let reopened = rt.block_on(NostrLmdb::open(&store_dir)).unwrap();
        assert!(
            rt.block_on(reopened.event_by_id(&ancient.id))
                .unwrap()
                .is_some(),
            "a note 400 days back, past eight empty windows, must survive"
        );
        drop(reopened);
        drop(env);
    }

    /// Plant a small real store: a couple of events, env dropped (the
    /// gate needs exclusive access, like at boot). The caller holds the
    /// EnvGuard — taking it here too would self-deadlock on the
    /// non-reentrant env lock.
    fn planted_store(_tag: &str) -> (std::path::PathBuf, Vec<Event>) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let store_dir = config::events_dir();
        make_store_dir(&store_dir).unwrap();
        let db = rt.block_on(NostrLmdb::open(&store_dir)).unwrap();
        let keys = Keys::generate();
        let events = vec![note(&keys, "one", 1000), note(&keys, "two", 2000)];
        for event in &events {
            rt.block_on(db.save_event(event)).unwrap();
        }
        drop(db);
        (store_dir, events)
    }

    #[test]
    fn gate_first_observation_records_and_waits() {
        let env = EnvGuard::new("gate-first");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (store_dir, events) = planted_store("gate-first");

        // No watermark yet: record one, rebuild nothing.
        let action = rt.block_on(boot_gate_at(&store_dir, 1.5, 1024)).unwrap();
        assert!(matches!(action, GateAction::Idle));
        let watermark =
            std::fs::read_to_string(store_dir.with_extension("watermark.json")).unwrap();
        let base: serde_json::Value = serde_json::from_str(&watermark).unwrap();
        let extent = std::fs::metadata(store_dir.join("data.mdb")).unwrap().len();
        assert_eq!(base.get("bytes").unwrap().as_u64().unwrap(), extent);

        // A second boot under threshold: still nothing.
        let action = rt.block_on(boot_gate_at(&store_dir, 1.5, 1024)).unwrap();
        assert!(matches!(action, GateAction::Idle));
        drop(env);
        let _ = events;
    }

    /// D121: the background copy plus its write log. After the snapshot
    /// the session keeps writing, including an OLD-dated event (the case
    /// D87's created_at delta missed: profiles, back-scroll, replies to
    /// old threads) and a delete. The next launch replays the log into
    /// the copy, the counts agree, and the swapped-in store holds
    /// exactly what the live one held.
    #[test]
    fn background_copy_replays_session_writes_and_adopts() {
        let env = EnvGuard::new("bg-copy");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (store_dir, events) = planted_store("bg-copy");

        let store = rt.block_on(Store::open()).unwrap();
        rt.block_on(background_compact_at(&store, &store_dir))
            .unwrap();
        let state: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(store_dir.with_extension("vacuum.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state.get("state").unwrap().as_str().unwrap(), "complete");

        // The session goes on writing after the snapshot.
        let keys = Keys::generate();
        let old = note(&keys, "an old note fetched late", 500);
        let new = note(&keys, "a new note", 3000);
        rt.block_on(store.db.save_event(&old)).unwrap();
        rt.block_on(store.db.save_event(&new)).unwrap();
        rt.block_on(store.db.delete(Filter::new().id(events[0].id)))
            .unwrap();
        drop(store);

        let action = rt.block_on(boot_gate_at(&store_dir, 1.5, 1024)).unwrap();
        let GateAction::Swapped(report, store) = action else {
            panic!("a complete copy with its write log adopts, got {action:?}")
        };
        assert_eq!(report.events, 3);
        assert!(!store_dir.with_extension("vacuum").exists());
        assert!(!store_dir.with_extension("vacuum.json").exists());
        assert!(!store_dir.with_extension("journal").exists());
        let held = |id: EventId| rt.block_on(store.db.event_by_id(&id)).unwrap().is_some();
        assert!(held(old.id), "the old-dated late arrival made it across");
        assert!(held(new.id));
        assert!(held(events[1].id));
        assert!(!held(events[0].id), "the session's delete was replayed");
        drop(store);
        drop(env);
    }

    /// D120/D121: a copy that can't be trusted is discarded and the live
    /// store opens, never a blocking rebuild. A wipe in the log refuses
    /// the handoff; an interrupted copy (still `copying`) is debris.
    #[test]
    fn gate_discards_untrusted_copies_without_blocking() {
        let env = EnvGuard::new("gate-discard");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (store_dir, events) = planted_store("gate-discard");

        // A complete-looking copy whose log saw a wipe.
        make_store_dir(&store_dir.with_extension("vacuum")).unwrap();
        let db = rt
            .block_on(NostrLmdb::open(store_dir.with_extension("vacuum")))
            .unwrap();
        rt.block_on(db.save_event(&events[0])).unwrap();
        drop(db);
        write_vacuum_state(&store_dir, "complete", 0, 0, 0);
        std::fs::write(store_dir.with_extension("journal"), "W\n").unwrap();
        let action = rt.block_on(boot_gate_at(&store_dir, 1.5, 1024)).unwrap();
        assert!(matches!(action, GateAction::Idle));
        assert!(!store_dir.with_extension("vacuum").exists());
        assert!(!store_dir.with_extension("vacuum.json").exists());
        assert!(!store_dir.with_extension("journal").exists());

        // An interrupted copy.
        make_store_dir(&store_dir.with_extension("vacuum")).unwrap();
        write_vacuum_state(&store_dir, "copying", 0, 0, 0);
        let action = rt.block_on(boot_gate_at(&store_dir, 1.5, 1024)).unwrap();
        assert!(matches!(action, GateAction::Idle));
        assert!(!store_dir.with_extension("vacuum").exists());

        // The live store is whole.
        let live = rt.block_on(NostrLmdb::open(&store_dir)).unwrap();
        assert_eq!(rt.block_on(live.count(Filter::new())).unwrap(), 2);
        drop(live);
        drop(env);
    }

    #[test]
    fn gate_holds_under_threshold() {
        let env = EnvGuard::new("gate-hold");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (store_dir, _) = planted_store("gate-hold");

        // Watermark at the current extent: under ratio, no rebuild.
        let extent = std::fs::metadata(store_dir.join("data.mdb")).unwrap().len();
        std::fs::write(
            store_dir.with_extension("watermark.json"),
            format!(r#"{{"bytes": {extent}, "at": 0}}"#),
        )
        .unwrap();

        let action = rt.block_on(boot_gate_at(&store_dir, 1.5, 1024)).unwrap();
        assert!(matches!(action, GateAction::Idle));
        drop(env);
    }

    /// D100: the stamp round-trips; a pre-versioning store is stamped
    /// v1 in place (retroactive, no disruption); a NEWER stamp refuses
    /// at the gate before any env opens.
    #[test]
    fn schema_stamps_retroactively_and_refuses_downgrades() {
        let env = EnvGuard::new("schema-stamp");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (store_dir, _) = planted_store("schema-stamp");

        // No stamp: retroactive v1, and the verdict is Current (a store
        // exists and speaks this binary's schema).
        assert_eq!(check_schema(&store_dir), SchemaCheck::Current);
        let stamp: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(store_dir.join("schema.json")).unwrap())
                .unwrap();
        assert_eq!(stamp.get("version").unwrap().as_u64().unwrap(), 1);

        // A fresh boot over NO store: Fresh, stamped on the way through.
        let empty = env.dir().join("empty-events");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(check_schema(&empty), SchemaCheck::Fresh);

        // A downgrade: newer stamp, refused before anything opens.
        std::fs::write(store_dir.join("schema.json"), r#"{"version": 99, "at": 0}"#).unwrap();
        assert_eq!(check_schema(&store_dir), SchemaCheck::TooNew(99));
        let action = rt.block_on(boot_gate_at(&store_dir, 1.5, 1024)).unwrap();
        assert!(matches!(action, GateAction::Refused(99)));
        // The store itself was not touched by the refusal.
        let opened = rt.block_on(NostrLmdb::open(&store_dir)).unwrap();
        assert_eq!(rt.block_on(opened.count(Filter::new())).unwrap(), 2);
        drop(opened);
        drop(env);
    }

    /// An unstamped store is a PRE-versioning store: a v2 binary must
    /// see it as Outdated(1) and migrate it, never stamp it v2 and open
    /// v1 bytes as Current (review, Nov 27 2026).
    #[test]
    fn unstamped_store_is_v1_not_code_schema() {
        let env = EnvGuard::new("schema-unstamped");
        let (store_dir, _) = planted_store("schema-unstamped");
        let _ = std::fs::remove_file(store_dir.join("schema.json"));
        assert_eq!(check_schema_for(&store_dir, 2), SchemaCheck::Outdated(1));
        // The stamp written is 1, so the next boot agrees.
        assert_eq!(read_schema_version(&store_dir), Some(1));
        assert_eq!(check_schema_for(&store_dir, 2), SchemaCheck::Outdated(1));
        // A torn stamp reads as unstamped: same verdict.
        std::fs::write(store_dir.join("schema.json"), "{\"vers").unwrap();
        assert_eq!(check_schema_for(&store_dir, 2), SchemaCheck::Outdated(1));
        // No store at all: born in the binary's schema.
        let empty = env.dir().join("empty-events-v2");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(check_schema_for(&empty, 2), SchemaCheck::Fresh);
        assert_eq!(read_schema_version(&empty), Some(2));
        drop(env);
    }

    /// D100: a migration is the vacuum walk with a transform hooked in —
    /// full copy, count verified, same two-rename swap, the stamp
    /// advanced only after the swap lands. Driven with a fake step (a
    /// real one does not exist yet: v1 is the only schema).
    #[test]
    fn migration_walks_transforms_and_swaps() {
        fn drop_ones(event: Event) -> Option<Event> {
            (!event.content.contains("one")).then_some(event)
        }
        let env = EnvGuard::new("schema-migrate");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (store_dir, _) = planted_store("schema-migrate");

        // Plant a v0 store (one version below this binary's, one step
        // away): the shape every future upgrade starts from.
        std::fs::write(store_dir.join("schema.json"), r#"{"version": 0, "at": 0}"#).unwrap();

        let (report, store) = rt
            .block_on(migrate_with(&store_dir, 0, &[drop_ones]))
            .unwrap();
        assert_eq!(report.events, 2);

        // The swap happened: no staging left, the stamp advanced, and
        // the returned store — the env that walked the copy, never a
        // reopen — holds exactly the transform's output.
        assert!(!store_dir.with_extension("migrate").exists());
        let stamp: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(store_dir.join("schema.json")).unwrap())
                .unwrap();
        assert_eq!(
            stamp.get("version").unwrap().as_u64().unwrap(),
            u64::from(CODE_SCHEMA)
        );
        let kept = rt.block_on(store.query(Filter::new())).unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept.first().unwrap().content, "two");
        drop(store);

        // No path, no migration: a from/steps mismatch is an error, not
        // a silent no-op.
        let err = rt.block_on(migrate_with(&store_dir, 0, &[])).unwrap_err();
        assert!(err.contains("no migration path"), "{err}");
        drop(env);
    }
}
