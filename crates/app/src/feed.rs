//! The following feed — port of `src/routes/feed.ts` + `render/note.ts`
//! (slice 1): the new-notes window, deliberate "view older" batches, the
//! caught-up ensō clearing. The design-for-exit pattern is kept: the feed
//! ends; nothing refills it behind your back.

use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use nostr::prelude::*;
use satori_core::manager::{FeedEvent, FeedPage, Profile};

use crate::bridge::drain;
use crate::content::esc;
use crate::ctx::Ctx;
use crate::cards::card_for;

pub struct FeedView {
    ctx: Ctx,
    pub page: adw::NavigationPage,
    list: gtk::ListBox,
    state: gtk::Stack,
    older_btn: gtk::Button,
    caught_up: gtk::Box,
    error: gtk::Label,
    oldest_at: std::cell::Cell<Option<u64>>,
    /// token → pending card (the undo window).
    pending: std::cell::RefCell<HashMap<u64, gtk::Widget>>,
    profiles: std::cell::RefCell<HashMap<String, Profile>>,
    faces: std::cell::RefCell<HashMap<String, gtk::Box>>,
    /// Prefetched older page (rendered instantly on View older).
    prefetched: std::cell::RefCell<Option<(FeedPage, bool)>>,
    prefetching: std::cell::Cell<bool>,
}

impl FeedView {
    pub fn new(ctx: &Ctx) -> Rc<Self> {
        let list = gtk::ListBox::builder()
            .css_classes(["feed-list"])
            .selection_mode(gtk::SelectionMode::None)
            .build();

        let older_btn = gtk::Button::builder()
            .label("View older posts")
            .css_classes(["flat", "older-btn"])
            .halign(gtk::Align::Center)
            .margin_top(12)
            .visible(false)
            .build();
        let caught_up = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(48)
            .margin_bottom(64)
            .halign(gtk::Align::Center)
            .visible(false)
            .build();
        let enso = crate::icons::enso(120);
        let words = gtk::Label::builder()
            .label("Caught up.")
            .css_classes(["serif", "empty-quote"])
            .build();
        caught_up.append(&enso);
        caught_up.append(&words);

        let error = gtk::Label::builder()
            .css_classes(["error-text"])
            .wrap(true)
            .visible(false)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();

        let column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(6)
            .build();
        column.append(&list);
        column.append(&older_btn);
        column.append(&caught_up);

        let clamp = crate::shell::reading_column(&column, 720);
        let scroll = gtk::ScrolledWindow::builder()
            .child(&clamp)
            .hexpand(true)
            .vexpand(true)
            .build();

        // Compose FAB (styles.css:525) — overlay, bottom-right of the column.
        let fab = gtk::Button::builder()
            .child(&crate::icons::icon_sized("compose", 22))
            .css_classes(["fab"])
            .width_request(52)
            .height_request(52)
            .halign(gtk::Align::End)
            .valign(gtk::Align::End)
            .margin_bottom(28)
            .margin_end(28)
            .build();

        // Scroll-top circle (revealed past ~800px; instant scroll — the
        // web app's scroll-top, minus the scroll-driven animation).
        let top_btn = gtk::Button::builder()
            .child(&crate::icons::icon_sized("up", 18))
            .css_classes(["fab", "scroll-top"])
            .width_request(44)
            .height_request(44)
            .halign(gtk::Align::End)
            .valign(gtk::Align::End)
            .margin_bottom(96)
            .margin_end(28)
            .visible(false)
            .build();
        {
            let adj = scroll.vadjustment();
            let btn = top_btn.clone();
            adj.connect_value_changed(move |a| {
                btn.set_visible(a.value() > 800.0);
            });
            let adj2 = adj.clone();
            top_btn.connect_clicked(move |_| adj2.set_value(0.0));
        }
        let feed_overlay = gtk::Overlay::builder().child(&scroll).build();
        feed_overlay.add_overlay(&top_btn);
        feed_overlay.add_overlay(&fab);

        let loading = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        let enso_load = crate::icons::enso(120);
        crate::motion::breathe(&enso_load);
        loading.append(&enso_load);
        loading.append(&gtk::Label::new(Some("Gathering the pond…")));

        let state = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        state.add_named(&loading, Some("loading"));
        state.add_named(&feed_overlay, Some("feed"));
        state.add_named(&error, Some("error"));

        let page = adw::NavigationPage::builder()
            .title("Following")
            .child(&state)
            .build();

        let view = Rc::new(Self {
            ctx: ctx.clone(),
            page,
            list,
            state,
            older_btn,
            caught_up,
            error,
            oldest_at: std::cell::Cell::new(None),
            pending: std::cell::RefCell::new(HashMap::new()),
            profiles: std::cell::RefCell::new(HashMap::new()),
            faces: std::cell::RefCell::new(HashMap::new()),
            prefetched: std::cell::RefCell::new(None),
            prefetching: std::cell::Cell::new(false),
        });

        // Prefetch the next older page while scrolling (the web's
        // h-prefetch: work begins before the click).
        {
            let view_pf = Rc::clone(&view);
            let adj = scroll.vadjustment();
            adj.connect_value_changed(move |a| {
                if a.value() / a.upper().max(1.0) > 0.7 {
                    view_pf.maybe_prefetch();
                }
            });
        }

        // Compose.
        {
            let view = Rc::clone(&view);
            let ctx = ctx.clone();
            fab.connect_clicked(move |_| {
                let view = Rc::clone(&view);
                crate::composer::compose(
                    &ctx,
                    None,
                    Some(hooks(&view)),
                );
            });
        }

        // Older batch.
        {
            let older_btn = view.older_btn.clone();
            let ctx_btn = ctx.clone();
            let view_btn = Rc::clone(&view);
            older_btn.connect_clicked(move |_| {
                view_btn.older_btn.set_sensitive(false);
                view_btn.older_btn.set_label("Settling…");
                let ctx = ctx_btn.clone();
                let view = Rc::clone(&view_btn);
                drain(ctx.core.older(view.oldest_at.get().unwrap_or(0)), move |ev| {
                    view.on_older(ev);
                });
            });
        }

        view
    }

