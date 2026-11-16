//! Test-only NIP-01 relay mock: enough wire for the live-subscription
//! tests (REQ/CLOSED/EOSE bookkeeping, EVENT broadcast, late-REQ replay,
//! a server-side kick). The mock never matches filters — the SDK routes
//! by subscription id and the tests control what publishes.

use crate::pool::Pool;
use futures_util::{SinkExt, StreamExt};
use nostr::prelude::*;
use nostr_sdk::client::ClientNotification;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

pub struct MockRelay {
    addr: std::net::SocketAddr,
    /// Every live connection: its broadcast sink + its open sub ids.
    conns: Arc<Mutex<Vec<(mpsc::UnboundedSender<Message>, Arc<Mutex<Vec<String>>>)>>>,
    /// Published events, replayed to late REQs.
    events: Arc<Mutex<Vec<Event>>>,
    /// Total WS connections ever accepted (reconnect detection).
    conns_seen: Arc<std::sync::atomic::AtomicUsize>,
}

impl MockRelay {
    pub async fn spawn() -> Self {
        Self::spawn_with_auth(false).await
    }

    /// An auth-gated mock (the relay.nostr.build shape): every REQ
    /// before a valid NIP-42 auth gets an ["AUTH", challenge] frame and
    /// a `CLOSED ... auth-required` reply; the REQ is NOT registered.
    /// A signed kind-22242 event carrying the challenge (sig verified,
    /// relay tag checked) marks the connection authenticated, and a
    /// subsequent REQ is accepted.
    pub async fn spawn_auth_gated() -> Self {
        Self::spawn_with_auth(true).await
    }

