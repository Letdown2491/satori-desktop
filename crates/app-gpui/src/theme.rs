//! Sumi-e design tokens — port of `crates/app/theme/sumi-e.css` (light) and
//! `sumi-e-dark.css` (dark); the tokens are the contract and they port
//! verbatim (DESIGN.md §5.1).
//!
//! Two palettes, switchable live: accessors read the current palette
//! (`select` writes it, `ThemeSignal`'s gpui-global effects wake every
//! watching view). Fonts stay constants — both palettes share them.

use std::sync::RwLock;

/// The token contract. u32 RGB — callsites say `rgb(theme::ink())`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub washi: u32,     // app background
    pub panel: u32,     // cards, surfaces
    pub hairline: u32,  // borders, separators
    pub ink: u32,       // primary text
    pub muted: u32,     // secondary text
    pub vermilion: u32, // seal moments: engaged, destructive accents
    pub ok: u32,
    pub warn: u32,
    pub err: u32,
    pub live: u32,
}

/// sumi-e-dark.css — the original palette (ink paper at night).
pub const DARK: Palette = Palette {
    washi: 0x15140f,
    panel: 0x1e1c16,
    hairline: 0x322e25,
    ink: 0xe9e3d6,
    muted: 0x8b8275,
    vermilion: 0xdf6a4f,
    ok: 0x8a9a7b,
    warn: 0xcfa14e,
    err: 0xd9543a,
    live: 0x4caf6a,
};

/// sumi-e.css — ink on washi, the daylight paper.
pub const LIGHT: Palette = Palette {
    washi: 0xe3dac4,
    panel: 0xd9cfb5,
    hairline: 0xc9bd9f,
    ink: 0x191714,
    muted: 0x6e695f,
    vermilion: 0xb23a26,
    ok: 0x6f7d5e,
    warn: 0x9a7320,
    err: 0x9e2b1c,
    live: 0x4caf6a,
};

static CURRENT: RwLock<Palette> = RwLock::new(DARK);

/// Swap the palette. Views observe [`ThemeSignal`] to repaint.
pub fn select(palette: Palette) {
    *CURRENT.write().unwrap() = palette;
}

pub fn current() -> Palette {
    *CURRENT.read().unwrap()
}

pub fn washi() -> u32 {
    current().washi
}
pub fn panel() -> u32 {
    current().panel
}
pub fn hairline() -> u32 {
    current().hairline
}
pub fn ink() -> u32 {
    current().ink
}
pub fn muted() -> u32 {
    current().muted
}
pub fn vermilion() -> u32 {
    current().vermilion
}
#[allow(dead_code)]
pub fn ok() -> u32 {
    current().ok
}
#[allow(dead_code)]
pub fn warn() -> u32 {
    current().warn
}
pub fn err() -> u32 {
    current().err
}
pub fn live() -> u32 {
    current().live
}

/// The gpui-global that carries a theme change to every observing view —
/// `select` is the source of truth (helpers without a `cx` read it
/// directly); this signal is only the wake-up call.
pub struct ThemeSignal;

impl gpui::Global for ThemeSignal {}

/// Register a view for theme-change wake-ups (call from the constructor).
pub fn watch<V: 'static>(cx: &mut gpui::Context<V>) {
    cx.observe_global::<ThemeSignal>(|_, cx| cx.notify())
        .detach();
}

// §fonts — host fontconfig fallbacks until the bundled OFL serif (D9) is
// embedded and registered via `cx.text_system().add_fonts`. A missing family
// fails silently to a default — check rendering after font changes.
// P052: the Palatino face — a book serif with a big x-height, ink on washi
// (was Nimbus Roman, a Times clone that read cramped at body sizes).
pub const SANS: &str = "Cantarell";
pub const SERIF: &str = "P052";
pub const MONO: &str = "Noto Sans Mono";
