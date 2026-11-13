//! One-off: capture REAL NIP-45 COUNT wire responses as fixtures. The SDK's
//! `Relay::count_events` returns a bare usize; these fixtures need the raw
//! JSON (`{"count":N,"approximate":B,"hll":...}`), refusals (`CLOSED`), and
//! silence (no COUNT support) to test `nip45::parse_response` and the merge
//! against what relays actually send. Writes to
//! `crates/core/tests/fixtures/nip45/<relay>-<what>.json`.
use futures_util::{SinkExt, StreamExt};
use nostr::prelude::*;
use nostr_sdk::client::Client;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const HODLBOD: &str = "97c70a44366a6535c145b333f973ea86dfdc2d7a99da618c40c64705ad98e322";

// Findings (Nov 13 2026): relay.primal.net answers every canonical query
// (plain counts, no hll today); relay.snort.social answers NOTICE
// "Unknown message type: COUNT"; relay.damus.io and nos.social sit silent
// or down; nostr.wine 403s; nostr.band + coracle don't resolve from here.
// COUNT support among the big relays is thin — the strip must treat each
// relay's answer as optional.
const COUNT_RELAYS: [&str; 3] = [
    "wss://relay.primal.net",
    "wss://relay.damus.io",
    "wss://relay.snort.social",
];

/// One COUNT exchange on a raw socket: send the frame, read until the
/// matching COUNT response or CLOSED, bounded (relays without COUNT support
/// just never answer).
async fn count_once(url: &str, sub_id: &str, filter: Value) -> Result<Vec<String>, String> {
    let (ws, _) = connect_async(url).await.map_err(|e| e.to_string())?;
    let (mut tx, mut rx) = ws.split();
    let frame = json!(["COUNT", sub_id, filter]).to_string();
    tx.send(Message::Text(frame.into()))
        .await
        .map_err(|e| e.to_string())?;

    let mut lines = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    loop {
        let msg = tokio::time::timeout_at(deadline, rx.next()).await;
        let Ok(Some(Ok(msg))) = msg else {
            break; // silence (or drop): the no-COUNT-support shape
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => continue,
            _ => break,
        };
        let is_done = text.contains(&format!("\"COUNT\",\"{sub_id}\""))
            || text.contains(&format!("\"CLOSED\",\"{sub_id}\""));
        lines.push(text);
        if is_done {
            break;
        }
    }
    Ok(lines)
}

#[tokio::main]
async fn main() {
    let out = std::path::Path::new("crates/core/tests/fixtures/nip45");
    std::fs::create_dir_all(out).unwrap();

    // A real, live-popular note: hodlbod's latest kind-1 with reactions on
    // it. The counts don't matter; the WIRE SHAPES do.
    let author: PublicKey = HODLBOD.parse().unwrap();
    let client = Client::default();
    for r in COUNT_RELAYS {
        let _ = client.add_relay(r).await;
    }
    client.connect().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let notes = client
        .fetch_events(Filter::new().kind(Kind::TextNote).author(author).limit(5))
        .timeout(Duration::from_secs(15))
        .await
        .unwrap_or_default();
    let note_id = match notes.first() {
        Some(e) => e.id.to_string(),
        // Fallback: the target of a live reaction stream is popular by
        // construction (the probe only needs wire shapes, not a spec note).
        None => {
            println!("no hodlbod notes; falling back to live reaction targets");
            let rx = client
                .fetch_events(Filter::new().kind(Kind::Reaction).limit(20))
                .timeout(Duration::from_secs(15))
                .await
                .unwrap_or_default();
            rx.first()
                .and_then(|e| {
                    e.tags.iter().find_map(|t| {
                        if t.as_slice().first().map(String::as_str) == Some("e") {
                            t.as_slice().get(1).cloned()
                        } else {
                            None
                        }
                    })
                })
                .expect("a live reaction target")
        }
    };
    println!("target note {note_id}");

    // Canonical filter shapes straight from the NIP (plus the notification
    // window's kinds), one per exchange.
    let cases: Vec<(&str, Value)> = vec![
        ("replies", json!({"#e": [note_id], "kinds": [1]})),
        ("reactions", json!({"#e": [note_id], "kinds": [7]})),
        ("reposts", json!({"#e": [note_id], "kinds": [6, 16]})),
        ("followers", json!({"#p": [HODLBOD], "kinds": [3]})),
        ("zaprequests", json!({"#p": [HODLBOD], "kinds": [9735]})),
    ];

    for relay in COUNT_RELAYS {
        let tag = relay.trim_start_matches("wss://");
        for (what, filter) in &cases {
            let sub = format!("probe-{what}");
            match count_once(relay, &sub, filter.clone()).await {
                Ok(lines) if !lines.is_empty() => {
                    let file = out.join(format!("{tag}-{what}.json"));
                    std::fs::write(&file, lines.join("\n")).unwrap();
                    println!("{} {what}: {} frame(s)", tag, lines.len());
                }
                Ok(_) => println!("{tag} {what}: SILENT (no COUNT support)"),
                Err(e) => println!("{tag} {what}: ERR {e}"),
            }
        }
    }
}
