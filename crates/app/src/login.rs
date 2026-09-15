//! Login view — port of `src/routes/login.ts` (the /login page), minus NIP-07.
//!
//! Two doors, matching the product's key posture (DESIGN.md D2): bunker
//! (default — the key never touches us) and nsec (explicit opt-in, sealed in
//! the secret store). The waiting state breathes the ensō at the web app's
//! 3.4s period instead of a spinner.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;
use nostr::prelude::*;
use satori_core::manager::{ConnectEvent, ConnectRx, Core};


#[derive(Clone)]
pub struct LoginPage {
    root: gtk::Box,
    pages: gtk::Stack,
    error: gtk::Label,
    bunker_entry: gtk::Entry,
    nsec_entry: gtk::PasswordEntry,
    enso: gtk::Image,
    waiting_status: gtk::Label,
    cancelled: Rc<Cell<bool>>,
    anim: Rc<RefCell<Option<glib::SourceId>>>,
    tick: Rc<Cell<f64>>,
}

impl LoginPage {
    pub fn new(core: Arc<Core>, on_success: impl Fn(PublicKey) + Clone + 'static) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .valign(gtk::Align::Center)
            .halign(gtk::Align::Center)
            .margin_start(24)
            .margin_end(24)
            .build();

        let wordmark = gtk::Label::builder()
            .label("Satori")
            .css_classes(["wordmark", "serif"])
            .build();
        let tagline = gtk::Label::builder()
            .label("A quiet Nostr client. Your key never has to touch it.")
            .css_classes(["tagline"])
            .build();

        let error = gtk::Label::builder()
            .css_classes(["error-text"])
            .wrap(true)
            .visible(false)
            .build();

        let bunker_entry = gtk::Entry::builder()
            .placeholder_text("bunker://…")
            .width_chars(36)
            .hexpand(true)
            .build();
        let bunker_btn = gtk::Button::builder().label("Connect").css_classes(["seal-btn"]).build();
        let bunker_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        bunker_row.append(&bunker_entry);
        bunker_row.append(&bunker_btn);
        let bunker_hint = gtk::Label::builder()
            .label("Sign in with a remote signer (NIP-46). The app holds only a transport secret.")
            .css_classes(["tagline"])
            .wrap(true)
            .build();

        let nsec_entry = gtk::PasswordEntry::builder()
            .placeholder_text("nsec1…")
            .show_peek_icon(true)
            .width_chars(36)
            .hexpand(true)
            .build();
        let nsec_btn = gtk::Button::builder()
            .label("Sign in with nsec")
            .css_classes(["flat"])
            .build();
        let nsec_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        nsec_row.append(&nsec_entry);
        nsec_row.append(&nsec_btn);
        let nsec_hint = gtk::Label::builder()
            .label("Uses a local key, stored in your Secret Service. Only do this if you understand the trade-off.")
            .css_classes(["tagline"])
            .wrap(true)
            .build();

        let form = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .build();
        form.append(&bunker_row);
        form.append(&bunker_hint);
        form.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        form.append(&nsec_row);
        form.append(&nsec_hint);

        // Waiting page: breathing ensō + status + cancel.
        let enso = crate::icons::enso(120);
        let waiting_status = gtk::Label::builder()
            .label("Waiting for approval…")
            .css_classes(["serif"])
            .build();
        let waiting_hint = gtk::Label::builder()
            .label("Approve the request in your bunker app.")
            .css_classes(["tagline"])
            .build();
        let cancel_btn = gtk::Button::builder().label("Cancel").css_classes(["flat"]).build();
        let waiting = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .valign(gtk::Align::Center)
            .halign(gtk::Align::Center)
            .build();
        waiting.append(&enso);
        waiting.append(&waiting_status);
        waiting.append(&waiting_hint);
        waiting.append(&cancel_btn);

        let pages = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        pages.add_named(&form, Some("form"));
        pages.add_named(&waiting, Some("waiting"));

        root.append(&wordmark);
        root.append(&tagline);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        root.append(&error);
        root.append(&pages);

        let page = Self {
            root,
            pages,
            error,
            bunker_entry,
            nsec_entry,
            enso,
            waiting_status,
            cancelled: Rc::new(Cell::new(false)),
            anim: Rc::new(RefCell::new(None)),
            tick: Rc::new(Cell::new(0.0)),
        };

        // --- wire actions ------------------------------------------------------
        let this = page.clone();
        let core_b = Arc::clone(&core);
        let ok = on_success.clone();
        bunker_btn.connect_clicked(move |_| {
            let uri = this.bunker_entry.text().to_string();
            if uri.trim().is_empty() {
                return;
            }
            this.begin_connect(&core_b, uri, ok.clone());
        });

        let this = page.clone();
        let core_b = Arc::clone(&core);
        let ok = on_success.clone();
        page.bunker_entry.connect_activate(move |_| {
            let uri = this.bunker_entry.text().to_string();
            if uri.trim().is_empty() {
                return;
            }
            this.begin_connect(&core_b, uri, ok.clone());
        });

        let this = page.clone();
        let core_b = Arc::clone(&core);
        let ok = on_success.clone();
        nsec_btn.connect_clicked(move |_| {
            let secret = this.nsec_entry.text().to_string();
            if secret.trim().is_empty() {
                return;
            }
            this.begin_nsec(&core_b, secret, ok.clone());
        });

        let this = page.clone();
        let core_c = Arc::clone(&core);
        cancel_btn.connect_clicked(move |_| {
            this.cancelled.set(true);
            core_c.cancel_login();
            this.show_form();
        });

        page
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn reset(&self) {
        self.show_form();
        self.error.set_visible(false);
        self.bunker_entry.set_text("");
        self.nsec_entry.set_text("");
    }

    /// A resume failure is surfaced here — the user must know why they're
    /// back on the login page.
    pub fn set_resume_error(&self, msg: &str) {
        self.error.set_text(msg);
        self.error.set_visible(true);
    }

    fn show_form(&self) {
        self.stop_anim();
        self.pages.set_visible_child_full("form", gtk::StackTransitionType::Crossfade);
    }

    fn show_waiting(&self, status: &str) {
        self.waiting_status.set_text(status);
        self.pages.set_visible_child_full("waiting", gtk::StackTransitionType::Crossfade);
        self.start_anim();
    }

    fn begin_connect(&self, core: &Arc<Core>, uri: String, on_success: impl Fn(PublicKey) + 'static) {
        self.cancelled.set(false);
        self.show_waiting("Waiting for approval…");
        let rx = core.connect_bunker(uri);
        self.drive(rx, core.clone(), on_success);
    }

    fn begin_nsec(&self, core: &Arc<Core>, secret: String, on_success: impl Fn(PublicKey) + 'static) {
        self.cancelled.set(false);
        self.show_waiting("Unsealing your key…");
        let rx = core.login_nsec(secret);
        self.drive(rx, core.clone(), on_success);
    }

    /// Await core events on the GLib main context; the UI never blocks and the
    /// tokio runtime is never touched from here (DESIGN.md §3.2).
    fn drive(&self, mut rx: ConnectRx, core: Arc<Core>, on_success: impl Fn(PublicKey) + 'static) {
        let pages = self.pages.clone();
        let error = self.error.clone();
        let waiting_status = self.waiting_status.clone();
        let cancelled = Rc::clone(&self.cancelled);
        let anim = Rc::clone(&self.anim);

        glib::MainContext::default().spawn_local(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    ConnectEvent::Waiting => {}
                    ConnectEvent::AuthUrl(url) => {
                        // TODO(M1): surface as a clickable link chip; open in browser.
                        waiting_status.set_text("Open to approve:");
                        eprintln!("[login] auth_url: {url}");
                    }
                    ConnectEvent::Approved(pk) => {
                        if let Some(id) = anim.borrow_mut().take() {
                            id.remove();
                        }
                        on_success(pk);
                        return;
                    }
                    ConnectEvent::Failed(msg) => {
                        if let Some(id) = anim.borrow_mut().take() {
                            id.remove();
                        }
                        if !cancelled.get() {
                            error.set_text(&msg);
                            error.set_visible(true);
                        }
                        core.cancel_login();
                        pages.set_visible_child_full("form", gtk::StackTransitionType::Crossfade);
                        return;
                    }
                }
            }
        });
    }

    fn start_anim(&self) {
        if self.anim.borrow().is_some() {
            return;
        }
        // TODO(M1): honor gtk-enable-animations / reduced motion.
        let pic = self.enso.clone();
        let tick = Rc::clone(&self.tick);
        let id = glib::timeout_add_local(std::time::Duration::from_millis(60), move || {
            let t = tick.get() + 0.06;
            tick.set(t);
            // 0.4→1.0 opacity, 3.4s period (the web app's enso-breath).
            let phase = 0.5 + 0.5 * (t * 2.0 * std::f64::consts::PI / 3.4).sin();
            pic.set_opacity(0.4 + 0.6 * phase);
            glib::ControlFlow::Continue
        });
        *self.anim.borrow_mut() = Some(id);
    }

    fn stop_anim(&self) {
        if let Some(id) = self.anim.borrow_mut().take() {
            id.remove();
        }
    }
}
