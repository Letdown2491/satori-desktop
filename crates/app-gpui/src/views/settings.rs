//! Settings — port of `crates/app/src/settings.rs` (General/Content/Media/
//! Relays pages) as one quiet scroll: the relay editor (NIP-65, kind 10002),
//! media servers (Blossom, kind 10063), the undo window, the feed-content
//! catalog, and list backup (export/import of the signed events via rfd's
//! portal dialogs).
//!
//! TODO(port): theme choice waits for P4 (both themes).

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, rgb, AnyElement, App, Context, Div, Entity, FocusHandle, Focusable,
    KeyDownEvent, Render, SharedString, Stateful, Window,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, PublishEvent};
use satori_core::wallet::{self, Status};

use std::str::FromStr;

use gpui::EventEmitter;

use crate::{input::Input, prefs, theme};

use super::{nav::Nav, states};

pub struct SettingsView {
    core: Arc<Core>,
    focus: FocusHandle,
    // Relays (the NIP-65 editor): local edits; Publish commits as kind 10002.
    relays: Vec<RelayEntry>,
    add_relay: Entity<Input>,
    relay_read: bool,
    relay_write: bool,
    // Media servers (Blossom): local edits; Publish commits as kind 10063.
    servers: Vec<String>,
    add_server: Entity<Input>,
    // Composing: the undo window (the Input holds the seconds; the cached
    // value is what a toggle applies without re-parsing).
    dark: Option<bool>,
    undo_enabled: bool,
    undo_secs_val: u32,
    undo_secs: Entity<Input>,
    // Content: the feed catalog.
    machine_hidden: bool,
    hidden_kinds: Vec<u16>,
    // Notifications: reactions behind the bell's pref (core reads it
    // per stream — applies the next time the stream opens).
    reaction_notifs: bool,
    // Bookmarks (D38): what the star WRITES — private kind 10003 (the
    // default) or the public NIP-51 list. Read back per star click.
    bookmark_public: bool,
    // Wallet (D20): mirrored status for the row (the broadcast is truth;
    // the manage pane owns the detail).
    wallet: Status,
    // Set when the sign-out row is clicked: the pane freezes on "Signing
    // out…" while the shell tears the session down (the whole view is
    // replaced by the login doors a beat later).
    signing_out: bool,
    // The signed-in identity, resolved name for the Session row (the short
    // npub stands in until the profile lands).
    me_name: Option<String>,
    // The short npub, computed once — it used to re-read session.json from
    // disk and re-bech32 the key on EVERY render of the pane.
    me_npub: String,
    // Toast equivalent (the pane has no header seal to hang one on).
    status: String,
    status_err: bool,
}