    /// Bootstrap once a session exists (after login or resume). The feed is a
    /// projection of core state — without a session there is nothing to
    /// project, so construction never fetches.
    pub fn bootstrap(self: &Rc<Self>) {
        let view = Rc::clone(self);
        view.error.set_visible(false);
        view.state.set_visible_child_name("loading");
        let ctx = view.ctx.clone();
        drain(ctx.core.bootstrap(), move |ev| match ev {
            FeedEvent::Loaded(page) => view.on_loaded(&page, true),
            FeedEvent::BootstrapDone => {
                view.state.set_visible_child_name("feed");
                view.older_btn.set_visible(true);
                view.older_btn.set_sensitive(true);
            }
            FeedEvent::Failed(msg) => {
                eprintln!("[feed] bootstrap failed: {msg}");
                view.error.set_text(&msg);
                view.error.set_visible(true);
                view.state.set_visible_child_name("error");
            }
            _ => {}
        });
    }

    /// Home button: fetch new since the newest known note.
    pub fn refresh(self: &Rc<Self>) {
        let view = Rc::clone(self);
        let ctx = self.ctx.clone();
        drain(ctx.core.refresh(), move |ev| match ev {
            FeedEvent::Loaded(page) => view.on_loaded(&page, true),
            FeedEvent::NothingNew => view.ctx.toast("You're caught up"),
            FeedEvent::Failed(msg) => eprintln!("[feed] refresh: {msg}"),
            _ => {}
        });
    }

