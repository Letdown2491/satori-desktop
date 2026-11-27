//! Notifications, read-state dots, list backup, comments, polls, goals.

use super::*;

impl Core {
    // --- notifications ------------------------------------------------------------

    /// The notification stream (port of `src/data/notifications.ts`, slice 1):
    /// replies to my notes + mentions (#p=me), newest first, own events
    /// dropped, reactions/poll-votes/zap classes land with their milestones.
    pub fn notifications(self: &Arc<Self>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<(Vec<Event>, Vec<Profile>, bool)> = async {
                let active = this.active().await?;
                let me = active.me();

                // Cached candidates paint instantly; the relay phase runs in
                // parallel below and the response re-renders.
                let my_notes = active
                    .pool
                    .store
                    // 500, not 100: bots hunt OLDER notes (a reply to
                    // anything past the window isn't a "reply" and would
                    // ring the bell ungated). 500 own ids is still one
                    // small HashSet (D24: bounded, named).
                    .feed_page(&[me], None, 500)
                    .await
                    .unwrap_or_default();
                let ids: Vec<EventId> = my_notes.iter().map(|e| e.id).collect();

                // Instant paint: the store's cached candidates (kind-1
                // replies, NIP-22 comments — the Amethyst migration
                // made 1111 the carrier for non-kind-1 replies — and the
                // engagement kinds: reactions/zaps/votes by `e` tag).
                // Three batched index passes for the whole id set: this
                // used to run one query per category PER NOTE — up to 300
                // sequential LMDB transactions for a 100-note window.
                let reaction_notifs = this.reaction_notifs();
                let mut cached: Vec<Event> = Vec::new();
                cached.extend(
                    active
                        .pool
                        .store
                        .replies_of(ids.clone())
                        .await
                        .unwrap_or_default(),
                );
                cached.extend(
                    active
                        .pool
                        .store
                        .comments_of(ids.clone())
                        .await
                        .unwrap_or_default(),
                );
                cached.extend(
                    active
                        .pool
                        .store
                        .engagement_of(ids.clone())
                        .await
                        .unwrap_or_default(),
                );
                cached.retain(|ev| ev.pubkey != me);
                // The relay phase filters these too — the cached phase
                // must wear the same gates (a cold start previously showed
                // a stranger's thumbs-down the relay phase would have
                // dropped, and reactions when the pref was off).
                cached.retain(|ev| {
                    !(u16::from(ev.kind) == 7 && (!reaction_notifs || ev.content.trim() == "-"))
                });
                // The chokepoint gates too: the union keeps cached rows in
                // the final page, so a deleted (NIP-09) or expired
                // (NIP-40) cached copy must not paint here either.
                cached.retain(|ev| !nip40::is_expired(ev) && !deletions::is_deleted(ev));
                {
                    let muted = this.muted.lock();
                    cached.retain(|ev| !muted.contains(&ev.pubkey));
                }
                // The WoT tier (WS4): out-of-network reactions/reposts come
                // out as receipts — the cached phase's list rides the same
                // gates the relay phase wears (the phase-equality rule
                // above). The final union re-splits; the receipts store
                // holds the union.
                let my_note_ids: HashSet<EventId> = ids.iter().copied().collect();
                let (kept, mut cached_suppressed) = split_wot_tier(cached, &my_note_ids, false);
                let cached = kept;
                let profiles = Self::profiles_for(&active, &cached).await;
                // A clone rides the instant paint; `cached` stays here
                // for the final union (below) — the second emit must be
                // a SUPERSET of the first, never a replacement.
                let _ = tx.send(FeedEvent::Notifications(cached.clone(), profiles));

                let since = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
                    .saturating_sub(7 * 86400);

                // Mentions (#p=me) and replies (#e=my notes): PARALLEL.
                // (The pref was already read for the cached phase — no
                // second disk read here.)
                let mut mention_kinds =
                    vec![Kind::TextNote, Kind::Custom(1111), Kind::Custom(9735)];
                if reaction_notifs {
                    mention_kinds.push(Kind::Reaction);
                }
                let mention_q = Box::pin(
                    active.pool.get(
                        Filter::new()
                            .kinds(mention_kinds)
                            .pubkeys([me])
                            .since(Timestamp::from(since))
                            .limit(200),
                    ),
                );
                let reply_q = if ids.is_empty() {
                    None
                } else {
                    Some(Box::pin(
                        active.pool.get(
                            Filter::new()
                                .kinds([Kind::TextNote, Kind::Custom(1111)])
                                .events(ids.clone())
                                .since(Timestamp::from(since))
                                .limit(200),
                        ),
                    ))
                };
                let poll_ids: Vec<EventId> = my_notes
                    .iter()
                    .filter(|e| u16::from(e.kind) == 1068)
                    .map(|e| e.id)
                    .collect();
                let vote_q = Box::pin(async {
                    if poll_ids.is_empty() {
                        return Default::default();
                    }
                    let mut f = Filter::new()
                        .kind(Kind::Custom(1018))
                        .since(Timestamp::from(since))
                        .limit(200);
                    for id in poll_ids {
                        f = f.event(id);
                    }
                    active.pool.get(f).await.unwrap_or_default()
                });
                // NIP-18 reach (this task): reposts of my notes (kind 6,
                // or 16 for the verticals) arrive #e-tagged; a QUOTE of my
                // note (kind 1/1111 carrying `q`) tags nobody at all —
                // each needs its own query. No pref gate: direct
                // engagement, like a reply.
                let repost_q = if ids.is_empty() {
                    None
                } else {
                    Some(Box::pin(
                        active.pool.get(
                            Filter::new()
                                .kinds([Kind::Repost, Kind::Custom(16)])
                                .events(ids.clone())
                                .since(Timestamp::from(since))
                                .limit(200),
                        ),
                    ))
                };
                let quote_ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
                let quote_q = if quote_ids.is_empty() {
                    None
                } else {
                    Some(Box::pin(
                        active.pool.get(
                            Filter::new()
                                .kinds([Kind::TextNote, Kind::Custom(1111)])
                                .custom_tags(SingleLetterTag::LOWERCASE_Q, quote_ids)
                                .since(Timestamp::from(since))
                                .limit(200),
                        ),
                    ))
                };
                let (mentions_res, replies_res, votes_res, reposts_res, quotes_res) = tokio::join!(
                    mention_q,
                    async {
                        match reply_q {
                            Some(q) => q.await.unwrap_or_default(),
                            None => Default::default(),
                        }
                    },
                    vote_q,
                    async {
                        match repost_q {
                            Some(q) => q.await.unwrap_or_default(),
                            None => Default::default(),
                        }
                    },
                    async {
                        match quote_q {
                            Some(q) => q.await.unwrap_or_default(),
                            None => Default::default(),
                        }
                    }
                );
                let mine_events: Vec<Event> = mentions_res
                    .into_iter()
                    .flat_map(|set| set.into_iter())
                    .chain(replies_res)
                    .chain(votes_res)
                    .chain(reposts_res)
                    .chain(quotes_res)
                    .collect();
                // (no explicit save: the SDK persists what it receives)

                // Dedup + drop own + sort desc — as a UNION with the
                // cached paint, not a replacement. The two phases have
                // different memberships (the cached paint is
                // replies/comments/engagement on my recent notes, any age;
                // the relay phase is mentions/replies/votes within the
                // 7-day window), so emitting the relay set ALONE made the
                // list visibly reload on every open: old rows vanished
                // while the window's new ones appeared. The union keeps
                // both: the page paints instantly, then only GROWS as the
                // fresh rows land on top.
                let mut seen = std::collections::HashSet::new();
                let muted = this.muted.lock().clone();
                let mut notifs: Vec<Event> = mine_events
                    .into_iter()
                    .chain(cached)
                    .filter(|ev| {
                        ev.pubkey != me
                            && seen.insert(ev.id)
                            // The @geek gate: the relay's answer to the
                            // #p=me filter must BE about me (the bell
                            // already verified; the page now does too).
                            && notif_about_me(ev, &me, &my_note_ids)
                            && !muted.contains(&ev.pubkey)
                            // A stranger's thumbs-down is pure agitation
                            // (notifications.ts): never shown. +/emoji stay.
                            && !(u16::from(ev.kind) == 7 && ev.content.trim() == "-")
                    })
                    .collect();
                notifs.sort_by_key(|e| std::cmp::Reverse(e.created_at.as_secs()));

                // The tier on the final union, then the receipts store
                // becomes the union of both phases' pulls (deduped by id) —
                // the bell's "N hidden" reads exactly this.
                // `count_velocity = true`: this is the delivering split —
                // the receipts store is written from it, so only its
                // admitted replies feed the velocity counter (a suppressed
                // reply must never count toward the flood verdict; the
                // cached-phase pass above checked the gate without
                // counting).
                let (kept, mut suppressed) = split_wot_tier(notifs, &my_note_ids, true);
                cached_suppressed.append(&mut suppressed);
                let mut seen_sup = HashSet::new();
                cached_suppressed.retain(|s| seen_sup.insert(s.event.id));
                // RAM wears the same cap as the disk ledger (the memory
                // lens: the boot load and the flusher truncate; the
                // in-memory store must not hold more than it writes).
                cached_suppressed.truncate(SUPPRESSED_CAP);
                *SUPPRESSED.lock() = cached_suppressed;
                suppressed_dirty(); // the receipts ledger persists (D48)
                let notifs = kept;

                let profiles = Self::profiles_for(&active, &notifs).await;

                // Dot state: anything newer than the high-water?
                let high_water = crate::read_state::load().notif;
                let unread = notifs
                    .first()
                    .map(|e| e.created_at.as_secs() > high_water)
                    .unwrap_or(false);

                Ok((notifs, profiles, unread))
            }
            .await;
            match res {
                Ok((notifs, profiles, unread)) => {
                    let _ = tx.send(FeedEvent::Notifications(notifs, profiles));
                    let _ = tx.send(FeedEvent::NotifDot(unread));
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    /// The "Show reactions in notifications" pref (prefs.json
    /// reactionNotifs, default off — render/settings.ts PREF_LABELS).
    pub(super) fn reaction_notifs(&self) -> bool {
        crate::config::prefs()
            .and_then(|v| v.get("reactionNotifs").and_then(|b| b.as_bool()))
            .unwrap_or(false)
    }

    /// Opening the notifications page advances the high-water (monotonic).
    pub fn mark_notifications_seen(self: &Arc<Self>) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        crate::read_state::advance(|s| s.notif = s.notif.max(now));
    }

    /// Reply-presence: per note, who replied (up to 3, earliest first) —
    /// the web app's "faces" hydration (routes/feed.ts getFaces).
    pub fn reply_faces(self: &Arc<Self>, ids: Vec<EventId>) -> FeedRx {
        self.feed_task(move |active| {
            Box::pin(async move {
                let filter = Filter::new()
                    .kind(Kind::TextNote)
                    .events(ids.clone())
                    .limit(600);
                let mut out: Vec<(EventId, Vec<PublicKey>)> = Vec::new();
                let mut per_note: std::collections::HashMap<EventId, Vec<PublicKey>> =
                    std::collections::HashMap::new();
                let id_set: std::collections::HashSet<EventId> = ids.iter().copied().collect();
                let mut replies: Vec<Event> = active
                    .pool
                    .get(filter)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                replies.sort_by_key(|e| e.created_at.as_secs());
                for r in replies {
                    // Parent = last e tag within the requested set.
                    if let Some(pid) = r
                        .tags
                        .clone()
                        .event_ids()
                        .last()
                        .filter(|pid| id_set.contains(pid))
                    {
                        let names = per_note.entry(pid).or_default();
                        if names.len() < 3 && !names.contains(&r.pubkey) {
                            names.push(r.pubkey);
                        }
                    }
                }
                for (id, names) in per_note {
                    out.push((id, names));
                }
                Ok(FeedEvent::Faces(out))
            })
        })
    }

    /// Comment counts per note (the action row's bubble): how many
    /// kind-1 replies + kind-1111 comments name the note as their parent.
    /// One batched relay round (reply_faces' shape — the REQ also
    /// hydrates the store for later thread opens), parent = the last `e`
    /// tag within the requested set (a reply-to-a-reply counts for the
    /// reply, not the note — the same parent rule the faces use).
    /// Addressable parents (pictures with a d tag, articles) are keyed by
    /// ADDRESS, not event id — their counts ride the address-carrying
    /// path, not this one (#32's notifications-door item).
    pub fn comment_counts(
        self: &Arc<Self>,
        targets: Vec<(EventId, crate::nip::comment::CommentTarget)>,
    ) -> FeedRx {
        self.feed_task(move |active| {
            Box::pin(async move {
                // Two filter shapes per page, BOTH batched (one REQ each):
                // a #e OR-list for the event-target rows and #a OR-lists
                // for the addressable rows (a parameterized picture's
                // comment thread hangs off the ADDRESS). The addresses
                // chunk at 8 per filter — the fan-out is bounded by page
                // size, not by row count (audit usage/security: the old
                // per-row REQ fired one subscription per picture row, and
                // Bookmarks mode had no page bound at all). custom_tags
                // serializes a NIP-01 OR list — the same form the quotes
                // fetch rides (.custom_tags(LOWERCASE_Q, ...)).
                let mut ids: Vec<EventId> = Vec::new();
                let mut addrs: Vec<String> = Vec::new();
                let mut addr_rows: std::collections::HashMap<String, EventId> =
                    std::collections::HashMap::new();
                for (row, t) in &targets {
                    match t {
                        crate::nip::comment::CommentTarget::Event { .. } => ids.push(*row),
                        _ => {
                            let a = t
                                .address_string()
                                .expect("only the Address arm falls out of for_event's else");
                            addrs.push(a.clone());
                            addr_rows.insert(a, *row);
                        }
                    }
                }
                let mut filters = Vec::new();
                if !ids.is_empty() {
                    filters.push(
                        Filter::new()
                            .kinds([Kind::TextNote, Kind::Custom(1111)])
                            .events(ids)
                            .limit(crate::outbox::QUERY_LIMIT),
                    );
                }
                const ADDR_CHUNK: usize = 8;
                for chunk in addrs.chunks(ADDR_CHUNK) {
                    filters.push(
                        Filter::new()
                            .kind(Kind::Custom(1111))
                            .custom_tags(SingleLetterTag::LOWERCASE_A, chunk.iter().cloned())
                            .limit(crate::outbox::QUERY_LIMIT),
                    );
                }
                // The shapes fetch concurrently (each READ_BUDGET-bounded);
                // the tally dedups by id — a both-shapes comment comes back
                // from two filters.
                let gets: Vec<_> = filters.into_iter().map(|f| active.pool.get(f)).collect();
                let rows: Vec<Event> = futures_util::future::join_all(gets)
                    .await
                    .into_iter()
                    .flat_map(|r| r.unwrap_or_default())
                    .collect();
                Ok(FeedEvent::CommentCounts(
                    count_comment_targets(rows, &targets).into_iter().collect(),
                ))
            })
        })
    }

    /// Does the following feed hold notes newer than the last home view?
    /// (Quiet-dot poll — the store answers, no relays.)
    pub fn feed_dot(self: &Arc<Self>) -> FeedRx {
        self.feed_task(move |active| {
            Box::pin(async move {
                // Same probe as refresh (newest_created_at docs): the
                // all-kinds page materialized one event per author-kind
                // pair per poll.
                let newest = active
                    .pool
                    .store
                    .newest_created_at(&active.follows)
                    .await
                    .map_err(|e| LoginError::Resume(e.to_string()))?;
                Ok(FeedEvent::NotifDot(
                    newest
                        .map(|t| t > crate::read_state::load().feed_seen)
                        .unwrap_or(false),
                ))
            })
        })
    }

    /// Home opened: the feed is seen up to now (monotonic).
    pub fn mark_feed_seen(self: &Arc<Self>) {
        crate::read_state::advance(|s| s.feed_seen = s.feed_seen.max(now_secs()));
    }

    /// Export the user's list events (kinds 3/10000/10002/10003) as raw
    /// signed JSON — the list-backup.ts export.
    pub fn export_lists(self: &Arc<Self>) -> FeedRx {
        self.feed_task(move |active| {
            Box::pin(async move {
                let me = active.me();
                let filter = Filter::new().author(me).kinds([
                    Kind::ContactList,
                    Kind::Custom(10000),
                    Kind::Custom(10002),
                    Kind::Custom(10003),
                ]);
                let raw = active.pool.get(filter).await.unwrap_or_default();
                // Keep the newest per kind.
                let mut by_kind: std::collections::HashMap<u16, Event> =
                    std::collections::HashMap::new();
                for ev in raw {
                    by_kind.insert(u16::from(ev.kind), ev);
                }
                let events: Vec<&Event> = by_kind.values().collect();
                let json = serde_json::to_string_pretty(&events)
                    .map_err(|e| LoginError::Resume(e.to_string()))?;
                Ok(FeedEvent::BackupJson(json))
            })
        })
    }

    /// Restore from a backup: re-sign each event fresh (list-backup.ts:
    /// replace = same tags/content, new created_at) and publish.
    pub fn import_lists(self: &Arc<Self>, json: String) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<u32> = async {
                let active = this.active().await?;
                let me = active.me();
                let events: Vec<Event> = serde_json::from_str(&json)
                    .map_err(|e| LoginError::Resume(format!("bad backup: {e}")))?;
                let mut n = 0;
                for ev in events {
                    if ev.pubkey != me {
                        continue; // only restore own lists
                    }
                    let unsigned = UnsignedEvent::new(
                        me,
                        Timestamp::now(),
                        ev.kind,
                        ev.tags.clone().to_vec(),
                        ev.content.clone(),
                    );
                    let signed = active.sign_report(unsigned).await?;
                    Self::commit(&active, &signed, &tx).await?;
                    n += 1;
                }
                Ok(n)
            }
            .await;
            match res {
                Ok(n) => {
                    let _ = tx.send(PublishEvent::Restored(n));
                }
                Err(e) => {
                    let _ = tx.send(PublishEvent::Failed {
                        token: 0,
                        error: e.to_string(),
                    });
                }
            }
        });
        rx
    }

    /// Comments on a target (NIP-22 kind:1111), oldest first.
    pub fn comments(self: &Arc<Self>, target: crate::nip::comment::CommentTarget) -> FeedRx {
        let this = Arc::clone(self);
        self.feed_task(move |active| {
            Box::pin(async move {
                let mut comments: Vec<Event> = Vec::new();
                for filter in target.fetch_filters() {
                    for ev in active.pool.get(filter).await.unwrap_or_default() {
                        comments.push(ev);
                    }
                }
                // (no explicit save: the SDK persists what it receives)
                // Dedup + oldest first.
                let mut seen = std::collections::HashSet::new();
                comments.retain(|ev| seen.insert(ev.id));
                comments.sort_by_key(|e| e.created_at.as_secs());
                let _authors: Vec<PublicKey> = comments.iter().map(|e| e.pubkey).collect();
                let profiles = Self::profiles_for(active, &comments).await;
                this.absorb_profiles(&profiles);
                // Include target-author profile (fetch may be empty).

                Ok(FeedEvent::Comments(comments, profiles))
            })
        })
    }

    /// Publish a comment (top-level when `parent` is None, else a reply).
    /// `imetas` (the uploaded NIP-92 arrays) ride the kind-1111 when the
    /// comment carries media — NIP-92 is event-kind agnostic, and dropping
    /// them here would silently lose an attachment on every reply to a
    /// non-kind-1 root (issue #3 WS1).
    pub fn comment(
        self: &Arc<Self>,
        content: String,
        target: crate::nip::comment::CommentTarget,
        parent: Option<Event>,
        mentions: Vec<PublicKey>,
        imetas: Vec<Vec<String>>,
    ) -> PublishRx {
        let core = Arc::clone(self);
        self.publish_task(move |active, tx| {
            Box::pin(async move {
                let mut tags = match &parent {
                    Some(p) => crate::nip::comment::reply_tags(p, &target),
                    None => target.root_tags(),
                };
                let mut seen_p: Vec<String> = tags
                    .iter()
                    .filter(|t| t.as_slice().first().map(String::as_str) == Some("p"))
                    .filter_map(|t| t.as_slice().get(1).cloned())
                    .collect();
                for who in mentions {
                    let hex = who.to_string();
                    if hex != active.me().to_string() && !seen_p.contains(&hex) {
                        seen_p.push(hex.clone());
                        tags.push(Tag::public_key(who));
                    }
                }
                for m in &imetas {
                    if m.len() >= 2 && m[0] == "imeta" {
                        tags.push(Tag::custom("imeta", m[1..].iter()));
                    }
                }
                tags.push(Tag::custom("client", ["satori-desktop"]));
                let unsigned = UnsignedEvent::new(
                    active.me(),
                    Timestamp::now(),
                    Kind::Custom(1111),
                    tags,
                    Self::content_with_media_urls(content, &imetas),
                );
                let signed = active.sign_report(unsigned).await?;
                // Same undo window as notes (the reply toast is the note
                // toast — one seam, hold_and_commit).
                match active.undo.hold_duration() {
                    Some(hold) => {
                        let token = core.hold_seq.fetch_add(1, Ordering::SeqCst);
                        let _ = tx.send(PublishEvent::Held {
                            token,
                            event: signed.clone(),
                            hold_secs: hold.as_secs(),
                        });
                        core.hold_and_commit(
                            Arc::new(active.clone()),
                            token,
                            signed,
                            hold,
                            tx.clone(),
                        )
                        .await;
                    }
                    None => {
                        Self::commit(active, &signed, tx).await?;
                    }
                }
                Ok(())
            })
        })
    }

    /// Tallies for a poll: kind:1018 ballots #e=poll id.
    pub fn poll_votes(self: &Arc<Self>, poll_id: EventId) -> FeedRx {
        self.feed_task(move |active| {
            Box::pin(async move {
                let filter = Filter::new()
                    .kind(Kind::Custom(1018))
                    .event(poll_id)
                    .limit(crate::outbox::QUERY_LIMIT);
                let mut votes: Vec<Event> = Vec::new();
                for ev in active.pool.get(filter).await.unwrap_or_default() {
                    votes.push(ev);
                }
                // (no explicit save: the SDK persists what it receives)
                Ok(FeedEvent::PollVotes(crate::nip::poll::tally(&votes)))
            })
        })
    }

    /// A zap goal's raised total (NIP-75): the receipts that name the
    /// goal's coordinate in their `a` tag, summed. The goal rides in (the
    /// view has it in hand); the store/network ask is receipts-only, `a`
    /// -scoped so other goals' receipts never land here.
    pub fn goal_progress(self: &Arc<Self>, goal: &Event) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        let goal = goal.clone();
        self.rt.spawn(async move {
            let res: Result<u64> = async {
                let coord = match crate::nip::goal::coordinate(&goal) {
                    Some(c) => c,
                    None => return Ok(0),
                };
                let active = this.active().await?;
                let filter = Filter::new()
                    .kind(Kind::Custom(9735))
                    .custom_tag(SingleLetterTag::LOWERCASE_A, coord)
                    .limit(crate::outbox::QUERY_LIMIT);
                let receipts: Vec<Event> = active
                    .pool
                    .get(filter)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                // (no explicit save: the SDK persists what it receives)
                Ok(crate::nip::goal::raised_msat(&goal, &receipts))
            }
            .await;
            match res {
                Ok(msat) => {
                    let _ = tx.send(FeedEvent::GoalMsat(goal.id, msat));
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    /// Vote in a poll (NIP-88 kind:1018; re-voting replaces your ballot).
    pub fn poll_vote(self: &Arc<Self>, poll: &Event, option_ids: Vec<String>) -> PublishRx {
        let poll = poll.clone();
        self.publish_task(move |active, tx| {
            Box::pin(async move {
                // The vote's e tag hints the poll author's most-recent
                // seen relay (actions.ts:118) so clients that never saw
                // the poll can still ask where it lives.
                let mut tags = vec![
                    seen::hint_tag(poll.id, &poll.pubkey),
                    Tag::public_key(poll.pubkey),
                ];
                for opt in option_ids {
                    tags.push(Tag::custom("response", [opt]));
                }
                let unsigned =
                    UnsignedEvent::new(active.me(), Timestamp::now(), Kind::Custom(1018), tags, "");
                let signed = active.sign_report(unsigned).await?;
                Self::commit(active, &signed, tx).await?;
                Ok(())
            })
        })
    }

    /// RSVP to a calendar event (NIP-52 kind:31925).
    pub fn rsvp(self: &Arc<Self>, calendar: &Event, status: &str) -> PublishRx {
        let calendar = calendar.clone();
        let status = status.to_string();
        self.publish_task(move |active, tx| {
            Box::pin(async move {
                let data = crate::nip::calendar::parse(&calendar)
                    .ok_or_else(|| LoginError::Resume("bad calendar event".into()))?;
                let a = crate::nip::calendar::a_tag(&data, &calendar.pubkey);
                let unsigned = UnsignedEvent::new(
                    active.me(),
                    Timestamp::now(),
                    Kind::Custom(31925),
                    [
                        Tag::custom("a", [a]),
                        Tag::event(calendar.id),
                        Tag::public_key(calendar.pubkey),
                        Tag::custom("status", [status]),
                    ],
                    "",
                );
                let signed = active.sign_report(unsigned).await?;
                Self::commit(active, &signed, tx).await?;
                Ok(())
            })
        })
    }

    /// Repost (NIP-18): kind 6 for notes, kind 16 + a `k` tag for anything
    /// else — and the same undo window as every publish (hold_and_commit;
    /// the shell's toast carries the countdown and the Undo).
    pub fn repost(self: &Arc<Self>, target: &Event) -> PublishRx {
        let core = Arc::clone(self);
        let target = target.clone();
        self.publish_task(move |active, tx| {
            Box::pin(async move {
                let note = u16::from(target.kind) == 1;
                let mut tags = vec![
                    seen::hint_tag(target.id, &target.pubkey),
                    Tag::public_key(target.pubkey),
                ];
                if !note {
                    tags.push(Tag::custom("k", [u16::from(target.kind).to_string()]));
                }
                let unsigned = UnsignedEvent::new(
                    active.me(),
                    Timestamp::now(),
                    if note { Kind::Repost } else { Kind::Custom(16) },
                    tags,
                    format!("nostr:{}", target.id.to_bech32().unwrap_or_default()),
                );
                let signed = active.sign_report(unsigned).await?;
                // Same undo window as every publish (hold_and_commit — the
                // comment/poll_vote shape; the audit's duplication lens:
                // this was the 15th site the #81 collapse missed).
                match active.undo.hold_duration() {
                    Some(hold) => {
                        let token = core.hold_seq.fetch_add(1, Ordering::SeqCst);
                        let _ = tx.send(PublishEvent::Held {
                            token,
                            event: signed.clone(),
                            hold_secs: hold.as_secs(),
                        });
                        core.hold_and_commit(
                            Arc::new(active.clone()),
                            token,
                            signed,
                            hold,
                            tx.clone(),
                        )
                        .await;
                    }
                    None => Self::commit(active, &signed, tx).await?,
                }
                Ok(())
            })
        })
    }

    /// Resolve a note/nevent bech32 to its event (embed/quote previews).
    pub fn event_by_id_dech32(self: &Arc<Self>, bech32: &str) -> FeedRx {
        use nostr::prelude::*;
        match Nip19::from_bech32(bech32) {
            // The nevent carries author + relay hints when the publisher
            // was polite (NIP-01 recommends both) — resolveEvent uses
            // them to skip the indexer fallback entirely.
            Ok(Nip19::Event(e)) => self.event_by_id(
                e.event_id,
                e.relays.iter().map(|r| r.to_string()).collect(),
                e.author,
            ),
            Ok(Nip19::EventId(id)) => self.event_by_id(id, Vec::new(), None),
            _ => {
                let (tx, rx) = mpsc::unbounded_channel();
                let _ = tx.send(FeedEvent::One(None, None));
                rx
            }
        }
    }

    /// Resolve one event by id (repost targets); None if unknown everywhere.
    /// Batch-resolve reply parents (the timeline's context whisper): store
    /// first, then one by-id relay round trip for the misses. Emits
    /// FeedEvent::Parents with whatever resolved — misses (deleted parents,
    /// relay gaps) just don't resolve, and the whisper degrades.
    /// The signed-in identity's pubkey hex, if any — views use it to
    /// phrase "your note" vs "{name}'s note" (the notifications whisper).
    pub fn me(&self) -> Option<String> {
        self.active
            .lock()
            .as_ref()
            .map(|a| a.session.pubkey.clone())
    }

    pub fn load_parents(self: &Arc<Self>, ids: Vec<String>) -> FeedRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let mut resolved: std::collections::HashMap<String, ParentNote> =
                std::collections::HashMap::new();
            let _: Result<()> = async {
                let active = this.active().await?;
                // Parents key by hex id OR coordinate ("kind:author:d" —
                // NIP-22 address parents have no event id to resolve; the
                // coordinate's replaceable pointer is the target).
                let mut want: Vec<EventId> = Vec::new();
                let mut coords: Vec<(u16, PublicKey, String)> = Vec::new();
                for id in &ids {
                    if let Ok(eid) = EventId::from_hex(id) {
                        want.push(eid);
                    } else if let Some(crate::nip::comment::Ref::Address { kind, author, d }) =
                        crate::nip::comment::Ref::from_coordinate(id)
                    {
                        coords.push((kind, author, d));
                    }
                }
                let mut events: Vec<Event> = Vec::new();
                for id in &want {
                    // The store hit path wears the same gates as the
                    // network one (the chokepoint): expired or
                    // author-deleted parents don't resolve — the whisper
                    // degrades, which is the point.
                    if let Some(ev) = active.pool.store.event_by_id(*id).await.ok().flatten()
                        && !nip40::is_expired(&ev)
                        && !deletions::is_deleted(&ev)
                    {
                        events.push(ev);
                    }
                }
                for (kind, author, d) in &coords {
                    let k = Kind::from_u16(*kind);
                    if let Ok(Some(ev)) = active.pool.store.latest_by_kind_d(*author, k, d).await
                        && !nip40::is_expired(&ev)
                        && !deletions::is_deleted(&ev)
                    {
                        events.push(ev);
                    }
                }
                let have: HashSet<EventId> = events.iter().map(|e| e.id).collect();
                let missing: Vec<EventId> = want
                    .iter()
                    .filter(|id| !have.contains(id))
                    .cloned()
                    .collect();
                if !missing.is_empty() {
                    // feeds.ts fetchEventsByIds: a TARGETED query on the
                    // indexers (parents' authors are unknown until they
                    // resolve, so seen-on can't narrow this one). Split at
                    // the REQ ceiling (WS1: OutboxRouter.kt
                    // MAX_ETAGS_PER_FILTER — big relays reject oversized
                    // filters).
                    let fallback: Vec<String> = INDEXERS.iter().map(|s| s.to_string()).collect();
                    for id_shard in outbox::chunk(&missing, outbox::MAX_ETAGS_PER_FILTER) {
                        let n = id_shard.len();
                        let filter = Filter::new().ids(id_shard).limit(n);
                        let fetched: Vec<Event> = active
                            .pool
                            .get_from(&fallback, &filter)
                            .await
                            .into_iter()
                            .collect();
                        // (no explicit save: the SDK persists what it receives)
                        events.extend(fetched);
                    }
                }
                // Coordinate misses: one filter per coordinate (they're
                // rare — only comments on addressable parents land here),
                // on the indexers. Concurrent (audit: the serial loop was
                // up to 60 sequential indexer round-trips inside one
                // spawned task — one comment-heavy thread on addressable
                // parents stalled the whole ask; the shape count() uses).
                let misses = futures_util::future::join_all(coords.iter().filter_map(
                    |(kind, author, d)| {
                        // A store hit already resolved this coordinate.
                        if events.iter().any(|e| {
                            u16::from(e.kind) == *kind
                                && e.pubkey == *author
                                && e.tags.identifier().as_deref() == Some(d.as_str())
                        }) {
                            return None;
                        }
                        let k = Kind::from_u16(*kind);
                        let filter = Filter::new().kind(k).author(*author).identifier(d).limit(1);
                        let pool = &active.pool;
                        Some(async move {
                            let fetched = pool
                                .get_from(
                                    &INDEXERS.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                                    &filter,
                                )
                                .await;
                            // (no explicit save: the SDK persists what it
                            // receives)
                            fetched
                                .into_iter()
                                .filter(|ev| !nip40::is_expired(ev) && !deletions::is_deleted(ev))
                                .collect::<Vec<_>>()
                        })
                    },
                ))
                .await;
                for batch in misses {
                    events.extend(batch);
                }
                for ev in &events {
                    let snippet = plain_snippet(&ev.content, 140);
                    resolved.insert(
                        ev.id.to_string(),
                        ParentNote {
                            author: ev.pubkey,
                            snippet,
                        },
                    );
                }
                // The coordinate parents key by the coordinate (the string
                // notif_parent_id returned), not the event id.
                for (kind, author, d) in &coords {
                    let key = format!("{kind}:{author}:{d}");
                    if let Some(ev) = events.iter().find(|e| {
                        u16::from(e.kind) == *kind
                            && e.pubkey == *author
                            && e.tags.identifier().as_deref() == Some(d.as_str())
                    }) {
                        resolved.insert(
                            key,
                            ParentNote {
                                author: ev.pubkey,
                                snippet: plain_snippet(&ev.content, 140),
                            },
                        );
                    }
                }
                Ok(())
            }
            .await;
            if !resolved.is_empty() {
                let _ = tx.send(FeedEvent::Parents(resolved));
            }
        });
        rx
    }

    /// One event by id: the store, then the author's declared write
    /// relays + where we've seen them (+ hints), then the indexers;
    /// NIP-40/NIP-09 screened. Shared by `event_by_id` and the thread's
    /// ancestor walk.
    pub(super) async fn resolve_event(
        self: &Arc<Self>,
        active: &Active,
        id: EventId,
        hints: Vec<String>,
        author: Option<PublicKey>,
    ) -> Option<Event> {
        let this = self;
        let cached = active.pool.store.event_by_id(id).await.ok().flatten();
        match cached {
            Some(ev) => Some(ev),
            None => {
                // feeds.ts resolveEvent: with a known author, race
                // their DECLARED write relays + the relays we've
                // empirically SEEN them on (+ nevent hints) first —
                // read-dead relays dropped, a get against one just
                // burns its share of the race. The big indexers are
                // the fallback (an aggregator that can miss), not
                // the primary.
                let filter = Filter::new().id(id).limit(1);
                let mut primary = hints.clone();
                if let Some(pk) = author {
                    let lists = active.pool.relay_lists(&[pk]).await;
                    if let Some(list) = lists.get(&pk.to_string()) {
                        primary.extend(list.write.iter().cloned());
                    }
                    primary.extend(seen::for_pubkey(&pk));
                }
                primary.retain(|u| !relay_latency::is_read_dead(u));
                primary.sort();
                primary.dedup();
                let mut ev = if primary.is_empty() {
                    None
                } else {
                    active
                        .pool
                        .get_from(&primary, &filter)
                        .await
                        .into_iter()
                        .next()
                };
                if ev.is_none() {
                    let mut fallback = hints;
                    fallback.extend(INDEXERS.iter().map(|s| s.to_string()));
                    fallback.sort();
                    fallback.dedup();
                    ev = active
                        .pool
                        .get_from(&fallback, &filter)
                        .await
                        .into_iter()
                        .next();
                }
                // NIP-40 + NIP-09 re-check on the resolution (the
                // pool chokepoint covers the network paths; the
                // STORE hit needs this gate — a long-lived cache
                // can serve an event that expired mid-retention
                // or was deleted after it landed). A deleted
                // event also earns one background sweep: ask the
                // relays for its tombstone (at most once per TTL
                // per id — deletions.ts checkDeletions) so the
                // NEXT open reflects it.
                let ev = ev.filter(|e| !nip40::is_expired(e) && !deletions::is_deleted(e));
                if let Some(resolved) = &ev
                    && deletions::should_check(&resolved.id.to_string())
                {
                    let pool = Arc::clone(&active.pool);
                    let sweep = Filter::new()
                        .kind(Kind::EventDeletion)
                        .event(resolved.id)
                        .limit(50);
                    // Phase 2 (NIP-65/#32): the tombstone ask goes
                    // to the relays the ORIGINAL was seen on (the
                    // per-event sidecar) — the places a deletion
                    // would have been published to and seen — with
                    // the blanket read-set req (pool.get) as the
                    // fallback when we've never seen it with
                    // attribution.
                    let seen_on = seen_event::for_event(&resolved.id);
                    this.rt.spawn(async move {
                        if seen_on.is_empty() {
                            // The chokepoint inside get() records
                            // whatever comes back; nothing else to
                            // do with the (filtered) result.
                            let _ = pool.get(sweep).await;
                        } else {
                            let _ = pool.get_from(&seen_on, &sweep).await;
                        }
                    });
                }
                ev
            }
        }
    }

    pub fn event_by_id(
        self: &Arc<Self>,
        id: EventId,
        hints: Vec<String>,
        author: Option<PublicKey>,
    ) -> FeedRx {
        let this = Arc::clone(self);
        self.feed_task(move |active| {
            Box::pin(async move {
                let ev = this.resolve_event(active, id, hints, author).await;
                let profile = match &ev {
                    Some(ev) => {
                        let authors = [ev.pubkey];
                        let cached = active
                            .pool
                            .store
                            .profiles(&authors)
                            .await
                            .unwrap_or_default();
                        let mut p: Option<Profile> =
                            cached.iter().filter_map(Profile::from_event).next();
                        if p.is_none() {
                            let filter = Filter::new()
                                .kind(Kind::Metadata)
                                .author(ev.pubkey)
                                .limit(1);
                            for meta in active.pool.get(filter).await.unwrap_or_default() {
                                if p.is_none() {
                                    p = Profile::from_event(&meta);
                                }
                            }
                        }
                        p
                    }
                    None => None,
                };
                Ok(FeedEvent::One(ev, profile))
            })
        })
    }

    /// One addressable event by (kind, author, d) — naddr resolution.
    pub fn event_by_address(
        self: &Arc<Self>,
        kind: u16,
        author: PublicKey,
        identifier: String,
    ) -> FeedRx {
        let this = Arc::clone(self);
        self.feed_task(move |active| {
            Box::pin(async move {
                // Store-first (audit: the sibling event_by_id checks the
                // store before any pool REQ; this bought a full-pool
                // round trip per address even after the event landed).
                // NIP-40 + NIP-09 gate the hit — a long-lived store copy
                // can serve an event that expired mid-retention or was
                // deleted after it landed.
                let mut ev = active
                    .pool
                    .store
                    .latest_by_kind_d(author, Kind::from_u16(kind), &identifier)
                    .await
                    .ok()
                    .flatten()
                    .filter(|e| !nip40::is_expired(e) && !deletions::is_deleted(e));
                if ev.is_none() {
                    let filter = Filter::new()
                        .author(author)
                        .kind(Kind::from_u16(kind))
                        .identifier(&identifier)
                        .limit(1);
                    let fetched = active.pool.get(filter).await.unwrap_or_default();
                    // Addressable events: newest replaces (NIP-01); take
                    // the greatest (id-ordered BTreeSet ≈ latest seen).
                    ev = fetched.into_iter().next_back();
                }
                // (no explicit save: the SDK persists what it receives)
                let profile = match &ev {
                    Some(ev) => {
                        let authors = [ev.pubkey];
                        let cached = active
                            .pool
                            .store
                            .profiles(&authors)
                            .await
                            .unwrap_or_default();
                        let mut p: Option<Profile> =
                            cached.iter().filter_map(Profile::from_event).next();
                        if p.is_none() {
                            let f = Filter::new()
                                .kind(Kind::Metadata)
                                .author(ev.pubkey)
                                .limit(1);
                            for meta in active.pool.get(f).await.unwrap_or_default() {
                                if p.is_none() {
                                    p = Profile::from_event(&meta);
                                }
                            }
                        }
                        p
                    }
                    None => None,
                };
                if let Some(p) = &profile {
                    this.absorb_profiles(std::slice::from_ref(p));
                }
                Ok(FeedEvent::One(ev, profile))
            })
        })
    }
}