    async fn spawn_with_auth(auth_required: bool) -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind loopback mock relay");
        let addr = listener.local_addr().unwrap();
        let conns: Arc<Mutex<Vec<(mpsc::UnboundedSender<Message>, Arc<Mutex<Vec<String>>>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        // Published events, replayed to late REQs (a REQ may land
        // after the test publishes — the relay still serves it).
        let events: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        let conns_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let conns_accept = conns.clone();
        let events_accept = events.clone();
        let seen_accept = conns_seen.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                    continue;
                };
                seen_accept.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (mut sink, mut peer) = ws.split();
                let subs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
                // Broadcast pump: frames the relay "sends" land here.
                let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
                conns_accept
                    .lock()
                    .unwrap()
                    .push((tx.clone(), subs.clone()));
                tokio::spawn(async move {
                    while let Some(frame) = rx.recv().await {
                        if sink.send(frame).await.is_err() {
                            break;
                        }
                    }
                });
                // Inbound pump: REQ/CLOSE bookkeeping, EOSE replies.
                let (tx_in, subs_in, events_in) = (tx.clone(), subs.clone(), events_accept.clone());
                // Per-connection NIP-42 state (auth-gated mode): the
                // challenge this conn was issued + whether a valid auth
                // event landed.
                let authed = Arc::new(Mutex::new(false));
                let challenge = Arc::new(Mutex::new(String::new()));
                tokio::spawn(async move {
                    // Pings/Pongs/Binary frames must NOT end the pump
                    // (the SDK pings its relays) — skip, don't exit.
                    while let Some(msg) = peer.next().await {
                        let Ok(Message::Text(text)) = msg else {
                            continue;
                        };
                        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) else {
                            continue;
                        };
                        let Some(arr) = msg.as_array() else {
                            continue;
                        };
                        let Some(kind) = arr.first().and_then(|v| v.as_str()) else {
                            continue;
                        };
                        if auth_required && kind == "AUTH" {
                            // ["AUTH", <event>]: the client's NIP-42
                            // answer. Verify kind, challenge, relay tag,
                            // and the signature itself.
                            let want_challenge = challenge.lock().unwrap().clone();
                            let parsed = arr
                                .get(1)
                                .and_then(|v| serde_json::from_value::<Event>(v.clone()).ok());
                            let valid = parsed.as_ref().is_some_and(|ev| {
                                let tags_ok = ev.tags.iter().any(|t| {
                                    t.as_slice().first().map(String::as_str) == Some("challenge")
                                        && t.as_slice().get(1) == Some(&want_challenge)
                                }) && ev.tags.iter().any(|t| {
                                    t.as_slice().first().map(String::as_str) == Some("relay")
                                        && t.as_slice().get(1).map(String::as_str)
                                            == Some(format!("ws://{addr}").as_str())
                                });
                                u16::from(ev.kind) == 22242 && tags_ok && ev.verify().is_ok()
                            });
                            if valid {
                                *authed.lock().unwrap() = true;
                                if let Some(ev) = parsed {
                                    let ok = Message::Text(
                                        serde_json::json!(["OK", ev.id.to_string(), true, ""])
                                            .to_string()
                                            .into(),
                                    );
                                    let _ = tx_in.send(ok);
                                }
                            } else {
                                let bad = Message::Text(
                                    serde_json::json!([
                                        "OK",
                                        parsed.map(|e| e.id.to_string()).unwrap_or_default(),
                                        false,
                                        "invalid auth event"
                                    ])
                                    .to_string()
                                    .into(),
                                );
                                let _ = tx_in.send(bad);
                            }
                            continue;
                        }
                        let Some(sub_id) = arr.get(1).and_then(|v| v.as_str()) else {
                            continue;
                        };
                        if auth_required && kind == "REQ" && !*authed.lock().unwrap() {
                            // Unauthenticated REQ: issue the challenge
                            // (if not yet issued) and refuse the sub —
                            // the relay.nostr.build shape.
                            let issued = challenge.lock().unwrap().clone();
                            let challenge_str = if issued.is_empty() {
                                let c = format!(
                                    "challenge-{}",
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_nanos())
                                        .unwrap_or(0)
                                );
                                *challenge.lock().unwrap() = c.clone();
                                c
                            } else {
                                issued
                            };
                            let auth_frame = Message::Text(
                                serde_json::json!(["AUTH", challenge_str])
                                    .to_string()
                                    .into(),
                            );
                            let _ = tx_in.send(auth_frame);
                            let closed = Message::Text(
                                serde_json::json!([
                                    "CLOSED",
                                    sub_id,
                                    "auth-required: authenticate before subscribing"
                                ])
                                .to_string()
                                .into(),
                            );
                            let _ = tx_in.send(closed);
                            continue;
                        }
                        match kind {
                            "REQ" => {
                                subs_in.lock().unwrap().push(sub_id.to_string());
                                // Replay the store (naive kind+author
                                // match — the test controls both).
                                let store = events_in.lock().unwrap().clone();
                                for ev in store {
                                    let ev_kind: u16 = ev.kind.into();
                                    let kind_ok = arr[2..].iter().all(|f| {
                                        f.get("kinds")
                                            .and_then(|k| k.as_array())
                                            .map(|ks| {
                                                ks.iter().any(|k| {
                                                    k.as_u64() == Some(ev_kind as u64)
                                                        || k.as_str()
                                                            == Some(ev_kind.to_string().as_str())
                                                })
                                            })
                                            .unwrap_or(true)
                                    });
                                    let author_ok = arr[2..]
                                        .iter()
                                        .filter_map(|f| f.get("authors"))
                                        .filter_map(|a| a.as_array())
                                        .all(|as_| {
                                            as_.is_empty()
                                                || as_.iter().any(|a| {
                                                    a.as_str() == Some(&ev.pubkey.to_string())
                                                })
                                        });
                                    if kind_ok && author_ok {
                                        let frame = Message::Text(
                                            serde_json::json!([
                                                "EVENT",
                                                sub_id,
                                                serde_json::to_value(&ev)
                                                    .expect("event serializes"),
                                            ])
                                            .to_string()
                                            .into(),
                                        );
                                        let _ = tx_in.send(frame);
                                    }
                                }
                                let eose = Message::Text(
                                    serde_json::json!(["EOSE", sub_id]).to_string().into(),
                                );
                                let _ = tx_in.send(eose);
                            }
                            "CLOSE" => {
                                subs_in.lock().unwrap().retain(|id| id != sub_id);
                            }
                            _ => {}
                        }
                    }
                });
            }
        });

        Self {
            addr,
            conns,
            events,
            conns_seen,
        }
    }

    /// The relay url (loopback, plain ws — pair with
    /// net_guard::allow_loopback_relays_for_test + allow_direct_url so
    /// the pool dials it direct and routes to it).
    pub fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// Wait until at least `want` subscriptions are open across the
    /// relay (the client's REQs landed), bounded — else the test
    /// publishes before any sub exists and nothing can be delivered.
    pub async fn wait_open_subs(&self, want: usize) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let open: usize = self
                .conns
                .lock()
                .unwrap()
                .iter()
                .map(|(_, subs)| subs.lock().unwrap().len())
                .sum();
            if open >= want {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "mock never saw {want} open subscriptions (saw {open})"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// Broadcast an event to every connection with an open
    /// subscription, framed under that connection's first sub id.
    pub fn publish(&self, event: &Event) {
        self.events.lock().unwrap().push(event.clone());
        let wire = serde_json::to_value(event).expect("event serializes");
        for (tx, subs) in self.conns.lock().unwrap().iter() {
            let guard = subs.lock().unwrap();
            let Some(sub_id) = guard.first() else {
                continue;
            };
            let frame = Message::Text(
                serde_json::json!(["EVENT", sub_id, wire])
                    .to_string()
                    .into(),
            );
            let _ = tx.send(frame);
        }
    }

    /// Relay answers CLOSED for the idx'th open subscription (a plain
    /// message — no machine-readable prefix — the SDK's Remove path).
    /// The socket STAYS OPEN: CLOSED is an in-band reply, and the
    /// relay simply stops serving that subscription.
    pub fn closed_sub(&self, idx: usize) {
        self.closed_sub_reason(idx, "no reason");
    }

    /// In-band CLOSED of the idx-th open subscription on every
    /// connection, with a reason string (machine-readable prefix tests:
    /// auth-required vs restricted vs bare).
    pub fn closed_sub_reason(&self, idx: usize, reason: &str) {
        for (tx, subs) in self.conns.lock().unwrap().iter() {
            let Some(sub_id) = subs.lock().unwrap().get(idx).cloned() else {
                continue;
            };
            let frame = Message::Text(
                serde_json::json!(["CLOSED", sub_id, reason])
                    .to_string()
                    .into(),
            );
            let _ = tx.send(frame);
        }
    }

    /// Server-side disconnect of every connection (kick the client;
    /// a healthy SDK reconnects and resubscribes).
    pub fn kick(&self) {
        for (tx, _) in self.conns.lock().unwrap().iter() {
            let _ = tx.send(Message::Close(None));
        }
    }

    /// Wait until at least `want` connections have been accepted IN
    /// TOTAL (reconnect detection), bounded.
    pub async fn wait_conns_seen(&self, want: usize) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let seen = self.conns_seen.load(std::sync::atomic::Ordering::SeqCst);
            if seen >= want {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "mock never saw {want} connections (saw {seen})"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Total open subscriptions across every connection right now.
    pub fn open_subs(&self) -> usize {
        self.conns
            .lock()
            .unwrap()
            .iter()
            .map(|(_, s)| s.lock().unwrap().len())
            .sum()
    }

    /// Map of open sub ids across connections (test introspection).
    pub fn sub_ids(&self) -> Vec<String> {
        self.conns
            .lock()
            .unwrap()
            .iter()
            .flat_map(|(_, s)| s.lock().unwrap().clone())
            .collect()
    }
}

/// Did `want` land on the notification stream within 3s? (Silence or
/// stream end → false.)
pub async fn delivered(
    notifications: &mut std::pin::Pin<
        Box<dyn futures_util::Stream<Item = ClientNotification> + Send>,
    >,
    want: EventId,
) -> bool {
    loop {
        let n = tokio::time::timeout(std::time::Duration::from_secs(3), notifications.next()).await;
        match n {
            Ok(Some(ClientNotification::Event { event, .. })) => {
                if event.id == want {
                    return true;
                }
            }
            Ok(Some(_)) => continue,
            _ => return false,
        }
    }
}

/// The shared live-test preamble: isolated env, an exempted loopback
/// mock, a test pool dialing it direct. The guard retracts the
/// process-global direct-dial exemption on drop (net_guard's ledger is
/// global; other tests assert refusal of private dials and must not see
/// a leaked 127.0.0.1 exemption).
pub struct LiveGuard {
    _env: crate::test_env::EnvGuard,
    url: String,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        crate::net_guard::disallow_direct_url(&self.url);
    }
}

