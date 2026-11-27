//! Wallet / zaps (D20), blocktime, and the paid-target lookups.

use super::*;

impl Core {
    // --- wallet / zaps (D20) ---------------------------------------------------

    /// The wallet handle (the settings pane reads status/balance directly).
    pub fn wallet(&self) -> Arc<Wallet> {
        Arc::clone(&self.wallet)
    }

    // --- blocktime (Phase 2 §4.4) ----------------------------------------------

    /// Resolve a timestamp to a Bitcoin block height (Feather's blocktime
    /// spec): hot answers immediately, otherwise one guarded single-flight
    /// fetch to mempool.space. The answer lands on every waiter.
    pub fn block_height(self: &Arc<Self>, ts: u64) -> crate::blocktime::BlockRx {
        self.blocktime.get(ts)
    }

    /// The display-ready answer: exact when known, interpolated from the
    /// nearest point otherwise. Pure read — per-frame safe, never fetches.
    pub fn block_height_cached(&self, ts: u64) -> Option<u64> {
        self.blocktime.cached(ts)
    }

    /// Is there an exact (fetched or recorded) height for this timestamp?
    /// The ensure path's skip test — interpolation doesn't count.
    pub fn block_height_known(&self, ts: u64) -> bool {
        self.blocktime.known(ts)
    }

    /// Wallet event stream: status changes, pay outcomes, verified
    /// receipts. The global pump every surface keeps.
    pub fn subscribe_wallet(&self) -> wallet::WalletRx {
        self.wallet.subscribe()
    }

    /// Connect a `nostr+walletconnect://` URI. Status streams on
    /// subscribe_wallet (Connecting → Ready | ConnectFailed).
    pub fn connect_wallet(&self, uri: String) {
        let w = Arc::clone(&self.wallet);
        self.rt.spawn(async move { w.connect(uri).await });
    }

    /// Disconnect and forget (wallet.json + the secret store entry).
    pub fn disconnect_wallet(&self) {
        let w = Arc::clone(&self.wallet);
        self.rt.spawn(async move { w.disconnect().await });
    }

    /// Reconnect a persisted wallet and seed the own-zapped mirror from
    /// the ledger. Best-effort: a dead wallet backend is a settings
    /// concern, never a login failure.
    pub(super) fn resume_wallet(&self) {
        // Paid/Confirmed notes render settled without waiting on the
        // wallet; Pending/Uncertain still owe the wallet an answer.
        let mut zapped = self.own_zapped.lock();
        for z in wallet::ledger_history() {
            if matches!(
                z.state,
                wallet::ZapState::Paid | wallet::ZapState::Confirmed
            ) && let Ok(id) = EventId::from_hex(&z.note)
            {
                zapped.insert(id);
            }
        }
        drop(zapped);
        let w = Arc::clone(&self.wallet);
        self.rt.spawn(async move { w.resume().await });
    }

    /// Zap-state lookup for render (the settled bolt).
    pub fn is_zapped(&self, note: &EventId) -> bool {
        self.own_zapped.lock().contains(note)
    }

    /// The whole own-zapped set (a view seeds its snapshot from this).
    pub fn zapped_set(&self) -> HashSet<EventId> {
        self.own_zapped.lock().clone()
    }

