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
        std::fs::create_dir_all(config::events_dir()).map_err(|e| DbError::other(e.to_string()))?;
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
            let _ = std::fs::remove_dir_all(config::events_dir().with_extension("vacuum"));
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
            let _ = std::fs::remove_dir_all(config::events_dir().with_extension("prev"));
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

    /// The store's event count (D99's Settings readout). One index walk
    /// over ids, no materialized events — the H-2 lesson's shape.
    pub async fn event_count(&self) -> NostrResult<u64> {
        self.db.count(Filter::new()).await.map(|c| c as u64)
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

// --- The deferred rebuild (D87) -----------------------------------------
//
// The boot gate no longer rebuilds inline (the owner, mid-rebuild:
// "the store optimization shouldn't run at startup... it should run
// after the app has started"). Three phases now:
//
//   1. GATE (boot, exclusive window): past the thresholds, it stages —
//      creates the staging dir + the state file — and returns
//      `Deferred`. The app opens immediately.
//   2. WALK (beside the app, spawned at Pool::open): copies every
//      surviving event into staging through the LIVE store's own
//      handle, resuming from the state file's cursor; marks the state
//      `complete` when the walk reaches the epoch.
//   3. ADOPT (a later boot's gate): the delta since the copy's cutoff
//      migrates into staging, both counts must agree, then the swap is
//      two renames + a watermark. Bounded by one session's writes.
//
// The state file (`events.vacuum.json`) is the handoff: the gate, the
// walk, and `Store::open`'s debris policy all read it.

/// The walk's created_at window (30 days, shared with the blocking
/// vacuum — one constant so they cannot silently diverge; D89 audit).
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
        let _ = std::fs::write(vacuum_state_path(dir), json);
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
    if read_vacuum_state(&dir)
        .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(str::to_string))
        .is_some_and(|state| state == "copying" || state == "complete")
    {
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    write_vacuum_state(&dir, "copying", now, now, 0);
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
        let _ = std::fs::write(schema_path(dir), json);
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
    match read_schema_version(dir) {
        None => {
            // Retroactive stamp: whatever is here (or nothing) is v1.
            write_schema_version(dir, CODE_SCHEMA);
            if dir.join("data.mdb").exists() {
                SchemaCheck::Current
            } else {
                SchemaCheck::Fresh
            }
        }
        Some(v) if v == CODE_SCHEMA => SchemaCheck::Current,
        Some(v) if v < CODE_SCHEMA => SchemaCheck::Outdated(v),
        Some(v) => SchemaCheck::TooNew(v),
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
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
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

/// Phase 3: adopt a completed staging copy. The delta (events saved
/// since the copy's cutoff — their created_at window was already
/// walked) migrates into staging, then the counts must agree EXACTLY
/// before the swap: a shortfall means old-dated backfill landed in
/// already-walked windows, and the caller falls back to the full
/// blocking rebuild rather than swap a lossy store in.
async fn adopt_staged(dir: &std::path::Path) -> Result<(VacuumReport, Store), String> {
    let staging = dir.with_extension("vacuum");
    let bytes_of = |d: &std::path::Path| {
        std::fs::metadata(d.join("data.mdb"))
            .map(|m| m.len())
            .unwrap_or(0)
    };
    let bytes_before = bytes_of(dir);
    let cutoff = read_vacuum_state(dir)
        .and_then(|v| v.get("cutoff").and_then(|c| c.as_u64()))
        .ok_or_else(|| "staged adoption: no cutoff in the state file".to_string())?;

    let source = NostrLmdb::open(dir).await.map_err(|e| e.to_string())?;
    let fresh = NostrLmdb::open(&staging).await.map_err(|e| e.to_string())?;

    let mut moved = 0usize;
    copy_range(
        &source,
        &fresh,
        cutoff,
        Timestamp::now().as_secs(),
        &|event| Some(event),
        &mut moved,
    )
    .await?;

    // The honesty check, stricter than the blocking vacuum's: staging
    // plus the delta must hold exactly what the live store holds.
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
            "staged adoption aborted: staging holds {fresh_count} events, the live store {source_count} (delta was {moved})"
        ));
    }

    for d in [&staging, dir, staging.parent().unwrap_or(&staging)] {
        std::fs::File::open(d)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
    }
    swap_staged(dir, &staging)?;
    // The stamp rides the swap (five-lens audit Nov 2026): the live dir
    // must never sit unstamped — a pre-CODE_SCHEMA binary reading it
    // bare would retroactively stamp it as its own and open it (the
    // silent downgrade this machinery refuses everywhere else).
    write_schema_version(dir, CODE_SCHEMA);
    // The adoption is the handoff's end: the state file goes with the
    // staging dir it described.
    let _ = std::fs::remove_file(vacuum_state_path(dir));

    record_watermark(dir, bytes_of(dir));
    let report = VacuumReport {
        events: fresh_count,
        bytes_before,
        bytes_after: bytes_of(dir),
    };
    // The swapped-in env IS the store (the heed path-cache rule — see
    // migrate_with): this session runs on it instead of reopening dir.
    Ok((
        report,
        Store {
            db: Arc::new(fresh),
        },
    ))
}