pub async fn pool_with_mock(tag: &str) -> (LiveGuard, MockRelay, Pool, String) {
    let env = crate::test_env::EnvGuard::new(tag);
    let mock = MockRelay::spawn().await;
    let url = mock.url();
    crate::net_guard::allow_direct_url(&url);
    let pool = Pool::open_for_tests().await.expect("pool opens");
    pool.connect_reads(vec![url.clone()]).await.expect("reads");
    (
        LiveGuard {
            _env: env,
            url: url.clone(),
        },
        mock,
        pool,
        url,
    )
}

/// Same preamble against an AUTH-GATED mock (relay.nostr.build shape),
/// with a LOCAL signing identity attached before the connect so
/// PoolAuthenticator can answer challenges.
pub async fn pool_with_auth_mock(tag: &str) -> (LiveGuard, MockRelay, Pool, String) {
    let env = crate::test_env::EnvGuard::new(tag);
    let mock = MockRelay::spawn_auth_gated().await;
    let url = mock.url();
    crate::net_guard::allow_direct_url(&url);
    let pool = Pool::open_for_tests().await.expect("pool opens");
    pool.set_identity(crate::signer::Signer::Local(Keys::generate()));
    pool.connect_reads(vec![url.clone()]).await.expect("reads");
    (
        LiveGuard {
            _env: env,
            url: url.clone(),
        },
        mock,
        pool,
        url,
    )
}
