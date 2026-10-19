//! One-off: dump the articles in the store — tags + content head — to see
//! where covers actually live. Run with the app STOPPED (LMDB lock).
use nostr::prelude::*;
use nostr_database::NostrDatabase;
use nostr_lmdb::NostrLmdb;
use satori_core::config;

#[tokio::main]
async fn main() {
    let db = NostrLmdb::open(config::events_dir()).await.unwrap();
    let filter = Filter::new().kind(Kind::Custom(30023)).limit(50);
    let events: Vec<Event> = db.query(filter).await.unwrap().into_iter().collect();
    println!("=== {} articles ===", events.len());
    for e in &events {
        let tag_first: Vec<String> = e
            .tags
            .iter()
            .map(|t| t.clone().to_vec().first().cloned().unwrap_or_default())
            .collect();
        println!(
            "-- id {} author {}",
            &e.id.to_string()[..8],
            &e.pubkey.to_string()[..8]
        );
        println!("   tags: {:?}", tag_first);
        for t in e.tags.iter() {
            let v = t.clone().to_vec();
            match v.first().map(String::as_str) {
                Some("image") | Some("imeta") => println!("   {:?} ", v),
                _ => {}
            }
        }
        println!("   content[:90]: {:?}", e.content.chars().take(90).collect::<String>());
    }
}
