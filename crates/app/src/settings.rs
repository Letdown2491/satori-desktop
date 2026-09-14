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

    // --- Content (content-prefs.ts): what the feed carries -------------------
    let content_sec = section("Content");
    let content_hint = gtk::Label::builder()
        .label("What your feed carries. Changes apply on the next home refresh.")
        .css_classes(["tagline"])
        .wrap(true)
        .halign(gtk::Align::Start)
        .build();
    content_sec.append(&content_hint);

    let hidden: Vec<u16> = std::fs::read_to_string(satori_core::config::state_file(
        satori_core::config::files::PREFS,
    ))
    .ok()
    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
    .and_then(|v| {
        v.get("content_hidden")
            .and_then(|h| serde_json::from_value(h.clone()).ok())
    })
    .unwrap_or_default();

    let grid = gtk::Grid::builder()
        .orientation(gtk::Orientation::Horizontal)
        .column_spacing(24)
        .row_spacing(8)
        .build();
    for (i, (label, kinds)) in satori_core::store::CATALOG.iter().enumerate() {
        let row = i / 3;
        let col = i % 3;
        let on = !kinds.iter().any(|k| hidden.contains(k));
        let sw = gtk::Switch::builder().active(on).build();
        {
            let kinds: Vec<u16> = kinds.to_vec();
            sw.connect_state_set(move |_, state| {
                set_kind_hidden(&kinds, !state);
                gtk::glib::Propagation::Proceed
            });
        }
        grid.attach(&gtk::Label::builder().label(*label).halign(gtk::Align::Start).build(), col as i32, row as i32, 1, 1);
        grid.attach(&sw, col as i32, row as i32 + 1, 1, 1);
    }
    content_sec.append(&grid);
    column.append(&content_sec);

    // --- Backup (list-backup.ts): raw signed list events ---------------------
    let backup = section("Backup");
    let backup_hint = gtk::Label::builder()
        .label("Export your follows, mutes, bookmarks and relay lists as signed events; restoring re-publishes them fresh.")
        .css_classes(["tagline"])
        .wrap(true)
        .halign(gtk::Align::Start)
        .build();
    backup.append(&backup_hint);
    let backup_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let export_btn = gtk::Button::builder()
        .label("Export")
        .css_classes(["flat", "pill"])
        .build();
    let import_btn = gtk::Button::builder()
        .label("Import & restore")
        .css_classes(["flat", "pill"])
        .build();
    backup_row.append(&export_btn);
    backup_row.append(&import_btn);
    backup.append(&backup_row);
    column.append(&backup);

    {
        let ctx = ctx.clone();
        export_btn.connect_clicked(move |_| {
            let ctx2 = ctx.clone();
            drain(ctx2.core.export_lists(), move |reply| {
                if let satori_core::manager::FeedEvent::BackupJson(json) = reply {
                    let dialog = gtk::FileDialog::builder()
                        .title("Save backup")
                        .initial_name("satori-backup.json")
                        .build();
                    let win = ctx2
                        .nav
                        .root()
                        .map(|r| r.upcast::<gtk::Widget>())
                        .and_downcast::<adw::ApplicationWindow>();
                    let ctx3 = ctx2.clone();
                    dialog.save(win.as_ref(), gtk::gio::Cancellable::NONE, move |file| {
                        if let Ok(file) = file {
                            let _ = file.replace_contents(
                                json.as_bytes(),
                                None,
                                false,
                                gtk::gio::FileCreateFlags::NONE,
                                gtk::gio::Cancellable::NONE,
                            );
                            ctx3.toast("Backup saved");
                        }
                    });
                }
            });
        });
    }
    {
        let ctx = ctx.clone();
        import_btn.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Choose backup")
                .build();
            let ctx2 = ctx.clone();
            let win = ctx
                .nav
                .root()
                .map(|r| r.upcast::<gtk::Widget>())
                .and_downcast::<adw::ApplicationWindow>();
            dialog.open(win.as_ref(), gtk::gio::Cancellable::NONE, move |file| {
                if let Ok(file) = file {
                    match file.load_contents(gtk::gio::Cancellable::NONE) {
                        Ok((bytes, _)) => {
                            let json = String::from_utf8_lossy(&bytes).to_string();
                            let ctx3 = ctx2.clone();
                            drain(ctx3.core.import_lists(json), move |reply| match reply {
                                satori_core::manager::PublishEvent::Restored(n) => {
                                    ctx3.toast(&format!("Restored {n} list(s)"));
                                }
                                satori_core::manager::PublishEvent::Failed { error, .. } => {
                                    ctx3.toast("Import failed");
                                    eprintln!("[backup] {error}");
                                }
                                _ => {}
                            });
                        }
                        Err(e) => {
                            ctx2.toast("Could not read file");
                            eprintln!("[backup] {e}");
                        }
                    }
                }
            });
        });
    }

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

/// Flip a catalog kind's visibility in prefs.json (`content_hidden`).
fn set_kind_hidden(kinds: &[u16], hidden: bool) {
    let path = satori_core::config::state_file(satori_core::config::files::PREFS);
    let mut value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(obj) = value.as_object_mut() {
        let mut hidden_list: Vec<u16> = obj
            .get("content_hidden")
            .and_then(|h| serde_json::from_value(h.clone()).ok())
            .unwrap_or_default();
        for k in kinds {
            if hidden {
                if !hidden_list.contains(k) {
                    hidden_list.push(*k);
                }
            } else {
                hidden_list.retain(|h| !kinds.contains(h));
            }
        }
        obj.insert("content_hidden".into(), serde_json::json!(hidden_list));
    }
    if let Ok(body) = serde_json::to_string_pretty(&value) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(parent) = satori_core::config::data_dir_parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| f.write_all(body.as_bytes()));
    }
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
