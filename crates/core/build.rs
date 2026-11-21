//! The build's one injection: the gifs.nostr.build API key (D104). The
//! key lives in `gifs_key.txt` beside this manifest — git-ignored, so
//! the public repo never shows it — and reaches the binary as
//! `GIFS_API_KEY`. CI (release.yml) writes the file from the
//! `GIFS_API_KEY` repo secret before building; a missing file compiles//! fine with an empty key (the GIF feature reports itself unconfigured
//! via `gifs::configured()`).

use std::fs;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=gifs_key.txt");
    let key = fs::read_to_string(Path::new("gifs_key.txt"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    // A key pasted with a trailing newline or quotes stays usable.
    let key = key.trim().trim_matches('"').trim_matches('\'').to_string();
    println!("cargo:rustc-env=GIFS_API_KEY={key}");
}
