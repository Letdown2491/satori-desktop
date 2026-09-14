//! The composer — port of `src/render/compose.ts` + `src/routes/note.ts`
//! (slice 1: note type only). The undo window runs in core; the UI shows the
//! toast the web app drew as a pending pill.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use nostr::prelude::*;
use satori_core::manager::{PublishEvent, Profile};

use crate::bridge::drain;
use crate::ctx::Ctx;

/// Where write-path lifecycle events go (pending-card prepend in the
/// feed/thread that opened the composer).
#[derive(Clone)]
pub struct WriteHooks {
    pub held: Rc<dyn Fn(Event, u64, u64)>,
    pub undone: Rc<dyn Fn(u64)>,
    pub committed: Rc<dyn Fn(u64)>,
}

impl WriteHooks {
    pub fn noop() -> Self {
        Self {
            held: Rc::new(|_, _, _| {}),
            undone: Rc::new(|_| {}),
            committed: Rc::new(|_| {}),
        }
    }
}

/// Open the compose dialog. `reply_to` makes it a NIP-10 reply.
pub fn compose(ctx: &Ctx, reply_to: Option<Event>, hooks: Option<WriteHooks>) {
    let hooks = hooks.unwrap_or_else(WriteHooks::noop);
    let reply_profile: Option<Profile> = None;

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .width_request(420)
        .build();

    let title = match &reply_to {
        Some(_) => "Reply".to_string(),
        None => "New note".to_string(),
    };
    let title_label = gtk::Label::builder()
        .label(&title)
        .css_classes(["serif", "wordmark"])
        .halign(gtk::Align::Start)
        .build();
    if let Some(parent) = &reply_to {
        let parent = parent.clone();
        // Reply context: the quoted parent, collapsed to one line.
        let ctx_line = gtk::Label::builder()
            .css_classes(["tagline"])
            .wrap(true)
            .build();
        let snippet: String = parent
            .content
            .chars()
            .take(80)
            .collect::<String>();
        ctx_line.set_text(&format!("↩ {}", snippet.replace('\n', " ")));
        content.append(&ctx_line);
        let _ = reply_profile;
    }
    content.append(&title_label);

    let scroll = gtk::ScrolledWindow::builder()
        .height_request(130)
        .hexpand(true)
        .vexpand(true)
        .build();
    let view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .css_classes(["compose-text"])
        .build();
    scroll.set_child(Some(&view));
    content.append(&scroll);

    let publish = gtk::Button::builder()
        .label(if reply_to.is_some() { "Reply" } else { "Publish" })
        .css_classes(["seal-btn"])
        .halign(gtk::Align::End)
        .build();
    content.append(&publish);

    let dialog = adw::Dialog::builder()
        .title(&title)
        .content_width(520)
        .content_height(320)
        .build();
    dialog.set_child(Some(&content));

    {
        let ctx = ctx.clone();
        let view = view.clone();
        let dialog = dialog.clone();
        let reply_to = reply_to.clone();
        let hooks = hooks.clone();
        publish.connect_clicked(move |_| {
            let buffer = view.buffer();
            let text = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), true)
                .to_string();
            if text.trim().is_empty() {
                return;
            }
            let core = ctx.core.clone();
            let ctx = ctx.clone();
            let hooks = hooks.clone();
            drain(core.post_note(text, reply_to.clone()), move |ev| match ev {
                PublishEvent::Held { token, event, hold_secs } => {
                    (hooks.held)(event, token, hold_secs);
                    let toast = adw::Toast::new(&format!("Posting in {}s", hold_secs));
                    toast.set_timeout(hold_secs.max(1) as u32);
                    toast.set_button_label(Some("Undo"));
                    {
                        let core = core.clone();
                        let hooks = hooks.clone();
                        toast.connect_button_clicked(move |_| {
                            core.undo(token);
                            (hooks.undone)(token);
                        });
                    }
                    ctx.overlay.add_toast(toast);
                }
                PublishEvent::Undone { token } => {
                    (hooks.undone)(token);
                    ctx.toast("Undone — nothing was sent");
                }
                PublishEvent::Committed { token } => {
                    (hooks.committed)(token);
                }
                PublishEvent::Failed { error, .. } => {
                    ctx.toast("Publish failed");
                    eprintln!("[post] {error}");
                }
                _ => {}
            });
            dialog.close();
        });
    }

    // Ctrl+Enter publishes (the web composer's keybinding).
    {
        let publish_btn = publish.clone();
        let key = gtk::EventControllerKey::new();
        key.connect_key_pressed(move |_, keyval, _code, mods| {
            let enter = matches!(keyval, gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter);
            let ctrl = mods.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            if enter && ctrl {
                publish_btn.activate();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        view.add_controller(key);
    }
    dialog.present(ctx.nav().root().as_ref());
    view.grab_focus();
}
