//! Desktop environment probes — the freedesktop portal's appearance
//! settings, the same signals the web app got free from media queries
//! (`prefers-color-scheme`, `prefers-reduced-motion`).
//!
//! One D-Bus round trip at startup (org.freedesktop.portal.Settings
//! ReadOne), cached into a gpui Global. Portal missing or failing (a
//! compositor without xdg-desktop-portal, a sandbox without the bus) →
//! None/false: night paper, motion allowed — today's behavior.

use gpui::{App, Global};

/// The probed desktop appearance, set once in main before the window.
pub struct Desktop {
    /// org.freedesktop.appearance color-scheme: Some(true) = prefer dark.
    pub dark: Option<bool>,
    /// org.freedesktop.appearance prefers-reduced-motion: 1 = reduce.
    pub reduced_motion: bool,
}

impl Global for Desktop {}

/// Mirror of `Desktop.reduced_motion` set at probe time — a cx-less read for
/// render helpers that have no App (video.rs's breathing glyphs; the same
/// pattern as prefs::privacy_mode). The probe runs once, before the window.
static REDUCED_MOTION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The portal's prefers-reduced-motion, without an App.
pub fn reduced_motion() -> bool {
    REDUCED_MOTION.load(std::sync::atomic::Ordering::Relaxed)
}

impl Desktop {
    pub fn get(cx: &App) -> &Desktop {
        cx.global::<Desktop>()
    }
}

fn read_one(name: &str) -> Option<u32> {
    use zbus::blocking::Proxy;

    let conn = zbus::blocking::Connection::session().ok()?;
    let proxy = Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    )
    .ok()?;
    // ReadOne(ns, key) → v (u32 for the appearance namespace's values).
    let reply: zbus::zvariant::OwnedValue = proxy
        .call("ReadOne", &("org.freedesktop.appearance", name))
        .ok()?;
    // OwnedValue TryFrom→Value: the appearance namespace's values are u32.
    let value = zbus::zvariant::Value::from(reply);
    match value {
        zbus::zvariant::Value::U32(v) => Some(v),
        _ => None,
    }
}

/// Query the portal once (blocking — main before the window opens).
pub fn probe() -> Desktop {
    let dark = read_one("color-scheme").map(|v| v == 1);
    let reduced_motion = read_one("prefers-reduced-motion") == Some(1);
    REDUCED_MOTION.store(reduced_motion, std::sync::atomic::Ordering::Relaxed);
    Desktop { dark, reduced_motion }
}
