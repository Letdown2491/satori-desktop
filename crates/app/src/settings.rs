//! Settings — port of `src/render/settings.ts` + `src/routes/settings.ts`,
//! as an AdwPreferencesWindow: General (appearance + undo window), Content
//! (feed kind visibility), Backup (list export/restore), Relays (NIP-65
//! editor). The grouped-row idiom is Adw's native settings UX.

use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use satori_core::manager::{FeedEvent, PublishEvent};

use crate::bridge::drain;
use crate::ctx::Ctx;

pub fn open_settings(ctx: &Ctx) {
    let win = adw::PreferencesWindow::builder()
        .title("Settings")
        .icon_name("satori-logo")
        .modal(true)
        .search_enabled(false)
        .default_width(620)
        .default_height(660)
        .build();
    if let Some(parent) = ctx
        .nav
        .root()
        .map(|r| r.upcast::<gtk::Widget>())
        .and_downcast::<adw::ApplicationWindow>()
    {
        win.set_transient_for(Some(&parent));
    }

    win.add(&general_page(ctx));
    win.add(&content_page());
    win.add(&backup_page(ctx));
    win.add(&relays_page(ctx));
    win.present();
}

// --- General: appearance + the undo window -----------------------------------

fn general_page(ctx: &Ctx) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("General")
        .icon_name("satori-gear-symbolic")
        .build();

    let prefs = crate::prefs::load();

    let appearance = adw::PreferencesGroup::builder()
        .title("Appearance")
        .description("Ink on washi, light or dark.")
        .build();
    let theme_model = gtk::StringList::new(&["System", "Light", "Dark"]);
    let selected = match prefs.dark {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    let theme_row = adw::ComboRow::builder()
        .title("Theme")
        .model(&theme_model)
        .selected(selected)
        .build();
    {
        let _ctx = ctx.clone();
        theme_row.connect_selected_notify(move |row| {
            let choice = row.selected();
            let mut p = crate::prefs::load();
            match choice {
                0 => {
                    let system_dark = adw::StyleManager::default().is_dark();
                    crate::theme::set_dark(system_dark);
                    p.dark = None;
                    crate::prefs::save(&p);
                }
                1 => crate::theme::set_dark(false),
                2 => crate::theme::set_dark(true),
                _ => {}
            }
        });
    }
    appearance.add(&theme_row);
    page.add(&appearance);

    let composing = adw::PreferencesGroup::builder()
        .title("Composing")
        .description("The undo window is the breath before speech: posts are held before they go out.")
        .build();
    let undo_row = adw::SwitchRow::builder()
        .title("Undo window")
        .subtitle("Hold each post before publishing")
        .active(prefs.undo_enabled.unwrap_or(true))
        .build();
    let hold_row = adw::SpinRow::builder()
        .title("Hold for")
        .subtitle("Seconds (1–30)")
        .adjustment(&gtk::Adjustment::new(
            prefs.undo_seconds.unwrap_or(5) as f64,
            1.0,
            30.0,
            1.0,
            1.0,
            1.0,
        ))
        .numeric(true)
        .sensitive(prefs.undo_enabled.unwrap_or(true))
        .build();
    {
        let ctx = ctx.clone();
        let hold = hold_row.clone();
        undo_row.connect_active_notify(move |row| {
            hold.set_sensitive(row.is_active());
            apply_undo(&ctx, row.is_active(), None);
        });
    }
    {
        let ctx = ctx.clone();
        let undo = undo_row.clone();
        let hold_recv = hold_row.clone();
        hold_row.connect_changed(move |_| {
            let undo_on = undo.is_active();
            let secs = hold_recv.value() as u32;
            apply_undo(&ctx, undo_on, Some(secs));
        });
    }
    composing.add(&undo_row);
    composing.add(&hold_row);
    page.add(&composing);

    page
}

fn apply_undo(ctx: &Ctx, enabled: bool, seconds: Option<u32>) {
    let secs = seconds.unwrap_or_else(|| {
        crate::prefs::load().undo_seconds.unwrap_or(5)
    });
    let mut p = crate::prefs::load();
    p.undo_enabled = Some(enabled);
    p.undo_seconds = Some(secs);
    crate::prefs::save(&p);
    ctx.core.set_undo(enabled, secs);
}

// --- Content: what the feed carries -------------------------------------------