#[derive(Clone)]
struct RelayEntry {
    url: String,
    read: bool,
    write: bool,
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl SettingsView {
    pub fn new(core: Arc<Core>, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        let p = prefs::load();
        let undo_secs_val = p.undo_seconds.unwrap_or(5);
        let undo_secs = cx.new(|cx| {
            let mut i = Input::new(cx);
            i.multiline = false;
            i.content = undo_secs_val.to_string();
            i.placeholder("")
        });
        let view = Self {
            core: core.clone(),
            focus: cx.focus_handle(),
            relays: relay_rows(core.current_relays()),
            add_relay: cx.new(|cx| single_line("wss://…", cx)),
            relay_read: true,
            relay_write: true,
            servers: vec![],
            add_server: cx.new(|cx| single_line("https://…", cx)),
            dark: p.dark,
            undo_enabled: p.undo_enabled.unwrap_or(true),
            undo_secs_val,
            undo_secs,
            machine_hidden: satori_core::store::machine_notes_hidden(),
            hidden_kinds: prefs::content_hidden(),
            reaction_notifs: prefs::get_bool("reactionNotifs"),
            bookmark_public: prefs::get_bool("bookmarkPublic"),
            wallet: core.wallet().status(),
            signing_out: false,
            me_name: None,
            me_npub: Self::me_npub_short(),
            status: String::new(),
            status_err: false,
        };
        view.pump_feed(core.fetch_media_servers(), cx);
        view.pump_me(cx);
        view.pump_wallet(cx);
        // Relay health (D33/D36) refreshes on a slow poll: the latency
        // profiles update as queries run, and this pane is the only reader.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(30)).await;
                let _ = this.update(cx, |_this, cx| cx.notify());
            }
        })
        .detach();
        // The pane is a projection — bootstrap may still be landing the
        // NIP-65 (settings.rs catch-up poll: 30 × 800ms).
        cx.spawn(async move |this, cx| {
            for _ in 0..15 {
                cx.background_executor()
                    .timer(Duration::from_millis(800))
                    .await;
                let done = this
                    .update(cx, |this, cx| {
                        if this.relays.is_empty() {
                            this.relays = relay_rows(this.core.current_relays());
                            cx.notify();
                        }
                        !this.relays.is_empty()
                    })
                    .is_ok_and(|done| done);
                if done {
                    break;
                }
            }
        })
        .detach();
        view
    }

    // --- pumps --------------------------------------------------------------

    /// The wallet broadcast → the Wallet row's status (one line; the
    /// manage pane owns the detail).
    fn pump_wallet(&self, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let mut rx = self.core.subscribe_wallet();
        cx.spawn(async move |_, cx| {
            loop {
                match rx.recv().await {
                    Ok(wallet::WalletEvent::Status(s)) => {
                        let _ = this.update(cx, |this, cx| {
                            this.wallet = s;
                            cx.notify();
                        });
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(_) => continue,
                }
            }
        })
        .detach();
    }

    /// The signed-in profile's name for the Session row. The profiles
    /// service resolves a miss once, so retry like the rail avatar does —
    /// though this pane opens long after connect, so it usually lands
    /// first try.
    fn pump_me(&self, cx: &mut Context<Self>) {
        let Some(me) = satori_core::session::load()
            .and_then(|s| PublicKey::from_str(&s.pubkey).ok())
        else {
            return;
        };
        let me_hex = me.to_string();
        cx.spawn(async move |this, cx| {
            for _ in 0..5 {
                let mut rx = this
                    .update(cx, |this, _| {
                        this.core.clone().ensure_profiles(vec![me])
                    })
                    .expect("settings alive");
                // One event per ask (the debouncer resolves once); any
                // miss waits out the 2s and retries — never a busy loop.
                if let Some(FeedEvent::Profiles(ps)) = rx.recv().await
                    && let Some(p) = ps.into_iter().find(|p| p.pubkey == me_hex)
                {
                    let _ = this.update(cx, |this, cx| {
                        let name = p.display();
                        this.me_name =
                            (!name.is_empty() && name != me_hex).then_some(name);
                        cx.notify();
                    });
                    return;
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
        .detach();
    }

    fn pump_feed(&self, mut rx: satori_core::manager::FeedRx, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |this, cx| this.on_feed_event(ev, cx));
            }
        })
        .detach();
    }

    fn on_feed_event(&mut self, ev: FeedEvent, cx: &mut Context<Self>) {
        match ev {
            FeedEvent::MediaServers(servers) => {
                for url in servers {
                    if !self.servers.contains(&url) {
                        self.servers.push(url);
                    }
                }
            }
            // §4.3: kinded copy, not a raw "… {e}" dump — the relays didn't
            // answer. Settings is a form pane, so the strip rides the pane's
            // existing err-styled status line (set_status(_, true)).
            FeedEvent::Failed(e) => {
                self.set_status(format!("The relays didn't answer — {e}"), true)
            }
            _ => {}
        }
        cx.notify();
    }

    fn pump_publish(
        &self,
        mut rx: satori_core::manager::PublishRx,
        done: &'static str,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let stop = this
                    .update(cx, |this, _cx| match ev {
                        PublishEvent::Committed { .. } => {
                            this.set_status(done.to_string(), false);
                            true
                        }
                        PublishEvent::Failed { error, .. } => {
                            this.set_status(format!("Publish failed: {error}"), true);
                            true
                        }
                        _ => false,
                    })
                    .is_ok_and(|stop| stop);
                if stop {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// The signed-in identity, short npub form for the Session row. Called
    /// ONCE (at construction): it reads session.json from disk and bech32s
    /// the key — not per-render work.
    fn me_npub_short() -> String {
        satori_core::session::load()
            .and_then(|s| PublicKey::from_str(&s.pubkey).ok())
            .and_then(|pk| {
                pk.to_bech32()
                    .ok()
                    .map(|full| format!("{}…", &full[..12]))
            })
            .unwrap_or_else(|| "unknown key".to_string())
    }

    fn set_status(&mut self, msg: String, err: bool) {
        self.status = msg;
        self.status_err = err;
    }

    // --- actions ------------------------------------------------------------

    fn add_relay(&mut self, cx: &mut Context<Self>) {
        let url = self.add_relay.read(cx).content.trim().to_string();
        if url.is_empty() {
            return;
        }
        if !url.starts_with("wss://") && !url.starts_with("ws://") {
            self.set_status("Relay URLs start with wss://".into(), true);
            cx.notify();
            return;
        }
        if self.relays.iter().any(|r| r.url == url) {
            return;
        }
        self.relays.push(RelayEntry {
            url,
            read: self.relay_read,
            write: self.relay_write,
        });
        self.add_relay.update(cx, Input::clear);
        cx.notify();
    }

    fn add_server(&mut self, cx: &mut Context<Self>) {
        // normalizeMediaInput: trim, assume https, strip trailing slashes,
        // reject junk, drop duplicates.
        let raw = self.add_server.read(cx).content.trim().to_string();
        if raw.is_empty() {
            return;
        }
        let mut url = raw;
        if !url.starts_with("http://") && !url.starts_with("https://") {
            url = format!("https://{url}");
        }
        while url.ends_with('/') {
            url.pop();
        }
        let host = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .unwrap_or("");
        if host.is_empty() || !host.contains('.') || host.contains(' ') {
            self.set_status("That doesn't look like a URL".into(), true);
            cx.notify();
            return;
        }
        if self.servers.contains(&url) {
            return;
        }
        self.servers.push(url);
        self.add_server.update(cx, Input::clear);
        cx.notify();
    }

    fn remove_server(&mut self, url: &str, cx: &mut Context<Self>) {
        self.servers.retain(|s| s != url);
        cx.notify();
    }

    fn remove_relay(&mut self, url: &str, cx: &mut Context<Self>) {
        self.relays.retain(|r| r.url != url);
        cx.notify();
    }

    fn toggle_relay(&mut self, url: &str, read: bool, cx: &mut Context<Self>) {
        if let Some(entry) = self.relays.iter_mut().find(|r| r.url == url) {
            if read {
                entry.read = !entry.read;
            } else {
                entry.write = !entry.write;
            }
        }
        cx.notify();
    }

    fn publish_relays(&mut self, cx: &mut Context<Self>) {
        let read: Vec<String> = self
            .relays
            .iter()
            .filter(|r| r.read)
            .map(|r| r.url.clone())
            .collect();
        let write: Vec<String> = self
            .relays
            .iter()
            .filter(|r| r.write)
            .map(|r| r.url.clone())
            .collect();
        if read.is_empty() && write.is_empty() {
            self.set_status("Nothing to publish — add a relay first".into(), true);
            cx.notify();
            return;
        }
        let rx = self.core.set_relays(read, write);
        self.pump_publish(rx, "Relay list published", cx);
    }

    fn publish_servers(&mut self, cx: &mut Context<Self>) {
        let servers = self.servers.clone();
        if servers.is_empty() {
            self.set_status("Nothing to publish — add a media server first".into(), true);
            cx.notify();
            return;
        }
        let rx = self.core.set_media_servers(servers);
        self.pump_publish(rx, "Media server list published", cx);
    }

    fn set_undo(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.undo_enabled = enabled;
        let mut p = prefs::load();
        p.undo_enabled = Some(enabled);
        p.undo_seconds = Some(self.undo_secs_val);
        prefs::save(&p);
        self.core.set_undo(enabled, self.undo_secs_val);
        cx.notify();
    }

    fn apply_undo_secs(&mut self, cx: &mut Context<Self>) {
        let raw = self.undo_secs.read(cx).content.trim().to_string();
        match raw.parse::<u32>() {
            Ok(secs) if (1..=30).contains(&secs) => {
                self.undo_secs_val = secs;
                self.set_undo(self.undo_enabled, cx);
            }
            _ => {
                self.set_status("Hold is 1–30 seconds".into(), true);
                cx.notify();
            }
        }
    }

    fn toggle_machine_notes(&mut self, cx: &mut Context<Self>) {
        self.machine_hidden = !self.machine_hidden;
        prefs::set_bool("machine_notes_hidden", self.machine_hidden);
        // NIP-78: sync (Phase 2 §4.1).
        self.core
            .publish_app_data("machine_notes_hidden", self.machine_hidden.into());
        cx.notify();
    }

    fn toggle_reaction_notifs(&mut self, cx: &mut Context<Self>) {
        self.reaction_notifs = !self.reaction_notifs;
        prefs::set_bool("reactionNotifs", self.reaction_notifs);
        // NIP-78: sync (Phase 2 §4.1).
        self.core
            .publish_app_data("reactionNotifs", self.reaction_notifs.into());
        cx.notify();
    }

    /// Bookmarks scope (D38): local-only for now — the star reads the file
    /// per click, so a flip applies to the next star with no sync hop.
    fn toggle_bookmark_public(&mut self, cx: &mut Context<Self>) {
        self.bookmark_public = !self.bookmark_public;
        prefs::set_bool("bookmarkPublic", self.bookmark_public);
        cx.notify();
    }

    fn toggle_kind(&mut self, kinds: &[u16], cx: &mut Context<Self>) {
        // "On" means no kind of the group is hidden; toggling flips that.
        let hidden_now = kinds.iter().any(|k| self.hidden_kinds.contains(k));
        prefs::set_kinds_hidden(kinds, !hidden_now);
        self.hidden_kinds = prefs::content_hidden();
        // NIP-78: sync (Phase 2 §4.1).
        self.core
            .publish_app_data("content_hidden", serde_json::json!(self.hidden_kinds));
        cx.notify();
    }

    // --- Backup: the signed list events, out and back -------------------------

    fn export_backup(&mut self, cx: &mut Context<Self>) {
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let mut rx = core.export_lists();
            if let Some(FeedEvent::BackupJson(json)) = rx.recv().await {
                // Blocking portal dialog on the background executor
                // (composer.rs attachments pattern).
                let path = cx
                    .background_executor()
                    .spawn(async move {
                        rfd::FileDialog::new()
                            .set_file_name("satori-backup.json")
                            .save_file()
                    })
                    .await;
                let _ = this.update(cx, |this, _cx| {
                    if let Some(path) = path {
                        match std::fs::write(&path, json.as_bytes()) {
                            Ok(()) => this.set_status("Backup saved".into(), false),
                            Err(_) => this.set_status("Could not write backup".into(), true),
                        }
                    } // else: cancelled
                });
            }
        })
        .detach();
        cx.notify();
    }

    fn import_backup(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let picked = cx
                .background_executor()
                .spawn(async move {
                    rfd::FileDialog::new()
                        .add_filter("JSON", &["json"])
                        .pick_file()
                })
                .await;
            let Some(path) = picked else {
                return; // cancelled
            };
            let json = std::fs::read_to_string(&path).ok();
            let Some(json) = json else {
                let _ = this.update(cx, |this, _cx| {
                    this.set_status("Could not read file".into(), true)
                });
                return;
            };
            let mut rx = match this.update(cx, |this, _| this.core.import_lists(json)) {
                Ok(rx) => rx,
                Err(_) => return,
            };
            while let Some(ev) = rx.recv().await {
                let stop = this
                    .update(cx, |this, _cx| match ev {
                        PublishEvent::Restored(n) => {
                            this.set_status(format!("Restored {n} list(s)"), false);
                            true
                        }
                        PublishEvent::Failed { error, .. } => {
                            this.set_status(format!("Import failed: {error}"), true);
                            true
                        }
                        _ => false,
                    })
                    .is_ok_and(|stop| stop);
                if stop {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }
}

fn relay_rows((read, write): (Vec<String>, Vec<String>)) -> Vec<RelayEntry> {
    let mut rows: Vec<RelayEntry> = read
        .iter()
        .map(|url| RelayEntry {
            url: url.clone(),
            read: true,
            write: write.contains(url),
        })
        .collect();
    for url in write {
        if !rows.iter().any(|r| r.url == url) {
            rows.push(RelayEntry {
                url,
                read: false,
                write: true,
            });
        }
    }
    rows
}

fn single_line(placeholder: &str, cx: &mut Context<Input>) -> Input {
    let mut i = Input::new(cx);
    i.multiline = false;
    i.placeholder(placeholder)
}

/// The quiet pill switch (sumi-e: a dot moving between two states).
fn toggle(id: impl Into<SharedString>, on: bool) -> Stateful<Div> {
    div()
        .id(id.into())
        .cursor_pointer()
        .w(px(30.))
        .h(px(16.))
        .rounded_full()
        .border_1()
        .border_color(rgb(if on { theme::vermilion() } else { theme::hairline() }))
        .bg(rgb(theme::washi()))
        .flex()
        .items_center()
        .when(on, |d| d.justify_end())
        .when(!on, |d| d.justify_start())
        .p(px(2.))
        .child(
            div()
                .size(px(10.))
                .rounded_full()
                .bg(rgb(if on { theme::vermilion() } else { theme::hairline() })),
        )
}

/// A settings group: title + description + rows chained as children.
fn group(title: &str, description: &str) -> Div {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .p_4()
        .pt_3()
        // The group's inner separator: hairline at FULL strength — this is
        // a rule inside the pane's composed surface, not a panel border
        // (theme::hairline_strong, 0.2 spec §1.1's two-stroke discipline).
        .border_b_1()
        .border_color(rgb(theme::hairline_strong()))
        .child(
            div()
                .text_size(px(theme::T_SMALL))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(theme::muted()))
                .child(title.to_string()),
        )
        .when(!description.is_empty(), |d| {
            d.child(
                div()
                    .text_size(px(theme::T_SMALL))
                    .text_color(rgb(theme::muted()))
                    .child(description.to_string()),
            )
        })
}

fn field(input: Entity<Input>, id: &str) -> impl IntoElement {
    let focus_target = input.clone();
    div()
        .id(SharedString::from(id.to_string()))
        .cursor_text()
        .flex_1()
        .min_w_0()
        .h(px(30.))
        .px_2()
        .rounded(px(theme::R_INPUT))
        .bg(rgb(theme::panel()))
        .border_1()
        .border_color(rgb(theme::hairline()))
        .child(input)
        // Click-to-focus, explicitly: the Input's own hitbox didn't win the
        // focus transfer in this pane (login/search autofocus instead).
        .on_click(move |_, window, cx| {
            let fh = focus_target.read(cx).focus_handle(cx);
            window.focus(&fh, cx);
        })
}

fn seal(id: impl Into<SharedString>, label: &str, color: u32) -> Stateful<Div> {
    div()
        .id(id.into())
        .cursor_pointer()
        .px_2()
        .py_1()
        .text_size(px(theme::T_UI))
        .text_color(rgb(color))
        .child(label.to_string())
}

fn muted(text: impl Into<String>) -> Div {
    div()
        .text_size(px(theme::T_SMALL))
        .text_color(rgb(theme::muted()))
        .child(text.into())
}

/// The relay-health group (WS4): surface what core already LEARNS about
/// each relay (D33 latency EWMA + read-dead, D36 write-dead) as a passive
/// pane — no probing, no edits; everyday queries did the measuring. All
/// configured relays read-dead renders §4.4's degraded pill (a state, not
/// an error — D26's split).
fn relay_health_group(relays: &[RelayEntry]) -> Div {
    use satori_core::relay_latency::RelayHealth;
    let health: Vec<RelayHealth> = satori_core::manager::Core::relay_health();
    let lookup =
        |url: &str| health.iter().find(|h| h.url == url).cloned();
    // Configured relays first, in list order; profiled-but-dropped ones
    // (the LRU still remembers them) append, alphabetically for stable order.
    let mut extras: Vec<RelayHealth> = health
        .iter()
        .filter(|h| !relays.iter().any(|r| r.url == h.url))
        .cloned()
        .collect();
    extras.sort_by(|a, b| a.url.cmp(&b.url));

    let degraded = !relays.is_empty()
        && relays
            .iter()
            .all(|r| lookup(&r.url).is_some_and(|h| h.read_dead));

    let stat_line = |h: &RelayHealth| {
        if h.n == 0 {
            "no samples yet".to_string()
        } else {
            format!("{} ms · {} queries", h.ms, h.n)
        }
    };

    // Configured relays first, in list order; profiled-but-dropped ones
    // (the LRU still remembers them) append, alphabetically for stability.
    let mut rows: Vec<(String, Option<RelayHealth>)> = relays
        .iter()
        .map(|r| (r.url.clone(), lookup(&r.url)))
        .collect();
    rows.extend(extras.into_iter().map(|h| (h.url.clone(), Some(h))));

    let rows_el: Vec<AnyElement> = rows
        .into_iter()
        .map(|(url, h)| {
            let mut row = div()
                .w_full()
                .flex()
                .items_center()
                .gap_2()
                .py_0p5()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(theme::T_UI))
                        .text_color(rgb(theme::ink()))
                        .child(url),
                );
            if let Some(h) = h {
                if h.read_dead {
                    row = row.child(
                        div()
                            .text_size(px(theme::T_SMALL))
                            .text_color(rgb(theme::err()))
                            .child("read-dead"),
                    );
                }
                if h.write_dead {
                    row = row.child(
                        div()
                            .text_size(px(theme::T_SMALL))
                            .text_color(rgb(theme::warn()))
                            .child("write-dead"),
                    );
                }
                row = row.child(
                    div()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .child(stat_line(&h)),
                );
            }
            row.into_any_element()
        })
        .collect();

    group(
        "Relay health",
        "What everyday queries have learned about each relay — response \
         times, and relays that stopped answering reads or writes.",
    )
    .when(degraded, |d| {
        d.child(
            div().py_1().child(states::offline_pill(
                "the relays didn't answer — reading is degraded",
            )),
        )
    })
    .child(
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_0p5()
            .children(rows_el),
    )
}