    fn on_loaded(self: &Rc<Self>, page: &FeedPage, replace: bool) {
        if replace && self.list.first_child().is_some() {
            // The ink-wash dispersal (styles.css .feed.clearing): soften the
            // current set, dissolve, rebuild — the new rows fade in via
            // inkfade.
            self.list.add_css_class("clearing");
            let view = Rc::clone(self);
            let page = page.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(280), move || {
                view.rebuild_from(&page);
                glib::ControlFlow::Break
            });
            return; // the timeout rebuilds
        }
        self.rebuild_from(page);
        for ev in &page.notes {
            self.append_note(ev);
        }
        self.hydrate_faces();
        self.state.set_visible_child_name("feed");
        if let Some(oldest) = page.notes.last().map(|e| e.created_at.as_secs()) {
            self.oldest_at.set(Some(oldest));
        }
        // Empty landing: the quiet quote + still ensō (svg.ts quoteEmpty).
        if page.notes.is_empty() && replace {
            let empty = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(8)
                .halign(gtk::Align::Center)
                .margin_top(48)
                .margin_bottom(64)
                .build();
            let quote = gtk::Label::builder()
                .css_classes(["serif", "empty-quote"])
                .build();
            quote.set_text("“Nothing exists; all is a beginning.”");
            empty.append(&quote);
            empty.append(&crate::icons::enso(90));
            self.list.append(&empty);
        }
    }

    /// Clear + render a page (the dispersal's second half).
    fn rebuild_from(&self, page: &FeedPage) {
        self.list.remove_all();
        self.faces.borrow_mut().clear();
        self.absorb_profiles(page);
        for ev in &page.notes {
            self.append_note(ev);
        }
        self.hydrate_faces();
        self.state.set_visible_child_name("feed");
        self.list.remove_css_class("clearing");
        if let Some(oldest) = page.notes.last().map(|e| e.created_at.as_secs()) {
            self.oldest_at.set(Some(oldest));
        }
        // Empty landing: the quiet quote + still ensō (svg.ts quoteEmpty).
        if page.notes.is_empty() {
            let empty = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(8)
                .halign(gtk::Align::Center)
                .margin_top(48)
                .margin_bottom(64)
                .build();
            let quote = gtk::Label::builder()
                .css_classes(["serif", "empty-quote"])
                .build();
            quote.set_text("\u{201c}Nothing exists; all is a beginning.\u{201d}");
            empty.append(&quote);
            empty.append(&crate::icons::enso(90));
            self.list.append(&empty);
        }
    }

    /// Queue the next older page while the reader approaches the bottom
    /// (the web's h-prefetch hover/intersect semantics, scroll edition).
    fn maybe_prefetch(self: &Rc<Self>) {
        if self.prefetching.get() || self.prefetched.borrow().is_some() {
            return;
        }
        if !self.older_btn.get_visible() || !self.older_btn.get_sensitive() {
            return;
        }
        self.prefetching.set(true);
        let view = Rc::clone(self);
        let ctx = self.ctx.clone();
        let until = self.oldest_at.get().unwrap_or(0);
        drain(ctx.core.older(until), move |reply| {
            view.prefetching.set(false);
            if let FeedEvent::Older(page, more) = reply
                && !page.notes.is_empty() && more {
                    *view.prefetched.borrow_mut() = Some((page, more));
                }
        });
    }

    /// Batch hydration of reply-faces (routes/feed.ts getFaces): the reply
    /// authors, then their profiles reactively (avatars replace initials).
    fn hydrate_faces(&self) {
        let ids: Vec<EventId> = self
            .faces
            .borrow()
            .keys()
            .filter_map(|h| EventId::from_hex(h).ok())
            .collect();
        if ids.is_empty() {
            return;
        }
        let ctx = self.ctx.clone();
        let faces_map = self.faces.clone();
        drain(ctx.core.reply_faces(ids), move |reply| {
            if let FeedEvent::Faces(per_note) = reply {
                // Initial fill (hue discs).
                let map = faces_map.borrow();
                for (id, who) in &per_note {
                    if let Some(slot) = map.get(&id.to_string()) {
                        crate::note_card::fill_faces(slot, who);
                    }
                }
            }
        });
    }

    fn on_older(&self, ev: FeedEvent) {
        self.older_btn.set_sensitive(true);
        self.older_btn.set_label("View older posts");
        match ev {
            FeedEvent::Older(page, has_more) => {
                self.absorb_profiles(&page);
                for note in &page.notes {
                    self.append_note(note);
                }
                if let Some(oldest) = page.notes.last().map(|e| e.created_at.as_secs()) {
                    self.oldest_at.set(Some(oldest));
                }
                if !has_more {
                    self.older_btn.set_visible(false);
                    self.caught_up.set_visible(true);
                }
            }
            FeedEvent::Failed(msg) => {
                eprintln!("[feed] older: {msg}");
            }
            _ => {}
        }
    }

    fn absorb_profiles(&self, page: &FeedPage) {
        let mut m = self.profiles.borrow_mut();
        for p in &page.profiles {
            m.insert(p.pubkey.clone(), p.clone());
        }
    }

    fn append_note(&self, ev: &Event) {
        let profile = self.profiles.borrow().get(&ev.pubkey.to_string()).cloned();
        if ev.kind == Kind::Repost {
            // NIP-18 wrapper: quiet "reposted by" line + the reposted note.
            let wrap = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .css_classes(["repost-wrap"])
                .build();
            let by = profile
                .as_ref()
                .map(|p| p.display())
                .unwrap_or_else(|| {
                    let hex = ev.pubkey.to_string();
                    format!("npub1…{}", &hex[hex.len().saturating_sub(6)..])
                });
            let head = gtk::Label::builder()
                .css_classes(["tagline", "repost-head"])
                .halign(gtk::Align::Start)
                .build();
            head.set_markup(&format!("↻ Reposted by {}", esc(&by)));
            wrap.append(&head);

            let slot = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .build();
            let waiting = gtk::Label::builder()
                .label("Loading repost…")
                .css_classes(["tagline"])
                .build();
            slot.append(&waiting);
            wrap.append(&slot);
            wrap.add_css_class("feed-row");
            crate::motion::fade_in(&wrap);
            self.list.append(&wrap);

            // Target: e tag first, else the nostr: ref in content (NIP-18).
            let target = ev.tags.event_ids().next().or_else(|| {
                ev.content
                    .strip_prefix("nostr:")
                    .and_then(|bech| EventId::from_bech32(bech).ok())
            });
            if let Some(id) = target {
                let ctx = self.ctx.clone();
                let slot = slot.clone();
                let waiting = waiting.clone();
                drain(ctx.core.event_by_id(id), move |e| match e {
                    FeedEvent::One(Some(target_ev), target_profile) => {
                        waiting.set_visible(false);
                        slot.append(&card_for(&ctx, &target_ev, target_profile.as_ref(), false));
                    }
                    FeedEvent::One(None, _) => {
                        waiting.set_text("Repost unavailable.");
                    }
                    _ => {}
                });
            } else {
                waiting.set_text("Repost unavailable.");
            }
            return;
        }
        let card = card_for(&self.ctx, ev, profile.as_ref(), false);
        card.add_css_class("feed-row");
        crate::motion::fade_in(&card);
        // Register the faces slot for batch hydration (kind 1 only).
        if ev.kind == Kind::TextNote
            && let Some(slot) = crate::note_card::faces_slot(&card) {
                self.faces.borrow_mut().insert(ev.id.to_string(), slot);
            }
        self.list.append(&card);
    }

    pub fn add_pending(&self, event: &Event, token: u64, _secs: u64) {
        let card = card_for(&self.ctx, event, None, false);
        card.add_css_class("pending");
        self.list.prepend(&card);
        self.pending.borrow_mut().insert(token, card.upcast());
    }

    pub fn remove_pending(&self, token: u64) {
        if let Some(row) = self.pending.borrow_mut().remove(&token) {
            row.set_visible(false);
        }
    }

    pub fn mark_committed(&self, token: u64) {
        if let Some(row) = self.pending.borrow().get(&token) {
            row.remove_css_class("pending");
        }
        self.pending.borrow_mut().remove(&token);
    }
}

/// Write-path lifecycle hooks bound to this feed (pending-card prepend).
pub fn hooks(view: &Rc<FeedView>) -> crate::composer::WriteHooks {
    let v = Rc::clone(view);
    let v2 = Rc::clone(view);
    let v3 = Rc::clone(view);
    crate::composer::WriteHooks {
        held: Rc::new(move |event, token, secs| v.add_pending(&event, token, secs)),
        undone: Rc::new(move |token| v2.remove_pending(token)),
        committed: Rc::new(move |token| v3.mark_committed(token)),
    }
}