/// Phase 2's spawn point: `Pool::open` calls this with the session's
/// store handle. The walk no-ops unless the state file says a copy is
/// pending, so unconditional spawning is safe.
pub fn spawn_deferred_vacuum(store: Store) {
    tokio::spawn(async move {
        if let Err(err) = deferred_vacuum_walk(&store).await {
            eprintln!("[core] deferred vacuum failed ({err}); the walk retries at the next launch");
        }
    });
}

/// The walk: copy every surviving event into the staging env, in
/// created_at windows walked backwards from the state file's cursor
/// (resume is free — saves are idempotent by id and the cursor only
/// advances past whole completed windows). Reads go through the LIVE
/// store's own handle; the staging env is a different LMDB file, so
/// the two-envs-one-process hazard never crosses the same file.
async fn deferred_vacuum_walk(store: &Store) -> Result<(), String> {
    let dir = config::events_dir();
    let staging = dir.with_extension("vacuum");
    let state = read_vacuum_state(&dir);
    let (cutoff, cursor, mut copied) = match state {
        Some(v) if v.get("state").and_then(|s| s.as_str()) == Some("copying") => (
            v.get("cutoff").and_then(|c| c.as_u64()).unwrap_or(0),
            v.get("cursor").and_then(|c| c.as_u64()).unwrap_or(0),
            v.get("copied").and_then(|c| c.as_u64()).unwrap_or(0) as usize,
        ),
        _ => return Ok(()), // stale spawn: nothing pending
    };
    let mut start_over = false;
    if !staging.exists() {
        // The debris policy kept this only if the state file said
        // copying; a walk that outlived its staging starts the dir over
        // — and recopies from the TOP (D89 audit): the cursor only
        // covers windows already staged, so resuming there into a fresh
        // env is a structurally short copy that always falls back to
        // the blocking rebuild at adoption.
        std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
        start_over = true;
    }
    let fresh = NostrLmdb::open(&staging).await.map_err(|e| e.to_string())?;

    let now = Timestamp::now().as_secs();
    let mut hi = if start_over { now } else { cursor.min(now) };
    loop {
        let lo = hi.saturating_sub(VACUUM_WINDOW - 1);
        copy_range(
            store.database().as_ref(),
            &fresh,
            lo,
            hi,
            &|event| Some(event),
            &mut copied,
        )
        .await?;
        write_vacuum_state(&dir, "copying", cutoff, lo, copied);
        if lo == 0 {
            break;
        }
        hi = lo - 1;
    }

    // The tail catch (future-dated events, same as the blocking
    // vacuum): idempotent on resume.
    copy_range(
        store.database().as_ref(),
        &fresh,
        now + 1,
        now + 10 * 365 * 24 * 3600,
        &|event| Some(event),
        &mut copied,
    )
    .await?;

    write_vacuum_state(&dir, "complete", cutoff, 0, copied);
    eprintln!(
        "[core] deferred vacuum: copy complete ({copied} events staged); the slimmer store applies at the next launch"
    );
    Ok(())
}

// --- The boot gate (WS3, issue #4; deferred in D87) ---------------------
//
// The threshold-gated rebuild the 0.5.0 comments waited for. Shape: a
// watermark beside the store records the extent at the last rebuild (or
// first observation); at boot, BEFORE the store opens (the same
// exclusive window --vacuum uses), the gate compares the current extent
// against it. Growth past BOTH thresholds now STAGES the rebuild
// instead of running it: the app opens immediately, the copy walks in
// the background (spawned at Pool::open), and a later boot adopts the
// finished staging copy (delta + count check + two renames). A failed
// adoption falls back to the blocking rebuild; a crashed walk resumes
// from its cursor.

