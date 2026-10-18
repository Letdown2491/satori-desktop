//! The composer — port of `src/render/compose.ts` + `src/routes/note.ts`
//! (note + picture types). The undo window runs in core; the UI shows the
//! toast the web app drew as a pending pill.
//!
//! Picture flow (postPicture): "Attach image…" picks a file, core uploads it
//! to your Blossom servers (`upload.rs`), and Publish posts the NIP-68
//! kind:20 with the returned NIP-92 imeta tag. Videos are rejected — the web
//! app posts video as a note (nip68.ts), and that lands later here.

use std::cell::RefCell;
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

/// A picked image waiting to ride along with the publish.
#[derive(Clone)]
struct Attachment {
    /// Full path (drives the thumbnail).
    path: String,
    name: String,
    content_type: &'static str,
    bytes: Vec<u8>,
    /// Set once the Blossom upload succeeded (the NIP-92 imeta tag) — a
    /// failed upload doesn't cost the already-uploaded images a re-send.
    imeta: Option<Vec<String>>,
}

thread_local! {
    /// The last un-published top-level note text, restored on reopen — the
    /// session-scoped slice of the web app's drafts (note.ts saveDraft).
    /// Nothing nags, nothing is trapped: text simply survives a stray close.
    static DRAFT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    /// Set when a publish consumed the buffer; the close that follows must
    /// not re-stash the published text as a draft.
    static PUBLISHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
    // Draft restore: a top-level compose reopens with whatever was typed and
    // lost to a stray close (replies start fresh — they have their context).
    PUBLISHED.set(false);
    if reply_to.is_none() {
        DRAFT.with(|d| {
            if let Some(text) = d.borrow_mut().take() {
                view.buffer().set_text(&text);
            }
        });
    }
    scroll.set_child(Some(&view));
    content.append(&scroll);

    // Attached images: one row per pick (upload.ts mediaItem — the web app
    // appended a thumbnail per upload; so do we, with remove per row).
    let attach_list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .visible(false)
        .build();
    content.append(&attach_list);

    // Bottom row: attach + publish.
    let bottom = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    let attach_btn = gtk::Button::builder()
        .icon_name("image-symbolic")
        .tooltip_text("Attach image (posts as a picture)")
        .css_classes(["flat"])
        .build();
    let publish = gtk::Button::builder()
        .label(if reply_to.is_some() { "Reply" } else { "Publish" })
        .css_classes(["seal-btn"])
        .build();
    bottom.append(&attach_btn);
    bottom.append(&publish);
    content.append(&bottom);

    let dialog = adw::Dialog::builder()
        .title(&title)
        .content_width(520)
        .content_height(360)
        .build();
    dialog.set_child(Some(&content));

    // --- attachment state -------------------------------------------------------
    // Model-then-render: the Vec<Attachment> is the truth; rows re-render from
    // it on every add/remove. `imeta` fills in as each upload succeeds, so a
    // failed upload only costs the not-yet-uploaded images their retry.
    let attachments: Rc<RefCell<Vec<Attachment>>> = Rc::new(RefCell::new(Vec::new()));
    let is_reply = reply_to.is_some();

    let render = {
        let attachments = attachments.clone();
        let attach_list = attach_list.clone();
        let publish = publish.clone();
        move || {
            let atts = attachments.borrow().clone();
            render_list(&attach_list, &atts, &publish, is_reply);
        }
    };
    {
        let ctx = ctx.clone();
        let attachments = attachments.clone();
        let render = render.clone();
        attach_btn.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Attach image")
                .modal(true)
                .build();
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Images"));
            filter.add_mime_type("image/png");
            filter.add_mime_type("image/jpeg");
            filter.add_mime_type("image/gif");
            filter.add_mime_type("image/webp");
            dialog.set_default_filter(Some(&filter));
            let ctx = ctx.clone();
            let attachments = attachments.clone();
            let render = render.clone();
            glib::spawn_future_local(async move {
                let parent = ctx
                    .nav()
                    .root()
                    .map(|r| r.upcast::<gtk::Widget>())
                    .and_downcast::<adw::ApplicationWindow>();
                match dialog.open_future(parent.as_ref()).await {
                    Ok(file) => {
                        let Some(path) = file.path() else { return };
                        let Some(mime) =
                            satori_core::upload::image_mime(&path.to_string_lossy())
                        else {
                            ctx.toast("Pictures take PNG, JPEG, GIF or WebP");
                            return;
                        };
                        match std::fs::read(&path) {
                            Ok(bytes) => {
                                if bytes.len() as u64 > satori_core::upload::MAX_BYTES {
                                    ctx.toast("That file is too large (max 25 MB)");
                                    return;
                                }
                                attachments.borrow_mut().push(Attachment {
                                    path: path.to_string_lossy().to_string(),
                                    name: path
                                        .file_name()
                                        .map(|n| n.to_string_lossy().to_string())
                                        .unwrap_or_default(),
                                    content_type: mime,
                                    bytes,
                                    imeta: None,
                                });
                                render();
                            }
                            Err(e) => ctx.toast(&format!("Couldn't read that file: {e}")),
                        }
                    }
                    Err(e) => eprintln!("[attach] {e}"),
                }
            });
        });
    }

    // --- publish ---------------------------------------------------------------
    {
        let ctx = ctx.clone();
        let view = view.clone();
        let dialog = dialog.clone();
        let reply_to = reply_to.clone();
        let hooks = hooks.clone();
        let attachments = attachments.clone();
        let attach_list = attach_list.clone();
        let publish_in = publish.clone();
        publish.connect_clicked(move |_| {
            let buffer = view.buffer();
            let text = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), true)
                .to_string();
            let images = attachments.borrow().clone();
            if text.trim().is_empty() && images.is_empty() {
                return;
            }
            let core = ctx.core.clone();
            let ctx = ctx.clone();
            let hooks = hooks.clone();
            let dialog = dialog.clone();
            if images.is_empty() {
                start_publish(&ctx, &core, text, reply_to.clone(), None, hooks.clone(), dialog.clone());
            } else {
                // Upload every image first (upload.ts: one POST per file), then
                // post the kind:20 with all imetas. The dialog STAYS OPEN with
                // a quiet busy state — silence during a multi-second upload
                // reads as "nothing happened". Already-uploaded images keep
                // their imetas, so a failure resumes mid-chain on retry.
                publish_in.set_sensitive(false);
                attach_list.set_sensitive(false);
                upload_chain(
                    &ctx,
                    &core,
                    attachments.clone(),
                    attach_list.clone(),
                    publish_in.clone(),
                    reply_to.is_none(),
                    text,
                    reply_to.clone(),
                    hooks.clone(),
                    dialog.clone(),
                );
            }
        });
    }

    // Draft keep: closing with un-published text stashes it (top-level only);
    // the next plain compose restores it.
    {
        let view = view.clone();
        let reply_to = reply_to.clone();
        dialog.connect_closed(move |_| {
            if reply_to.is_some() || PUBLISHED.get() {
                return;
            }
            let buffer = view.buffer();
            let text = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), true)
                .to_string();
            DRAFT.with(|d| {
                *d.borrow_mut() = if text.trim().is_empty() { None } else { Some(text) };
            });
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

/// One line of an error for a toast (toasts can't scroll; keep it sane).
fn short_err(s: &str) -> String {
    let one_line: String = s.lines().collect::<Vec<_>>().join(" · ");
    one_line.chars().take(140).collect()
}

/// The publish button label follows the attachment count.
fn publish_label(n: usize, is_reply: bool) -> String {
    match (n, is_reply) {
        (0, true) => "Reply".to_string(),
        (0, false) => "Publish".to_string(),
        (1, _) => "Post picture".to_string(),
        (n, _) => format!("Post {n} pictures"),
    }
}

/// Rebuild the attachment rows from the model (the model is the truth).
fn render_list(attach_list: &gtk::Box, atts: &[Attachment], publish: &gtk::Button, is_reply: bool) {
    while let Some(c) = attach_list.first_child() {
        attach_list.remove(&c);
    }
    attach_list.set_visible(!atts.is_empty());
    for (i, a) in atts.iter().enumerate() {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        let pic = gtk::Picture::builder()
            .width_request(48)
            .height_request(48)
            .build();
        pic.set_filename(Some(std::path::Path::new(&a.path)));
        row.append(&pic);
        let mark = gtk::Label::builder()
            .valign(gtk::Align::Center)
            .css_classes(["tagline"])
            .build();
        mark.set_text(if a.imeta.is_some() { "✓" } else { "" });
        row.append(&mark);
        let name = gtk::Label::builder()
            .label(&a.name)
            .css_classes(["tagline"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .hexpand(true)
            .halign(gtk::Align::Start)
            .build();
        row.append(&name);
        let remove_btn = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Remove image")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build();
        row.append(&remove_btn);
        {
            let atts2: Vec<Attachment> = atts.to_vec();
            let attach_list2 = attach_list.clone();
            let publish2 = publish.clone();
            remove_btn.connect_clicked(move |_| {
                let remaining: Vec<Attachment> = atts2
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, a)| a.clone())
                    .collect();
                render_list(&attach_list2, &remaining, &publish2, is_reply);
            });
        }
        attach_list.append(&row);
    }
    publish.set_label(&publish_label(atts.len(), is_reply));
}

/// Upload the images one by one, then post the kind:20 with every imeta.
/// Resume-safe: images already carrying an imeta are skipped.
#[allow(clippy::too_many_arguments)]
fn upload_chain(
    ctx: &Ctx,
    core: &std::sync::Arc<satori_core::manager::Core>,
    attachments: Rc<RefCell<Vec<Attachment>>>,
    attach_list: gtk::Box,
    publish_btn: gtk::Button,
    top_level: bool,
    text: String,
    reply_to: Option<Event>,
    hooks: WriteHooks,
    dialog: adw::Dialog,
) {
    let ctx = ctx.clone();
    let core = core.clone();
    let total = attachments.borrow().len();
    let Some(i) = attachments.borrow().iter().position(|a| a.imeta.is_none()) else {
        // All uploaded — assemble + post (closes the dialog).
        attach_list.set_sensitive(true);
        let imetas: Vec<Vec<String>> = attachments
            .borrow()
            .iter()
            .filter_map(|a| a.imeta.clone())
            .collect();
        start_publish(&ctx, &core, text, reply_to, Some(imetas), hooks, dialog);
        return;
    };
    publish_btn.set_label(&format!("Uploading {}/{}…", i + 1, total));
    let att = attachments.borrow()[i].clone();
    drain(
        core.blossom_upload(att.bytes, att.content_type.to_string()),
        move |ev| match ev {
            PublishEvent::Uploaded { imeta, .. } => {
                attachments.borrow_mut()[i].imeta = Some(imeta);
                let atts = attachments.borrow().clone();
                render_list(&attach_list, &atts, &publish_btn, top_level);
                upload_chain(
                    &ctx,
                    &core,
                    attachments.clone(),
                    attach_list.clone(),
                    publish_btn.clone(),
                    top_level,
                    text.clone(),
                    reply_to.clone(),
                    hooks.clone(),
                    dialog.clone(),
                );
            }
            PublishEvent::Failed { error, .. } => {
                publish_btn.set_sensitive(true);
                publish_btn.set_label("Post pictures");
                attach_list.set_sensitive(true);
                ctx.toast(&format!("Upload failed: {}", short_err(&error)));
                eprintln!("[upload] {error}");
            }
            _ => {}
        },
    );
}

/// The note/picture publish: signs, holds for the undo window, shows the
/// countdown toast with the Undo button (the web app's pending pill).
fn start_publish(
    ctx: &Ctx,
    core: &std::sync::Arc<satori_core::manager::Core>,
    text: String,
    reply_to: Option<Event>,
    imeta: Option<Vec<Vec<String>>>,
    hooks: WriteHooks,
    dialog: adw::Dialog,
) {
    let ctx = ctx.clone();
    let core = core.clone();
    // A top-level publish consumes the buffer: the draft is spent.
    if reply_to.is_none() {
        PUBLISHED.set(true);
        DRAFT.with(|d| *d.borrow_mut() = None);
    }
    let rx = match &imeta {
        Some(imeta) => core.post_picture(String::new(), text, imeta.clone(), None),
        None => core.post_note(text, reply_to),
    };
    drain(rx, move |ev| match ev {
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
            // The buffer was never sent — keep it as the draft on close.
            PUBLISHED.set(false);
            ctx.toast(&format!("Publish failed: {}", short_err(&error)));
            eprintln!("[post] {error}");
        }
        _ => {}
    });
    dialog.close();
}
