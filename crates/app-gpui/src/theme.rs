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

// §1.1 — derived role helpers: computed from the existing palette, no new
// colors. Vermilion stays a seal (never body text, never large surfaces);
// error text uses `err()`, never vermilion.
#[allow(dead_code)] // consumed when card-internal separators land (§2.2 settings groups, WS4)
pub fn hairline_strong() -> u32 {
    // Separators INSIDE a composed surface — the hairline token at full
    // strength, vs. the quiet borders around panels (0.2-design-spec §1.1).
    current().hairline
}
/// `ink()` at 5% alpha — the hover background. Replaces the seven hand-rolled
/// `hover(|s| s.opacity(0.72))` fades in feed.rs (deficiency 3): the wash
/// marks WHERE you are without dimming the glyph itself.
pub fn hover_wash() -> gpui::Hsla {
    gpui::Hsla::from(gpui::rgb(current().ink)).opacity(0.05)
}
/// `muted()` at ~65% — placeholder-grade whispers (deficiency: ad-hoc
/// `.opacity(0.6)` on muted rows).
pub fn ink_faint() -> gpui::Hsla {
    gpui::Hsla::from(gpui::rgb(current().muted)).opacity(0.65)
}
/// D23's one-link-token rule: rest = muted + underline, hover = vermilion.
/// Wrap the clickable text's `div()` — the caller keeps id/on_click/children.
pub fn pressable<E: gpui::Styled + gpui::InteractiveElement>(el: E) -> E {
    use gpui::Styled as _;
    el.text_color(gpui::rgb(current().muted))
        .underline()
        .hover(|s| s.text_color(gpui::rgb(current().vermilion)))
}

// §1.2 — the spacing scale, named at last: gpui's default p_N ladder IS a
// 4px scale, but nothing named it, so ad-hoc values leaked (composer.rs's
// raw px(16.0)). One irregular pair (S1=2, S7=32). Call sites keep gpui's
// gap_N utilities where they already match; new code consumes these.
#[allow(dead_code)]
pub const S0: f32 = 0.0; // nothing
#[allow(dead_code)]
pub const S1: f32 = 2.0; // inline nudges
#[allow(dead_code)]
pub const S2: f32 = 4.0; // inline-control vertical, head→body gap (§2.1)
#[allow(dead_code)]
pub const S3: f32 = 8.0; // card/row internal gap
pub const S4: f32 = 12.0; // feed card vertical padding per side (§2.1)
#[allow(dead_code)]
pub const S5: f32 = 16.0; // page gutter, card horizontal padding
#[allow(dead_code)]
pub const S6: f32 = 20.0;
#[allow(dead_code)]
pub const S7: f32 = 32.0;

// §1.3 — the type scale: four sizes, not eight. Under 12px goes mushy at
// gpui's unhinted grayscale AA — a measured constraint, not taste.
pub const T_BODY: f32 = 14.0; // note bodies, author names
pub const T_UI: f32 = 13.0; // timestamps, meta rows, list secondary
pub const T_SMALL: f32 = 12.0; // whispers, glyph labels, badges
#[allow(dead_code)]
pub const T_READER: f32 = 15.0; // reader/article body ONLY (spec rollout step 5)

// §1.4 — the radii ladder (DESIGN.md §5.1, reconciled for the no-card-box
// feed: the boxed things are embeds, pills, media, inputs). Anything under
// 4px is a mistake at hairline borders — the px(3.) chips read as accidental.
pub const R_EMBED: f32 = 6.0; // quote embeds, code chips
#[allow(dead_code)]
pub const R_INPUT: f32 = 8.0; // text inputs, buttons, suggestion popovers
#[allow(dead_code)]
pub const R_MEDIA: f32 = 10.0; // images, video tiles, lightbox thumbs

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
// Crispness pass (owner report: text reads soft): gpui's Linux rasterizer
// (cosmic-text/swash) is unhinted grayscale-only — softer than the
// hinting + subpixel AA GTK apps get — so the faces have to carry the
// sharpness. Adwaita Sans: the GNOME UI face, Inter-derived, sturdier than
// Cantarell at scale-1.0 sizes. C059: the Century Schoolbook clone — thick
// hairlines and a big x-height, made for legibility (P052's Palatino
// hairlines were what grayscale AA ate first; Nimbus Roman before that read
// cramped at body sizes).
pub const SANS: &str = "Adwaita Sans";
pub const SERIF: &str = "C059";
pub const MONO: &str = "Noto Sans Mono";

/// The color emoji face, set as the PRIMARY font on emoji runs (not as a
/// fallback). Shaping is primary-font-wins: cosmic-text's fallback chain
/// (`Noto Sans` → `DejaVu Sans` → … → `Noto Color Emoji` last) only runs on
/// glyphs the primary font lacks, and the sandbox's DejaVu Sans carries
/// monochrome outline glyphs for common emoji (😂 😮 😢), while Adwaita Sans
/// covers ❤ outright. Probed Nov 14 2026 (sandbox e2e): only codepoints no
/// earlier font in the chain covers (👍 🤣) ever reach the color font. Emoji
/// text painted with this family shapes the CBDT bitmap face (postscript
/// `NotoColorEmoji`, the winner of fontdb's identical-family tie) and paints
/// through gpui's color sprite path.
pub const EMOJI: &str = "Noto Color Emoji";
