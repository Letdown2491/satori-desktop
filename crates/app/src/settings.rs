//! Settings — port of `src/render/settings.ts` + `src/routes/settings.ts`
//! (slice 1): the Relays pane (NIP-65 read/write editor) and Appearance
//! (light/dark/system). Backup/Content/Privacy tabs land with their
//! milestones.

use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use satori_core::manager::PublishEvent;

use crate::bridge::drain;
use crate::ctx::Ctx;

pub fn settings_page(ctx: &Ctx) -> adw::NavigationPage {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(14)
        .margin_top(12)
        .margin_bottom(48)
        .build();

    // --- Appearance ---------------------------------------------------------
    let appearance = section("Appearance");
    let seg = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::Start)
        .build();
    let current = crate::prefs::load().dark;
    let mk = |label: &str, active: bool| {
        gtk::ToggleButton::builder()
            .label(label)
            .css_classes(if active { vec!["seal-btn", "pill"] } else { vec!["flat", "pill"] })
            .active(active)
            .build()
    };
    let sys = mk("System", current.is_none());
    let light = mk("Light", current == Some(false));
    let dark = mk("Dark", current == Some(true));
    seg.append(&sys);
    seg.append(&light);
    seg.append(&dark);
    appearance.append(&seg);
    column.append(&appearance);

    // --- Relays (NIP-65) ----------------------------------------------------
    let relays = section("Relays");
    let hint = gtk::Label::builder()
        .label("Where your events go and come from. Published as your NIP-65 list. Five indexers are always used for reading; add your own relays to route writes.")
        .css_classes(["tagline"])
        .wrap(true)
        .halign(gtk::Align::Start)
        .build();
    relays.append(&hint);

    // Rows live in a map so Save reads them back (form-as-state).
    let rows: Rc<std::cell::RefCell<HashMap<String, (gtk::CheckButton, gtk::CheckButton)>>> =
        Rc::new(std::cell::RefCell::new(HashMap::new()));
    let list = gtk::ListBox::builder()
        .css_classes(["feed-list", "boxed-list"])
        .selection_mode(gtk::SelectionMode::None)
        .build();
    relays.append(&list);

    let add_row_entry = gtk::Entry::builder()
        .placeholder_text("wss://relay.example")
        .hexpand(true)
        .build();
    let add_read = gtk::CheckButton::builder().label("read").active(true).build();
    let add_write = gtk::CheckButton::builder().label("write").active(true).build();
    let add_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Add relay")
        .build();
    let add_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(6)
        .build();
    add_row.append(&add_row_entry);
    add_row.append(&add_read);
    add_row.append(&add_write);
    add_row.append(&add_btn);
    relays.append(&add_row);

    let save = gtk::Button::builder()
        .label("Publish relay list")
        .css_classes(["seal-btn"])
        .halign(gtk::Align::End)
        .margin_top(8)
        .build();
    relays.append(&save);
    column.append(&relays);

    let clamp = adw::Clamp::builder().maximum_size(720).child(&column).build();
    let scroll = gtk::ScrolledWindow::builder()
        .child(&clamp)
        .hexpand(true)
        .vexpand(true)
        .build();

    let page = adw::NavigationPage::builder()
        .title("Settings")
        .child(&scroll)
        .build();

    // Populate from the session's current NIP-65. If bootstrap is still
    // landing (or found nothing yet), catch up on a short poll — the pane is
    // a projection of core state.
    {
        let ctx_pop = ctx.clone();
        let rows = Rc::clone(&rows);
        let list = list.clone();
        let populate = move || {
            let rows = Rc::clone(&rows);
            let (read, write) = ctx_pop.core.current_relays();
            let mut urls = read.clone();
            urls.extend(write.iter().filter(|w| !read.contains(*w)).cloned());
            if urls.is_empty() {
                return false; // keep polling
            }
            for url in urls {
                if rows.borrow().contains_key(&url) {
                    continue;
                }
                let r = read.contains(&url);
                let w = write.contains(&url);
                let row = relay_row(&url, r, w, rows.clone());
                list.append(&row);
            }
            true
        };
        if !populate() {
            let mut tries = 0;
            glib::timeout_add_local(std::time::Duration::from_millis(800), move || {
                tries += 1;
                if populate() || tries > 30 {
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });
        }
    }

    // Add relay.
    {
        let rows = Rc::clone(&rows);
        let list = list.clone();
        let entry = add_row_entry.clone();
        let read = add_read.clone();
        let write = add_write.clone();
        add_btn.connect_clicked(move |_| {
            let url = entry.text().to_string().trim().to_string();
            if !url.starts_with("wss://") && !url.starts_with("ws://") {
                return;
            }
            let row = relay_row(&url, read.is_active(), write.is_active(), Rc::clone(&rows));
            list.append(&row);
            entry.set_text("");
        });
    }

    // Publish.
    {
        let ctx = ctx.clone();
        let rows = Rc::clone(&rows);
        save.connect_clicked(move |_| {
            let mut read = Vec::new();
            let mut write = Vec::new();
            for (url, (r, w)) in rows.borrow().iter() {
                if r.is_active() {
                    read.push(url.clone());
                }
                if w.is_active() {
                    write.push(url.clone());
                }
            }
            if read.is_empty() && write.is_empty() {
                ctx.toast("Nothing to publish — add a relay first");
                return;
            }
            let core = ctx.core.clone();
            let cb_ctx = ctx.clone();
            drain(core.set_relays(read, write), move |ev| match ev {
                PublishEvent::Committed { .. } => cb_ctx.toast("Relay list published"),
                PublishEvent::Failed { error, .. } => {
                    cb_ctx.toast("Publish failed");
                    eprintln!("[relays] {error}");
                }
                _ => {}
            });
        });
    }

    // Theme buttons: active one wears the seal, the rest go flat.
    {
        let group = [sys.clone(), light.clone(), dark.clone()];
        let buttons = [(sys, None), (light, Some(false)), (dark, Some(true))];
        for (btn, value) in buttons {
            let group = group.clone();
            btn.connect_toggled(move |toggle| {
                if !toggle.is_active() {
                    return;
                }
                let active_label = toggle.label();
                for b in &group {
                    let on = b.label().as_deref() == active_label.as_deref();
                    b.set_css_classes(if on { &["seal-btn", "pill"] } else { &["flat", "pill"] });
                }
                match value {
                    None => {
                        let system_dark = adw::StyleManager::default().is_dark();
                        crate::theme::set_dark(system_dark);
                        // System: clear the override.
                        let mut p = crate::prefs::load();
                        p.dark = None;
                        crate::prefs::save(&p);
                    }
                    Some(d) => crate::theme::set_dark(d),
                }
            });
        }
    }

    page
}

