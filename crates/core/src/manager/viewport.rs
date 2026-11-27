//! Viewport engagement subscriptions (WS2) and the session accessors.

use super::*;

impl Core {
    // --- viewport engagement (WS2, viewport.rs) ---------------------------------

    /// The feed's settled visible window: the ids whose engagement should
    /// arrive LIVE. One subscription, closed/replaced as the window moves
    /// (no-op when the id set is unchanged). An empty vec drops the sub.
    pub fn set_viewport(self: &Arc<Self>, ids: Vec<EventId>) {
        // Unchanged sets don't mark the watch — no churn on no-op calls.
        let _ = self.viewport_ids.send(ids);
        self.start_viewport();
    }

    /// ONE task per process owns the viewport subscription's lifecycle
    /// (the live bell's discipline, minus the session scoping — the pool
    /// is per-process): watch the id channel; on change, plan → close the
    /// current sub → arm the next; deliver matching events into the store
    /// (the SDK saves on receive) and signal `Live::Engagement`.
    pub(super) fn start_viewport(self: &Arc<Self>) {
        if self
            .viewport_started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            use futures_util::StreamExt as _;
            // The pool is built before any session paints a feed; a task
            // that raced it simply re-arms on the next set_viewport.
            let Some(pool) = this.pool.lock().clone() else {
                this.viewport_started
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                return;
            };
            let mut rx_ids = this.viewport_ids.subscribe();
            let mut tracker = crate::viewport::ViewportTracker::default();
            let mut current: Option<Output<SubscriptionId>> = None;
            let kinds: Vec<Kind> = crate::viewport::ENGAGEMENT_KINDS
                .iter()
                .map(|k| Kind::from_u16(*k))
                .collect();
            let mut notifs = pool.client().notifications();
            // CLOSED re-arm state (D85): the ids the live sub covers,
            // a debounce, and the scheduled retry (a CLOSED skipped for
            // the cooldown, or a failed re-arm, must retry on a timer —
            // the removed sub can never CLOSE again).
            let mut armed: Vec<EventId> = Vec::new();
            let mut last_rearm: Option<std::time::Instant> = None;
            let mut pending_rearm: Option<tokio::time::Instant> = None;
            // The re-arm body, shared by the CLOSED handler and the
            // timer: close before arming (D24: no churn pileup), re-issue
            // the same id set (subscribe_for_ids re-chunks). False = the
            // re-arm failed; the caller schedules the retry.
            async fn rearm_viewport(
                pool: &Arc<Pool>,
                kinds: &[Kind],
                tracker: &mut crate::viewport::ViewportTracker,
                armed: &mut Vec<EventId>,
                current: &mut Option<Output<SubscriptionId>>,
            ) -> bool {
                if let Some(o) = current.take() {
                    let _ = pool.client().unsubscribe(o.id()).await;
                }
                let ids = std::mem::take(armed);
                if ids.is_empty() {
                    return true;
                }
                match pool
                    .subscribe_for_ids(kinds.to_vec(), &ids, Some(Timestamp::now()))
                    .await
                {
                    Ok(o) => {
                        tracker.commit(ids.clone());
                        *armed = ids;
                        *current = Some(o);
                        true
                    }
                    Err(e) => {
                        eprintln!("[core] viewport: re-arm failed ({e})");
                        // Bookkeeping matches the relay's view so the
                        // next change re-arms.
                        tracker.commit(Vec::new());
                        false
                    }
                }
            }
            loop {
                tokio::select! {
                    _ = async {
                        match pending_rearm {
                            Some(at) => tokio::time::sleep_until(at).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        pending_rearm = None;
                        if !rearm_viewport(&pool, &kinds, &mut tracker, &mut armed, &mut current)
                            .await
                        {
                            pending_rearm = Some(
                                tokio::time::Instant::now() + Self::REARM_COOLDOWN,
                            );
                        }
                    }
                    res = rx_ids.changed() => {
                        if res.is_err() {
                            // All senders gone (process teardown).
                            return;
                        }
                        let ids = rx_ids.borrow().clone();
                        match tracker.plan(&ids) {
                            crate::viewport::Plan::Keep => {}
                            crate::viewport::Plan::Drop => {
                                // The viewport went away (pane closed, no
                                // rows): the sub MUST close with it.
                                if let Some(o) = current.take() {
                                    let _ = pool.client().unsubscribe(o.id()).await;
                                }
                                tracker.commit(Vec::new());
                            }
                            crate::viewport::Plan::Replace(shards) => {
                                // Close before arming — never two live
                                // viewport subs (D24: no churn pileup).
                                if let Some(o) = current.take() {
                                    let _ = pool.client().unsubscribe(o.id()).await;
                                }
                                let flat: Vec<EventId> =
                                    shards.iter().flatten().copied().collect();
                                match pool
                                    .subscribe_for_ids(
                                        kinds.clone(),
                                        &flat,
                                        Some(Timestamp::now()),
                                    )
                                    .await
                                {
                                    Ok(o) => {
                                        tracker.commit(flat.clone());
                                        armed = flat;
                                        current = Some(o);
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "[core] viewport: subscribe failed ({e})"
                                        );
                                        // Bookkeeping matches the relay's
                                        // view so the next change re-arms.
                                        tracker.commit(Vec::new());
                                        current = None;
                                    }
                                }
                            }
                        }
                    }
                    Some(n) = notifs.next() => {
                        // Only OUR subscription's events; the bell's own
                        // loop consumes its own ids from the same stream.
                        let Some(o) = &current else { continue };
                        match n {
                            nostr_sdk::client::ClientNotification::Event {
                                relay_url,
                                subscription_id,
                                event,
                                ..
                            } if o.id() == &subscription_id => {
                                // Same ingestion discipline as the bell:
                                // seen-on + author hints, the chokepoint
                                // gates, then the signal. The SDK has
                                // already saved the event.
                                seen::record(&event.pubkey, &[relay_url.as_str()]);
                                seen_event::record(&event.id, relay_url.as_str());
                                hints::observe(&event, relay_url.as_str());
                                // Proof of life for the rail's live dot
                                // (relay_latency::connected): an event
                                // arrived over this relay's socket.
                                crate::relay_latency::mark_delivery(relay_url.as_str());
                                if u16::from(event.kind) == 5 {
                                    deletions::record(&event, false);
                                    continue;
                                }
                                if nip40::is_expired(&event) || deletions::is_deleted(&event) {
                                    continue;
                                }
                                // Own engagement arriving from the relays
                                // (another device/client): the mirrors are
                                // the render state — flip them live.
                                if Some(event.pubkey) == this.pubkey() {
                                    let target = event
                                        .tags
                                        .clone()
                                        .event_ids()
                                        .next()
                                        ;
                                    if let Some(target) = target {
                                        match u16::from(event.kind) {
                                            7 => {
                                                this.own_liked
                                                    .lock()
                                                    .insert(target);
                                            }
                                            6 | 16 => {
                                                this.own_reposted
                                                    .lock()
                                                    .insert(target);
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                let _ = this.live.send(Live::Engagement);
                            }
                            nostr_sdk::client::ClientNotification::Message { message, .. }
                                if matches!(
                                    &*message,
                            nostr::message::RelayMessage::Closed { subscription_id, .. }
                                if o.id() == subscription_id.as_ref()
                                ) =>
                            {
                                // CLOSED for the viewport sub (D85): the
                                // SDK removed it and no reconnect replays
                                // a removed id — the counts would sit
                                // frozen until the next scroll changed the
                                // id set. Re-issue the same ids, debounced.
                                if last_rearm.is_some_and(|t| {
                                    t.elapsed() < Self::REARM_COOLDOWN
                                }) {
                                    pending_rearm = Some(
                                        tokio::time::Instant::now() + Self::REARM_COOLDOWN,
                                    );
                                    continue;
                                }
                                last_rearm = Some(std::time::Instant::now());
                                if !rearm_viewport(
                                    &pool,
                                    &kinds,
                                    &mut tracker,
                                    &mut armed,
                                    &mut current,
                                )
                                .await
                                {
                                    pending_rearm = Some(
                                        tokio::time::Instant::now() + Self::REARM_COOLDOWN,
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        });
    }

    /// The hold-state mirror for the shell's undo toast (Held → countdown,
    /// Committed/Undone/Failed → clear). The composer pops on Held, so the
    /// toast outlives it — subscribe once at shell creation.
    pub fn subscribe_hold(&self) -> tokio::sync::broadcast::Receiver<PublishEvent> {
        self.hold_tx.subscribe()
    }

    pub(super) fn mirror_hold(&self, ev: &PublishEvent) {
        let _ = self.hold_tx.send(ev.clone());
    }

    /// ONE persistent subscription per session, three filters: fresh posts
    /// from follows (timeline pill), #p me (bell — every NIP-10/NIP-22 reply
    /// tags the parent author), kind-5 deletions (render filter). First-seen
    /// events route to broadcast signals; everything saves to the store.
    /// Follows are snapshotted at start — a mid-session follow change rides
    /// the next refresh's fetches until the next session.
    pub(super) async fn start_live(&self, kinds: Vec<Kind>) {
        // Resolve the session BEFORE arming the flag: a failed resolve must
        // not wedge live_on true (it used to — the sub could never re-arm).
        let Ok(active) = self.active().await else {
            return;
        };
        if self.live_on.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // Captured at arm time; a mismatch at loop time means this session
        // was signed out and the loop tears its sub down below.
        let my_epoch = self.live_epoch.load(std::sync::atomic::Ordering::SeqCst);
        let me = active.me();
        let follows = active.follows.clone();
        self.rt.spawn(Self::run_live(
            active.pool.clone(),
            kinds,
            follows,
            me,
            self.live.clone(),
            self.live_on.clone(),
            self.live_epoch.clone(),
            my_epoch,
        ));
    }

    /// Minimum gap between live re-arms: a relay CLOSED storm must not
    /// spin REQs (a relay under subscription pressure would answer every
    /// re-subscribe with another CLOSED).
    pub(super) const REARM_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

    /// The escalation ceiling (perf-lens audit): a relay that never
    /// stops CLOSEing the re-armed sub doubles its cooldown each time —
    /// 60s, 2m, 4m … capped here — so a hostile or broken relay cannot
    /// pin the session into a teardown/rebuild cycle every minute
    /// forever. Any delivered bell event resets the escalation (the
    /// storm is over; the subs are demonstrably live again).
    pub(super) const REARM_COOLDOWN_MAX: std::time::Duration =
        std::time::Duration::from_secs(30 * 60);

    /// Membership test shared by the Event and CLOSED arms (DRYness
    /// audit): one definition of "one of the live bell's subscriptions",
    /// so a third live sub updates both guards in lockstep instead of
    /// two hand-maintained copies.
    pub(super) fn live_sub_id(
        output: &Output<SubscriptionId>,
        routed: &Option<Output<SubscriptionId>>,
        sid: &SubscriptionId,
    ) -> bool {
        output.id() == sid || routed.as_ref().is_some_and(|r| r.id() == sid)
    }

    /// The live bell's consume loop, spawned once per session by
    /// start_live. Owns the session's two subscriptions — the me-scoped
    /// bell filters on the read set, and the follows' fan-out over the
    /// outbox — and re-arms them when a relay takes one away.
    ///
    /// Why re-arm: a relay answering CLOSED (no machine-readable prefix —
    /// subscription pressure, a relay restart, an auth flip) makes the
    /// SDK REMOVE the subscription from that relay, and reconnects never
    /// re-send removed subs (nostr-sdk `should_resubscribe`: None =>
    /// false). Without a re-arm, every relay that closes the live sub
    /// once stops delivering notifications for the rest of the session —
    /// the bell and the new-notes pill both starve, silently.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_live(
        pool: Arc<Pool>,
        kinds: Vec<Kind>,
        follows: Vec<PublicKey>,
        me: PublicKey,
        live: tokio::sync::broadcast::Sender<Live>,
        live_on: Arc<std::sync::atomic::AtomicBool>,
        live_epoch: Arc<std::sync::atomic::AtomicU64>,
        my_epoch: u64,
    ) {
        use futures_util::StreamExt as _;
        let now = Timestamp::now();
        // NIP-01: an empty list can never match — the follows-scoped
        // filters join only when there are follows to scope by. Bumped to
        // a fresh window on every re-arm (a stale `since` would replay
        // the whole session into the bell).
        let bell_filters = |since: Timestamp| -> Vec<Filter> {
            let mut filters = vec![
                Filter::new()
                    .kinds([Kind::TextNote, Kind::Custom(1111), Kind::Custom(9735)])
                    .pubkey(me)
                    .since(since),
            ];
            if !follows.is_empty() {
                filters.push(
                    Filter::new()
                        .kind(Kind::EventDeletion)
                        .authors(follows.clone())
                        .since(since),
                );
            }
            filters
        };
        let mut filters = bell_filters(now);
        let mut output = match pool.client().subscribe(filters.clone()).await {
            Ok(o) => o,
            Err(e) => {
                eprintln!("[core] live: subscribe failed ({e}); re-arming");
                live_on.store(false, std::sync::atomic::Ordering::SeqCst);
                return;
            }
        };
        // The follows' notes ride the OUTBOX (each author from the
        // relays they write to — routed live fan-out, feeds.ts): a
        // second subscription, merged into the same notification
        // stream. Failure degrades to the main read set (indexers +
        // own inbox relays), not to a dead bell.
        let mut routed = if follows.is_empty() {
            None
        } else {
            match pool
                .subscribe_routed(kinds.clone(), &follows, Some(now))
                .await
            {
                Ok(o) => Some(o),
                Err(e) => {
                    eprintln!("[core] live: outbox fan-out failed ({e}); indexers only");
                    None
                }
            }
        };
        let mut notifications = pool.client().notifications();
        // Debounce for the CLOSED re-arm below (None = never re-armed).
        let mut last_rearm: Option<std::time::Instant> = None;
        // Consecutive re-arms without an intervening delivery: drives
        // the escalating cooldown. Reset on every delivered bell event.
        let mut rearm_backoff: u32 = 0;
        // The scheduled re-arm (D85 audit): a CLOSED that lands inside a
        // cooldown, or a failed re-arm subscribe, is retried on this
        // timer. The SDK has ALREADY removed a closed sub, so no later
        // CLOSED can ever re-trigger — without the timer those paths
        // strand the bell until re-login. `pending()` parks forever when
        // nothing is scheduled.
        let mut pending_rearm: Option<tokio::time::Instant> = None;
        // The re-arm body, shared by the CLOSED handler and the timer
        // (they were hand-copied; the timer's failure path is exactly
        // the case the old `break` stranded the bell on). Arms the main
        // sub on a fresh window, then the routed fan-out; false = the
        // main subscribe failed (old ids are unsubscribed; schedule a
        // retry). Order is close-before-arm (D24: no churn pileup).
        async fn rearm_live(
            pool: &Arc<Pool>,
            filters: &mut Vec<Filter>,
            output: &mut Output<SubscriptionId>,
            routed: &mut Option<Output<SubscriptionId>>,
            bell_filters: &(dyn Fn(Timestamp) -> Vec<Filter> + Send + Sync),
            kinds: &[Kind],
            follows: &[PublicKey],
        ) -> bool {
            let _ = pool.client().unsubscribe(output.id()).await;
            if let Some(r) = routed {
                let _ = pool.client().unsubscribe(r.id()).await;
            }
            *filters = bell_filters(Timestamp::now());
            match pool.client().subscribe(filters.clone()).await {
                Ok(o) => *output = o,
                Err(e) => {
                    eprintln!("[core] live: re-arm subscribe failed ({e})");
                    return false;
                }
            }
            if !follows.is_empty() {
                *routed = match pool
                    .subscribe_routed(kinds.to_vec(), follows, Some(Timestamp::now()))
                    .await
                {
                    Ok(o) => Some(o),
                    Err(e) => {
                        eprintln!("[core] live: re-arm fan-out failed ({e}); indexers only");
                        None
                    }
                };
            }
            true
        }
        'live: loop {
            tokio::select! {
                _ = async {
                    match pending_rearm {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    pending_rearm = None;
                    rearm_backoff += 1;
                    if !rearm_live(
                        &pool,
                        &mut filters,
                        &mut output,
                        &mut routed,
                        &bell_filters,
                        &kinds,
                        &follows,
                    )
                    .await
                    {
                        pending_rearm =
                            Some(tokio::time::Instant::now() + Self::REARM_COOLDOWN);
                    }
                }
                n = notifications.next() => {
                let Some(n) = n else { break 'live };
            // Session changed (sign-out, or a different account): drop
            // the stale identity's subscription. The check runs before
            // anything touches the store or the bell. live_on is left
            // alone here — sign-out already reset it, and storing
            // again could clobber a NEWER session's armed flag.
            if live_epoch.load(std::sync::atomic::Ordering::SeqCst) != my_epoch {
                eprintln!("[core] live: session changed; unsubscribing");
                let _ = pool.client().unsubscribe(output.id()).await;
                if let Some(r) = &routed {
                    let _ = pool.client().unsubscribe(r.id()).await;
                }
                return;
            }
            let event = match n {
                nostr_sdk::client::ClientNotification::Event {
                    relay_url,
                    subscription_id,
                    event,
                    ..
                } if Self::live_sub_id(&output, &routed, &subscription_id) => (relay_url, *event),
                nostr_sdk::client::ClientNotification::Message { relay_url, message }
                    if matches!(
                        &*message,
                        nostr::message::RelayMessage::Closed { message, .. }
                            if relay_latency::is_read_denial(message)
                    ) =>
                {
                    // A refusal-shaped CLOSED (pool.ts onclose →
                    // recordReadDenial; the broadcast stream delivers
                    // every relay's CLOSED here). Split the shapes:
                    let reason = match &*message {
                        nostr::message::RelayMessage::Closed { message, .. } => message,
                        _ => "",
                    };
                    if relay_latency::is_auth_wall(reason) {
                        // An auth wall, not futility: the relay is
                        // healthy and serves reads after NIP-42. The
                        // SDK signs the challenge with the session
                        // identity and resubscribes the closed subs on
                        // success; stamping read-dead here only starves
                        // profile fetches of a working relay for 12h.
                        eprintln!(
                            "[core] live: auth challenge from {relay_url}: {}",
                            crate::pool::clean_reason(reason)
                        );
                    } else if relay_latency::is_limit_rejection(reason) {
                        // A limit rejection: our filter's cap exceeds
                        // the relay's max (purplepag.es: "limit too
                        // high: 1000 (max 500)"). The relay is healthy
                        // and serves the query capped — the fetch
                        // simply gets no contribution from it. Never a
                        // read-dead stamp.
                        eprintln!(
                            "[core] live: limit rejected by {relay_url}: {}",
                            crate::pool::clean_reason(reason)
                        );
                    } else {
                        // An explicit REQ refusal (write-only blaster,
                        // paywall, a restriction we can't satisfy) —
                        // the read-dead signal.
                        relay_latency::record_read_denial(relay_url.as_str());
                        eprintln!(
                            "[core] live: read denial from {relay_url}: {}",
                            crate::pool::clean_reason(reason)
                        );
                    }
                    continue;
                }
                nostr_sdk::client::ClientNotification::Message { relay_url, message }
                    if matches!(
                        &*message,
                        nostr::message::RelayMessage::Ok {
                            status: false,
                            message,
                            ..
                        } if relay_latency::is_auth_rejection(message)
                    ) =>
                {
                    // Our SIGNED auth event was refused (unpaid
                    // subscription, a key the relay won't accept) — the
                    // AUTH went out and came back no. Distinct from an
                    // unanswered challenge: that one means the SIGNER
                    // leg never even produced an auth event (a bunker
                    // that won't sign kind 22242).
                    let reason = match &*message {
                        nostr::message::RelayMessage::Ok { message, .. } => message.to_string(),
                        _ => String::new(),
                    };
                    eprintln!(
                        "[core] live: auth rejected by {relay_url}: {}",
                        crate::pool::clean_reason(&reason)
                    );
                    continue;
                }
                nostr_sdk::client::ClientNotification::Message { message, .. }
                    if matches!(
                        &*message,
                        nostr::message::RelayMessage::Closed { subscription_id, .. }
                            if Self::live_sub_id(&output, &routed, subscription_id)
                    ) =>
                {
                    // A relay closed one of OUR live subs without a
                    // denial prefix — the SDK has already removed it,
                    // and no reconnect will bring it back. Unsubscribe
                    // the stale ids (other relays may still serve the
                    // old id) and re-arm both subs on a fresh window.
                    // Escalating cooldown (perf-lens audit): each
                    // consecutive re-arm doubles the gap, capped; the
                    // count resets the moment a bell event delivers.
                    let cooldown = Self::REARM_COOLDOWN
                        .checked_mul(1 << rearm_backoff.min(5))
                        .unwrap_or(Self::REARM_COOLDOWN_MAX)
                        .min(Self::REARM_COOLDOWN_MAX);
                    if last_rearm.is_some_and(|t| t.elapsed() < cooldown) {
                        // D85: do NOT drop this CLOSED — the sub is
                        // already gone and no later CLOSED can arrive.
                        // Schedule the retry for the cooldown's expiry.
                        pending_rearm = Some(tokio::time::Instant::now() + cooldown);
                        continue;
                    }
                    last_rearm = Some(std::time::Instant::now());
                    rearm_backoff += 1;
                    eprintln!("[core] live: a relay closed a live sub; re-arming");
                    if !rearm_live(
                        &pool,
                        &mut filters,
                        &mut output,
                        &mut routed,
                        &bell_filters,
                        &kinds,
                        &follows,
                    )
                    .await
                    {
                        // The re-arm's own subscribe failed: retry on
                        // the timer (the old `break` left the bell dead
                        // for the session — D85).
                        pending_rearm = Some(tokio::time::Instant::now() + cooldown);
                    }
                    continue;
                }
                nostr_sdk::client::ClientNotification::Shutdown => break,
                _ => continue,
            };
            let (relay_url, event) = event;
            // Seen-on memory (seen-relays.ts recordSeenOn): which
            // relays we ACTUALLY receive each author from — the live
            // bell is the same drain the TS wrapped around its
            // subscription's onevent. The author-keyed hint store
            // (WS1, RelayHintStore.kt) feeds off the same drain.
            seen::record(&event.pubkey, &[relay_url.as_str()]);
            seen_event::record(&event.id, relay_url.as_str());
            hints::observe(&event, relay_url.as_str());
            // Same proof-of-life mark (the live dot, §4.4).
            crate::relay_latency::mark_delivery(relay_url.as_str());
            // A delivery proves the re-armed subs are live: a CLOSED
            // storm is over, so the escalation restarts from 60s.
            rearm_backoff = 0;
            // The chokepoint lives here too (pool.ts onevent):
            // kind:5s are recorded as tombstones; expired copies
            // (NIP-40) and author-deleted content (NIP-09) never
            // ping the bell. The SDK has already persisted the
            // event either way — the tombstone store is how it
            // stops rendering.
            if u16::from(event.kind) == 5 {
                deletions::record(&event, false);
                continue;
            }
            if nip40::is_expired(&event) || deletions::is_deleted(&event) {
                continue;
            }
            // WoT graph feed (issue #2 WS1): live contact lists
            // apply to the two-hop graph incrementally.
            if u16::from(event.kind) == 3 {
                crate::wot::observe(&event);
            }
            // Duplicate-content gate (issue #2 WS2, AntiSpamFilter
            // .kt): a hidden spammer's events never ping the bell
            // (the SDK has already persisted them — the gate keeps
            // them out of every view; the record rides with it).
            if crate::dedup::observe(&event, relay_url.as_str()) {
                continue;
            }
            let sig = if event.tags.clone().public_keys().any(|pk| pk == me) {
                Some(Live::Mention)
            } else {
                Some(Live::Post)
            };
            if let Some(sig) = sig {
                let _ = live.send(sig);
            }
                }
            }
        }
        // The stream ended without a Shutdown (client teardown): the
        // bell used to stay armed-dead until the next login (audit
        // perf L1) — drop the flag so a later bootstrap re-arms, but
        // only if no newer arm took over (that one owns the flag).
        if live_epoch.load(std::sync::atomic::Ordering::SeqCst) == my_epoch {
            live_on.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        eprintln!("[core] live stream ended (relay disconnects resume via fetches)");
    }

    /// The fire-once task scaffold (#31 DRY): spawn on the runtime, route
    /// one value to a fresh channel. Was a pasted dance at every
    /// standalone fetch (json, nip05, image, poster, ...). The boxed
    /// future keeps the spawn's Send bound nameable on stable (async
    /// closures' CallOnceFuture can't be).
    pub(super) fn task_once<T>(
        &self,
        work: impl FnOnce() -> std::pin::Pin<Box<dyn Future<Output = T> + Send>> + Send + 'static,
    ) -> mpsc::UnboundedReceiver<T>
    where
        T: Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        self.rt.spawn(async move {
            let value = work().await;
            let _ = tx.send(value);
        });
        rx
    }

    /// The FeedRx active-session scaffold (#31 DRY): spawn + the session
    /// handshake + the Ok/Failed routing — the repeated skeleton.
    /// `work` gets the resolved session and returns the event to route;
    /// an error routes as FeedEvent::Failed. (Multi-send and
    /// side-channel sites — the wallet emit, the two-snapshot list —
    /// keep their own spawn.)
    pub(super) fn feed_task<F>(self: &Arc<Self>, work: F) -> FeedRx
    where
        F: for<'a> FnOnce(
                &'a Active,
            )
                -> std::pin::Pin<Box<dyn Future<Output = Result<FeedEvent>> + Send + 'a>>
            + Send
            + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            match async {
                let active = this.active().await?;
                work(&active).await
            }
            .await
            {
                Ok(ev) => {
                    let _ = tx.send(ev);
                }
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    /// The PublishRx active-session scaffold — feed_task's twin for the
    /// publish side (#31's deferred DRY list, the census's category C):
    /// spawn + the session handshake + the Ok/Failed routing. `work` gets
    /// the resolved session AND the sender (progress sends mid-body are
    /// normal: commit() reports per-relay) and returns the outcome; an
    /// error routes as PublishEvent::Failed, success as Committed (both
    /// token 0, the single-shot shape). (Multi-variant tails — Restored,
    /// poll tallies, the composer's progress fns — keep their own spawn.)
    pub(super) fn publish_task<F>(self: &Arc<Self>, work: F) -> PublishRx
    where
        F: for<'a> FnOnce(
                &'a Active,
                &'a mpsc::UnboundedSender<PublishEvent>,
            )
                -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>
            + Send
            + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                work(&active, &tx).await
            }
            .await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed {
                    token: 0,
                    error: e.to_string(),
                });
            } else {
                let _ = tx.send(PublishEvent::Committed { token: 0 });
            }
        });
        rx
    }

    /// The streaming sibling of publish_task (the audit's duplication
    /// lens): the same spawn + handshake scaffold, but `work` owns its
    /// tail — progress-bearing publishes (the composer's post_note/
    /// post_picture/post_video inner fns stream per-relay events) send
    /// their own Committed, so stream_task only routes the Failed arm.
    pub(super) fn stream_task<F>(self: &Arc<Self>, work: F) -> PublishRx
    where
        F: for<'a> FnOnce(
                &'a Arc<Core>,
                &'a mpsc::UnboundedSender<PublishEvent>,
            )
                -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>
            + Send
            + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res = work(&this, &tx).await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed {
                    token: 0,
                    error: e.to_string(),
                });
            }
        });
        rx
    }

    /// The ScheduleRx scaffold (the schedule_note/schedule_article twin —
    /// publish_task's third cousin, #31's DRY list): spawn + session
    /// handshake + the Scheduled/Failed routing. `work` returns the
    /// scheduled token.
    pub(super) fn schedule_task<F>(
        self: &Arc<Self>,
        scheduled_at: u64,
        work: F,
    ) -> mpsc::UnboundedReceiver<ScheduleEvent>
    where
        F: for<'a> FnOnce(
                &'a Active,
            )
                -> std::pin::Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>
            + Send
            + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: Result<String> = async {
                let active = this.active().await?;
                work(&active).await
            }
            .await;
            match res {
                Ok(token) => {
                    let _ = tx.send(ScheduleEvent::Scheduled {
                        token,
                        scheduled_at,
                    });
                }
                Err(e) => {
                    let _ = tx.send(ScheduleEvent::Failed {
                        error: e.to_string(),
                    });
                }
            }
        });
        rx
    }

    pub(super) async fn active(&self) -> Result<Active> {
        let held = self.active.lock().clone();
        held.ok_or_else(|| {
            eprintln!("[core] ERROR: something asked for the session while logged out");
            eprintln!("{}", std::backtrace::Backtrace::force_capture());
            LoginError::Resume("no active session".into())
        })
    }

    /// The quiet twin: same lookup, no alarm. For callers whose tick
    /// legitimately spans logged-out stretches (the scheduled sweep) —
    /// a miss is a state, not a diagnosis.
    pub(super) async fn try_active(&self) -> Result<Active> {
        let held = self.active.lock().clone();
        held.ok_or(LoginError::Resume("no active session".into()))
    }
}