fn content_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Content")
        .icon_name("satori-notes-symbolic")
        .build();

    let group = adw::PreferencesGroup::builder()
        .title("Feed content")
        .description("What kinds your feed carries. Applies on the next home refresh.")
        .build();

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

    for (label, kinds) in satori_core::store::CATALOG {
        let on = !kinds.iter().any(|k| hidden.contains(k));
        let row = adw::SwitchRow::builder().title(label).active(on).build();
        {
            let kinds: Vec<u16> = kinds.to_vec();
            row.connect_active_notify(move |r| {
                set_kind_hidden(&kinds, !r.is_active());
            });
        }
        group.add(&row);
    }
    page.add(&group);
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

// --- Backup: raw signed list events ---------------------------------------------

fn backup_page(ctx: &Ctx) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Backup")
        .icon_name("satori-shield-symbolic")
        .build();

    let group = adw::PreferencesGroup::builder()
        .title("List backup")
        .description("Your follows, mutes, bookmarks and relay lists as signed events. Restoring re-publishes them fresh.")
        .build();

    let export_row = adw::ActionRow::builder()
        .title("Export")
        .subtitle("Save the newest signed event per list")
        .build();
    let export_btn = gtk::Button::builder()
        .label("Save…")
        .css_classes(["flat", "pill"])
        .valign(gtk::Align::Center)
        .build();
    export_row.add_suffix(&export_btn);
    group.add(&export_row);

    let import_row = adw::ActionRow::builder()
        .title("Import & restore")
        .subtitle("Re-signs and publishes each list event")
        .build();
    let import_btn = gtk::Button::builder()
        .label("Choose file…")
        .css_classes(["flat", "pill"])
        .valign(gtk::Align::Center)
        .build();
    import_row.add_suffix(&import_btn);
    group.add(&import_row);
    page.add(&group);

    {
        let ctx = ctx.clone();
        export_btn.connect_clicked(move |_| {
            let ctx2 = ctx.clone();
            drain(ctx2.core.export_lists(), move |reply| {
                if let FeedEvent::BackupJson(json) = reply {
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
                    dialog.save(
                        win.as_ref(),
                        gtk::gio::Cancellable::NONE,
                        move |file| match file {
                            Ok(file) => {
                                let ok = file
                                    .replace_contents(
                                        json.as_bytes(),
                                        None,
                                        false,
                                        gtk::gio::FileCreateFlags::NONE,
                                        gtk::gio::Cancellable::NONE,
                                    )
                                    .is_ok();
                                if ok {
                                    ctx3.toast("Backup saved");
                                } else {
                                    ctx3.toast("Could not write backup");
                                }
                            }
                            Err(e) if !e.matches(gtk::gio::IOErrorEnum::Cancelled) => {
                                ctx3.toast("Could not write backup");
                            }
                            _ => {}
                        },
                    );
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
            let win = ctx
                .nav
                .root()
                .map(|r| r.upcast::<gtk::Widget>())
                .and_downcast::<adw::ApplicationWindow>();
            let ctx2 = ctx.clone();
            dialog.open(
                win.as_ref(),
                gtk::gio::Cancellable::NONE,
                move |file| match file {
                    Ok(file) => match file.load_contents(gtk::gio::Cancellable::NONE) {
                        Ok((bytes, _)) => {
                            let json = String::from_utf8_lossy(&bytes).to_string();
                            let ctx3 = ctx2.clone();
                            drain(ctx3.core.import_lists(json), move |reply| match reply {
                                PublishEvent::Restored(n) => {
                                    ctx3.toast(&format!("Restored {n} list(s)"));
                                }
                                PublishEvent::Failed { error, .. } => {
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
                    },
                    Err(e) if !e.matches(gtk::gio::IOErrorEnum::Cancelled) => {
                        ctx2.toast("Could not open file");
                    }
                    _ => {}
                },
            );
        });
    }

    page
}

// --- Relays: the NIP-65 editor ----------------------------------------------------

type RelayRows = Rc<std::cell::RefCell<HashMap<String, (gtk::CheckButton, gtk::CheckButton)>>>;

fn relays_page(ctx: &Ctx) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("Relays")
        .icon_name("satori-globe-symbolic")
        .build();

    let rows: RelayRows = Rc::new(std::cell::RefCell::new(HashMap::new()));

    let your = adw::PreferencesGroup::builder()
        .title("Your relays")
        .description("Where your events go and come from — published as your NIP-65 list. Five indexers always serve reads.")
        .build();

    // Add row first (top of the group).
    let add_row = adw::EntryRow::builder().title("Add a relay (wss://…)").build();
    let add_read = gtk::CheckButton::builder().label("read").active(true).valign(gtk::Align::Center).build();
    let add_write = gtk::CheckButton::builder().label("write").active(true).valign(gtk::Align::Center).build();
    let add_btn = gtk::Button::builder()
        .icon_name("satori-plus-symbolic")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .tooltip_text("Add")
        .build();
    add_row.add_suffix(&add_read);
    add_row.add_suffix(&add_write);
    add_row.add_suffix(&add_btn);
    your.add(&add_row);
    page.add(&your);

    // Publish action group.
    let publish_group = adw::PreferencesGroup::builder().build();
    let publish_row = adw::ActionRow::builder()
        .title("Publish relay list")
        .subtitle("Signed as your NIP-65 (kind 10002) to the write set")
        .build();
    let publish_btn = gtk::Button::builder()
        .label("Publish")
        .css_classes(["seal-btn", "pill"])
        .valign(gtk::Align::Center)
        .build();
    publish_row.add_suffix(&publish_btn);
    publish_group.add(&publish_row);
    page.add(&publish_group);

    // Populate from bootstrap's fetched NIP-65, with a catch-up poll (the
    // pane is a projection; bootstrap may still be landing).
    {
        let rows = Rc::clone(&rows);
        let ctx2 = ctx.clone();
        let your2 = your.clone();
        let mut tries = 0;
        let fill = move |your: &adw::PreferencesGroup| -> bool {
            let (read, write) = ctx2.core.current_relays();
            let mut urls = read.clone();
            urls.extend(write.iter().filter(|w| !read.contains(*w)).cloned());
            for url in urls {
                if rows.borrow().contains_key(&url) {
                    continue;
                }
                let row = relay_row(
                    &url,
                    read.contains(&url),
                    write.contains(&url),
                    Rc::clone(&rows),
                    your,
                );
                your.add(&row);
            }
            !ctx2.core.current_relays().0.is_empty()
        };
        if !fill(&your) {
            glib::timeout_add_local(std::time::Duration::from_millis(800), move || {
                tries += 1;
                if fill(&your2) || tries > 30 {
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });
        }
    }

    // Publish.    // Publish.
    {
        let ctx = ctx.clone();
        let rows = Rc::clone(&rows);
        publish_btn.connect_clicked(move |_| {
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
            drain(core.set_relays(read, write), move |reply| match reply {
                PublishEvent::Committed { .. } => cb_ctx.toast("Relay list published"),
                PublishEvent::Failed { error, .. } => {
                    cb_ctx.toast("Publish failed");
                    eprintln!("[relays] {error}");
                }
                _ => {}
            });
        });
    }

    // Add (button and Enter share one handler).
    let add = {
        let _ctx = ctx.clone();
        let rows = Rc::clone(&rows);
        let your_group = your.clone();
        let add_read = add_read.clone();
        let add_write = add_write.clone();
        move |row: &adw::EntryRow, toast_ctx: &Ctx| {
            let url = row.text().to_string().trim().to_string();
            if !url.starts_with("wss://") && !url.starts_with("ws://") {
                toast_ctx.toast("Relay URLs start with wss://");
                return;
            }
            if rows.borrow().contains_key(&url) {
                return;
            }
            let r = relay_row(
                &url,
                add_read.is_active(),
                add_write.is_active(),
                Rc::clone(&rows),
                &your_group,
            );
            your_group.add(&r);
            row.set_text("");
        }
    };
    {
        let add = add.clone();
        let ctx2 = ctx.clone();
        let row_ref = add_row.clone();
        add_btn.connect_clicked(move |_| {
            add(&row_ref, &ctx2);
        });
    }
    {
        let add = add.clone();
        let ctx2 = ctx.clone();
        add_row.connect_entry_activated(move |row| {
            add(row, &ctx2);
        });
    }

    page
}

/// One relay row: url + read/write toggles + remove.
fn relay_row(
    url: &str,
    read: bool,
    write: bool,
    rows: RelayRows,
    group: &adw::PreferencesGroup,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(url).build();
    let r = gtk::CheckButton::builder()
        .label("read")
        .active(read)
        .valign(gtk::Align::Center)
        .css_classes(["relay-toggle"])
        .build();
    let w = gtk::CheckButton::builder()
        .label("write")
        .active(write)
        .valign(gtk::Align::Center)
        .css_classes(["relay-toggle"])
        .build();
    row.add_suffix(&r);
    row.add_suffix(&w);

    let remove = gtk::Button::builder()
        .child(&crate::icons::icon("trash"))
        .css_classes(["flat", "act"])
        .tooltip_text("Remove")
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&remove);

    rows.borrow_mut().insert(url.to_string(), (r, w));

    {
        let rows = Rc::clone(&rows);
        let url = url.to_string();
        let group = group.clone();
        let row = row.clone();
        remove.connect_clicked(move |_| {
            rows.borrow_mut().remove(&url);
            group.remove(&row);
        });
    }
    row
}
