//! The following feed — port of `src/routes/feed.ts` + `render/note.ts`
//! (slice 1): the new-notes window, deliberate "view older" batches, the
//! caught-up ensō clearing. The design-for-exit pattern is kept: the feed
//! ends; nothing refills it behind your back.

use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use nostr::prelude::*;
use satori_core::manager::{FeedEvent, FeedPage, Profile};

use crate::bridge::drain;
use crate::ctx::Ctx;
use crate::note_card::note_card;

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
        let enso = gtk::Picture::for_file(&gtk::gio::File::for_path(crate::shell::enso_path()));
        enso.set_size_request(120, 120);
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

        let clamp = adw::Clamp::builder().maximum_size(720).child(&column).build();
        let scroll = gtk::ScrolledWindow::builder()
            .child(&clamp)
            .hexpand(true)
            .vexpand(true)
            .build();

        // Compose FAB (styles.css:525) — overlay, bottom-right of the column.
        let fab = gtk::Button::builder()
            .icon_name("document-edit-symbolic")
            .css_classes(["fab"])
            .width_request(52)
            .height_request(52)
            .halign(gtk::Align::End)
            .valign(gtk::Align::End)
            .margin_bottom(28)
            .margin_end(28)
            .build();

        let feed_overlay = gtk::Overlay::builder().child(&scroll).build();
        feed_overlay.add_overlay(&fab);

        let loading = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        let enso_load =
            gtk::Picture::for_file(&gtk::gio::File::for_path(crate::shell::enso_path()));
        enso_load.set_size_request(120, 120);
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
        });

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

        // Bootstrap.
        {
            let view_boot = Rc::clone(&view);
            let ctx_boot = ctx.clone();
            drain(ctx_boot.core.bootstrap(), move |ev| match ev {
                FeedEvent::Loaded(page) => view_boot.on_loaded(&page, true),
                FeedEvent::BootstrapDone => {
                    view_boot.state.set_visible_child_name("feed");
                    view_boot.older_btn.set_visible(true);
                    view_boot.older_btn.set_sensitive(true);
                }
                FeedEvent::Failed(msg) => {
                    view_boot.error.set_text(&msg);
                    view_boot.error.set_visible(true);
                    view_boot.state.set_visible_child_name("error");
                }
                _ => {}
            });
        }

        view
    }

    /// Home button: fetch new since the newest known note.
    pub fn refresh(self: &Rc<Self>) {
        let view = Rc::clone(self);
        let ctx = self.ctx.clone();
        drain(ctx.core.refresh(), move |ev| match ev {
            FeedEvent::Loaded(page) => view.on_loaded(&page, true),
            FeedEvent::Failed(msg) => eprintln!("[feed] refresh: {msg}"),
            _ => {}
        });
    }

    fn on_loaded(&self, page: &FeedPage, replace: bool) {
        if replace {
            self.list.remove_all();
        }
        self.absorb_profiles(page);
        for ev in &page.notes {
            self.append_note(ev);
        }
        self.state.set_visible_child_name("feed");
        if let Some(oldest) = page.notes.last().map(|e| e.created_at.as_secs()) {
            self.oldest_at.set(Some(oldest));
        }
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
        let card = note_card(&self.ctx, ev, profile.as_ref(), false);
        self.list.append(&card);
    }

    pub fn add_pending(&self, event: &Event, token: u64, _secs: u64) {
        let card = note_card(&self.ctx, event, None, false);
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