impl EventEmitter<Nav> for SettingsView {}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let relays = self.relays.clone();
        let servers = self.servers.clone();
        let machine_hidden = self.machine_hidden;
        let hidden_kinds = self.hidden_kinds.clone();
        let undo_enabled = self.undo_enabled;
        let dark = self.dark.unwrap_or(true); // None = night (today's default)
        let relay_read = self.relay_read;
        let relay_write = self.relay_write;
        let status = self.status.clone();
        let status_err = self.status_err;
        let add_relay = self.add_relay.clone();
        let add_server = self.add_server.clone();
        let undo_secs = self.undo_secs.clone();

        div()
            .id("settings-root")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                // Enter acts on whichever row is focused (single-line Inputs
                // don't bind enter, so it bubbles here).
                if ev.keystroke.key == "enter" {
                    if this.add_relay.read(cx).focus_handle(cx).is_focused(window) {
                        this.add_relay(cx);
                    } else if this.add_server.read(cx).focus_handle(cx).is_focused(window) {
                        this.add_server(cx);
                    } else if this.undo_secs.read(cx).focus_handle(cx).is_focused(window) {
                        this.apply_undo_secs(cx);
                    }
                }
            }))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            // --- Appearance (light / dark) ----------------------------------
            .child(
                group(
                    "Appearance",
                    "Ink on paper — the day palette or the night one. Applies at once.",
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .py_1()
                        .child(
                            div()
                                .id("theme-light")
                                .cursor_pointer()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(if !dark { theme::ink() } else { theme::muted() }))
                                .when(!dark, |d| d.font_weight(gpui::FontWeight::SEMIBOLD))
                                .on_click(cx.listener(|_this, _, _, cx| {
                                    cx.emit(Nav::SetTheme(false));
                                }))
                                .child("Light"),
                        )
                        .child(
                            div()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(theme::hairline()))
                                .child("·"),
                        )
                        .child(
                            div()
                                .id("theme-dark")
                                .cursor_pointer()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(if dark { theme::ink() } else { theme::muted() }))
                                .when(dark, |d| d.font_weight(gpui::FontWeight::SEMIBOLD))
                                .on_click(cx.listener(|_this, _, _, cx| {
                                    cx.emit(Nav::SetTheme(true));
                                }))
                                .child("Dark"),
                        ),
                ),
            )
            // --- Session (identity + sign out) ------------------------------
            .child(
                group(
                    "Session",
                    "The key stays in this device's secret store until you sign out.",
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(theme::ink()))
                                .child(match &self.me_name {
                                    Some(name) => format!("Signed in as {name}"),
                                    None => format!("Signed in as {}", self.me_npub),
                                }),
                        )
                        .child(
                            div()
                                .id("sign-out")
                                .cursor_pointer()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(theme::vermilion()))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    cx.emit(Nav::SignOut);
                                    this.signing_out = true;
                                    cx.notify();
                                }))
                                .when(
                                    self.signing_out,
                                    |d| d.text_color(rgb(theme::muted())).child("Signing out…"),
                                )
                                .when(!self.signing_out, |d| d.child("Sign out")),
                        ),
                ),
            )
            // --- Wallet (NWC zaps, D20) --------------------------------------
            .child(
                group(
                    "Wallet",
                    "Connect a lightning wallet (NIP-47) and the bolt on every note zaps.",
                )
                .child(
                    div()
                        .id("wallet-row")
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(theme::ink()))
                                .child(match &self.wallet {
                                    Status::Disconnected => "No wallet connected".to_string(),
                                    Status::Connecting => "Reaching the wallet…".to_string(),
                                    Status::Ready(info) => {
                                        let mut line = if info.alias.is_empty() {
                                            "Wallet connected".to_string()
                                        } else {
                                            format!("Wallet connected — {}", info.alias)
                                        };
                                        if let Some(msat) = info.balance_msat {
                                            line.push_str(&format!(" · {} sats", msat / 1000));
                                        }
                                        line
                                    }
                                }),
                        )
                        .child(
                            div()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(theme::vermilion()))
                                .child("Manage"),
                        )
                        .on_click(cx.listener(|_this, _, _, cx| {
                            cx.emit(Nav::OpenWallet);
                        })),
                ),
            )
            // --- Composing (the undo window) --------------------------------
            .child(
                group(
                    "Composing",
                    "The undo window is the breath before speech: posts are held before they go out.",
                )
                .child(
                    div()
                        .id("undo-row")
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(theme::ink()))
                                .child("Undo window"),
                        )
                        .child(toggle("undo-toggle", undo_enabled))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_undo(!this.undo_enabled, cx);
                        })),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .when(undo_enabled, |d| {
                            d.child(field(undo_secs, "undo-secs-field"))
                                .child(muted("seconds (1–30, Enter applies)"))
                        })
                        .when(!undo_enabled, |d| {
                            d.child(
                                div()
                                    .text_size(px(theme::T_UI))
                                    .text_color(rgb(theme::muted()))
                                    .child("Posts publish immediately."),
                            )
                        }),
                ),
            )
            // --- Content (the feed catalog) ---------------------------------
            .child(
                group(
                    "Feed content",
                    "What kinds your feed carries. Applies on the next home refresh.",
                )
                .child(
                    div()
                        .id("machine-notes-row")
                        .w_full()
                        .flex()
                        .items_start()
                        .gap_2()
                        .py_1()
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_size(px(theme::T_UI))
                                        .text_color(rgb(theme::ink()))
                                        .child("Hide machine notes"),
                                )
                                .child(muted(
                                    "Kind-1 events whose entire content is JSON — app data \
                                     transports, not speech",
                                )),
                        )
                        .child(toggle("machine-notes", machine_hidden))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_machine_notes(cx))),
                )
                .children(satori_core::store::CATALOG.iter().map(|(label, kinds)| {
                    let kinds: Vec<u16> = kinds.to_vec();
                    let on = !kinds.iter().any(|k| hidden_kinds.contains(k));
                    div()
                        .id(SharedString::from(format!("kind-row-{label}")))
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(theme::ink()))
                                .child(label.to_string()),
                        )
                        .child(toggle(format!("kind-{label}"), on))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_kind(&kinds, cx);
                        }))
                })),
            )
            // --- Notifications (the bell's content) --------------------------
            .child(
                group(
                    "Notifications",
                    "What the bell counts. Applies the next time the stream opens.",
                )
                .child(
                    div()
                        .id("reaction-notifs-row")
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(theme::ink()))
                                .child("Show reactions in notifications"),
                        )
                        .child(toggle("reaction-notifs", self.reaction_notifs))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_reaction_notifs(cx);
                        })),
                ),
            )
            // --- Bookmarks (D38) ---------------------------------------------
            .child(
                group(
                    "Bookmarks",
                    "Private bookmarks are nip44-encrypted (kind 10003). Public ones \
                     are a plain NIP-51 list (kind 30001) anyone can read; both show \
                     in the Library.",
                )
                .child(
                    div()
                        .id("bookmark-public-row")
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(theme::ink()))
                                .child("Stars write public bookmarks"),
                        )
                        .child(toggle("bookmark-public", self.bookmark_public))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_bookmark_public(cx);
                        })),
                ),
            )
            // --- Media servers (Blossom, kind 10063) -------------------------
            .child(
                group(
                    "Media servers",
                    "Where your pictures upload (Blossom). With no list, uploads use \
                     blossom.primal.net.",
                )
                .child(div().w_full().flex().flex_col().gap_0p5().children(
                    servers.iter().map(|url| {
                        let url_for_x = url.clone();
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .py_0p5()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(theme::T_UI))
                                    .text_color(rgb(theme::ink()))
                                    .child(url.clone()),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("media-remove-{url}")))
                                    .cursor_pointer()
                                    .px_1()
                                    .text_size(px(theme::T_UI))
                                    .text_color(rgb(theme::muted()))
                                    .child("×")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove_server(&url_for_x, cx);
                                    })),
                            )
                    }),
                ))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .pt_1()
                        .child(field(add_server, "add-server-field"))
                        .child(
                            seal("add-server", "Add", theme::vermilion())
                                .on_click(cx.listener(|this, _, _, cx| this.add_server(cx))),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .justify_end()
                        .pt_1()
                        .child(
                            seal("publish-media", "Publish media server list", theme::vermilion())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.publish_servers(cx);
                                })),
                        ),
                ),
            )
            // --- Relays (NIP-65, kind 10002) ---------------------------------
            .child(
                group(
                    "Relays",
                    "Where your events go and come from — published as your NIP-65 list. \
                     Five indexers always serve reads.",
                )
                .child(div().w_full().flex().flex_col().gap_0p5().children(
                    relays.iter().map(|relay| {
                        let url_t = relay.url.clone();
                        let url_w = relay.url.clone();
                        let url_for_x = relay.url.clone();
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .py_0p5()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(theme::T_UI))
                                    .text_color(rgb(theme::ink()))
                                    .child(relay.url.clone()),
                            )
                            .child(
                                toggle(SharedString::from(format!("relay-r-{}", relay.url)), relay.read)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_relay(&url_t, true, cx);
                                    })),
                            )
                            .child(
                                toggle(SharedString::from(format!("relay-w-{}", relay.url)), relay.write)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_relay(&url_w, false, cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("relay-remove-{}", relay.url)))
                                    .cursor_pointer()
                                    .px_1()
                                    .text_size(px(theme::T_UI))
                                    .text_color(rgb(theme::muted()))
                                    .child("×")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove_relay(&url_for_x, cx);
                                    })),
                            )
                    }),
                ))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .pt_1()
                        .child(field(add_relay, "add-relay-field"))
                        .child(
                            toggle("relay-add-read", relay_read)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.relay_read = !this.relay_read;
                                    cx.notify();
                                })),
                        )
                        .child(
                            toggle("relay-add-write", relay_write)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.relay_write = !this.relay_write;
                                    cx.notify();
                                })),
                        )
                        .child(
                            seal("add-relay", "Add", theme::vermilion())
                                .on_click(cx.listener(|this, _, _, cx| this.add_relay(cx))),
                        ),
                )
                .child(muted("the two toggles set read / write for the relay you add"))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .justify_end()
                        .pt_1()
                        .child(
                            seal("publish-relays", "Publish relay list", theme::vermilion())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.publish_relays(cx);
                                })),
                        ),
                ),
            )
            // --- Relay health (D33/D36 learned state, WS1's last item) ------
            .child(relay_health_group(&relays))
            // --- Backup: the signed list events ------------------------------
            .child(
                group(
                    "List backup",
                    "Your follows, mutes, bookmarks and relay lists as signed events.                      Restoring re-publishes them fresh.",
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(theme::ink()))
                                .child("Export"),
                        )
                        .child(
                            seal("export-backup", "Save…", theme::muted())
                                .on_click(cx.listener(|this, _, _, cx| this.export_backup(cx))),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(theme::ink()))
                                .child("Import & restore"),
                        )
                        .child(
                            seal("import-backup", "Choose file…", theme::muted())
                                .on_click(cx.listener(|this, _, _, cx| this.import_backup(cx))),
                        ),
                ),
            )
            // --- status ------------------------------------------------------
            .when(!status.is_empty(), |d| {
                d.child(
                    div()
                        .p_4()
                        .pt_2()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(if status_err {
                            theme::err()
                        } else {
                            theme::muted()
                        }))
                        .child(status),
                )
            })
    }
}
