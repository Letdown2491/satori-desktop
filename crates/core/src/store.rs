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
pub const CATALOG: [(&str, &[u16]); 13] = [
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
pub const FEED_KINDS: [Kind; 15] = [
    Kind::TextNote,      // 1
    Kind::Repost,        // 6
    Kind::Custom(20),    // picture (NIP-68)
    Kind::Custom(21),    // short vertical video (NIP-71)
    Kind::Custom(22),    // video (NIP-71)
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

pub struct Store {
    db: Arc<NostrLmdb>,
}

impl Store {
    pub async fn open() -> NostrResult<Self> {
        std::fs::create_dir_all(config::events_dir()).map_err(|e| DbError::other(e.to_string()))?;
        // Post-vacuum hygiene (vacuum's swap left events.prev): this boot
        // opened the REBUILT store, so the previous generation is dead.
        // A staging dir from a crashed vacuum is debris too.
        let _ = std::fs::remove_dir_all(config::events_dir().with_extension("prev"));
        let _ = std::fs::remove_dir_all(config::events_dir().with_extension("vacuum"));
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
        // The whole visible catalog (visible_kinds honors the content prefs).
        self.feed_page_kinds(authors, until, limit, visible_kinds())
            .await
    }

    /// feed_page over an explicit kind set — the articles surface (30023
    /// only) reads through the same chokepoint with a narrower filter.
    pub async fn feed_page_kinds(
        &self,
        authors: &[PublicKey],
        until: Option<u64>,
        limit: usize,
        kinds: Vec<Kind>,
    ) -> NostrResult<Vec<Event>> {
        let mut filter = Filter::new()
            .kinds(kinds)
            .authors(authors.to_vec())
            .limit(limit);
        if let Some(ts) = until {
            filter = filter.until(Timestamp::from(ts));
        }
        let mut events: Vec<Event> = self.query(filter).await?.into_iter().collect();
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
    pub async fn prune(&self, protect: &std::collections::HashSet<EventId>) -> NostrResult<u32> {
        const PRUNE_DAYS: u64 = 90;
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
        let cutoff = Timestamp::from_secs(now.saturating_sub(PRUNE_DAYS * 86400));
        for kind in FEED_KINDS.iter().chain(std::iter::once(&Kind::Reaction)) {
            loop {
                let filter = Filter::new().kind(*kind).until(cutoff).limit(PRUNE_BATCH);
                let batch = self.query(filter).await?;
                if batch.is_empty() {
                    break;
                }
                let mut deleted_here = 0usize;
                for ev in &batch {
                    if protect.contains(&ev.id) {
                        continue;
                    }
                    self.delete_event(ev.id).await?;
                    deleted += 1;
                    deleted_here += 1;
                }
                if deleted_here == 0 || batch.len() < PRUNE_BATCH {
                    break;
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
            for id in stale {
                self.delete_event(id).await?;
                deleted += 1;
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

    async fn query(&self, filter: Filter) -> NostrResult<BTreeSet<Event>> {
        self.db.query(filter).await
    }
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
// prints the report, and exits. A boot-time threshold gate waits for
// the trigger (issue #4: a month of week-over-week growth).

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
    vacuum_at(&config::events_dir()).await
}

/// The rebuild itself, against explicit paths: `dir` is the live store
/// (`events/`), the staging copy goes to `events.vacuum` beside it, and
/// on success `dir` becomes `events.prev` while the staging copy takes
/// its name. Same-filesystem renames: atomic, no copy.
async fn vacuum_at(dir: &std::path::Path) -> Result<VacuumReport, String> {
    let staging = dir.with_extension("vacuum"); // events/ -> events.vacuum/
    let prev = dir.with_extension("prev");
    let bytes_of = |d: &std::path::Path| {
        std::fs::metadata(d.join("data.mdb"))
            .map(|m| m.len())
            .unwrap_or(0)
    };
    let bytes_before = bytes_of(dir);

    // Staging from a crashed vacuum is debris: start clean.
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;

    // Both envs open here — the caller's contract is that the app is
    // closed (the --vacuum flag runs instead of it), so nothing else
    // holds the live store.
    let source = NostrLmdb::open(dir).await.map_err(|e| e.to_string())?;
    let fresh = NostrLmdb::open(&staging).await.map_err(|e| e.to_string())?;

    let total = source
        .count(Filter::new())
        .await
        .map_err(|e| e.to_string())?;

    // Copy in created_at windows, walking BACKWARDS from now: the hot
    // 90 days first, then bookmark-preserved elders. The stop is
    // EIGHT consecutive empty windows (~8 months past the oldest
    // survivor) — cheap window queries against the created_at index.
    const WINDOW: u64 = 30 * 24 * 3600;
    const EMPTY_STOP: u64 = 8;
    let now = Timestamp::now().as_secs();
    let mut copied = 0usize;
    let mut empty_run = 0u64;
    let mut hi = now;
    while empty_run < EMPTY_STOP {
        let lo = hi.saturating_sub(WINDOW - 1);
        let filter = Filter::new()
            .since(Timestamp::from(lo))
            .until(Timestamp::from(hi));
        let events = source.query(filter).await.map_err(|e| e.to_string())?;
        if events.is_empty() {
            empty_run += 1;
        } else {
            empty_run = 0;
        }
        for event in &events {
            fresh.save_event(event).await.map_err(|e| e.to_string())?;
        }
        copied += events.len();
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
    let tail = source
        .query(Filter::new().since(Timestamp::from(now + 1)))
        .await
        .map_err(|e| e.to_string())?;
    for event in &tail {
        fresh.save_event(event).await.map_err(|e| e.to_string())?;
    }
    copied += tail.len();

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

    if prev.exists() {
        std::fs::remove_dir_all(&prev).map_err(|e| e.to_string())?;
    }
    std::fs::rename(dir, &prev).map_err(|e| e.to_string())?;
    std::fs::rename(&staging, dir).map_err(|e| e.to_string())?;
    if let Some(parent) = dir.parent() {
        let _ = std::fs::File::open(parent).and_then(|f| f.sync_all());
    }

    Ok(VacuumReport {
        events: total,
        bytes_before,
        bytes_after: bytes_of(dir),
    })
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
        std::fs::create_dir_all(&store_dir).unwrap();

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
}
