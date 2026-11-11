//! `nostr:` URI handling (NIP-21) — the argv/socket seam and the parser.
//!
//! The .desktop file declares `x-scheme-handler/nostr` (D25), so GNOME hands
//! a `nostr:nevent1…` argv to the wrapper. This module turns that into a
//! routing target for the shell — and solves the "already running" question
//! with a single-instance socket: a second launch forwards its URI to the
//! primary and exits; the primary pumps the channel into the window root.
//!
//! The socket lives at `$XDG_RUNTIME_DIR/satori.sock` (per-session tmpfs —
//! no cleanup across reboots, no cross-user collisions). Connect-first: if
//! anything answers, we are the second instance. Otherwise a stale file
//! (crashed primary) is removed and this process binds and serves.
//!
//! TODO(port): crates/app (GTK) never had this — desktop-only seam, new here.

use std::{
    io::{Read as _, Write as _},
    os::unix::net::UnixStream,
    path::PathBuf,
};

use nostr::prelude::*;
use tokio::sync::mpsc::UnboundedReceiver;

/// Where a NIP-21 URI points, reduced to Satori's three navigation targets
/// (shell.rs's open_thread / open_profile / open_article).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UriTarget {
    /// note1… / nevent1… — a thread.
    Thread(EventId),
    /// npub1… / nprofile1… — a profile.
    Profile(PublicKey),
    /// naddr1… — an addressable event: resolve (kind, author, d) against
    /// relays first, then open what came back.
    Article {
        kind: u16,
        author: PublicKey,
        identifier: String,
    },
}

/// Parse a NIP-21 URI (`nostr:npub1…`) — or a bare bech32 entity, which
/// some apps strip the scheme from. Scheme case-insensitive (NIP-21 says
/// "applications MUST NOT impose case-sensitivity on the scheme").
/// `None` for everything else: https links, `nostrconnect://` (kumaOS owns
/// that verb), garbage.
pub fn uri_target(raw: &str) -> Option<UriTarget> {
    let uri = raw.trim();
    // Nip21::parse wants the exact lowercase `nostr:` prefix — normalize,
    // and treat a scheme-less bech32 as a URI with the prefix added.
    let normalized = match uri.find(':') {
        Some(i) if uri[..i].eq_ignore_ascii_case("nostr") => {
            format!("nostr:{}", &uri[i + 1..])
        }
        // A colon under any other scheme (nostrconnect://, https://) is
        // deliberately not ours.
        Some(_) => return None,
        None => format!("nostr:{uri}"),
    };
    match Nip21::parse(&normalized) {
        Ok(n) => Some(match n {
            Nip21::Pubkey(pk) => UriTarget::Profile(pk),
            Nip21::Profile(p) => UriTarget::Profile(p.public_key),
            Nip21::EventId(id) => UriTarget::Thread(id),
            Nip21::Event(e) => UriTarget::Thread(e.event_id),
            Nip21::Coordinate(c) => UriTarget::Article {
                kind: c.coordinate.kind.as_u16(),
                author: c.coordinate.public_key,
                identifier: c.coordinate.identifier,
            },
        }),
        Err(_) => None,
    }
}

/// What the single-instance handshake decided for this process.
pub enum Instance {
    /// This process is the primary. `Some(rx)` while the socket is served;
    /// `None` when IPC is unavailable (no `XDG_RUNTIME_DIR`, lost bind race)
    /// — the app still runs, URIs just won't follow it in.
    Primary(Option<UnboundedReceiver<String>>),
    /// A live primary owns the socket: the argv URI (if any) was written to
    /// it and this process should exit immediately.
    Forwarded,
}

fn socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|dir| {
        let mut p = PathBuf::from(dir);
        p.push("satori.sock");
        p
    })
}

