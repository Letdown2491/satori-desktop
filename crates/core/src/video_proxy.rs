//! The guarded range-proxy — the door playbin's bytes walk through
//! (M5 WP3: "no byte is fetched by any pipeline we don't pin").
//!
//! GStreamer's souphttpsrc resolves, redirects and dials with its own
//! stack (GResolver, its own sockets, its own redirect policy) — none
//! of media.rs's discipline applies inside gst, and there is no hook
//! to teach it. So video.rs never hands playbin an origin URL: it
//! hands playbin `http://127.0.0.1:<port>/<token>/stream` and this
//! module serves it. Every origin fetch — the initial GET, every
//! seek's Range re-GET, every redirect hop — rides `media::client()`
//! (SafeResolver, per-hop is_safe_media_url re-validation, hop cap 5).
//!
//! Why a range-proxy and not a pre-fetch-to-file: the store copy
//! promises "the video itself never downloads before you tap" — a
//! full pre-fetch re-introduces exactly that wait (plus bandwidth
//! waste on tap-and-bail), and moov-driven seeking needs Range
//! semantics. One listener per player (the process-wide one-player
//! rule owns the lifecycle); the 128-bit token in the path is the
//! capability — a local process without it can neither use nor probe.
//!
//! Honest failure: if the proxy cannot bind, video.rs degrades the
//! tile to the browser link, exactly like a dead URL. Nothing falls
//! back to an unguarded playbin dial.
//!
//! TODO(port): tor.rs — the origin fetch already rides media::client();
//! when the Tor transport lands, this door tor-ifies for free.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use tokio::io::AsyncWriteExt;

/// The origin fetch budget per REQUEST. Playbin re-GETs with a Range
/// on every seek, so the cap is per-request, NOT cumulative — a
/// cumulative counter would punish legitimate rewinds (every seek
/// re-streams). 4 GiB is "no real clip exceeds this"; the point is a
/// bounded runaway (redirect loops, infinite streams), not a quota.
pub const MAX_STREAM_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Request-head cap (playbin sends a dozen headers; 16 KiB is already
/// generous).
const MAX_HEAD: usize = 16 * 1024;

struct State {
    origin: String,
    token: String,
}

/// One proxy per player. Dropping it aborts the listener; open
/// connections drain as playbin closes them.
pub struct RangeProxy {
    addr: SocketAddr,
    token: String,
    task: tokio::task::JoinHandle<()>,
}

impl RangeProxy {
    /// The URI to hand to `Video::new` instead of the origin URL.
    pub fn uri(&self) -> String {
        format!("http://{}/{}/stream", self.addr, self.token)
    }
}

impl Drop for RangeProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The proxy's runtime: two worker threads, started on first use.
/// Deliberately NOT the manager's relay runtime — video bytes never
/// queue behind relays and zaps (the media gate's own rule).
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("satori-video-proxy")
            .enable_all()
            .build()
            .expect("video-proxy runtime")
    })
}

/// Bind a loopback listener and start serving the origin. Synchronous
/// from the caller's view (the bind happens on the proxy runtime; this
/// returns once the listener is live).
pub fn spawn(origin: String) -> Result<RangeProxy, std::io::Error> {
    let (tx, rx) = std::sync::mpsc::channel();
    let token = new_token();
    let state = Arc::new(State { origin, token: token.clone() });
    let task = runtime().spawn(async move {
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
            Ok(l) => l,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        let addr = match listener.local_addr() {
            Ok(a) => a,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        let _ = tx.send(Ok(addr));
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    // Only the process itself may use the door: this
                    // server fetches ARBITRARY origins.
                    if !peer.ip().is_loopback() {
                        continue;
                    }
                    let state = Arc::clone(&state);
                    tokio::spawn(serve(stream, state));
                }
                Err(_) => break,
            }
        }
    });
    let addr = rx
        .recv()
        .map_err(|_| std::io::Error::other("video-proxy startup died"))??;
    Ok(RangeProxy { addr, token, task })
}

/// 128 bits of capability. The token must be unguessable by another
/// local process — thread_rng, hex-encoded.
fn new_token() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn serve(stream: tokio::net::TcpStream, state: Arc<State>) {
    let (mut rd, mut wr) = stream.into_split();
    // Read the request head, bounded.
    let mut head: Vec<u8> = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        match rd.read(&mut byte).await {
            Ok(0) => return,
            Ok(_) => {
                head.push(byte[0]);
                if head.len() > MAX_HEAD {
                    return;
                }
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let text = String::from_utf8_lossy(&head);
    let request = text.lines().next().unwrap_or("");
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    if !method.eq_ignore_ascii_case("GET") {
        let _ = simple(&mut wr, 501).await;
        return;
    }
    // Capability: exactly /<token>/stream. Anything else is a probe.
    let expected = format!("/{}/stream", state.token);
    if path != expected {
        let _ = simple(&mut wr, 404).await;
        return;
    }
    // Range header, passed verbatim (playbin seeks with "bytes=N-").
    let range = text
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("range").then(|| v.trim().to_string())
        })
        .filter(|v| !v.is_empty());

    // THE guarded fetch — the only egress in this module.
    let mut req = crate::media::client().get(&state.origin);
    if let Some(r) = &range {
        req = req.header("Range", r);
    }
    let mut res = match req.send().await {
        Ok(res) => res,
        Err(_) => {
            let _ = simple(&mut wr, 502).await;
            return;
        }
    };
    if !res.status().is_success() {
        // Mirror the verdict — 404/410 stay precise for the demuxer.
        let _ = simple(&mut wr, res.status().as_u16()).await;
        return;
    }
    // Demuxer-relevant headers, passed through; the rest is noise.
    let head_out = format!(
        "HTTP/1.1 {} {}\r\nConnection: close\r\n{}\r\n",
        res.status().as_u16(),
        if res.status() == 200 { "OK" } else { "Partial Content" },
        [
            ("Accept-Ranges", header(&res, "accept-ranges").unwrap_or_else(|| "bytes".into())),
            ("Content-Length", header(&res, "content-length")),
            ("Content-Range", header(&res, "content-range")),
            ("Content-Type", header(&res, "content-type")),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| format!("{k}: {v}\r\n")))
        .collect::<String>(),
    );
    if wr.write_all(head_out.as_bytes()).await.is_err() {
        return;
    }
    // Stream, capped. A cap overrun cuts mid-body: playbin reports the
    // honest failure; nothing silently rewrites the file.
    let mut written: u64 = 0;
    loop {
        let chunk = match res.chunk().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(_) => return,
        };
        written += chunk.len() as u64;
        if written > MAX_STREAM_BYTES {
            return;
        }
        if wr.write_all(chunk.as_ref()).await.is_err() {
            return;
        }
    }
    let _ = wr.shutdown().await;
}

fn header(res: &reqwest::Response, name: &str) -> Option<String> {
    res.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

async fn simple(
    wr: &mut tokio::net::tcp::OwnedWriteHalf,
    status: u16,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        404 => "Not Found",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    wr.write_all(
        format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
            .as_bytes(),
    )
    .await?;
    wr.shutdown().await
}
