//! One-off: query the READ relays directly for kind-30023 population —
//! do follows other than the prolific one have articles? Also prints
//! created_at vs published_at for the store's articles. Read-only; no
//! signer. Run with the app STOPPED (LMDB lock).
use nostr::prelude::*;
use nostr_database::NostrDatabase;
use nostr_lmdb::NostrLmdb;
use nostr_sdk::client::Client;
use satori_core::config;

const READ_RELAYS: [&str; 4] = [
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://relay.coracle.social",
    "wss://nos.social",
];

#[tokio::main]
async fn main() {
    // --- the store's articles: created vs published ---
    let db = NostrLmdb::open(config::events_dir()).await.unwrap();
    let filter = Filter::new().kind(Kind::Custom(30023)).limit(60);
    let events: Vec<Event> = db.query(filter).await.unwrap().into_iter().collect();
    println!("=== store: {} articles ===", events.len());
    for e in events.iter().take(12) {
        let published = e
            .tags
            .iter()
            .find_map(|t| {
                let v = t.clone().to_vec();
                match v.first().map(String::as_str) {
                    Some("published_at") => v.get(1).cloned(),
                    _ => None,
                }
            })
            .and_then(|v| v.parse::<u64>().ok());
        println!(
            "created {}  published_at {:?}  author {}  {:?}",
            e.created_at.as_secs(),
            published,
            &e.pubkey.to_string()[..8],
            e.tags
                .iter()
                .find_map(|t| {
                    let v = t.clone().to_vec();
                    (v.first().map(String::as_str) == Some("title"))
                        .then(|| v.get(1).cloned().unwrap_or_default())
                })
                .unwrap_or_default()
                .chars()
                .take(40)
                .collect::<String>(),
        );
    }

    // --- follows from the store ---
    let me: PublicKey = {
        let s = std::fs::read_to_string(config::state_file(config::files::SESSION)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        v["pubkey"].as_str().unwrap().parse().unwrap()
    };
    let contact = Filter::new()
        .kind(Kind::ContactList)
        .author(me)
        .limit(1);
    let mut follows: Vec<PublicKey> = db
        .query(contact)
        .await
        .unwrap()
        .into_iter()
        .next()
        .map(|ev| ev.tags.public_keys().collect())
        .unwrap_or_default();
    follows.dedup();
    println!("=== {} follows ===", follows.len());

    // --- live relay population for kind 30023 over those follows ---
    let client = Client::default();
    for r in READ_RELAYS {
        let _ = client.add_relay(r).await;
    }
    client.connect().await;
    let filter = Filter::new()
        .kind(Kind::Custom(30023))
        .authors(follows.clone())
        .limit(300);
    let got = client.fetch_events(filter).timeout(std::time::Duration::from_secs(8)).await.unwrap();
    let mut hist: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut newest = 0u64;
    for e in got.iter() {
        *hist.entry(e.pubkey.to_string()[..8].to_string()).or_default() += 1;
        newest = newest.max(e.created_at.as_secs());
    }
    let mut hist: Vec<_> = hist.into_iter().collect();
    hist.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!(
        "=== live: {} articles from {} authors over {} follows (newest {}s ago) ===",
        hist.iter().map(|(_, n)| n).sum::<usize>(),
        hist.len(),
        follows.len(),
        now_secs().saturating_sub(newest)
    );
    for (pk, n) in hist.iter().take(15) {
        println!("  author {}: {}", pk, n);
    }

    // The app's EXACT deep fetch: one filter, all follows, the 4s budget.
    let t0 = std::time::Instant::now();
    let app_filter = Filter::new()
        .kind(Kind::Custom(30023))
        .authors(follows.clone())
        .limit(200);
    let got4 = client
        .fetch_events(app_filter)
        .timeout(std::time::Duration::from_secs(4))
        .await
        .unwrap();
    let mut authors4 = std::collections::HashSet::new();
    for e in got4.iter() {
        authors4.insert(e.pubkey.to_string()[..8].to_string());
    }
    println!(
        "=== app-style deep fetch (1 filter, {} authors, 4s): {} events, {} authors, took {:?} ===",
        follows.len(),
        got4.len(),
        authors4.len(),
        t0.elapsed()
    );
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
