//! net_guard: the ONE ruleset for "is this network address connectable".
//!
//! Issue #11: the wallet (nwc) and bunker (nostr-connect) clients build
//! their own transports, outside the pool's screens. Their relay URLs are
//! user-supplied at paste time, but a hostile or mistaken URI can point the
//! app's auth traffic at loopback, RFC1918, or the 169.254.169.254 metadata
//! endpoint. Everything connectable passes `routable_relay` — the SAME
//! screen the pool's routing uses (no second ruleset, per the ticket).
//!
//! Moved here from outbox.rs (which re-exports for its callers): the
//! url-crate parse canonicalizes sneaky IPv4 spellings (decimal
//! `2130706433`, hex `0x7f.0.0.1`, short `127.1`) before the literal
//! check, so they cannot pose as names.

use nostr::prelude::*;

/// Public unicast only: rejects this-network, RFC1918, loopback, link-local
/// (incl. the 169.254.169.254 metadata endpoint), CGNAT, multicast and
/// reserved ranges; IPv6 additionally unique-local/link-local/multicast,
/// with v4-mapped addresses judged by their embedded v4.
pub fn is_public_ip(ip: &std::net::IpAddr) -> bool {
    match *ip {
        std::net::IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            !(a == 0
                || a == 10
                || a == 127
                || (a == 169 && b == 254)
                || (a == 172 && (b & 0xf0) == 16)
                || (a == 192 && b == 168)
                || (a == 100 && (b & 0xc0) == 64)
                || a >= 224)
        }
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments();
            if seg[..5] == [0, 0, 0, 0, 0] && seg[5] == 0xffff {
                let v4 = std::net::Ipv4Addr::new(
                    (seg[6] >> 8) as u8,
                    seg[6] as u8,
                    (seg[7] >> 8) as u8,
                    (seg[7]) as u8,
                );
                return is_public_ip(&std::net::IpAddr::V4(v4));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || (seg[0] & 0xffc0) == 0xfe80
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xff00) == 0xff00)
        }
    }
}

/// The relay screen every connectable url passes: wss only, public IP
/// literals only. Domains pass for now; pinning resolved IPs per hop is
/// the SSRF-guard M5 item (the same gap the media guard carries).
pub fn routable_relay(url: &str) -> bool {
    // RelayUrl::parse check 1: at most one "://".
    if url.matches("://").count() > 1 {
        return false;
    }
    // Check 2: a real url-crate parse (schemes, brackets, ports, IPv4 forms).
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    // Check 3: the scheme (RelayUrlScheme accepts ws/wss; routed relays are
    // wss-only).
    if parsed.scheme() != "wss" {
        return false;
    }
    // Check 4: a host is present ("wss://" is Malformed there).
    let Some(host) = parsed.host() else {
        return false;
    };
    // Screen 5 (ours): IP literals must be public unicast. host() is the
    // typed form (host_str brackets IPv6, which then fails IpAddr::from_str
    // and would pose as a name), and the url crate already canonicalized
    // decimal/hex/short IPv4 into it.
    match host {
        url::Host::Ipv4(v4) => is_public_ip(&std::net::IpAddr::V4(v4)),
        url::Host::Ipv6(v6) => is_public_ip(&std::net::IpAddr::V6(v6)),
        // Domains (incl. IDN) pass for now; resolved-IP pinning is M5.
        url::Host::Domain(_) => true,
    }
}

/// Paste-time screen for a `nostr+walletconnect://` URI (issue #11): every
/// relay it asks the wallet transport to dial must pass the same
/// `routable_relay` screen the pool's routing uses. Err carries the
/// human-readable verdict for the connect dialog.
pub fn screen_nwc_uri(uri: &str) -> Result<(), String> {
    let parsed = nostr::nips::nip47::NostrWalletConnectUri::parse(uri.trim())
        .map_err(|e| format!("not a valid nostr+walletconnect:// URI: {e}"))?;
    for relay in &parsed.relays {
        let url = relay.as_str();
        if !routable_relay(url) {
            return Err(format!(
                "the URI's relay is not connectable: {url} — private addresses are refused"
            ));
        }
    }
    Ok(())
}

/// Paste-time screen for a `bunker://` URI (issue #11): same screen, the
/// relay list the nostrconnect transport would dial. nostrconnect:// is
/// deliberately not ours (D27: kumaOS owns that verb).
pub fn screen_bunker_uri(uri: &str) -> Result<(), String> {
    let parsed = nostr::prelude::NostrConnectUri::parse(uri.trim())
        .map_err(|e| format!("not a valid bunker:// URI: {e}"))?;
    let NostrConnectUri::Bunker { relays, .. } = &parsed else {
        return Err("expected bunker:// URI".into());
    };
    for relay in relays {
        let url = relay.as_str();
        if !routable_relay(url) {
            return Err(format!(
                "the URI's relay is not connectable: {url} — private addresses are refused"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_screens_refuse_private_relays() {
        // The hostile spellings the routable screen catches, carried by a
        // real NIP-47 URI.
        let hostile = [
            "127.0.0.1",
            "2130706433", // decimal loopback
            "0x7f.0.0.1", // hex loopback
            "127.1",      // short loopback
            "192.168.1.10",
            "10.0.0.5",
            "172.16.0.9",
            "169.254.169.254", // the metadata endpoint
            "100.64.0.1",      // CGNAT
            "[::1]",
            "[fd00::1]", // ULA
            "[fe80::1]", // link-local
        ];
        for host in hostile {
            let uri = format!(
                "nostr+walletconnect://b889ff5b1513b641e2a139f661a661364979c5beee91842f8f0ef42ab558e9d4?secret=71a8c14c1407c113601079c4302dab36460f0ccd0ad506f1f2dc73b5100e4f3c&relay=wss%3A%2F%2F{host}"
            );
            let err = screen_nwc_uri(&uri).unwrap_err();
            assert!(err.contains("not connectable"), "host {host}: {err}");
        }
    }

    #[test]
    fn uri_screens_pass_public_relays() {
        // The stock test URI (relay.damus.io) passes; a public IP literal
        // passes too.
        let uri = "nostr+walletconnect://b889ff5b1513b641e2a139f661a661364979c5beee91842f8f0ef42ab558e9d4?secret=71a8c14c1407c113601079c4302dab36460f0ccd0ad506f1f2dc73b5100e4f3c&relay=wss%3A%2F%2Frelay.damus.io";
        assert!(screen_nwc_uri(uri).is_ok());
    }

    #[test]
    fn uri_screens_reject_garbage() {
        assert!(screen_nwc_uri("nostr+walletconnect://nope").is_err());
        assert!(screen_bunker_uri("nostrconnect://not-ours").is_err());
    }
}
