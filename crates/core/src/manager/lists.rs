//! The own NIP-51 lists (D38) and their lazy member decryption.

use super::*;

impl Core {
    /// The own NIP-51 lists (D38's Lists tab): kind 30000 (follow sets)
    /// plus kind 30001 (categorized people/bookmark sets — Primal's
    /// "custom sets" live there), EXCEPT the 30001 d="bookmark" — that
    /// one IS the public bookmarks list and shows in the Bookmarks tab.
    /// TWO deliveries: a store-only snapshot first (the tab paints
    /// instantly — the relay round never gates the rows), then the
    /// merged fresh one. The relay round saves every fetched event
    /// EXPLICITLY before the read: the one-shot fetch's SDK-side saves
    /// are async and can still be landing when the read runs (v1.2's
    /// empty-tab race). Newest event per (kind, d) wins (parameterized
    /// replaceable); a private list's members (encrypted tag-JSON
    /// content, Bloom's folders) decrypt to-self. Sorted by display
    /// name.
    pub fn own_lists(self: &Arc<Self>) -> tokio::sync::mpsc::UnboundedReceiver<Vec<NostrList>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            // Snapshot 1: whatever the store holds right now.
            if let Ok(active) = this.active().await {
                let me = active.me();
                let snapshot = Self::lists_from_store(&active, me).await;
                eprintln!(
                    "[core] own_lists: store snapshot: {} list(s)",
                    snapshot.len()
                );
                let _ = tx.send(snapshot);
            }
            // Snapshot 2: after the relay round + explicit saves.
            let lists = async {
                let active = this.active().await?;
                let me = active.me();
                for kind in [30000u16, 30001] {
                    let filter = Filter::new().author(me).kind(Kind::Custom(kind)).limit(200);
                    let fetched = active.pool.get(filter).await.unwrap_or_default();
                    eprintln!(
                        "[core] own_lists: kind {kind}: {} event(s) fetched",
                        fetched.len()
                    );
                    for ev in &fetched {
                        let _ = active.pool.store.save(ev).await;
                    }
                }
                let merged = Self::lists_from_store(&active, me).await;
                eprintln!("[core] own_lists: merged: {} list(s)", merged.len());
                Ok::<Vec<NostrList>, LoginError>(merged)
            }
            .await;
            // Send ONLY on Ok (v1.2): an error's empty-default must never
            // overwrite the snapshot the tab already painted.
            match lists {
                Ok(merged) => {
                    let _ = tx.send(merged);
                }
                Err(e) => eprintln!("[core] own_lists: merge failed ({e}), snapshot stands"),
            }
        });
        rx
    }

    /// The store's own list events, collapsed to newest per (kind, d),
    /// display-sorted (own_lists' read side). Members stay undecrypted —
    /// the detail view decrypts them lazily (v1.3).
    pub(super) async fn lists_from_store(active: &Active, me: PublicKey) -> Vec<NostrList> {
        // (kind, d) is the replaceable coordinate; a plain d key would
        // collide 30000 and 30001 sets.
        let mut newest: HashMap<(u16, String), Event> = HashMap::new();
        for kind in [30000u16, 30001] {
            for ev in active
                .pool
                .store
                .all_by_kind(me, Kind::Custom(kind))
                .await
                .unwrap_or_default()
            {
                let Some(d) = ev
                    .tags
                    .clone()
                    .to_vec()
                    .iter()
                    .find(|t| t.kind() == "d")
                    .and_then(|t| t.content().map(str::to_string))
                else {
                    continue;
                };
                if kind == 30001 && d == "bookmark" {
                    continue;
                }
                match newest.get(&(kind, d.clone())) {
                    Some(old) if old.created_at >= ev.created_at => {}
                    _ => {
                        newest.insert((kind, d), ev);
                    }
                }
            }
        }
        let mut lists: Vec<NostrList> = Vec::new();
        for ev in newest.values() {
            // No decryption here (v1.2): rows paint from plain tags only;
            // a private list's members decrypt lazily in the detail view
            // (decrypt_list_members) — 78 serial decrypts blocked the
            // snapshot and the tab crawled.
            if let Some(l) = NostrList::from_event(ev) {
                lists.push(l);
            }
        }
        lists.sort_by_key(|l| l.display().to_lowercase());
        lists
    }

    /// A private list's members (NIP-51: the content is nip44-to-self of
    /// the tag array). The detail view's lazy path — called per open, so
    /// the Library rows never wait on crypto. Empty when the content is
    /// empty or undecryptable.
    pub fn decrypt_list_members(
        self: &Arc<Self>,
        ev: Event,
    ) -> tokio::sync::mpsc::UnboundedReceiver<Vec<PublicKey>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let members = async {
                if ev.content.is_empty() {
                    return Ok(Vec::new());
                }
                let active = this.active().await?;
                let me = active.me();
                let plain = active
                    .signer
                    .nip44_decrypt_self(&me, ev.content.as_str())
                    .await?;
                let mut members = Vec::new();
                if let Ok(tags) = serde_json::from_str::<Vec<Vec<String>>>(&plain) {
                    for tag in tags {
                        if tag.first().map(String::as_str) == Some("p")
                            && let Some(pk) = tag.get(1).and_then(|c| c.parse::<PublicKey>().ok())
                        {
                            members.push(pk);
                        }
                    }
                }
                Ok::<Vec<PublicKey>, LoginError>(members)
            }
            .await;
            let _ = tx.send(members.unwrap_or_default());
        });
        rx
    }
}