/// Connect-first: forward `uri` to the running primary and report
/// `Forwarded`, or take the socket and serve it.
pub fn acquire(uri: Option<&str>) -> Instance {
    let Some(path) = socket_path() else {
        return Instance::Primary(None);
    };
    // Connect-first: a live listener answers immediately or the OS returns
    // ECONNREFUSED — an AF_UNIX connect never parks the way TCP can.
    if let Ok(mut stream) = UnixStream::connect(&path) {
        if let Some(uri) = uri {
            let _ = stream.write_all(uri.as_bytes());
        }
        return Instance::Forwarded;
    }
    // Nobody answered: stale file from a crashed primary, or nothing. Take
    // ownership (bind fails only if a simultaneous launch won the race —
    // then run unowned rather than not at all).
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = std::os::unix::net::UnixListener::bind(&path) else {
        return Instance::Primary(None);
    };
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let _ = std::thread::Builder::new()
        .name("satori-uri".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                // One connection = one message: the peer writes its URI and
                // exits, so EOF completes the read. An empty message is a
                // bare "activate" from a URI-less launch.
                let mut stream = stream;
                let mut buf = String::new();
                let msg = match stream.read_to_string(&mut buf) {
                    Ok(_) => buf,
                    Err(_) => String::new(),
                };
                if tx.send(msg).is_err() {
                    break; // the app side is gone; stop serving
                }
            }
        });
    Instance::Primary(Some(rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed test key (32 bytes of hex) — only its encodings matter here.
    const HEX: &str = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";

    fn pk() -> PublicKey {
        PublicKey::from_hex(HEX).unwrap()
    }

    /// Every NIP-21 form routes to the right target: scheme-cased both
    /// ways, bare bech32 (scheme stripped by the sender), nevent/note.
    #[test]
    fn nip21_forms_route_to_targets() {
        let key = pk();
        let npub = key.to_bech32().unwrap();
        let note = EventId::from_byte_array([7u8; 32]).to_bech32().unwrap();
        // to_nostr_uri returns the FULL nostr:-prefixed form.
        let nevent = Nip19Event {
            event_id: EventId::from_byte_array([7u8; 32]),
            author: Some(key),
            kind: None,
            relays: vec![],
        }
        .to_nostr_uri()
        .unwrap();

        for raw in [
            format!("nostr:{npub}"),
            format!("NOSTR:{npub}"),
            npub.clone(),
            format!("nostr:{note}"),
            nevent.clone(),
        ] {
            match uri_target(&raw) {
                Some(UriTarget::Thread(_)) => {
                    assert!(raw.contains("note") || raw.contains("event"));
                }
                Some(UriTarget::Profile(who)) => assert_eq!(who, key, "{raw}"),
                other => panic!("{raw} routed as {other:?}"),
            }
        }
    }

    /// Foreign schemes and garbage are rejected, never half-routed.
    #[test]
    fn foreign_and_garbage_rejected() {
        for raw in [
            "https://example.com/nostr:note1abc",
            "nostrconnect://inky/secret",
            "nostr:",
            "nostr:https://example.com",
            "",
            "hello world",
        ] {
            assert!(uri_target(raw).is_none(), "{raw:?} must not parse");
        }
    }

    /// The full shape round-trips: a naddr keeps its (kind, author, d).
    #[test]
    fn naddr_keeps_coordinate_parts() {
        let coord = Coordinate {
            kind: Kind::LongFormTextNote,
            public_key: pk(),
            identifier: "slug".into(),
        };
        let uri = Nip19Coordinate {
            coordinate: coord,
            relays: vec![],
        }
        .to_nostr_uri()
        .unwrap();
        match uri_target(&uri) {
            Some(UriTarget::Article {
                kind,
                author,
                identifier,
            }) => {
                assert_eq!(kind, 30023);
                assert_eq!(author, pk());
                assert_eq!(identifier, "slug");
            }
            other => panic!("naddr must route as Article, got {other:?}"),
        }
    }

    /// nprofile carries the author too (the parser collapses both npub and
    /// nprofile into the same target).
    #[test]
    fn nprofile_routes_as_profile() {
        let profile_uri = Nip19Profile {
            public_key: pk(),
            relays: vec![],
        }
        .to_nostr_uri()
        .unwrap();
        match uri_target(&profile_uri) {
            Some(UriTarget::Profile(who)) => assert_eq!(who, pk()),
            other => panic!("nprofile must route as Profile, got {other:?}"),
        }
    }
}
