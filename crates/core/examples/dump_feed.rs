//! One-off store inspector: dump the following-feed population to find the
//! "blank notes". Run with the app STOPPED (LMDB exclusive lock).
//! `cargo run -p satori-core --example dump_feed -- [limit]`
use nostr::prelude::*;
use nostr_database::NostrDatabase;
use nostr_lmdb::NostrLmdb;
use satori_core::config;

#[tokio::main]
async fn main() {
    let limit: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(400);
    let db = NostrLmdb::open(config::events_dir()).await.unwrap();
    let kinds = satori_core::store::visible_kinds();
    let filter = Filter::new().kinds(kinds.clone()).limit(limit);
    let events: Vec<Event> = db.query(filter).await.unwrap().into_iter().collect();
    println!("=== {} events (kinds {:?}) ===", events.len(), kinds
        .iter()
        .map(|k| u16::from(*k))
        .collect::<Vec<_>>());
    let _drop_machines = satori_core::store::machine_notes_hidden();
    let mut blank = 0;
    for e in &events {
        let kind = u16::from(e.kind);
        let content = e.content.trim();
        let blankish = content.is_empty()
            || content.chars().all(|c| c.is_whitespace() || c == '\u{200b}');
        // Quote-only: every paragraph a standalone nostr ref — renders as
        // embeds alone ("…" until resolved).
        let paras: Vec<&str> = e
            .content
            .lines()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        let quote_only = !paras.is_empty()
            && paras.iter().all(|p| {
                p.starts_with("nostr:") || p.starts_with("note1") || p.starts_with("nevent1")
            });
        // Single bare link with no path extension: the body folds it into
        // the media gallery; until decode lands (or fails) it renders as
        // nothing but a provisional box.
        let one_url = paras.len() == 1
            && (paras[0].starts_with("http://") || paras[0].starts_with("https://"))
            && {
                let path_only = paras[0].split(['?', '#']).next().unwrap_or("");
                let last_seg = path_only.rsplit('/').next().unwrap_or("");
                !last_seg.contains('.')
            };
        if blankish || quote_only || one_url {
            blank += 1;
            println!(
                "SUSPECT kind {} id {} blank={} quote_only={} one_url={} content={:?}",
                kind,
                &e.id.to_string()[..8],
                blankish,
                quote_only,
                one_url,
                e.content.chars().take(70).collect::<String>(),
            );
        }
    }
    println!("=== suspects: {} / {} ===", blank, events.len());
    // Kind histogram
    let mut hist: std::collections::HashMap<u16, usize> = std::collections::HashMap::new();
    for e in &events {
        *hist.entry(u16::from(e.kind)).or_default() += 1;
    }
    let mut hist: Vec<_> = hist.into_iter().collect();
    hist.sort();
    for (k, n) in hist {
        println!("kind {}: {}", k, n);
    }
}
