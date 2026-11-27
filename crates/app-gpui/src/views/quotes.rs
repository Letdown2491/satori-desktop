//! The quote-embed resolution engine (#31): standalone note/nevent
//! references resolve to inline embeds. Was four copy-pasted pairs
//! (search, feed, notifications, thread) that had already drifted in
//! comments and the thread-only touch; the behavior comes from this
//! module's two functions, driven by the [`Host`] trait (the
//! engagement.rs discipline).

use std::sync::Arc;

use gpui::{Context, WeakEntity};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, FeedRx};

use super::content::{self, Quotes};

/// What the engine needs from its host view: the quote mirror, core, the
/// profile map for resolved authors' names, and the optional snapshot
/// invalidation (thread's `touch()`).
pub trait Host: Sized + 'static {
    /// The host's mirror field (bech32 ref → embed, `None` in flight).
    fn quotes(&mut self) -> &mut Quotes;
    fn core(&self) -> &Arc<Core>;
    /// The host's working profile map, for the resolved author's name.
    fn profiles(&self) -> &std::collections::HashMap<PublicKey, satori_core::manager::Profile>;
    /// The host's snapshot invalidation on LANDING: thread's `touch()`
    /// (its render snapshot must rebuild); a no-op where render reads
    /// the mirror directly.
    fn touched(&mut self) {}
    /// The embed re-ledgers on LANDING (the audit's memory lens): a host
    /// that FIFO-trims its quotes map by an order queue must re-track a
    /// key that landed after its reservation was evicted, or the embed
    /// sits outside the cap for the view's lifetime (one owned Event per
    /// occurrence, on the relay firehose where the window is easy).
    /// Default no-op — hosts without an order queue need nothing.
    fn quote_landed(&mut self, _bech: &str) {}
}

/// Queue event fetches for standalone note/nevent references (embeds).
/// REQUEST-time invalidation is the host's business: thread touches
/// before asking (its two call sites), the feed twins deliberately do
/// not (a quote fetch must not churn the list).
pub fn want_quotes<H: Host>(host: &mut H, note: &Event, cx: &mut Context<H>) {
    for bech in content::note_refs(note) {
        if host.quotes().contains_key(&bech) {
            continue;
        }
        // Reserve the key so we don't re-request while loading.
        host.quotes().insert(bech.clone(), None);
        let rx = host.core().event_by_id_dech32(&bech);
        pump_quote(bech, rx, cx);
    }
}

/// One quote-reference resolution: the fetch lands, the embed fills the
/// mirror, the host invalidates.
fn pump_quote<H: Host>(bech: String, mut rx: FeedRx, cx: &mut Context<H>) {
    let this: WeakEntity<H> = cx.entity().downgrade();
    cx.spawn(async move |_, cx| {
        while let Some(ev) = rx.recv().await {
            if let FeedEvent::One(Some(target), prof) = ev {
                let _ = this.update(cx, |host, cx| {
                    let key = target.pubkey;
                    let name = content::display_name(
                        prof.as_ref().or_else(|| host.profiles().get(&key)),
                        &key,
                    );
                    host.quotes().insert(
                        bech.clone(),
                        Some(content::Embed {
                            event: target,
                            name,
                        }),
                    );
                    host.quote_landed(&bech);
                    host.touched();
                    cx.notify();
                });
            }
        }
    })
    .detach();
}
