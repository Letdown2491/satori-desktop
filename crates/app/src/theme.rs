//! Sumi-e theme loading: the design language of the web app (styles.css) as a
//! GTK CSS provider. Tokens are the palette contract in DESIGN.md §5.1.
//!
//! Light/dark follow the saved pref, else the desktop color scheme; switching
//! swaps the provider live (the web app needed H-Refresh for this — native
//! state needs nothing).

use std::cell::RefCell;

use gtk::gdk;

thread_local! {
    static PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
}

pub fn load_current() {
    let dark = crate::prefs::load().dark.unwrap_or_else(|| adw::StyleManager::default().is_dark());
    apply(dark);
}

/// Swap the theme live and persist the choice.
pub fn set_dark(dark: bool) {
    let mut p = crate::prefs::load();
    p.dark = Some(dark);
    crate::prefs::save(&p);
    apply(dark);
}

fn apply(dark: bool) {
    eprintln!("[theme] applying {}", if dark { "dark" } else { "light" });
    let css = if dark {
        include_str!("../theme/sumi-e-dark.css")
    } else {
        include_str!("../theme/sumi-e.css")
    };
    let display = gdk::Display::default().expect("no default display");
    let provider = gtk::CssProvider::new();
    provider.load_from_string(css);
    PROVIDER.with(|cell| {
        if let Some(old) = cell.borrow_mut().take() {
            gtk::style_context_remove_provider_for_display(&display, &old);
        }
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );
        *cell.borrow_mut() = Some(provider);
    });
}