fn section(title: &str) -> gtk::Box {
    let box_ = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    let label = gtk::Label::builder()
        .label(title)
        .css_classes(["serif", "note-author", "section-title"])
        .halign(gtk::Align::Start)
        .build();
    box_.append(&label);
    box_
}

type RelayRowState = Rc<std::cell::RefCell<HashMap<String, (gtk::CheckButton, gtk::CheckButton)>>>;

fn relay_row(url: &str, read: bool, write: bool, rows: RelayRowState) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .css_classes(["relay-row"])
        .build();
    let label = gtk::Label::builder()
        .label(url)
        .halign(gtk::Align::Start)
        .hexpand(true)
        .css_classes(["mono"])
        .build();
    row.append(&label);

    let r = gtk::CheckButton::builder().label("read").active(read).build();
    let w = gtk::CheckButton::builder().label("write").active(write).build();
    row.append(&r);
    row.append(&w);

    let remove = crate::icons::action_button("trash", "Remove");
    {
        let row = row.clone();
        let rows = Rc::clone(&rows);
        let url = url.to_string();
        remove.connect_clicked(move |_| {
            rows.borrow_mut().remove(&url);
            row.set_visible(false);
        });
    }
    row.append(&remove);

    rows.borrow_mut().insert(url.to_string(), (r, w));
    row
}
