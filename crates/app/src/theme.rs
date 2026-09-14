//! Sumi-e theme loading: the design language of the web app (styles.css) as a GTK
//! CSS provider. Tokens are the palette contract in DESIGN.md §5.1.
//!
//! TODO(M1): live theme swap on StyleManager notify + user appearance pref
//! (currently chosen once at startup from the desktop color scheme).

use gtk::gdk;

pub fn load_current() {
    let dark = adw::StyleManager::default().is_dark();
    load(dark);
}

fn load(dark: bool) {
    let css = if dark {
        include_str!("../theme/sumi-e-dark.css")
    } else {
        include_str!("../theme/sumi-e.css")
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(css);
    let display = gdk::Display::default().expect("no default display");
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
}
