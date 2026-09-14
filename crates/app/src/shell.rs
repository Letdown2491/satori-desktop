//! Shell chrome — port of `src/render/layout.ts` structure: the bar (home + bell
//! left, feed switcher centered, search + account right), the 720px reading
//! column, and the caught-up ensō clearing. Real views land in M0/M2.

use gtk::gio;
use gtk::prelude::*;

pub fn build_shell() -> gtk::Widget {
    let header = gtk::HeaderBar::new();

    let home = gtk::Button::builder()
        .icon_name("home-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Home")
        .build();
    let bell = gtk::Button::builder()
        .icon_name("notification-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Notifications")
        .build();
    header.pack_start(&home);
    header.pack_start(&bell);

    // Feed switcher (center): Following / timelines / Browse a relay — a real
    // popover menu arrives with the feed views (M0/M2).
    let switcher = gtk::MenuButton::builder()
        .label("Following")
        .css_classes(["flat", "switcher"])
        .build();
    header.set_title_widget(Some(&switcher));

    let search_btn = gtk::Button::builder()
        .icon_name("edit-find-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Search")
        .build();
    let avatar = adw::Avatar::builder().size(28).show_initials(true).build();
    let avatar_btn = gtk::MenuButton::builder()
        .child(&avatar)
        .css_classes(["flat"])
        .build();
    header.pack_end(&search_btn);
    header.pack_end(&avatar_btn);

    // Reading column: 720px centered (styles.css:100).
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    for _ in 0..4 {
        column.append(&placeholder_note());
    }
    column.append(&caught_up_clearing());

    let clamp = adw::Clamp::builder().maximum_size(720).child(&column).build();
    let scroll = gtk::ScrolledWindow::builder()
        .child(&clamp)
        .hexpand(true)
        .vexpand(true)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scroll));
    toolbar.upcast()
}

fn placeholder_note() -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .css_classes(["note-card"])
        .build();
    let head = gtk::Label::builder()
        .label("Satori")
        .halign(gtk::Align::Start)
        .css_classes(["note-author", "serif"])
        .build();
    let body = gtk::Label::builder()
        .label("A quiet client. Ink on washi; the feed ends when you are caught up.")
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .build();
    card.append(&head);
    card.append(&body);
    card
}

/// The design-for-exit moment: the feed ends, ensō + a line, no refill.
fn caught_up_clearing() -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(48)
        .margin_bottom(64)
        .css_classes(["caught-up"])
        .build();
    let enso = gtk::Picture::for_file(&gio::File::for_path(enso_path()));
    enso.set_size_request(120, 120);
    let words = gtk::Label::builder()
        .label("Caught up.")
        .css_classes(["serif", "empty-quote"])
        .build();
    row.append(&enso);
    row.append(&words);
    row
}

fn enso_path() -> std::path::PathBuf {
    // Assets ship as GResources in packaging; dev-time path for now.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/enso.svg")
}