/// The walk's memory cap (D89 audit): a copy chunk materializes its
/// events as decoded `Event` structs, and the decoded set is 1.5-2x its
/// serialized size — the whole 30-day window on the 1.3 GB store that
/// triggered D87 ran beside the live UI. Ranges are split until each
/// query holds at most this many events.
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
        let _ = std::fs::write(watermark_path(dir), json);
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

    // Pending-work checks come before the thresholds: a staged copy is
    // work regardless of what the extent says this boot (the extent
    // only moves at an adoption).
    match read_vacuum_state(dir)
        .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(str::to_string))
    {
        Some(state) if state == "complete" => {
            // Phase 3: adopt. Any hard failure falls back to the full
            // blocking rebuild — a shortfall in staging never swaps.
            match adopt_staged(dir).await {
                Ok((report, store)) => {
                    println!(
                        "boot gate: staged store adopted, {} events, data.mdb {:.1} MB -> {:.1} MB",
                        report.events,
                        report.bytes_before as f64 / 1048576.0,
                        report.bytes_after as f64 / 1048576.0
                    );
                    return Ok(GateAction::Swapped(report, store));
                }
                Err(err) => {
                    eprintln!(
                        "boot gate: staged adoption failed ({err}); running the full rebuild"
                    );
                    // A failed adoption can be a FAILED SWAP: the live
                    // dir was renamed to prev and staging's rename in
                    // failed, so the restore below ran against a store
                    // that then existed. Restore again before the
                    // rebuild — rebuilding the empty dir and opening it
                    // would delete prev, the only recoverable copy
                    // (D89 audit).
                    if std::fs::metadata(dir.join("data.mdb"))
                        .map(|m| m.len())
                        .unwrap_or(0)
                        == 0
                        && dir.with_extension("prev").join("data.mdb").exists()
                    {
                        let _ = std::fs::rename(dir.with_extension("prev"), dir);
                    }
                    notify_rebuild("Optimizing the local store — this launch may take a moment.");
                    return vacuum_at(dir)
                        .await
                        .map(|(r, store)| GateAction::Swapped(r, store));
                }
            }
        }
        Some(state) if state == "copying" => {
            // Phase 2 resumes in the background (Pool::open spawns the
            // walk); this boot stays instant.
            return Ok(GateAction::Deferred);
        }
        _ => {}
    }

    let base = std::fs::read_to_string(watermark_path(dir))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("bytes").and_then(|b| b.as_u64()));
    let Some(base) = base else {
        record_watermark(dir, now_bytes);
        return Ok(GateAction::Idle);
    };
    let grown = now_bytes as f64 >= base as f64 * ratio;
    let real = now_bytes.saturating_sub(base) >= floor;
    if !grown || !real {
        return Ok(GateAction::Idle);
    }

    // Phase 1: stage the copy and defer. Staging debris without a
    // state file (a crashed --vacuum) is cleared; the state file's
    // cutoff is this copy's delta anchor.
    let staging = dir.with_extension("vacuum");
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    write_vacuum_state(dir, "copying", now, now, 0);
    notify_rebuild(
        "Satori is slimming its local store in the background; the slimmer store applies at your next launch.",
    );
    println!(
        "boot gate: store {:.1} MB is {:.0}% past its watermark; the rebuild runs in the background and adopts at a later launch",
        now_bytes as f64 / 1048576.0,
        (now_bytes as f64 / base as f64 - 1.0) * 100.0
    );
    Ok(GateAction::Deferred)
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
        std::fs::create_dir_all(&store_dir).unwrap();
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
        std::fs::create_dir_all(&store_dir).unwrap();
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

    /// The D87 regression: a trigger past the thresholds must NOT block
    /// the boot. The gate stages (staging dir + state file, instant),
    /// the walk runs beside the app through the live handle, and the
    /// NEXT boot adopts the copy: delta migrated, counts verified, swap
    /// done, watermark moved.
    #[test]
    fn gate_defers_the_rebuild_and_adopts_at_next_boot() {
        let env = EnvGuard::new("gate-defer");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (store_dir, events) = planted_store("gate-defer");

        // Plant a tiny pre-growth watermark: the real extent is far
        // past both thresholds, so the gate triggers.
        std::fs::write(
            store_dir.with_extension("watermark.json"),
            r#"{"bytes": 100, "at": 0}"#,
        )
        .unwrap();

        // Boot 1: staged, not run. The live store opens untouched.
        let action = rt.block_on(boot_gate_at(&store_dir, 1.5, 1024)).unwrap();
        assert!(matches!(action, GateAction::Deferred));
        assert!(store_dir.with_extension("vacuum").exists());
        assert!(store_dir.with_extension("vacuum.json").exists());
        let opened = rt.block_on(NostrLmdb::open(&store_dir)).unwrap();
        assert_eq!(rt.block_on(opened.count(Filter::new())).unwrap(), 2);
        drop(opened);

        // Phase 2: the walk, through the app's own handle. Store::open
        // must KEEP an in-progress staging copy (the resume contract —
        // a session shorter than the copy would never converge).
        let store = rt.block_on(Store::open()).unwrap();
        assert!(
            store_dir.with_extension("vacuum").exists(),
            "an in-progress staging copy must survive Store::open"
        );
        rt.block_on(deferred_vacuum_walk(&store)).unwrap();
        drop(store);
        let state: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(store_dir.with_extension("vacuum.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state.get("state").unwrap().as_str().unwrap(), "complete");
        assert_eq!(state.get("copied").unwrap().as_u64().unwrap(), 2);

        // Boot 2: adoption. The delta is empty (nothing new was saved),
        // so the whole boot is the count check + two renames.
        let action = rt.block_on(boot_gate_at(&store_dir, 1.5, 1024)).unwrap();
        let GateAction::Swapped(report, store) = action else {
            panic!("a completed staging copy adopts at the next boot, got {action:?}")
        };
        assert_eq!(report.events, 2);
        assert!(!store_dir.with_extension("vacuum").exists());
        assert!(!store_dir.with_extension("vacuum.json").exists());

        // Every event survived into the ADOPTED STORE — the handle the
        // gate hands through (a reopen of the swapped path would read
        // the stale heed-cached env; the hand-through is the contract).
        assert_eq!(rt.block_on(store.event_count()).unwrap(), 2);
        let watermark =
            std::fs::read_to_string(store_dir.with_extension("watermark.json")).unwrap();
        let base: serde_json::Value = serde_json::from_str(&watermark).unwrap();
        assert_eq!(
            base.get("bytes").unwrap().as_u64().unwrap(),
            std::fs::metadata(store_dir.join("data.mdb")).unwrap().len()
        );
        drop(store);
        let _ = &events;
        drop(env);
    }

    /// The adoption's honesty check: if staging falls short of the live
    /// store (old-dated backfill landing in already-walked windows),
    /// the gate must fall back to the full rebuild instead of swapping
    /// a lossy store in.
    #[test]
    fn gate_falls_back_when_staging_is_short() {
        let env = EnvGuard::new("gate-fallback");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (store_dir, events) = planted_store("gate-fallback");

        // Stage a copy that holds only ONE of the two events, marked
        // complete: an honest-looking handoff with a hole in it.
        std::fs::create_dir_all(store_dir.with_extension("vacuum")).unwrap();
        let db = rt
            .block_on(NostrLmdb::open(store_dir.with_extension("vacuum")))
            .unwrap();
        rt.block_on(db.save_event(&events[0])).unwrap();
        drop(db);
        std::fs::write(
            store_dir.with_extension("vacuum.json"),
            r#"{"state": "complete", "cutoff": 0, "cursor": 0, "copied": 1}"#,
        )
        .unwrap();

        let action = rt.block_on(boot_gate_at(&store_dir, 1.5, 1024)).unwrap();
        let GateAction::Swapped(report, store) = action else {
            panic!("the fallback runs a full rebuild, got {action:?}")
        };
        // The fallback rebuilt from the LIVE store: both events, and
        // the state file is gone with its superseded staging.
        assert_eq!(report.events, 2);
        assert!(!store_dir.with_extension("vacuum.json").exists());
        assert_eq!(rt.block_on(store.event_count()).unwrap(), 2);
        drop(store);
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
