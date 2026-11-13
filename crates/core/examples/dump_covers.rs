//! One-off: run the REAL core media pipeline (guarded client, SafeResolver,
//! byte cap, decode) against hodlbod's article covers — what the app runs,
//! not a bare reqwest probe.
use nostr::prelude::*;
use nostr_sdk::client::Client;

const HODLBOD: &str = "97c70a44366a6535c145b333f973ea86dfdc2d7a99da618c40c64705ad98e322";

const READ_RELAYS: [&str; 4] = [
    "wss://relay.damus.io",
    "wss://relay.primal.net",
    "wss://relay.coracle.social",
    "wss://nos.social",
];

#[tokio::main]
async fn main() {
    let author: PublicKey = HODLBOD.parse().unwrap();
    let client = Client::default();
    for r in READ_RELAYS {
        let _ = client.add_relay(r).await;
    }
    client.connect().await;
    let got = client
        .fetch_events(
            Filter::new()
                .kind(Kind::Custom(30023))
                .author(author)
                .limit(10),
        )
        .timeout(std::time::Duration::from_secs(10))
        .await
        .unwrap();

    for e in got.iter().take(6) {
        let Some(cover) = satori_core::nip::article::parse(e).and_then(|d| d.image) else {
            continue;
        };
        let t0 = std::time::Instant::now();
        let res = satori_core::media::fetch_image(&cover).await;
        match res {
            Ok(d) => println!(
                "OK   {:?} {}x{} in {:?}  {cover}",
                e.id.to_string()[..8].to_string(),
                d.width,
                d.height,
                t0.elapsed()
            ),
            Err(e) => println!("FAIL {:?} {e:?} in {:?}  {cover}", "", t0.elapsed()),
        }
    }
}
