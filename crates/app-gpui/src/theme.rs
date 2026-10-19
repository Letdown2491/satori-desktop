//! Sumi-e design tokens as Rust constants.
//!
//! TODO(port): crates/app/theme/sumi-e-dark.css — P0. The GTK CSS provider dies
//! with D13; the tokens are the contract, and they port verbatim. When the light
//! palette is ported (P4), these become a struct selected at startup.

use gpui::rgb;

pub const WASHI: u32 = 0x15140f; // app background
pub const PANEL: u32 = 0x1e1c16; // cards, surfaces
pub const HAIRLINE: u32 = 0x322e25; // borders, separators
pub const INK: u32 = 0xe9e3d6; // primary text
pub const MUTED: u32 = 0x8b8275; // secondary text
pub const VERMILION: u32 = 0xdf6a4f; // seal moments: engaged, destructive accents
#[allow(dead_code)] // full token set ported as the contract
pub const OK: u32 = 0x8a9a7b;
#[allow(dead_code)] // full token set ported as the contract
pub const WARN: u32 = 0xcfa14e;
#[allow(dead_code)] // full token set ported as the contract
pub const ERR: u32 = 0xd9543a;
#[allow(dead_code)] // full token set ported as the contract
pub const LIVE: u32 = 0x4caf6a;

#[allow(dead_code)] // P4: selected by theme at startup
pub fn washi() -> gpui::Hsla {
    rgb(WASHI).into()
}

// TODO(port): crates/app/theme/sumi-e-dark.css §fonts ("Source Serif 4",
// "Palatino", "Georgia") — P0. GPUI resolves one family per style; these are
// host fontconfig fallbacks until the bundled OFL serif (D9) is embedded and
// registered via `cx.text_system().add_fonts`. A missing family fails silently
// to a default — check rendering after font changes.
pub const SANS: &str = "Cantarell";
// P052: the Palatino face — a book serif with a big x-height, ink on washi
// (was Nimbus Roman, a Times clone that read cramped at body sizes).
pub const SERIF: &str = "P052";