    /// Zap a note: a NIP-57 zap through the connected NWC wallet (the
    /// tap = 21 sats default). `message` rides the 9734 content;
    /// `privacy` picks the request's author (D92). Events stream on the
    /// returned channel AND on subscribe_wallet — see [`WalletResultRx`].
    pub fn zap(
        self: &Arc<Self>,
        note: Event,
        msat: u64,
        message: String,
        privacy: wallet::ZapPrivacy,
    ) -> WalletResultRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        let w = Arc::clone(&self.wallet);
        self.rt.spawn(async move {
            if let Err(e) = this.zap_inner(&note, msat, &message, privacy, &tx).await {
                // Early failures never reach pay_stored's own emit — the
                // feed's whisper still needs to hear them.
                w.emit(e.clone());
                let _ = tx.send(e);
            }
        });
        rx
    }

    /// Zap a PROFILE (NIP-57's no-e-tag variant): the p-tag names the
    /// target, `lud` is the recipient's lightning pointer (read off
    /// their kind-0 by the caller). The ledger keys the record by the
    /// recipient's pubkey hex — a profile zap has no event id to
    /// correlate, so the receipt-watch matches the request's author
    /// instead (the UI's zapped mirror reads the same string).
    pub fn zap_profile(
        self: &Arc<Self>,
        who: PublicKey,
        lud: String,
        msat: u64,
        message: String,
        privacy: wallet::ZapPrivacy,
    ) -> WalletResultRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        let w = Arc::clone(&self.wallet);
        self.rt.spawn(async move {
            if let Err(e) = this
                .zap_profile_inner(&who, &lud, msat, &message, privacy, &tx)
                .await
            {
                w.emit(e.clone());
                let _ = tx.send(e);
            }
        });
        rx
    }

    /// Retry a stored zap (Pending/Uncertain only). The SAME signed request
    /// goes back out — same event id, no duplicate zap events; that is the
    /// ledger's whole reason for keeping the full signed 9734.
    pub fn retry_zap(self: &Arc<Self>, note: String) -> WalletResultRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        let w = Arc::clone(&self.wallet);
        self.rt.spawn(async move {
            if let Err(e) = this.retry_inner(note, &tx).await {
                w.emit(e.clone());
                let _ = tx.send(e);
            }
        });
        rx
    }

    pub(super) async fn retry_inner(
        self: &Arc<Self>,
        note: String,
        tx: &mpsc::UnboundedSender<WalletEvent>,
    ) -> ZapFlow {
        let pendings: Vec<wallet::ZapRecord> = wallet::ledger_for_note(&note)
            .into_iter()
            .filter(|z| {
                matches!(
                    z.state,
                    wallet::ZapState::Pending | wallet::ZapState::Uncertain
                )
            })
            .collect();
        if pendings.is_empty() {
            return Err(WalletEvent::ZapFailed {
                note,
                reason: "nothing to retry for this note".into(),
            });
        }
        // The stored requests replay as-is — re-signing would mint new
        // event ids and risk duplicate zap events (the Feather rule). A
        // split zap has one record per receiver: each replays, and one
        // corrupt record doesn't block the rest.
        // One in-flight payment per note — the retry reclaims the slot.
        if !self.wallet.claim_flight(&note) {
            return Err(WalletEvent::ZapFailed {
                note,
                reason: "a zap on this note is already in flight".into(),
            });
        }
        let _guard = FlightGuard(&self.wallet, &note);
        for record in pendings {
            let signed = match serde_json::from_str::<Event>(&record.event) {
                Ok(s) => s,
                Err(_) => {
                    let ev = WalletEvent::ZapFailed {
                        note: note.clone(),
                        reason: "stored zap request is corrupt".into(),
                    };
                    self.wallet.emit(ev.clone());
                    let _ = tx.send(ev);
                    continue;
                }
            };
            let _ = self.pay_stored(&record, &signed, tx).await;
        }
        Ok(())
    }

    pub(super) async fn zap_inner(
        self: &Arc<Self>,
        note: &Event,
        msat: u64,
        message: &str,
        privacy: wallet::ZapPrivacy,
        tx: &mpsc::UnboundedSender<WalletEvent>,
    ) -> ZapFlow {
        let note_hex = note.id.to_string();
        // One in-flight payment per note — the Feather rule (the wallet
        // owns the set; fresh zaps and retries both come through it).
        if !self.wallet.claim_flight(&note_hex) {
            return Err(WalletEvent::ZapFailed {
                note: note_hex,
                reason: "a zap on this note is already in flight".into(),
            });
        }
        let _guard = FlightGuard(&self.wallet, &note_hex);

        if !self.wallet.is_ready() {
            return Err(WalletEvent::ZapFailed {
                note: note_hex.clone(),
                reason: "no wallet connected".into(),
            });
        }
        let active = self.active().await.map_err(|e| WalletEvent::ZapFailed {
            note: note_hex.clone(),
            reason: e.to_string(),
        })?;
        let me = active.me();
        let recipient = note.pubkey;
        // One ephemeral key per zap — a split's requests all share it
        // (attributed zaps ignore it).
        let signer = ZapSigner::new(privacy);
        let author = signer.author(me);

        // NIP-57 Appendix G: `zap` tags on the target event name the split
        // receivers (the author is not implicitly among them); no tags →
        // the author takes the whole zap, exactly as before. The tags may
        // name receivers we've never met: warm their kind-0 before the
        // pointer lookups (bounded — a slow relay must not park the zap).
        let splits = wallet::zap_splits(note);
        let shares: Vec<(PublicKey, u64)> = if splits.is_empty() {
            vec![(recipient, msat)]
        } else {
            let pks: Vec<PublicKey> = splits.iter().map(|s| s.pk).collect();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(SPLIT_WARM_SECS),
                active.profiles.warm_await(pks),
            )
            .await;
            let mut shares = wallet::split_shares(msat, &splits);
            shares.retain(|(_, m)| *m > 0);
            shares
        };
        if shares.is_empty() {
            return Err(WalletEvent::ZapFailed {
                note: note_hex.clone(),
                reason: "the zap split added up to nothing".into(),
            });
        }

        // Per receiver: resolve the lightning pointer, sign, STORE, pay.
        // A receiver with no pointer fails the WHOLE zap (fail-closed):
        // paying only the resolvable shares would silently shortchange
        // the rest. One receiver's definite failure (dead lnurl server,
        // wallet refusal) doesn't block the others — each reports its own
        // outcome.
        for (share_pk, share_msat) in shares {
            let lud = match active
                .pool
                .store
                .latest_by_kind(share_pk, Kind::Metadata)
                .await
            {
                Ok(Some(meta)) => lightning_field(&meta.content),
                _ => None,
            }
            .ok_or_else(|| WalletEvent::ZapFailed {
                note: note_hex.clone(),
                reason: format!("no lightning address for {share_pk}"),
            })?;
            let lnurl = wallet::lnurlp_url(&lud).map_err(|e| WalletEvent::ZapFailed {
                note: note_hex.clone(),
                reason: e,
            })?;

            // Sign FIRST (the author key matches the privacy choice),
            // STORE the signed request, then pay. A crash between sign
            // and pay leaves a Pending record a retry can finish — same
            // event id. The relays choice (see zap_profile_inner):
            // attributed rides our write relays, anonymous the neutral
            // public set.
            let relays = zap_receipt_relays(privacy, &active.write_relays);
            let unsigned = wallet::zap_request(
                author, share_pk, note.id, share_msat, &lnurl, relays, message,
            );
            // A definite failure inside has already erased the record
            // and reported itself; the remaining receivers still get paid.
            self.store_and_pay(
                &active,
                &signer,
                unsigned,
                &note_hex,
                &share_pk.to_string(),
                share_msat,
                tx,
            )
            .await?;
        }
        Ok(())
    }

    pub(super) async fn zap_profile_inner(
        self: &Arc<Self>,
        who: &PublicKey,
        lud: &str,
        msat: u64,
        message: &str,
        privacy: wallet::ZapPrivacy,
        tx: &mpsc::UnboundedSender<WalletEvent>,
    ) -> ZapFlow {
        // The ledger key is the recipient's pubkey hex (see zap_profile).
        let note_hex = who.to_string();
        // One in-flight payment per target — the same Feather rule.
        if !self.wallet.claim_flight(&note_hex) {
            return Err(WalletEvent::ZapFailed {
                note: note_hex,
                reason: "a zap on this profile is already in flight".into(),
            });
        }
        let _guard = FlightGuard(&self.wallet, &note_hex);

        if !self.wallet.is_ready() {
            return Err(WalletEvent::ZapFailed {
                note: note_hex.clone(),
                reason: "no wallet connected".into(),
            });
        }
        let active = self.active().await.map_err(|e| WalletEvent::ZapFailed {
            note: note_hex.clone(),
            reason: e.to_string(),
        })?;
        let me = active.me();
        let signer = ZapSigner::new(privacy);
        let author = signer.author(me);
        // The receipt publishes where the 9734's relays tag says. An
        // attributed zap uses OUR write relays (we want the confirmation);
        // an anonymous one uses a NEUTRAL public set — the owner's relay
        // list is itself a fingerprint, and the audit pulled it out of
        // the anonymity promise (D91/D94).
        let relays = zap_receipt_relays(privacy, &active.write_relays);
        let lnurl = wallet::lnurlp_url(lud).map_err(|e| WalletEvent::ZapFailed {
            note: note_hex.clone(),
            reason: e,
        })?;

        // Sign FIRST (the author key matches the privacy choice),
        // STORE the signed request, then pay. A crash between sign
        // and pay leaves a Pending record a retry can finish — same
        // event id.
        let unsigned = wallet::zap_request_profile(author, *who, msat, &lnurl, relays, message);
        self.store_and_pay(
            &active,
            &signer,
            unsigned,
            &note_hex,
            &who.to_string(),
            msat,
            tx,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)] // the request geometry, spelled out
    /// The crash-safety invariant in ONE place (D94's duplication lens):
    /// sign the request, store the record Pending, then pay. A crash
    /// between sign and pay leaves a Pending record a retry can finish —
    /// same event id. Both zap shapes (note/profile) and the split loop
    /// come through here.
    pub(super) async fn store_and_pay(
        self: &Arc<Self>,
        active: &Active,
        signer: &ZapSigner,
        unsigned: UnsignedEvent,
        note_key: &str,
        recipient: &str,
        msat: u64,
        tx: &mpsc::UnboundedSender<WalletEvent>,
    ) -> ZapFlow {
        let signed =
            signer
                .sign(active, unsigned)
                .await
                .map_err(|reason| WalletEvent::ZapFailed {
                    note: note_key.to_string(),
                    reason,
                })?;
        let record = wallet::ZapRecord {
            req: signed.id.to_string(),
            note: note_key.to_string(),
            recipient: recipient.to_string(),
            msat,
            state: wallet::ZapState::Pending,
            created_at: now_secs(),
            event: serde_json::to_string(&signed).unwrap_or_default(),
        };
        wallet::ledger_update(record.clone());
        let _ = self.pay_stored(&record, &signed, tx).await;
        Ok(())
    }

    /// The author's NIP-A3 payment targets (kind 10133): store-first,
    /// one cold wire fetch when absent (D89's `latest_list_event`
    /// shape), parsed through `crate::nip::payto`. Empty = none known (never
    /// an error — the chips just don't render).
    pub async fn payment_targets(
        &self,
        author: PublicKey,
    ) -> Vec<crate::nip::payto::PaymentTarget> {
        let kind = Kind::Custom(crate::nip::payto::KIND);
        if let Some(ev) = self.store_latest(author, kind).await {
            return crate::nip::payto::parse(&ev);
        }
        // Cold store: the session negative cache first (most authors
        // never publish one — see payto_asked), then one targeted fetch
        // (the SDK persists what arrives), then read back.
        let asked_key = author.to_string();
        if self.payto_asked.lock().contains(&asked_key) {
            return Vec::new();
        }
        let Ok(active) = self.active().await else {
            return Vec::new();
        };
        let filter = Filter::new().kind(kind).author(author).limit(1);
        let _ = active.pool.get(filter).await;
        match self.store_latest(author, kind).await {
            Some(ev) => crate::nip::payto::parse(&ev),
            None => {
                self.payto_asked.lock().insert(asked_key);
                Vec::new()
            }
        }
    }

    /// Store-first read (the D89 shape): the latest kind-10133 by one
    /// author, or None when the store hasn't met one.
    pub(super) async fn store_latest(&self, author: PublicKey, kind: Kind) -> Option<Event> {
        let Ok(active) = self.active().await else {
            return None;
        };
        active
            .pool
            .store
            .latest_by_kind(author, kind)
            .await
            .ok()
            .flatten()
    }

    /// The shared pay path for a STORED, signed request (fresh or retry):
    /// fetch the invoice from the recipient's LNURL-pay endpoint, hand the
    /// bolt11 to the NWC wallet, classify the answer per the Feather rule.
    pub(super) async fn pay_stored(
        self: &Arc<Self>,
        record: &wallet::ZapRecord,
        signed: &Event,
        tx: &mpsc::UnboundedSender<WalletEvent>,
    ) -> ZapFlow {
        let note = record.note.clone();
        let req = record.req.clone();

        // The lnurl tag rides the signed request (bech32) — lnurlp_url
        // accepts it as-is (the lud06 branch). Reading it from the event
        // (not the caller) is what lets retries share this path.
        let lnurl_tag = signed
            .tags
            .iter()
            .find(|t| t.as_slice().first().map(String::as_str) == Some("lnurl"))
            .and_then(|t| t.as_slice().get(1).cloned())
            .ok_or_else(|| WalletEvent::ZapFailed {
                note: note.clone(),
                reason: "zap request has no lnurl tag".into(),
            })?;
        let endpoint = wallet::lnurlp_url(&lnurl_tag).map_err(|e| WalletEvent::ZapFailed {
            note: note.clone(),
            reason: e,
        })?;
        // No invoice yet → nothing CAN have been paid: a definite LNURL
        // error erases the record, the user just taps again.
        let callback = match wallet::fetch_lnurlp(&endpoint).await {
            Ok(cb) => cb,
            Err(e) => {
                wallet::ledger_remove(&req);
                return Err(WalletEvent::ZapFailed { note, reason: e });
            }
        };
        let bolt11 = match wallet::fetch_zap_invoice(&callback, record.msat, signed).await {
            Ok(pr) => pr,
            Err(e) => {
                wallet::ledger_remove(&req);
                return Err(WalletEvent::ZapFailed { note, reason: e });
            }
        };

        // The money moment. The wallet SPOKE and did not pay → retrying is
        // safe (erase + quiet error). Anything else → Uncertain, terminal
        // until the user (or a receipt) decides — never auto-retried.
        match self.wallet.pay(&bolt11).await {
            Ok(_preimage) => {
                wallet::ledger_set_state(&req, wallet::ZapState::Paid);
                let ev = WalletEvent::ZapPaid {
                    note: note.clone(),
                    req: req.clone(),
                    msat: record.msat,
                };
                self.wallet.emit(ev.clone());
                let _ = tx.send(ev);
                // The watch keys off the request's own geometry: the e
                // tag read off the SIGNED REQUEST (note zaps carry one;
                // profile zaps don't — a pubkey-hex ledger key parses as
                // an EventId, so the note field cannot discriminate this
                // and never gets to: the audit's phantom-e-tag bug),
                // the p tag always, and the sender match against the
                // request's author (me, or the anonymous ephemeral key).
                let e_tag = request_e_tag(signed);
                if let Ok(recipient) = PublicKey::from_str(&record.recipient) {
                    self.watch_receipt(
                        &note,
                        e_tag,
                        recipient,
                        signed.pubkey,
                        &req,
                        record.msat,
                        record.created_at,
                        tx,
                    );
                }
                Ok(())
            }
            Err(wallet::PayError::Uncertain) => {
                wallet::ledger_set_state(&req, wallet::ZapState::Uncertain);
                let ev = WalletEvent::ZapUncertain {
                    note: note.clone(),
                    req: req.clone(),
                    msat: record.msat,
                };
                self.wallet.emit(ev.clone());
                let _ = tx.send(ev);
                Ok(())
            }
            Err(e) => {
                // Code(_) or NoWallet: the wallet never paid.
                wallet::ledger_remove(&req);
                let ev = WalletEvent::ZapFailed {
                    note,
                    reason: e.to_string(),
                };
                self.wallet.emit(ev.clone());
                let _ = tx.send(ev);
                Ok(())
            }
        }
    }

    /// Watch for the wallet service's kind-9735 receipt (it publishes to
    /// OUR relays — the request's relays tag) and reconcile the ledger to
    /// Confirmed when OUR verified zap lands. Best-effort: Paid is already
    /// truthful (the wallet returned a preimage); the receipt is the
    /// stronger proof and recovers nothing else.
    #[allow(clippy::too_many_arguments)] // the watch geometry, spelled out
    pub(super) fn watch_receipt(
        self: &Arc<Self>,
        note_key: &str,
        e_tag: Option<EventId>,
        recipient: PublicKey,
        sender: PublicKey,
        req: &str,
        msat: u64,
        created_at: u64,
        tx: &mpsc::UnboundedSender<WalletEvent>,
    ) {
        let this = Arc::clone(self);
        let note_hex = note_key.to_string();
        let req = req.to_string();
        let tx = tx.clone(); // the spawned task outlives the borrow
        self.rt.spawn(async move {
            // The watch keys off the request's own geometry: the e tag
            // when there is one (note zaps), the p tag always (profile
            // zaps have no event to reference), and the sender match
            // against the request's author (me, or the anonymous
            // ephemeral key — the reconciliation). The no-e-tag shape
            // is bounded twice over: a small limit (the poll repeats,
            // a prolific recipient's full receipt history is not the
            // question) and since the request's creation (an OLDER
            // receipt of ours to the same recipient must not confirm
            // this zap — the amount match below finishes the job).
            let mut filter = Filter::new()
                .kind(Kind::Custom(9735))
                .custom_tag(SingleLetterTag::LOWERCASE_P, recipient)
                .limit(10);
            if let Some(note_id) = e_tag {
                filter = filter.event(note_id);
            } else {
                filter = filter.since(Timestamp::from(created_at));
            }
            for _ in 0..6 {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                let Ok(active) = this.active().await else {
                    return;
                };
                let events = match active.pool.get(filter.clone()).await {
                    Ok(set) => set,
                    Err(_) => continue, // relay budget blown — look again
                };
                // parse_zap_receipt verifies the embedded 9734 (signature +
                // p/e/a tags); the sender being the request's author AND
                // the bolt11 amount matching the record is the
                // reconciliation (a stale receipt from an earlier zap can
                // carry the right sender — attributed — but not the amount
                // of THIS one; anonymous sends have a fresh key and can't
                // collide at all).
                if events.iter().any(|ev| {
                    let info = crate::notifications::parse_zap_receipt(ev);
                    info.sender == Some(sender) && info.sats == msat / 1000
                }) {
                    wallet::ledger_set_state(&req, wallet::ZapState::Confirmed);
                    let ev = WalletEvent::ZapReceipt {
                        note: note_hex.clone(),
                        req: req.clone(),
                    };
                    this.wallet.emit(ev.clone());
                    let _ = tx.send(ev);
                    return;
                }
            }
        });
    }
}
