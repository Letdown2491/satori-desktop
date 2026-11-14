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

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The origin fetch budget per REQUEST. Playbin re-GETs with a Range
/// on every seek, so the cap is per-request, NOT cumulative — a
/// cumulative counter would punish legitimate rewinds (every seek
/// re-streams). 4 GiB is "no real clip exceeds this"; the point is a
/// bounded runaway (redirect loops, infinite streams), not a quota.
pub const MAX_STREAM_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Request-head cap (playbin sends a dozen headers; 16 KiB is already
/// generous).
const MAX_HEAD: usize = 16 * 1024;

/// Constant-time byte equality for the capability token. Length is
/// public (always 32 hex chars), so the length check leaks nothing;
/// the fold compares every byte regardless of where a mismatch sits —
/// no prefix oracle for a local process probing the gate.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

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
///
/// Literal origins screen by value at the facade (the SOCKS5 transport's
/// own rule for its targets): `media::client()`'s pinning covers NAMES —
/// hyper skips the resolver for an IP literal, so a feed-supplied
/// `http://127.0.0.1:6379/...` origin would dial loopback unscreened.
/// Non-public literal (or unparseable origin) refuses here, and the
/// caller degrades to the browser facade. Domain origins pin inside the
/// client on every fetch and every redirect hop.
pub fn spawn(origin: String) -> Result<RangeProxy, std::io::Error> {
    let parsed = url::Url::parse(&origin)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let non_public = match parsed.host() {
        Some(url::Host::Ipv4(v4)) => !crate::net_guard::is_public_ip(&std::net::IpAddr::V4(v4)),
        Some(url::Host::Ipv6(v6)) => !crate::net_guard::is_public_ip(&std::net::IpAddr::V6(v6)),
        Some(url::Host::Domain(_)) => false,
        None => true,
    };
    if non_public {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "origin is a non-public IP literal",
        ));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let token = new_token();
    let state = Arc::new(State {
        origin,
        token: token.clone(),
    });
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
        while let Ok((stream, peer)) = listener.accept().await {
            // Only the process itself may use the door: this
            // server fetches ARBITRARY origins.
            if !peer.ip().is_loopback() {
                continue;
            }
            let state = Arc::clone(&state);
            tokio::spawn(serve(stream, state));
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
    // Capability: /<token>/stream, tolerating a query string (cache-
    // busters); souphttpsrc sends none today, cost nothing to accept.
    let path = path.split(['?', ';']).next().unwrap_or(path);
    let token_ok = path
        .strip_prefix('/')
        .and_then(|rest| rest.strip_suffix("/stream"))
        .map(|t| ct_eq(t.as_bytes(), state.token.as_bytes()))
        .unwrap_or(false);
    if !token_ok {
        let _ = simple(&mut wr, 404).await;
        return;
    }
    // Range header, passed verbatim (playbin seeks with "bytes=N-").
    let range = text
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("range")
                .then(|| v.trim().to_string())
        })
        .filter(|v| !v.is_empty());

    // THE guarded fetch — the only egress in this module.
    let mut req = crate::media::client().get(&state.origin);
    if let Some(r) = &range {
        req = req.header("Range", r);
    }
    let mut res = match req.send().await {
        Ok(res) => res,
        Err(e) => {
            eprintln!(
                "[video] proxy origin fetch failed for {}: {e}",
                state.origin
            );
            let _ = simple(&mut wr, 502).await;
            return;
        }
    };
    if !res.status().is_success() {
        // Mirror the verdict — 404/410 stay precise for the demuxer.
        let _ = simple(&mut wr, res.status().as_u16()).await;
        return;
    }
    // Demuxer-relevant headers, passed through; the rest is noise. The
    // STATUS rides verbatim — souphttpsrc MUST see the origin's 206 for
    // a ranged seek (with its Content-Range), or qtdemux treats the
    // bytes as file-start and playback stalls or corrupts.
    let status = res.status().as_u16();
    let head_out = format!(
        "HTTP/1.1 {} {}\r\nConnection: close\r\n{}\r\n",
        status,
        reason(status),
        [
            (
                "Accept-Ranges",
                Some(header(&res, "accept-ranges").unwrap_or_else(|| "bytes".into()))
            ),
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

/// Canonical reason phrase for the statuses the proxy can emit or
/// forward (labeled verbatim-status passthrough: the CODE is what
/// souphttpsrc reads; the phrase is for humans).
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        206 => "Partial Content",
        226 => "IM Used",
        304 => "Not Modified",
        404 => "Not Found",
        416 => "Range Not Satisfiable",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

async fn simple(wr: &mut tokio::net::tcp::OwnedWriteHalf, status: u16) -> std::io::Result<()> {
    wr.write_all(
        format!(
            "HTTP/1.1 {} {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            status,
            reason(status)
        )
        .as_bytes(),
    )
    .await?;
    wr.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability gate, end to end and offline: a proxy bound on
    /// loopback refuses a wrong-token probe with 404 (the gate is the
    /// security boundary — the origin fetch path is media.rs's, tested
    /// there). Exercises spawn → uri → serve → 404 without any origin
    /// network traffic.
    /// ct_eq: equal bytes → true; any mismatch → false, regardless of
    /// position (the property that kills a prefix oracle).
    #[test]
    fn ct_eq_holds() {
        let t = "0123456789abcdef0123456789abcdef";
        assert!(ct_eq(t.as_bytes(), t.as_bytes()));
        assert!(!ct_eq(t.as_bytes(), b"1123456789abcdef0123456789abcdef"));
        assert!(!ct_eq(t.as_bytes(), b"0123456789abcdef0123456789abcdeF"));
        assert!(!ct_eq(b"short", b"shorter"));
        assert!(ct_eq(b"", b""));
    }

    /// Literal origins screen by value at the facade: a feed-supplied
    /// video URL pointing at loopback / RFC1918 / ULA must refuse before
    /// any listener exists (media::client's pinning covers names; hyper
    /// skips the resolver for an IP literal). Unparseable origins fail
    /// closed too.
    #[test]
    fn spawn_refuses_non_public_literal_origins() {
        for origin in [
            "http://127.0.0.1:9/x.mp4",
            "http://192.168.1.10/x.mp4",
            "http://[::1]:9/x.mp4",
            "http://[fc00::5]/x.mp4",
            "http://169.254.169.254/latest/x.mp4",
            "not a url",
        ] {
            let res = spawn(origin.into());
            assert!(res.is_err(), "{origin} must refuse at the facade");
        }
        // A public literal is fine (rare, but legal).
        assert!(spawn("https://8.8.8.8/x.mp4".into()).is_ok());
    }

    /// Live-network probe (run: cargo test -p satori-core -- --ignored
    /// --nocapture). The playback smoke test's socket half: spawn the
    /// proxy on a real public video origin and GET through the loopback
    /// door in exactly the shape souphttpsrc sends — an initial GET, then
    /// a seek's Range re-GET — asserting the 206/Content-Range passthrough
    /// is verbatim and the bytes match a direct fetch. The gst half
    /// (playbin actually renders) stays the human click test.
    #[tokio::test]
    #[ignore = "network"]
    async fn range_proxy_serves_a_live_origin_end_to_end() {
        const ORIGIN: &str = "https://test-videos.co.uk/vids/bigbuckbunny/mp4/h264/360/Big_Buck_Bunny_360_10s_1MB.mp4";

        // The direct comparison: the same client, same range, no proxy.
        let direct = crate::media::client()
            .get(ORIGIN)
            .header("Range", "bytes=0-65535")
            .send()
            .await
            .expect("direct origin fetch");
        assert_eq!(direct.status().as_u16(), 206, "origin serves ranges");
        let direct_bytes = direct.bytes().await.expect("direct body").to_vec();
        assert_eq!(direct_bytes.len(), 65_536);

        let proxy =
            crate::video_proxy::spawn(ORIGIN.to_string()).expect("proxy on a domain origin");
        let uri = proxy.uri();
        assert!(uri.starts_with("http://127.0.0.1:"), "{uri}");

        // Initial GET, no Range: 200 + a Content-Length the body honors.
        let (status, headers, body) = get_through(&uri, None).await;
        assert_eq!(status, 200, "initial GET through the proxy");
        let len: usize = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse().ok())
            .expect("Content-Length passes through");
        assert_eq!(body.len(), len, "body honors the declared length");

        // The seek: Range re-GET rides verbatim — 206, Content-Range,
        // and the SAME bytes a direct fetch of that range returns.
        let (status, headers, body) = get_through(&uri, Some("bytes=0-65535")).await;
        assert_eq!(status, 206, "Range GET rides upstream verbatim");
        assert!(
            headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("content-range")
                    && v.starts_with("bytes 0-65535/")),
            "Content-Range passes through, got {headers:?}"
        );
        assert_eq!(body, direct_bytes, "proxied bytes equal direct bytes");

        // A seek into the middle: byte identity is position-honest.
        let (_, _, mid) = get_through(&uri, Some("bytes=524288-589823")).await;
        let direct_mid = crate::media::client()
            .get(ORIGIN)
            .header("Range", "bytes=524288-589823")
            .send()
            .await
            .expect("direct mid fetch");
        assert_eq!(direct_mid.status().as_u16(), 206);
        assert_eq!(
            mid,
            direct_mid.bytes().await.expect("direct mid body").to_vec(),
            "mid-range bytes equal direct bytes"
        );
    }

    /// One plain-HTTP GET through the loopback door (raw TCP: the point
    /// is the wire contract playbin sees, not a client's interpretation
    /// of it). Returns status, headers, body.
    async fn get_through(uri: &str, range: Option<&str>) -> (u16, Vec<(String, String)>, Vec<u8>) {
        // http://127.0.0.1:<port>/<token>/stream
        let rest = uri.trim_start_matches("http://");
        let (authority, path) = rest.split_once('/').expect("uri has a path");
        let mut req = format!("GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
        if let Some(r) = range {
            req.push_str(&format!("Range: {r}\r\n"));
        }
        req.push_str("\r\n");

        let mut s = tokio::net::TcpStream::connect(authority)
            .await
            .expect("connect to the proxy");
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        s.write_all(req.as_bytes()).await.expect("write request");
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).await.expect("read response");

        let header_end = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("response has a head");
        let head = String::from_utf8_lossy(&raw[..header_end]);
        let status: u16 = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse().ok())
            .expect("status line");
        let headers: Vec<(String, String)> = head
            .lines()
            .skip(1)
            .filter_map(|l| l.split_once(": "))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        (status, headers, raw[header_end + 4..].to_vec())
    }

    #[test]
    fn capability_gate_refuses_wrong_token() {
        let proxy = spawn("https://blossom.example.invalid/x.mp4".into()).expect("loopback bind");
        let uri = proxy.uri();
        assert!(uri.starts_with("http://127.0.0.1:"), "{uri}");
        assert!(uri.ends_with("/stream"));
        // The token in the middle is 32 hex chars (128 bits): strip
        // scheme+host, then take the FIRST path segment (trim_start_matches
        // strips REPEATED prefixes and split().find() would hand back the
        // port — both wrong-door bugs in the test itself).
        let rest = uri
            .trim_start_matches("http://")
            .split_once('/')
            .expect("uri has a path")
            .1;
        let token = rest.split('/').next().unwrap();
        assert_eq!(token.len(), 32, "{token}");
        assert!(token.bytes().all(|c| c.is_ascii_hexdigit()));

        runtime().block_on(async {
            // A probe without the token: /stream, /wrongtoken/stream.
            let mut c = tokio::net::TcpStream::connect(proxy.addr).await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            c.write_all(b"GET /stream HTTP/1.1\r\nHost: x\r\n\r\n")
                .await
                .unwrap();
            let mut buf = Vec::new();
            loop {
                let mut b = [0u8; 512];
                let n = c.read(&mut b).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&b[..n]);
            }
            let text = String::from_utf8_lossy(&buf);
            assert!(text.starts_with("HTTP/1.1 404"), "{text}");

            // The RIGHT token reaches the origin path — which fails on
            // the .invalid domain → the honest 502, not a silent win.
            // A cache-buster query rides along (soup sends none today;
            // the gate must not care).
            let mut c = tokio::net::TcpStream::connect(proxy.addr).await.unwrap();
            c.write_all(format!("GET /{token}/stream?cb=1 HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut buf = Vec::new();
            loop {
                let mut b = [0u8; 512];
                let n = c.read(&mut b).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&b[..n]);
            }
            let text = String::from_utf8_lossy(&buf);
            assert!(text.starts_with("HTTP/1.1 502"), "{text}");
        });
    }
}
