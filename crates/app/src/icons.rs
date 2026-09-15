//! Satori's own icon set — port of `src/render/svg.ts` (ui/icons.ts): 28
//! monochrome feather-style line icons at 1.9 stroke, shipped as symbolic
//! SVGs in a gresource so GTK recolors them from our CSS (`color:` on .act,
//! .engaged → vermilion) exactly like the web's currentColor. Plus the
//! procedural ensō generated at build time (build.rs).

use std::sync::atomic::{AtomicBool, Ordering};

use gtk::gdk;
use gtk::gio;
use gtk::glib;

const ICON_RESOURCE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/satori-icons.gresource"));
const ENSO_LIGHT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/enso-light.svg"));
const ENSO_DARK: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/enso-dark.svg"));

/// Ink for ensōs created after a theme switch.
static DARK: AtomicBool = AtomicBool::new(false);

/// Register the icon set with the default icon theme. Call once at startup.
pub fn install() {
    gio::resources_register(
        &gio::Resource::from_data(&glib::Bytes::from_static(ICON_RESOURCE))
            .expect("icon gresource"),
    );
    let theme = gtk::IconTheme::for_display(&gdk::Display::default().expect("no default display"));
    theme.add_resource_path("/io/github/letdown2491/satori/icons");
    if !theme.has_icon("satori-home-symbolic") {
        eprintln!("[icons] WARNING: icon theme registration failed — icons will be blank");
    }
}

/// A line icon at the default 18px (note actions).
pub fn icon(name: &str) -> gtk::Image {
    icon_sized(name, 18)
}

pub fn icon_sized(name: &str, size: i32) -> gtk::Image {
    gtk::Image::builder()
        .icon_name(format!("satori-{name}-symbolic"))
        .pixel_size(size)
        .build()
}

/// A quiet action button (flat icon glyph; color via CSS .act/.engaged).
pub fn action_button(name: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .child(&icon(name))
        .css_classes(["flat", "act"])
        .tooltip_text(tooltip)
        .build()
}

/// Filled variant (active like/bookmark states).
#[allow(dead_code)]
pub fn icon_filled(name: &str) -> gtk::Image {
    gtk::Image::builder()
        .icon_name(format!("satori-{name}-filled-symbolic"))
        .pixel_size(18)
        .build()
}

/// The brushstroke ensō at the requested size. Ink follows the theme at
/// creation time (widgets are transient; feeds recreate on reload).
pub fn enso(size: i32) -> gtk::Image {
    let dark = DARK.load(Ordering::Relaxed);
    let bytes = if dark { ENSO_DARK } else { ENSO_LIGHT };

    // GtkImage wants a file; write the SVG fresh (it carries the brush
    // filter + dry-brush mask). pixel_size renders at exactly `size` — a
    // Picture would allocate at the SVG's natural size.
    let dir = std::env::temp_dir().join("satori-assets");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(if dark { "enso-dark.svg" } else { "enso-light.svg" });
    let _ = std::fs::write(&path, bytes); // always current content
    let img = gtk::Image::builder().pixel_size(size).build();
    img.set_from_file(Some(&path));
    img
}

/// Record the theme for future ensō creation.
pub fn set_dark(dark: bool) {
    DARK.store(dark, Ordering::Relaxed);
}
