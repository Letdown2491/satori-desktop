//! The home timeline: bootstrap, refresh, paging, profiles for the feed.

use super::*;

impl Core {
    // --- feed ------------------------------------------------------------------

    /// Bootstrap: NIP-65 relay lists, follows, connect reads, first page
    /// (deep fetch for older pagination), own engagement sets, profiles.
    /// Bootstrap the given surface: `visible_kinds()` for the timeline, a
    /// single kind for a narrow one (articles).
    pub fn bootstrap(self: &Arc<Self>, kinds: Vec<Kind>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            // D36: the outbox drainer arms on the first bootstrap —
            // crash leftovers and queued failures come due here.
            this.spawn_outbox_drain();
            // WS4: the scheduled-posts sweep arms alongside it — restart
            // catch-up is just its first tick finding due entries on disk.
            this.spawn_scheduled_sweep();
            if let Err(e) = this.bootstrap_inner(&tx, kinds).await {
                let _ = tx.send(FeedEvent::Failed(e.to_string()));
            }
        });
        rx
    }

    pub(super) async fn bootstrap_inner(
        &self,
        tx: &mpsc::UnboundedSender<FeedEvent>,
        kinds: Vec<Kind>,
    ) -> Result<()> {
        let mut active = self.active().await?;
        let me = active.me();

        // 1. PAINT FIRST: whatever the store already holds (instant — the
        // LMDB cache persists across runs). Cached follows drive the filter.
        let cached_follows: Vec<PublicKey> = active
            .pool
            .store
            .latest_by_kind(me, Kind::ContactList)
            .await
            .unwrap_or(None)
            .map(|ev| ev.tags.public_keys().collect())
            .unwrap_or_default();
        if !cached_follows.is_empty() {
            active.set_follows(cached_follows.clone());
            let page = self.page_from_store(&active, None, &kinds).await?;
            let _ = tx.send(FeedEvent::Loaded(page));
        }
        let _ = tx.send(FeedEvent::BootstrapDone);

        // 2. The network phase — everything independent, IN PARALLEL. The
        // UI re-renders when the fresh page lands (the reactive profile
        // patching fills names/avatars as they arrive).
        // Session-phase single-flight (audit perf M5): surfaces opened in
        // quick succession re-ran the nip65/follows phase CONCURRENTLY
        // against the same relays. The first surface runs it; the rest
        // wait, then read the (now warm) store. Keyed per account —
        // read_state persists across logouts — TTL 1h, armed only when a
        // phase actually found follows.
        let phase_key = format!("session-phase-{me}");
        {
            let _guard = self.session_boot.lock().await;
            let ran = crate::read_state::load()
                .warm_at
                .get(&phase_key)
                .is_some_and(|t| *t > now_secs().saturating_sub(3600));
            if !ran {
                active.pool.connect_reads(Vec::new()).await.ok();
                let (read, write) = active.pool.nip65(me).await.unwrap_or_else(|_| {
                    eprintln!("[core] nip65 fetch failed or timed out");
                    (Vec::new(), Vec::new())
                });
                eprintln!(
                    "[core] bootstrap: {} read relay(s), {} write relay(s)",
                    read.len(),
                    write.len()
                );
                active.read_relays = read.clone();
                active.write_relays = write.clone();
                if let Some(a) = self.active.lock().as_mut() {
                    a.read_relays = read.clone();
                    a.write_relays = write;
                }
                let n = crate::outbox_queue::len();
                if n > 0 {
                    eprintln!("[core] outbox: {n} item(s) pending, drain armed");
                }
                active.pool.connect_reads(read.clone()).await.ok();
                self.spawn_relay_info(&read);
                // D48: the anti-spam ledgers reload before anything can
                // paint — dedup's caught-spammer records + recent keys,
                // the velocity stamps + counted ids, the receipted
                // suppressions. Boot gates from last session's knowledge
                // instead of re-learning it from relays (a minute of
                // spam on the first frame). Guards make re-arms no-ops.
                crate::dedup::load_disk();
                crate::rate::load_disk();
                load_suppressed_disk();
                // Arm the trust vocabulary from LOCAL state first (D45):
                // the fresh contact-list fetch below is a relay round-trip,
                // and until the graph is armed every verdict reads Unknown,
                // which lets the notification tier pass everything — spam
                // painted first and got cleaned up a minute later. The
                // store's cached own list + the wot sidecar are local
                // reads; gating must be correct from the FIRST paint.
                // The fresh fetch below re-arms with a newer list (all
                // rebuilds are idempotent and ms-cheap; observe is
                // newest-wins).
                if let Ok(Some(own)) = active
                    .pool
                    .store
                    .latest_by_kind(me, Kind::ContactList)
                    .await
                {
                    let local_follows: Vec<PublicKey> = own.tags.public_keys().collect();
                    crate::wot::set_identity(me, &local_follows);
                    self.push_wot_mutes();
                    crate::dedup::set_exemptions(me, &local_follows);
                    self.rebuild_wot_graph(&active, &local_follows).await;
                }
                let follows = active.pool.follows(me).await.unwrap_or_default();
                active.set_follows(follows.clone());
                if let Some(a) = self.active.lock().as_mut() {
                    a.set_follows(follows.clone());
                }
                if !follows.is_empty() {
                    crate::read_state::advance(|st| {
                        st.warm_at.insert(phase_key.clone(), now_secs());
                    });
                }
            } else {
                // The phase ran recently (this or a previous launch): no
                // relay round trips for nip65/follows. But the pool is
                // PER PROCESS — the session's reads must still join it,
                // from the store's copy of the own kind:10002 (no
                // network). Without this the main live sub armed with
                // zero relays and "subscribe failed; re-arming" every
                // surface (live loop, stderr8).
                let (read, write): (Vec<String>, Vec<String>) = active
                    .pool
                    .store
                    .latest_by_kind(me, Kind::RelayList)
                    .await
                    .ok()
                    .flatten()
                    .map(|ev| {
                        let lists = outbox::parse_relay_list(&ev);
                        (
                            lists.read.into_iter().collect(),
                            lists.write.into_iter().collect(),
                        )
                    })
                    .unwrap_or_default();
                if !read.is_empty() {
                    active.read_relays = read.clone();
                    if let Some(a) = self.active.lock().as_mut() {
                        a.read_relays = read.clone();
                    }
                    active.pool.connect_reads(read.clone()).await.ok();
                    self.spawn_relay_info(&read);
                } else {
                    // No cached list at all: indexers only, still connect.
                    active.pool.connect_reads(Vec::new()).await.ok();
                    self.spawn_relay_info(&[]);
                }
                // D36: the WRITE half too. A warm start previously left
                // the write set empty (only `.read` was hydrated), so
                // every publish — commit or outbox drain — fell back to
                // the indexer fan-out instead of the user's declared
                // write relays until the next full phase ran (the
                // three recovered notes went out via indexers for
                // exactly this reason). Same staleness contract as the
                // read half: the store copy until a full phase refreshes.
                if !write.is_empty() {
                    eprintln!(
                        "[core] bootstrap: write set from store: {} relay(s)",
                        write.len()
                    );
                    active.write_relays = write.clone();
                    if let Some(a) = self.active.lock().as_mut() {
                        a.write_relays = write;
                    }
                }
                // The phase already landed the fresh contact list; pick it
                // up (a store read).
                if let Ok(Some(ev)) = active
                    .pool
                    .store
                    .latest_by_kind(me, Kind::ContactList)
                    .await
                {
                    let follows: Vec<PublicKey> = ev.tags.public_keys().collect();
                    active.set_follows(follows.clone());
                    if let Some(a) = self.active.lock().as_mut() {
                        a.set_follows(follows);
                    }
                }
            }
        }
        // The phase (or its store pickup) left active.follows authoritative.
        let follows = active.follows.clone();
        // Trust graph (issue #2 WS1): arm the WoT vocabulary + dedup
        // exemptions for this session, then rebuild the two-hop graph
        // from the store's cached kind-3s (an in-memory projection —
        // new kind-3s apply incrementally at the chokepoint/live bell).
        crate::wot::set_identity(me, &follows);
        self.push_wot_mutes();
        crate::dedup::set_exemptions(me, &follows);
        self.rebuild_wot_graph(&active, &follows).await;
        self.start_live(kinds.clone()).await;

        // Profiles + engagement + deep fetch: parallel, none block paint.
        let mut warm_set = follows.clone();
        warm_set.push(me);
        active.profiles.warm(warm_set);

        // The warm skip is per surface (keyed by kind set) — a timeline
        // deep-filled five minutes ago says nothing about the articles
        // shelf's store depth.
        let warm_key: String = {
            let mut ks: Vec<u16> = kinds.iter().map(|k| u16::from(*k)).collect();
            ks.sort();
            ks.dedup();
            ks.iter()
                .map(|k| k.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        let warm = crate::read_state::load()
            .warm_at
            .get(&warm_key)
            .is_some_and(|t| *t > now_secs().saturating_sub(600));
        // Only a deep fetch that actually RAN earns the stamp — stamping a
        // skipped bootstrap would let a surface opened every few minutes
        // defer its fill forever.
        let ran_deep = !follows.is_empty() && !warm;
        // The fetches' values are not read: their effect is the SDK's
        // save-on-receive; the page projects from the store below.
        let (engagement, fetched, _dels) = tokio::join!(
            async {
                let mine = Filter::new()
                    .author(me)
                    .kinds([
                        Kind::Reaction,
                        Kind::Repost,
                        Kind::Custom(16),
                        Kind::Custom(10003),
                        // D38 v1.1: the own surfaces the store never had a
                        // fetch for — the public bookmarks list (30001),
                        // the NIP-51 named lists (30000, one per d) and the
                        // web app's NIP-37 draft wraps (31234, retired
                        // format, imported into the 30078 stash).
                        Kind::Custom(30000),
                        Kind::Custom(30001),
                        Kind::Custom(31234),
                    ])
                    .limit(crate::outbox::QUERY_LIMIT);
                active.pool.get(mine).await.unwrap_or_default()
            },
            async {
                if !follows.is_empty() && !warm {
                    let filter = Filter::new()
                        .kinds(kinds.clone())
                        .authors(follows.clone())
                        .limit(BOOTSTRAP_DEPTH);
                    active.pool.get(filter).await.unwrap_or_default()
                } else {
                    eprintln!(
                        "[core] deep fetch skipped: follows_empty={} warm={}",
                        follows.is_empty(),
                        warm
                    );
                    Default::default()
                }
            },
            // Deletions ride along (NIP-09): rare, follows-scoped, needed
            // before the render filter can drop what they cover. The
            // chokepoint consumes the kind:5s (records them, filters them
            // from the set), so the arm's value is the tombstones it
            // seeds plus the ordering — the projection waits on it.
            async {
                if !follows.is_empty() && !warm {
                    let filter = Filter::new()
                        .kind(Kind::EventDeletion)
                        .authors(follows.clone())
                        .limit(crate::outbox::QUERY_LIMIT);
                    let _ = active.pool.get(filter).await;
                }
                0usize
            }
        );
        eprintln!(
            "[core] deep fetch: {} events",
            engagement.len() + fetched.len()
        );
        // No explicit saves: the SDK persists every relay-received event
        // before notifying (relay/inner.rs), so these loops re-saved what
        // was already on disk (audit perf M2). The page projects from the
        // store below.
        self.load_own_sets(&active).await;

        if ran_deep {
            crate::read_state::advance(|st| {
                st.warm_at.insert(warm_key, now_secs());
            });
        }
        let page = self.page_from_store(&active, None, &kinds).await?;
        eprintln!("[core] bootstrap page: {} notes", page.notes.len());
        let _ = tx.send(FeedEvent::Loaded(page));
        Ok(())
    }

    pub(super) async fn load_own_sets(&self, active: &Active) {
        let me = active.me();
        if let Ok(reactions) = active.pool.store.own_reactions(me).await {
            let mut liked = self.own_liked.lock();
            for ev in reactions {
                if let Some(target) = ev.tags.clone().event_ids().next() {
                    liked.insert(target);
                }
            }
        }

        // Own reposts (kind 6 and its generic cousin 16): the e-tag IS the
        // target.
        if let Ok(reposts) = active.pool.store.own_reposts(me).await {
            let mut rp = self.own_reposted.lock();
            for ev in reposts {
                if let Some(target) = ev.tags.clone().event_ids().next() {
                    rp.insert(target);
                }
            }
        }

        // Private lists (NIP-51, content = nip44-encrypted tag JSON — the web
        // app's private-by-default semantics): bookmarks 10003, mutes 10000.
        // FRESH FROM RELAYS first (the follows/nip65 pattern): the reads
        // below are store-fed, and a list published by ANOTHER client
        // never lands in the store unless something fetches it — mutes
        // set elsewhere were invisible for the whole session (the 0.10.0
        // test round: "i have mutes and they dont show"). One
        // client-level read each, newest only; the SDK persists what it
        // receives, so the store reads see it. Relay misses degrade to
        // the cached copies.
        let _ = futures_util::future::join(
            active
                .pool
                .get(Filter::new().kind(Kind::Custom(10000)).author(me).limit(1)),
            active
                .pool
                .get(Filter::new().kind(Kind::Custom(10003)).author(me).limit(1)),
        )
        .await;
        if let Ok(Some(ev)) = active
            .pool
            .store
            .latest_by_kind(me, Kind::Custom(10003))
            .await
        {
            if !ev.content.is_empty()
                && let Ok(plain) = active
                    .signer
                    .nip44_decrypt_self(&me, ev.content.as_str())
                    .await
            {
                let mut b = self.own_bookmarked.lock();
                let entries = parse_list_entries(&plain);
                for id in list_values(&entries, "e") {
                    if let Ok(id) = EventId::from_hex(id) {
                        b.insert(id);
                    }
                }
            }
            // Public tags still count (merge public+private — list semantics).
            let mut b = self.own_bookmarked.lock();
            for id in ev.tags.clone().event_ids() {
                b.insert(id);
            }
        }

        // The public bookmarks list (D38: NIP-51 kind 30001 d="bookmarks")
        // merges into the same display set — the star fills for either,
        // unbookmarking clears both.
        if let Ok(Some(ev)) = active
            .pool
            .store
            .latest_by_kind_d(me, Kind::Custom(30001), "bookmarks")
            .await
        {
            let mut b = self.own_bookmarked.lock();
            for id in ev.tags.clone().event_ids() {
                b.insert(id);
            }
        }

        // The web app's synced drafts (NIP-37 kind 31234 wraps — retired
        // with the web app): content = nip44-to-self of the inner draft
        // event, d = the draft id, empty content = deleted. They migrate
        // into the same array as id-marked entries (deduped by id across
        // logins); the Library shows them and opening/deleting one
        // republishes the wrap empty (NIP-37's delete) — the moment the
        // text becomes a Satori slot draft.
        if let Ok(wraps) = active.pool.store.all_by_kind(me, Kind::Custom(31234)).await {
            // Parameterized replaceable: the newest wrap per d wins.
            let mut newest: HashMap<String, Event> = HashMap::new();
            for wrap in wraps {
                let Some(d) = wrap
                    .tags
                    .clone()
                    .to_vec()
                    .iter()
                    .find(|t| t.kind() == "d")
                    .and_then(|t| t.content().map(str::to_string))
                else {
                    continue;
                };
                match newest.get(&d) {
                    Some(old) if old.created_at >= wrap.created_at => {}
                    _ => {
                        newest.insert(d, wrap);
                    }
                }
            }
            let mut drafts = self.own_drafts.lock().clone();
            for (d, wrap) in newest {
                if wrap.content.trim().is_empty() || nip40::is_expired(&wrap) {
                    continue;
                }
                if drafts.iter().any(|e| e.id.as_deref() == Some(d.as_str())) {
                    continue;
                }
                let Ok(plain) = active
                    .signer
                    .nip44_decrypt_self(&me, wrap.content.as_str())
                    .await
                else {
                    continue;
                };
                // The inner event: {kind, created_at, tags, content} — our
                // own data, but stay crash-proof (the TS parse's posture).
                let Ok(inner) = serde_json::from_str::<serde_json::Value>(&plain) else {
                    continue;
                };
                let kind = inner.get("kind").and_then(|k| k.as_u64()).unwrap_or(0);
                let content = inner
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                let title = inner
                    .get("tags")
                    .and_then(|t| t.as_array())
                    .and_then(|tags| {
                        tags.iter().find_map(|t| {
                            let arr = t.as_array()?;
                            if arr.first()?.as_str()? == "title" {
                                arr.get(1)?.as_str().map(str::to_string)
                            } else {
                                None
                            }
                        })
                    });
                // A note draft keeps its text; an article's body rides its
                // title; a poll's question is the text.
                let text = match kind {
                    30023 => match (&title, content.is_empty()) {
                        (Some(t), false) => format!("{t}\n\n{content}"),
                        (Some(t), true) => t.clone(),
                        _ => content,
                    },
                    _ => content,
                };
                if text.trim().is_empty() {
                    continue;
                }
                drafts.push(DraftEntry {
                    text,
                    reply_to: None,
                    quote: None,
                    id: Some(d),
                    attachments: Vec::new(),
                    photo_post: false,
                    at: wrap.created_at.as_secs(),
                });
            }
            *self.own_drafts.lock() = drafts;
        }

        // Synced drafts (D38): NIP-78 kind 30078 d="satori/drafts" — one
        // replaceable event, the draft array nip44-encrypted (self).
        if let Ok(Some(ev)) = active
            .pool
            .store
            .latest_by_kind_d(me, Kind::Custom(30078), "satori/drafts")
            .await
            && !ev.content.is_empty()
            && let Ok(plain) = active
                .signer
                .nip44_decrypt_self(&me, ev.content.as_str())
                .await
            && let Ok(entries) = serde_json::from_str::<Vec<DraftEntry>>(&plain)
        {
            *self.own_drafts.lock() = entries;
        }

        if let Ok(Some(ev)) = active
            .pool
            .store
            .latest_by_kind(me, Kind::Custom(10000))
            .await
        {
            let decrypted: Vec<Vec<String>> = if ev.content.is_empty() {
                Vec::new()
            } else {
                match active
                    .signer
                    .nip44_decrypt_self(&me, ev.content.as_str())
                    .await
                {
                    Ok(plain) => parse_list_entries(&plain),
                    Err(e) => {
                        // The list was published by another client with a
                        // different crypto/shape — say so instead of a
                        // silent empty mirror (the blank-Muted report).
                        eprintln!("[core] mutes: content DECRYPT FAILED: {e}");
                        Vec::new()
                    }
                }
            };
            let mut m = self.muted.lock();
            for pk in list_values(&decrypted, "p") {
                if let Ok(pk) = PublicKey::from_hex(pk) {
                    m.insert(pk);
                }
            }
            for pk in ev.tags.clone().public_keys() {
                m.insert(pk);
            }
            drop(m);
            // The WoT graph consults mutes at qualification time
            // (dark-wisp filters muted/blocked out) — push the set.
            self.push_wot_mutes();
        }
    }

    /// Newest page (home button): relay fetch since the newest known note.
    /// Paint-first: the live sub has already persisted every pushed note,
    /// so a store projection IS the news — the relay fan-out is a
    /// deepener whose second Loaded dedups in the pane (note_ids). The
    /// badge's click-to-paint delay was this fan-out gating the paint.
    pub fn refresh(self: &Arc<Self>, kinds: Vec<Kind>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        // Single-flight (audit perf M4): overlapping refreshes stacked
        // full routed fan-outs. A click while one runs used to drop the
        // new tx — the caller's pump saw the closed channel and exited,
        // a visible no-op; now it paints the store page (the in-flight
        // refresh deepens further).
        if self
            .refreshing
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let this = Arc::clone(self);
            self.rt.spawn(async move {
                if let Ok(active) = this.active().await
                    && let Ok(page) = this.page_from_store(&active, None, &kinds).await
                {
                    let _ = tx.send(FeedEvent::Loaded(page));
                }
            });
            return rx;
        }
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            if let Err(e) = this.refresh_inner(&tx, kinds).await {
                let _ = tx.send(FeedEvent::Failed(e.to_string()));
            }
            this.refreshing
                .store(false, std::sync::atomic::Ordering::SeqCst);
        });
        rx
    }

    pub(super) async fn refresh_inner(
        &self,
        tx: &mpsc::UnboundedSender<FeedEvent>,
        kinds: Vec<Kind>,
    ) -> Result<()> {
        let active = self.active().await?;
        let follows = active.follows.clone();
        if follows.is_empty() {
            return self.bootstrap_inner(tx, kinds).await;
        }
        // The store paints NOW (see refresh's doc): the badge's promise is
        // the notes the live sub already saved, and they are on disk.
        let page = self.page_from_store(&active, None, &kinds).await?;
        let _ = tx.send(FeedEvent::Loaded(page));
        // The 'since' probe: newest known kind-1 (newest_created_at docs:
        // the all-kinds page materialized one event per author-kind pair,
        // ~7,000 for 500 follows, on every refresh). A kind-1-only newest
        // underestimates, which just widens the refetch window.
        let since = active
            .pool
            .store
            .newest_created_at(&follows)
            .await
            .map_err(|e| LoginError::Resume(e.to_string()))?
            .unwrap_or(0);
        // The full outbox model (nip65.ts routeAuthorsToRelays): each follow
        // is read from the relays THEY write to — set-cover-bounded, 2x
        // redundancy, orphans recovered to their own relays. The guard
        // upstream already sent no-follow accounts to the bootstrap; the
        // call's own guard makes the empty case a no-op regardless.
        let fetched = active
            .pool
            .get_routed(
                kinds.clone(),
                &follows,
                Some(Timestamp::from(since)),
                None,
                100,
            )
            .await;
        if fetched.is_empty() {
            // Relays unreachable (post-suspend, offline): this used to end
            // the refresh as NothingNew — which the feed swallows — so a
            // new-notes chip click no-oped until the relays recovered. The
            // live sub saves pushed notes to the store even while relays
            // are down, so project from the store regardless: the missed
            // notes appear with zero relay round-trips.
            crate::dlog!("refresh: relay fetch empty; projecting the store");
            let page = self.page_from_store(&active, None, &kinds).await?;
            let _ = tx.send(FeedEvent::Loaded(page));
            return Ok(());
        }
        // The SDK saves what it receives; the store projection follows.
        if !follows.is_empty() {
            let del_filter = Filter::new()
                .kind(Kind::EventDeletion)
                .authors(follows.clone())
                .since(Timestamp::from(since.saturating_sub(86400)));
            let _ = active.pool.get(del_filter).await;
        }
        let page = self.page_from_store(&active, None, &kinds).await?;
        eprintln!("[core] bootstrap page: {} notes", page.notes.len());
        let _ = tx.send(FeedEvent::Loaded(page));
        Ok(())
    }

    /// Older page: project the store; when it runs dry, top up from the
    /// relays — strictly older than `until` (the oldest note currently
    /// shown), all visible kinds — and project again. The store stays the
    /// chokepoint; the relays are the reservoir.
    pub fn older(self: &Arc<Self>, until: u64, kinds: Vec<Kind>) -> FeedRx {
        let this = Arc::clone(self);
        self.feed_task(move |active| {
            Box::pin(async move {
                let mut notes = this.project_page(active, Some(until), &kinds).await?;
                // NIP-01: an empty list can never match — with no follows
                // there is nothing to top up from the relays.
                if notes.len() <= PAGE && !active.follows.is_empty() {
                    // Outbox routing: strictly older than the page bottom,
                    // from each follow's own write relays. The value is not
                    // read - the SDK saves what it receives.
                    let _ = active
                        .pool
                        .get_routed(
                            kinds.clone(),
                            &active.follows,
                            None,
                            Some(Timestamp::from(until)),
                            TOPUP_DEPTH,
                        )
                        .await;
                    // (no explicit save: the SDK persists what it receives;
                    // commit() saves own events)
                    // Deletions ride along (NIP-09).
                    let del_filter = Filter::new()
                        .kind(Kind::EventDeletion)
                        .authors(active.follows.clone())
                        .until(Timestamp::from(until))
                        .limit(crate::outbox::QUERY_LIMIT);
                    let _ = active.pool.get(del_filter).await;
                    notes = this.project_page(active, Some(until), &kinds).await?;
                }
                let has_more = notes.len() > PAGE;
                notes.truncate(PAGE);
                let profiles = Self::profiles_for(active, &notes).await;
                Ok(FeedEvent::Older(FeedPage { notes, profiles }, has_more))
            })
        })
    }

    pub(super) fn absorb_profiles(&self, profiles: &[Profile]) {
        let mut m = self.profiles_mem.lock();
        for p in profiles {
            m.insert(p.clone());
        }
    }

    /// Resolve profiles reactively: cards call this with whatever pks they
    /// need; the service coalesces + fetches, the UI patches on arrival.
    pub fn ensure_profiles(self: &Arc<Self>, pks: Vec<PublicKey>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        match &*self.active.lock() {
            Some(active) => {
                let service = Arc::clone(&active.profiles);
                let service2 = Arc::clone(&active.profiles);
                self.rt.spawn(async move {
                    let mut inner = service.ensure_many(pks);
                    while let Some(profs) = inner.recv().await {
                        let _ = tx.send(FeedEvent::Profiles(profs));
                    }
                    let _ = service2;
                });
            }
            None => {
                let _ = tx.send(FeedEvent::Profiles(Vec::new()));
            }
        }
        rx
    }

    /// Display resolution for inline mentions (content.ts: @names from the
    /// profile cache; misses render as short npubs).
    pub fn profile_of(&self, pk: &PublicKey) -> Option<Profile> {
        self.profiles_mem.lock().get(pk).cloned()
    }

    /// @-mention candidates for the composer (content.ts: @names from the
    /// profile cache): a substring scan of the whole hydrated profile
    /// cache — display name, raw kind-0 handle, NIP-05, pubkey hex —
    /// follow-ranked. The follow gate it replaced hid anyone you interact
    /// with but don't follow, and the display-name-only match hid handles
    /// (the reported case: followed, typed "@mpft", unfindable because
    /// `name` stores the display name). Profiles not yet in the cache
    /// can't match — the profiles service fills the rest as the feed
    /// loads them. One lock, eight hits max: this runs per keystroke on
    /// the UI thread, so per-call work is the query's lowercase plus
    /// borrowed lookups — matching keys were pre-lowered at absorb time
    /// and the follow list rides its cached hex mirror.
    pub fn mention_candidates(&self, query: &str) -> Vec<Profile> {
        let (follows, me): (Arc<Vec<String>>, String) = {
            let held = self.active.lock();
            let me = held
                .as_ref()
                .map(|a| a.session.pubkey.to_string())
                .unwrap_or_default();
            let follows = held
                .as_ref()
                .map(|a| Arc::clone(&a.follows_hex))
                .unwrap_or_default();
            (follows, me)
        };
        let m = self.profiles_mem.lock();
        let cache: Vec<&ProfileEntry> = m.entries().collect();
        select_mention_candidates(query, follows.as_slice(), &me, &cache)
    }

    /// People results for the search pane: a substring scan of the hydrated
    /// profile cache (name, NIP-05, pubkey hex), any author — mention
    /// candidates without the follow gate. Cache-local by design: kind-0
    /// search is thin relay-side, and the cache hydrates as the session
    /// reads. Sorted by name so the rows hold still run to run.
    pub fn search_people(&self, query: &str) -> Vec<Profile> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return vec![];
        }
        let m = self.profiles_mem.lock();
        let mut hits: Vec<&ProfileEntry> = m
            .entries()
            .filter(|e| e.name_l.contains(&q) || e.nip05_l.contains(&q) || e.hex.contains(&q))
            .collect();
        hits.sort_by(|a, b| a.name_l.cmp(&b.name_l));
        // The search pane's People tab is a proper results page now (not a
        // sidebar strip) — room for a real page.
        hits.truncate(20);
        hits.into_iter().map(|e| e.profile.clone()).collect()
    }

    pub(super) fn active_pubkey(&self) -> String {
        self.active
            .lock()
            .as_ref()
            .map(|a| a.session.pubkey.clone())
            .unwrap_or_default()
    }

    /// The active account's pubkey, when logged in — scheduled_list /
    /// scheduled_cancel gate on it (only the author touches their posts).
    pub(super) fn active_pubkey_opt(&self) -> Option<PublicKey> {
        PublicKey::from_str(&self.active_pubkey()).ok()
    }

    /// One page projection: store query (a PAGE+1 window so "has more" is
    /// knowable), then the mutes gate at the chokepoint.
    /// NIP-09/NIP-40 render filter: drop events their own author deleted
    /// (deletions.rs tombstone lookups — O(1), persisted, fed by the pool
    /// chokepoint) and events whose NIP-40 expiration has passed (the SDK
    /// persists expired copies; they must not paint). A stranger's
    /// kind:5 can't delete your notes, so the author must match — the
    /// tombstone store enforces that structurally.
    pub(super) fn drop_deleted(&self, notes: Vec<Event>) -> Vec<Event> {
        // One-time hydration: tombstones that entered the store BEFORE
        // the deletions module shipped would otherwise resurrect (the
        // module's own file starts empty). Idempotent — record keeps the
        // newest per key.
        if !self
            .tombstones_hydrated
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let pool = self.pool.lock().as_ref().map(Arc::clone);
            if let Some(pool) = pool {
                self.rt.spawn(async move {
                    if let Ok(tombs) = pool.store.tombstones().await {
                        for t in &tombs {
                            crate::deletions::record(t, false);
                        }
                        eprintln!("[core] tombstones hydrated: {}", tombs.len());
                    }
                });
            }
        }
        notes
            .into_iter()
            .filter(|ev| !crate::nip40::is_expired(ev) && !crate::deletions::is_deleted(ev))
            .collect()
    }

    pub(super) async fn project_page(
        &self,
        active: &Active,
        until: Option<u64>,
        kinds: &[Kind],
    ) -> Result<Vec<Event>> {
        let mut notes = active
            .pool
            .store
            .feed_page_kinds(&active.follows, until, PAGE + 1, kinds.to_vec())
            .await
            .map_err(|e| LoginError::Resume(e.to_string()))?;
        {
            let muted = self.muted.lock();
            notes.retain(|ev| !muted.contains(&ev.pubkey));
        }
        Ok(self.drop_deleted(notes))
    }

    pub(super) async fn page_from_store(
        &self,
        active: &Active,
        until: Option<u64>,
        kinds: &[Kind],
    ) -> Result<FeedPage> {
        let follows = active.follows.clone();
        let mut notes = active
            .pool
            .store
            .feed_page_kinds(&follows, until, PAGE, kinds.to_vec())
            .await
            .map_err(|e| LoginError::Resume(e.to_string()))?;
        // Mutes + deletions gate here — every feed page passes this
        // chokepoint (pool.ts: NIP-09/NIP-40 consume-at-the-chokepoint role).
        {
            let muted = self.muted.lock();
            notes.retain(|ev| !muted.contains(&ev.pubkey));
        }
        notes = self.drop_deleted(notes);
        let profiles = Self::profiles_for(active, &notes).await;
        self.absorb_profiles(&profiles);
        for p in &profiles {
            active.profiles.absorb(p.clone());
        }
        Ok(FeedPage { notes, profiles })
    }

    /// The per-event mention scan memo (D80 perf lens): profiles_for runs
    /// repeatedly over overlapping pages (the notifications task's cached
    /// and union phases, endless-scroll paging), and each scan is a bech32
    /// decode attempt per whitespace token — the render side already memos
    /// its own twin (content.rs's media_urls_cached shape). Keyed by event
    /// id, FIFO capped.
    pub(super) fn mention_pks_cached(note: &Event) -> Vec<PublicKey> {
        const CAP: usize = 4096;
        struct Memo {
            map: HashMap<EventId, std::sync::Arc<[PublicKey]>>,
            order: std::collections::VecDeque<EventId>,
        }
        static MEMO: std::sync::OnceLock<parking_lot::Mutex<Memo>> = std::sync::OnceLock::new();
        let memo = MEMO.get_or_init(|| {
            parking_lot::Mutex::new(Memo {
                map: HashMap::new(),
                order: std::collections::VecDeque::new(),
            })
        });
        let mut memo = memo.lock();
        if let Some(hit) = memo.map.get(&note.id) {
            return hit.to_vec();
        }
        let pks: std::sync::Arc<[PublicKey]> = crate::nip::nip19::mention_pks(&note.content).into();
        if memo.map.len() >= CAP
            && let Some(old) = memo.order.pop_front()
        {
            memo.map.remove(&old);
        }
        memo.order.push_back(note.id);
        memo.map.insert(note.id, pks.clone());
        pks.to_vec()
    }

    /// Cached profiles for a page's authors. Missing metadata is NOT
    /// fetched here — this sat between the deep fetch and every page
    /// landing (bootstrap, badge click, older), gating the paint on a
    /// client-wide relay round trip. The misses go to the profiles
    /// service's background warm instead; the reactive machinery (the
    /// panes' ensure_profiles pumps) patches names/avatars as they land.
    pub(super) async fn profiles_for(active: &Active, notes: &[Event]) -> Vec<Profile> {
        // Authors PLUS everyone a body @-mentions (nip19::mention_pks):
        // mention names used to resolve only where a view pumped them
        // (feed/reader/thread), so the same note showed @name in the
        // timeline and @npub1… in the notifications pane or on a profile.
        // The store cache answers what it has; the misses warm in the
        // background.
        // D80 (security lens): mention pks are attacker-controlled — a
        // hostile note can carry thousands of distinct npub tokens — so
        // the collection is deduped across the page and the mention count
        // CAPPED. Without the cap one page load scaled the warm set (and
        // the chunked kind-0 queries behind it) with the attacker's
        // tokens; the cap bounds it to a handful of round trips.
        const MENTION_HYDRATE_CAP: usize = 512;
        let mut seen: std::collections::HashSet<PublicKey> = std::collections::HashSet::new();
        let mut authors: Vec<PublicKey> = Vec::with_capacity(notes.len());
        for e in notes {
            if seen.insert(e.pubkey) {
                authors.push(e.pubkey);
            }
        }
        let mut mentions = 0usize;
        'scan: for note in notes {
            for pk in Self::mention_pks_cached(note) {
                if seen.insert(pk) {
                    if mentions >= MENTION_HYDRATE_CAP {
                        break 'scan;
                    }
                    mentions += 1;
                    authors.push(pk);
                }
            }
        }
        let cached = active
            .pool
            .store
            .profiles(&authors)
            .await
            .unwrap_or_default();
        let profiles: Vec<Profile> = cached.iter().filter_map(Profile::from_event).collect();
        // The resolved set once, not a String allocation per (author ×
        // profile) comparison (D80 perf lens).
        let resolved: std::collections::HashSet<PublicKey> =
            profiles.iter().map(|p| p.pubkey).collect();
        let missing: Vec<PublicKey> = authors
            .into_iter()
            .filter(|a| !resolved.contains(a))
            .collect();
        active.profiles.warm(missing);
        profiles
    }
}
