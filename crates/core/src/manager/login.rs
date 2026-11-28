//! Bunker / nostrconnect login and the session resume machinery.

use super::*;

impl Core {
    // --- bunker login ---------------------------------------------------------

    /// Connect to a `bunker://` URI. Events stream on the returned channel:
    /// Waiting → (AuthUrl?) → Approved | Failed. Cancel with `cancel_login`.
    pub fn connect_bunker(self: &Arc<Self>, uri: String) -> ConnectRx {
        // This flow's epoch = the counter value after the bump; any later
        // bump (cancel, a newer attempt) invalidates it.
        let seq = self.login_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let _ = tx.send(ConnectEvent::Waiting);
            crate::dlog!("bunker: flow started (seq {seq})");
            match this.connect_bunker_inner(&uri, seq).await {
                Ok(pk) => {
                    crate::dlog!("bunker: approved for {pk}");
                    let _ = tx.send(ConnectEvent::Approved(pk));
                }
                Err(LoginError::RelayScreened(verdicts)) => {
                    crate::dlog!("bunker: relay screening refused {}", verdicts.len());
                    let _ = tx.send(ConnectEvent::RelayScreened(verdicts));
                }
                Err(e) => {
                    crate::dlog!("bunker: failed: {e}");
                    let _ = tx.send(ConnectEvent::Failed(e.to_string()));
                }
            }
        });
        rx
    }

    /// Abandon an in-flight bunker connect (best-effort shutdown).
    pub fn cancel_login(&self) {
        self.login_seq.fetch_add(1, Ordering::SeqCst);
        self.retire_pending();
    }

    /// Drop the pending handshake's transport with a best-effort shutdown.
    /// A merely-dropped NostrConnect leaves its relay socket and its
    /// kind-24133 subscription alive for the process's remaining life
    /// (zombie dials, phantom approval prompts — see shutdown()'s bill);
    /// every replace of `pending` retires the old transport first.
    pub(super) fn retire_pending(&self) {
        if let Some(pending) = self.pending.lock().take() {
            self.rt
                .spawn(async move { Arc::unwrap_or_clone(pending).shutdown().await });
        }
    }

    pub(super) async fn connect_bunker_inner(&self, uri: &str, seq: u64) -> Result<PublicKey> {
        let parsed = NostrConnectUri::parse(uri.trim())
            .map_err(|e| LoginError::InvalidBunkerUri(e.to_string()))?;

        // The pairing secret rides the keyring so RESUMED sessions can
        // carry it back on the reconnect handshake (some signers demand
        // it on every connect). A paste without one clears any stale
        // value: a secret from a different pairing is worse than none.
        if let NostrConnectUri::Bunker { secret, .. } = &parsed {
            match secret {
                Some(s) => secrets::store(secrets::BUNKER_PAIRING_SECRET, s).await?,
                None => secrets::delete(secrets::BUNKER_PAIRING_SECRET),
            }
        }

        // bunker://<remote>?relay=... (client:// initiation is out of scope).
        let remote = match &parsed {
            NostrConnectUri::Bunker {
                remote_signer_public_key,
                ..
            } => remote_signer_public_key.to_string(),
            _ => {
                return Err(LoginError::InvalidBunkerUri(
                    "expected bunker:// URI".into(),
                ));
            }
        };
        crate::dlog!("bunker: parsed, remote signer {remote}");

        // Paste-time relay screen (issue #11): the nostr-connect client
        // dials these relays directly, outside the pool's screening —
        // private-space answers are refused before any socket opens, and
        // the dialog offers the consent toggle instead.
        let relay_urls: Vec<String> = parsed.relays().iter().map(|r| r.to_string()).collect();
        let verdicts =
            crate::net_guard::screen_uri_relays(relay_urls.iter().map(String::as_str)).await;
        let refused = crate::net_guard::refused_verdicts(&verdicts);
        if !refused.is_empty() {
            return Err(LoginError::RelayScreened(refused));
        }

        let client_keys = Keys::generate();
        let connect = Arc::new(
            NostrConnect::new(parsed, client_keys.clone(), LOGIN_TIMEOUT, None)
                .map_err(|e| LoginError::Connect(e.to_string()))?,
        );
        let relays: Vec<String> = connect.relays().iter().map(|r| r.to_string()).collect();
        crate::dlog!(
            "bunker: client built over {} relay(s); awaiting approval",
            relays.len()
        );
        self.retire_pending();
        *self.pending.lock() = Some(Arc::clone(&connect));

        let pk = match connect.get_public_key_async().await {
            Ok(pk) => pk,
            Err(e) => {
                crate::dlog!("bunker: get_public_key failed: {e}");
                return Err(LoginError::Connect(e.to_string()));
            }
        };
        crate::dlog!("bunker: approval received from signer");

        if self.login_seq.load(Ordering::SeqCst) != seq {
            crate::dlog!("bunker: login superseded mid-handshake; dropping");
            return Err(LoginError::Connect("superseded".into()));
        }

        secrets::store(
            secrets::BUNKER_CLIENT_SECRET,
            &Zeroizing::new(client_keys.secret_key().display_secret().to_string()),
        )
        .await?;
        let session = Session {
            pubkey: pk.to_string(),
            mode: SessionMode::Bunker { remote, relays },
        };
        session::save(&session)?;
        crate::dlog!("bunker: session saved");

        // Already on the runtime (spawned task): await, never block_on.
        // The handshake-completed client IS the session signer — the
        // rebuild would re-run the connect handshake secretless (the
        // doc comment on build_active).
        let active = self
            .build_active(&session, Some(Signer::Bunker(Arc::clone(&connect))))
            .await?;
        *self.active.lock() = Some(active);
        // Clear pending only if it still points at THIS handshake — a newer
        // attempt may have replaced it.
        let mut pending = self.pending.lock();
        if pending.as_ref().is_some_and(|p| Arc::ptr_eq(p, &connect)) {
            *pending = None;
        }
        drop(pending);
        crate::dlog!("bunker: active session built, login complete");
        Ok(pk)
    }

    /// nostrconnect login (NIP-46, client-initiated): generate a fresh
    /// transport keypair and advertise it as a QR'd nostrconnect:// URI on
    /// the shared NIP-46 relay — the signer scans and approves. The URI goes
    /// down the same stream first (ConnectEvent::Uri); from approval on,
    /// this is the bunker flow (transport key in BUNKER_CLIENT_SECRET,
    /// session mode Bunker — resume rebuilds a bunker:// URI).
    pub fn connect_nostrconnect(self: &Arc<Self>, relay: String) -> ConnectRx {
        let seq = self.login_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let _ = tx.send(ConnectEvent::Waiting);
            crate::dlog!("nostrconnect: flow started (seq {seq}, relay {relay})");
            match this.connect_nostrconnect_inner(&relay, seq, &tx).await {
                Ok(pk) => {
                    crate::dlog!("nostrconnect: approved for {pk}");
                    let _ = tx.send(ConnectEvent::Approved(pk));
                }
                Err(e) => {
                    crate::dlog!("nostrconnect: first window failed: {e}");
                    // One automatic retry on the SDK's 60s timeout: a fast
                    // signer (kumaOS's built-in panel approves in ~1s) can
                    // publish its response before the SDK's subscribe is
                    // live, and kind-24133 filters don't replay. A fresh
                    // URI re-arms the handshake — the Uri event repaints
                    // the QR. Superseded or cancelled flows don't retry.
                    if !e.to_string().contains("timeout")
                        || this.login_seq.load(Ordering::SeqCst) != seq
                    {
                        let _ = tx.send(ConnectEvent::Failed(e.to_string()));
                        return;
                    }
                    // (No lock across the shutdown await — take first.)
                    let pending = this.pending.lock().take();
                    if let Some(pending) = pending {
                        Arc::unwrap_or_clone(pending).shutdown().await;
                    }
                    crate::dlog!("nostrconnect: retrying with a fresh URI");
                    match this.connect_nostrconnect_inner(&relay, seq, &tx).await {
                        Ok(pk) => {
                            crate::dlog!("nostrconnect: approved for {pk} (retry)");
                            let _ = tx.send(ConnectEvent::Approved(pk));
                        }
                        Err(e2) => {
                            crate::dlog!("nostrconnect: retry failed: {e2}");
                            let _ = tx.send(ConnectEvent::Failed(e2.to_string()));
                        }
                    }
                }
            }
        });
        rx
    }

    pub(super) async fn connect_nostrconnect_inner(
        &self,
        relay: &str,
        seq: u64,
        tx: &mpsc::UnboundedSender<ConnectEvent>,
    ) -> Result<PublicKey> {
        let client_keys = Keys::generate();
        // The metadata the signer shows at approval time. url/icons stay
        // unset until the app has hosted art — both optional per NIP-46.
        let mut uri = NostrConnectUri::client(
            client_keys.public_key(),
            [RelayUrl::parse(relay).map_err(|e| LoginError::InvalidBunkerUri(e.to_string()))?],
            "Satori",
        );
        if let NostrConnectUri::Client { metadata, .. } = &mut uri {
            metadata.description = Some("A quiet Nostr reader.".to_string());
        }
        // The pairing secret is generated INSIDE the client URI. Persist
        // it now: a resumed session's reconnect handshake carries it back
        // (some signers demand the pairing secret on every connect).
        if let NostrConnectUri::Client { secret, .. } = &uri {
            secrets::store(secrets::BUNKER_PAIRING_SECRET, secret).await?;
        }
        // The SDK emits the client identity as a metadata= JSON blob (the
        // pre-2025 NIP-46 draft); the current spec spells flat query params
        // (name/url/image). Append both — old signers read the blob, current
        // ones (kumaOS) the flat param. The handshake itself validates the
        // secret against the parsed URI, so the extras ride along inert.
        let mut uri_str = uri.to_string();
        uri_str.push_str("&name=Satori");
        crate::dlog!("nostrconnect: uri ready ({} chars)", uri_str.len());
        let _ = tx.send(ConnectEvent::Uri(uri_str));

        let connect = Arc::new(
            NostrConnect::new(uri, client_keys.clone(), LOGIN_TIMEOUT, None)
                .map_err(|e| LoginError::Connect(e.to_string()))?,
        );
        crate::dlog!("nostrconnect: client built, awaiting signer…");
        self.retire_pending();
        *self.pending.lock() = Some(Arc::clone(&connect));

        // The probe: a bare diagnostic client riding the same relay,
        // subscribed to this key's response kind. The SDK's own pump skips
        // bad responses with a tracing::warn that vanishes (no subscriber),
        // and a fast approval can land before its subscribe — the probe
        // turns both into lines we can actually read. Stands down when the
        // handshake resolves.
        let (probe_done_tx, probe_done_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(Self::nostrconnect_probe(
            client_keys.clone(),
            relay.to_string(),
            probe_done_rx,
        ));

        let user_pk = match connect.get_public_key_async().await {
            Ok(pk) => pk,
            Err(e) => {
                let _ = probe_done_tx.send(());
                crate::dlog!("nostrconnect: get_public_key failed: {e}");
                return Err(LoginError::Connect(e.to_string()));
            }
        };
        let _ = probe_done_tx.send(());
        crate::dlog!("nostrconnect: approval received from signer");

        if self.login_seq.load(Ordering::SeqCst) != seq {
            crate::dlog!("nostrconnect: login superseded mid-handshake; dropping");
            return Err(LoginError::Connect("superseded".into()));
        }

        secrets::store(
            secrets::BUNKER_CLIENT_SECRET,
            &Zeroizing::new(client_keys.secret_key().display_secret().to_string()),
        )
        .await?;
        let remote = match connect
            .bunker_uri()
            .await
            .map_err(|e| LoginError::Connect(e.to_string()))?
        {
            NostrConnectUri::Bunker {
                remote_signer_public_key,
                ..
            } => remote_signer_public_key.to_string(),
            _ => unreachable!("bunker_uri returns the Bunker variant"),
        };
        let relays: Vec<String> = connect.relays().iter().map(|r| r.to_string()).collect();

        let session = Session {
            pubkey: user_pk.to_string(),
            mode: SessionMode::Bunker { remote, relays },
        };
        session::save(&session)?;
        crate::dlog!("nostrconnect: session saved");

        // The handshake-completed client IS the session signer — the
        // rebuild would re-run the connect handshake secretless and the
        // user's first note send would eat the refusal (the doc comment
        // on build_active).
        let active = self
            .build_active(&session, Some(Signer::Bunker(Arc::clone(&connect))))
            .await?;
        *self.active.lock() = Some(active);
        // Clear pending only if it still points at THIS handshake — a newer
        // attempt may have replaced it.
        let mut pending = self.pending.lock();
        if pending.as_ref().is_some_and(|p| Arc::ptr_eq(p, &connect)) {
            *pending = None;
        }
        drop(pending);
        crate::dlog!("nostrconnect: active session built, login complete");
        Ok(user_pk)
    }

    /// The nostrconnect diagnostic sub (see connect_nostrconnect_inner):
    /// one bare client on the same relay, subscribed to the transport
    /// key's response kind. Every signer response it sees is logged with
    /// its decrypt outcome — "the relay delivered it and the SDK pump
    /// missed it" becomes provable from the log instead of a guess.
    pub(super) async fn nostrconnect_probe(
        client_keys: Keys,
        relay: String,
        done: tokio::sync::oneshot::Receiver<()>,
    ) {
        use futures_util::StreamExt as _;
        // The login relay is user-supplied (typed/QR'd seconds ago):
        // user-chosen class, dials direct like the rest of the
        // exemptions (M5 WP2's policy boundary).
        crate::net_guard::allow_direct_url(&relay);
        let probe = nostr_sdk::client::ClientBuilder::new().build();
        if let Err(e) = probe.add_relay(&relay).await {
            crate::dlog!("nostrconnect probe: relay add failed: {e}");
            return;
        }
        probe.connect().await;
        crate::dlog!("nostrconnect probe: relay connected");
        let filter = Filter::new()
            .pubkey(client_keys.public_key())
            .kind(Kind::NostrConnect)
            .limit(0);
        let Ok(output) = probe.subscribe(vec![filter]).await else {
            crate::dlog!("nostrconnect probe: subscribe failed");
            let _ = probe.shutdown().await;
            return;
        };
        crate::dlog!("nostrconnect probe: subscribed for responses");
        let mut done = done;
        let mut notifications = probe.notifications();
        loop {
            tokio::select! {
                _ = &mut done => {
                    crate::dlog!("nostrconnect probe: handshake resolved, standing down");
                    let _ = probe.shutdown().await;
                    return;
                }
                n = notifications.next() => {
                    let Some(n) = n else {
                        crate::dlog!("nostrconnect probe: notification stream ended");
                        let _ = probe.shutdown().await;
                        return;
                    };
                    match n {
                        nostr_sdk::client::ClientNotification::Event { subscription_id, event, .. }
                            if output.id() == &subscription_id =>
                        {
                            let event = *event;
                            crate::dlog!(
                                "nostrconnect probe: response event from {}, the relay DID deliver",
                                event.pubkey
                            );
                            match nostr::nips::nip44::decrypt(
                                client_keys.secret_key(),
                                &event.pubkey,
                                &event.content,
                            ) {
                                Ok(json) => {
                                    match serde_json::from_str::<serde_json::Value>(&json) {
                                        // Method name only: `result` carries
                                        // the URI's one-time secret (on
                                        // connect) and signature material
                                        // (on sign) — none of that belongs
                                        // in a log file.
                                        Ok(v) => crate::dlog!(
                                            "nostrconnect probe: response method={:?}",
                                            v.get("method").or_else(|| v.get("type")),
                                        ),
                                        Err(_) => {
                                            crate::dlog!("nostrconnect probe: undecodable response");
                                        }
                                    }
                                }
                                Err(e) => {
                                    crate::dlog!("nostrconnect probe: decrypt failed: {e}");
                                }
                            }
                        }
                        nostr_sdk::client::ClientNotification::Shutdown => {
                            crate::dlog!("nostrconnect probe: client shutdown");
                            return;
                        }
                        _ => continue,
                    }
                }
            }
        }
    }

    // --- resume machinery ------------------------------------------------------

    pub(super) async fn build_signer(&self, session: &Session) -> Result<Signer> {
        match &session.mode {
            SessionMode::Bunker { remote, relays } => {
                let secret = secrets::load(secrets::BUNKER_CLIENT_SECRET)
                    .await?
                    .ok_or(LoginError::CredentialGone("transport secret"))?;
                let client_keys = signer::client_keys_from_secret(&secret)?;
                // The pairing secret goes back into the URI: the reconnect
                // handshake carries it, which strict signers (kumaOS's
                // panel) demand on every connect. Lenient signers answer
                // "ack" to a replayed secret per the spec's SHOULD, so
                // carrying it is correct against both. No stored secret
                // (pre-pairing-secret sessions) stays secretless: the
                // reconnect then relies on the signer's leniency.
                let pairing = secrets::load(secrets::BUNKER_PAIRING_SECRET).await?;
                // Repeated relays join with & (query params, not extra ?s).
                // The URI carries the pairing secret, so it dies zeroized.
                let mut uri = Zeroizing::new(format!("bunker://{remote}"));
                if !relays.is_empty() {
                    uri.push_str("?relay=");
                    uri.push_str(&relays.join("&relay="));
                }
                if let Some(p) = pairing.filter(|p| !p.is_empty()) {
                    let sep = if uri.contains('?') { '&' } else { '?' };
                    uri.push(sep);
                    uri.push_str("secret=");
                    uri.push_str(&p);
                }
                let parsed =
                    NostrConnectUri::parse(&uri).map_err(|e| LoginError::Resume(e.to_string()))?;
                let connect = NostrConnect::new(parsed, client_keys, LOGIN_TIMEOUT, None)
                    .map_err(|e| LoginError::Resume(e.to_string()))?;
                Ok(Signer::Bunker(Arc::new(connect)))
            }
            SessionMode::Local => {
                let secret = secrets::load(secrets::NSEC)
                    .await?
                    .ok_or(LoginError::CredentialGone("local key"))?;
                Ok(Signer::Local(signer::parse_local_secret(&secret)?))
            }
        }
    }

    /// The signer bar's watch (D103): `true` = reachable, `false` = the
    /// last sign failed on transport (after D85's retry). The shell
    /// pumps this into the amber pill.
    pub fn signer_health(&self) -> tokio::sync::watch::Receiver<bool> {
        self.signer_health.subscribe()
    }

    /// The process pool (create-or-get).
    pub(super) async fn pool(&self) -> Result<Arc<Pool>> {
        if let Some(p) = self.pool.lock().as_ref() {
            return Ok(Arc::clone(p));
        }
        let preopen = self.preopen.lock().take();
        let p = Arc::new(
            Pool::open_with(preopen)
                .await
                .map_err(|e| LoginError::Resume(e.to_string()))?,
        );
        *self.pool.lock() = Some(Arc::clone(&p));
        Ok(p)
    }

    /// Hand the boot gate's swapped-in store to the pool (D100). Call
    /// once, from main, before init: the first pool() call consumes it.
    pub fn preopen_store(&self, store: crate::store::Store) {
        *self.preopen.lock() = Some(store);
    }

    /// The handshake-completed bunker client, when the caller just finished
    /// one (both login paths hold a live `NostrConnect` whose pairing the
    /// signer's secret established). Reusing it matters: `build_signer`'s
    /// rebuild from the secretless bunker:// URI re-runs the NIP-46
    /// `connect` handshake with NO secret on the first sign — and a signer
    /// that requires the pairing secret refuses that ("the connect did not
    /// carry the secret"), failing the user's FIRST NOTE SEND after a
    /// perfectly green login. The live client's pairing is already
    /// consumed and subscribed; the session wears it.
    pub(super) async fn build_active(
        &self,
        session: &Session,
        prebuilt: Option<Signer>,
    ) -> Result<Active> {
        // The session pubkey, parsed once. A corrupt session.json fails
        // the login here, with a message on the login pane, instead of
        // panicking at the first me() call somewhere deep in a feed task.
        let me = PublicKey::from_str(&session.pubkey)
            .map_err(|e| LoginError::Resume(format!("session pubkey: {e}")))?;
        let signer = match prebuilt {
            Some(s) => s,
            None => self.build_signer(session).await?,
        };
        let pool = self.pool().await?;
        crate::boot_mark!("store and pool open");
        // The pool's NIP-42 authenticator now answers challenges AS this
        // account (pool.rs): strict relays serve kinds 78/30078 only to
        // the authenticated owner (NIP-78, Sep 2026).
        pool.set_identity(signer.clone());
        let profiles = crate::profiles::ProfileService::new(Arc::clone(&pool));
        // Seed the display mirror from the durable cache: mentions and
        // people search read profiles_mem, which otherwise starts empty
        // each launch and re-learns profiles one feed page at a time —
        // '@amethyst' missed right after restart even though the disk
        // cache held her.
        self.absorb_profiles(&profiles.snapshot());
        // Undo-window prefs live in prefs.json (theme.ts semantics); core
        // reads its slice at session build — TODO(M1): cleaner pref split.
        let mut undo = crate::undo::UndoWindow::default();
        if let Ok(raw) =
            std::fs::read_to_string(crate::config::state_file(crate::config::files::PREFS))
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
        {
            if let Some(b) = v.get("undo_enabled").and_then(|b| b.as_bool()) {
                undo.enabled = b;
            }
            if let Some(sec) = v.get("undo_seconds").and_then(|s| s.as_u64()) {
                undo.set_seconds(sec as u32);
            }
        }
        // NIP-78 app-data sync (Phase 2 §4.1): our kind-30078 slots →
        // prefs.json. Remote wins at login (Feather's semantics): every
        // local change publishes forward, so only offline edits lose.
        // PrefsSynced wakes the shell to re-read what it renders live.
        {
            let pool = Arc::clone(&pool);
            let signer = signer.clone();
            let live = self.live.clone();
            self.rt.spawn(async move {
                sync_app_data(&pool, &me, &signer, &live).await;
            });
        }
        Ok(Active {
            session: session.clone(),
            signer,
            signer_health: Arc::clone(&self.signer_health),
            pool,
            profiles,
            undo,
            write_relays: Vec::new(),
            read_relays: Vec::new(),
            follows: Vec::new(),
            follows_hex: Arc::new(Vec::new()),
            me,
        })
    }

    /// Live undo-window update (settings): active state + prefs.json,
    /// then NIP-78 sync of both undo keys (Phase 2 §4.1).
    pub fn set_undo(self: &Arc<Self>, enabled: bool, seconds: u32) {
        {
            let mut a = self.active.lock();
            if let Some(a) = a.as_mut() {
                a.undo.enabled = enabled;
                a.undo.set_seconds(seconds);
            }
        }
        // These ARE the local values (the caller just wrote + stamped
        // them); the overlay echo is a no-op either way.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        write_prefs_overlay(&[
            ("undo_enabled".to_string(), enabled.into(), now),
            ("undo_seconds".to_string(), seconds.into(), now),
        ]);
        // Fire and forget — sync needs no feedback surface.
        self.publish_app_data("undo_enabled", serde_json::Value::Bool(enabled));
        self.publish_app_data("undo_seconds", serde_json::Value::from(seconds));
    }

    /// NIP-78 app-data publish (Phase 2 §4.1): one kind-30078 per prefs
    /// key, `d` = prefs.json key verbatim, content NIP-44-encrypted to
    /// self — the same private-by-default semantics as the NIP-51 lists.
    pub fn publish_app_data(self: &Arc<Self>, key: &str, value: serde_json::Value) -> PublishRx {
        let key = key.to_string();
        self.publish_task(move |active, tx| {
            Box::pin(async move {
                let me = active.me();
                let content = active
                    .signer
                    .nip44_encrypt_self(&me, &value.to_string())
                    .await?;
                let unsigned = UnsignedEvent::new(
                    me,
                    Timestamp::now(),
                    Kind::Custom(nip78::KIND),
                    [Tag::custom("d", [key.as_str()])],
                    content,
                );
                let signed = active.sign_report(unsigned).await?;
                Self::commit(active, &signed, tx).await?;
                Ok(())
            })
        })
    }
}
