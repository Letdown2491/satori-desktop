//! net_guard: the ONE ruleset for "is this network address connectable".
//!
//! Two consumers, one authority (M5 WP1 reconciliation: this file used to
//! exist twice — a paste-time URI screen and a resolver-side screen —
//! with DIFFERENT strictness; they are one module now, so they cannot
//! drift apart again):
//!
//! - paste-time URI screens ([`screen_uri_relays`], issue #11: the
//!   wallet (nwc) and bunker (nostr-connect) clients build their own
//!   transports, outside the pool's screens; their relay URLs are
//!   user-supplied at paste time — wss-only, then resolved and
//!   address-classed concurrently),
//! - dial-time resolution pinning ([`SafeResolver`], moved here from
//!   media.rs; [`resolve_pinned_public`] for the screening transport).
//!
//! The classifier is the STRICT union — a name that resolves to ANY
//! non-public address yields no connectable address (fail closed: a
//! public/private mix never dials the private leg). The address classes
//! cover RFC1918, loopback, link-local (incl. the 169.254.169.254
//! metadata endpoint), CGNAT, this-network, benchmark (198.18/15),
//! documentation (TEST-NET-1/2/3), 192.0.0.0/24, reserved/multicast/
//! broadcast; v6: ULA, link-local, multicast, documentation 2001:db8,
//! everything outside 2000::/3, v4-mapped judged by the embedded v4,
//! and the transition mechanisms (NAT64 64:ff9b::/96, 6to4 2002::/16,
//! Teredo 2001::/32) screened through their embedded IPv4 — both
//! Teredo legs, the client obfuscated per RFC 4380.
//!
//! The url-crate parse canonicalizes sneaky IPv4 spellings (decimal
//! `2130706433`, hex `0x7f.0.0.1`, short `127.1`) before the literal
//! check, so they cannot pose as names.

use nostr::prelude::*;
use serde_json::json;
use std::collections::HashSet;
use std::net::{IpAddr, ToSocketAddrs};

/// Public unicast only — THE authority. Strict side: a block is asserted
/// only when the address is demonstrably not globally reachable, so a
/// stricter new rule lands here and every caller (URI screens, resolver,
/// relay routing) tightens together.
pub fn is_public_ip(ip: &std::net::IpAddr) -> bool {
    match *ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || o[0] == 0 // this-network (0.0.0.0/8)
                || o[0] == 100 // shared address space (CGNAT, 100.64/10)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // 198.18/15 benchmark
                || o[0] >= 240) // reserved + broadcast
        }
        std::net::IpAddr::V6(v6) => {
            if v6.is_multicast() || v6.is_loopback() || v6.is_unspecified() {
                return false;
            }
            let s = v6.segments();
            // ::ffff:0:0/96 (v4-mapped) dials the embedded v4 on a
            // dual-stack host — screen THAT address.
            if s[..5] == [0, 0, 0, 0, 0] && s[5] == 0xffff {
                return is_public_ip(&std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                    (s[6] >> 8) as u8,
                    s[6] as u8,
                    (s[7] >> 8) as u8,
                    s[7] as u8,
                )));
            }
            // Global unicast lives in 2000::/3; ULA (fc00::/7),
            // link-local (fe80::/10) and everything experimental dies.
            // Transition mechanisms embed IPv4 INSIDE 2000::/3 — screen
            // the embedded leg too, or 64:ff9b::192.0.2.9-style answers
            // ride a NAT64 gateway onto the internal network:
            // - 64:ff9b::/96 (well-known NAT64): v4 in segments 6+7
            // - 2002::/16 (6to4): v4 in segment 1 (hi) + 2 (lo)
            // - 2001::/32 (Teredo): server v4 in segments 2+3, client
            //   v4 obfuscated (XOR 0xffff) in segments 6+7 — screen BOTH
            if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0
            {
                return is_public_ip(&std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                    (s[6] >> 8) as u8,
                    s[6] as u8,
                    (s[7] >> 8) as u8,
                    s[7] as u8,
                )));
            }
            if s[0] == 0x2002 {
                return is_public_ip(&std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                    (s[1] >> 8) as u8,
                    s[1] as u8,
                    (s[2] >> 8) as u8,
                    s[2] as u8,
                )));
            }
            if s[0] == 0x2001 && s[1] == 0 {
                let server = is_public_ip(&std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                    (s[2] >> 8) as u8,
                    s[2] as u8,
                    (s[3] >> 8) as u8,
                    s[3] as u8,
                )));
                let client = is_public_ip(&std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                    !(s[6] >> 8) as u8,
                    !(s[6] as u8),
                    !(s[7] >> 8) as u8,
                    !(s[7] as u8),
                )));
                return server && client;
            }
            (s[0] & 0xe000) == 0x2000 && !(s[0] == 0x2001 && s[1] == 0x0db8)
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
    // decimal/hex/short IPv4 into it. The authority also sees through the
    // transition mechanisms: a NAT64/6to4/Teredo literal whose embedded
    // IPv4 is internal is refused here, at paste time.
    match host {
        url::Host::Ipv4(v4) => is_public_ip(&std::net::IpAddr::V4(v4)),
        url::Host::Ipv6(v6) => is_public_ip(&std::net::IpAddr::V6(v6)),
        // Domains (incl. IDN) pass for now; resolved-IP pinning is M5.
        url::Host::Domain(_) => true,
    }
}

// --- dial-time resolution pinning (WP1) ------------------------------------

/// The DNS-rebinding-safe resolver (issue #3 WS4). The URL guard checks
/// HOST STRINGS — `evil.example.com` resolving to 127.0.0.1 sailed past
/// it. reqwest dials WHATEVER its resolver returns, so the check lives on
/// the address list: a name that resolves anywhere non-global yields NO
/// connectable address (fail closed — a public and private mix fails the
/// whole resolution, so no leg of it leaks onto the internal network).
/// Every connection this client opens — first party, every redirect hop,
/// every retry — re-walks here, so a rebinding rotation between hops
/// re-runs the check. TOCTOU-free by construction: the addresses that
/// passed the filter are the addresses hyper dials.
#[derive(Debug)]
pub struct SafeResolver;

impl reqwest::dns::Resolve for SafeResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            // Port 0: reqwest replaces it with the scheme's conventional
            // port (the trait contract).
            let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(std::io::Error::other)?
                .filter(|a| is_public_ip(&a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("{host} resolved to no public addresses"),
                )
                .into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Resolve `(host, port)` and hand back ONLY public-unicast addresses.
/// The dial path's front door for transports that resolve on their own
/// (the screening transport, M5 WP2): same fail-closed mix rule as the
/// resolver — one internal answer poisons the name.
pub async fn resolve_pinned_public(
    host: &str,
    port: u16,
) -> Result<Vec<std::net::SocketAddr>, NetGuardError> {
    // tokio::net::lookup_host needs a running reactor (the pool callers
    // run inside core's runtime) — but the video overlay's pre-dial check
    // runs on GPUI's executor, where the first lookup panicked the whole
    // app (there is no reactor running). Inside a runtime, the async
    // lookup; outside it, std's getaddrinfo inline: that path is already
    // on a background executor (the tap spawns it off the UI thread), and
    // a blocking resolve there costs what the playbin dial would anyway.
    if tokio::runtime::Handle::try_current().is_ok() {
        let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| NetGuardError::Resolve(host.to_string()))?
            .collect();
        screened(host, addrs)
    } else {
        let addrs: Vec<std::net::SocketAddr> = (host, port)
            .to_socket_addrs()
            .map_err(|_| NetGuardError::Resolve(host.to_string()))?
            .collect();
        screened(host, addrs)
    }
}

/// The screen, split from the resolution: every answer public, else fail
/// closed on the WHOLE name (no partial dials).
fn screened(
    host: &str,
    addrs: Vec<std::net::SocketAddr>,
) -> Result<Vec<std::net::SocketAddr>, NetGuardError> {
    if addrs.is_empty() {
        return Err(NetGuardError::Resolve(host.to_string()));
    }
    if !addrs.iter().all(|a| is_public_ip(&a.ip())) {
        // Fail closed on the WHOLE name: no partial dials.
        return Err(NetGuardError::NonPublic(host.to_string()));
    }
    Ok(addrs)
}

/// Why a name was refused. `Resolve` is weather (DNS down, no answers);
/// `NonPublic` is the guard speaking (an internal address answered).
#[derive(Debug, PartialEq)]
pub enum NetGuardError {
    Resolve(String),
    NonPublic(String),
}

impl std::fmt::Display for NetGuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resolve(host) => write!(f, "{host}: no answer"),
            Self::NonPublic(host) => {
                write!(f, "{host}: resolved to a non-public address")
            }
        }
    }
}

// --- the user-relay exemption --------------------------------------------

/// Hosts the USER chose (Settings entries, the session's own NIP-65,
/// login-time bunker/wallet relays): dial direct, no screening — the
/// user may deliberately point at a LAN relay (`wss://umbrel.local`,
/// `ws://192.168.1.10:7777`). Remote data (kind:10002s, hints) can
/// never join this set; its members ride the screening path. Consumers
/// land with the screening transport (M5 WP2).
fn direct_hosts() -> &'static std::sync::RwLock<std::collections::HashSet<String>> {
    use std::collections::HashSet;
    use std::sync::{OnceLock, RwLock};
    static SET: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();
    // Consent persists across restarts (review Tier 2.1): the ledger file
    // is the one source, reloaded lazily on first access.
    SET.get_or_init(|| RwLock::new(read_ledger_file()))
}

fn ledger_path() -> std::path::PathBuf {
    crate::config::state_file(crate::config::files::DIRECT_RELAYS)
}

fn read_ledger_file() -> HashSet<String> {
    std::fs::read_to_string(ledger_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .map(|hosts| hosts.into_iter().collect())
        .unwrap_or_default()
}

fn write_ledger_file(set: &HashSet<String>) {
    let mut hosts: Vec<&String> = set.iter().collect();
    hosts.sort();
    let body = json!(hosts).to_string();
    // House convention (secrets.rs, the audit-L3 shape): temp sibling +
    // rename. Plain create() FOLLOWS a pre-planted symlink, and the mode
    // flag is a no-op on an existing file.
    let _ = std::fs::create_dir_all(crate::config::data_dir());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = ledger_path().with_extension(format!("tmp-{}-{nanos}", std::process::id()));
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
    else {
        return;
    };
    if f.write_all(body.as_bytes()).is_ok() {
        drop(f);
        let _ = std::fs::rename(&tmp, ledger_path());
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Snapshot of the exempted hosts, sorted — the relay settings pane's
/// revocation list (review Tier 2.1).
pub fn direct_relays() -> Vec<String> {
    let mut hosts: Vec<String> = direct_hosts().read().unwrap().iter().cloned().collect();
    hosts.sort();
    hosts
}

/// Exempt relay hosts from the screening path (bulk): the SAME ledger,
/// ONE file write for the batch — the wallet resume re-seed and the
/// dialogs' allow-and-retry all land here, so the write-amplification
/// and the allow-loop copies die with it. Sentinel "hosts" from
/// unparseable verdicts must never reach the ledger, whatever the UI
/// does (review Tier 2.6).
pub fn allow_direct_relays<'a, I>(hosts: I)
where
    I: IntoIterator<Item = &'a str>,
{
    let mut dirty = false;
    {
        let mut set = direct_hosts().write().unwrap();
        for host in hosts {
            let host = normalize_host(host);
            if host.is_empty() || host.starts_with('(') {
                continue;
            }
            dirty |= set.insert(host);
        }
    }
    if dirty {
        write_ledger_file(&direct_hosts().read().unwrap().clone());
    }
}

/// Exempt a relay host from the screening path (single host — the bulk
/// form's degenerate case, one write).
pub fn allow_direct_relay(host: &str) {
    allow_direct_relays([host]);
}

/// The refused half of a screening round: the one home for what counts
/// as a refusal, so the three screening callers (bunker connect, NWC
/// connect, NWC resume) can never disagree on the classification.
pub fn refused_verdicts(verdicts: &[RelayVerdict]) -> Vec<RelayVerdict> {
    verdicts
        .iter()
        .filter(|v| matches!(v.screen, RelayScreen::Refused(_)))
        .cloned()
        .collect()
}

/// Drop an exemption (relay retired from the session's lists).
pub fn disallow_direct_relay(host: &str) {
    direct_hosts()
        .write()
        .unwrap()
        .remove(&normalize_host(host));
    write_ledger_file(&direct_hosts().read().unwrap().clone());
}

/// Does this relay URL belong to the user-chosen set (direct dial)?
/// Anything NOT in the set must ride the screening transport.
pub fn user_chosen_relay(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    match parsed.host_str() {
        Some(host) => direct_hosts()
            .read()
            .unwrap()
            .contains(&normalize_host(host)),
        None => false,
    }
}

fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_end_matches('.')
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

// --- paste-time URI relay screening (issue #11) ----------------------------

// The NWC wallet and bunker clients build their own transports, outside the
// pool's `Proxy::custom` screening — their relays come from a URI the user
// PASTED, so the screen must run at paste/connect time, before anything
// dials. Same ruleset, same ledger: exemption is `allow_direct_relay` (the
// consent record the connect dialogs write), the address test is
// [`is_public_ip`]. No second ruleset.

/// How one pasted URI relay classified at paste-time screening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayScreen {
    /// Every resolved answer was public unicast (no answers = dial weather;
    /// the connect attempt reports it honestly).
    Public,
    /// The host sits in the user-exemption ledger (consent recorded at a
    /// past connect attempt — dial direct, LAN relays included).
    Exempted,
    /// Resolved to at least one non-public address. Fail closed on the
    /// whole name (the `resolve_pinned_public` mix rule): the dialog gets
    /// the offending addresses as evidence for the allow affordance.
    Refused(Vec<IpAddr>),
}

/// One pasted URI relay's verdict (the dialog's line item).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayVerdict {
    pub url: String,
    /// Normalized host key (lowercase, no brackets/dot) — the ledger key.
    pub host: String,
    pub screen: RelayScreen,
}

impl RelayVerdict {
    fn refused(url: &str, host: &str, addrs: Vec<IpAddr>) -> Self {
        Self {
            url: url.to_string(),
            host: host.to_string(),
            screen: RelayScreen::Refused(addrs),
        }
    }
}

/// Screen the relay URLs of a user-pasted URI (NWC `nostr+walletconnect://`,
/// bunker `bunker://`) at paste/connect time. Public/exempted relays pass;
/// relays resolving to RFC1918 / loopback / link-local / CGNAT / ULA (the
/// [`is_public_ip`] rules) are refused so the dialog can surface
/// "this URI wants to talk to 192.168.1.10 — allow?" before any socket
/// opens. DNS weather (resolve errors) is NOT a refusal — the dial fails
/// honestly on its own; this screen refuses ADDRESS classes.
pub async fn screen_uri_relays<'a, I>(urls: I) -> Vec<RelayVerdict>
where
    I: IntoIterator<Item = &'a str>,
{
    // Phase 1, sync: parse and exempt-check everything. Unparseable
    // inside an already-parsed URI: refuse shut.
    let mut verdicts = Vec::new();
    let mut to_resolve: Vec<(String, url::Url)> = Vec::new();
    for url in urls {
        match url::Url::parse(url) {
            Ok(parsed) => {
                let host = parsed.host_str().map(normalize_host);
                if user_chosen_relay(url) {
                    verdicts.push(RelayVerdict {
                        url: url.to_string(),
                        host: host.unwrap_or_default(),
                        screen: RelayScreen::Exempted,
                    });
                } else {
                    to_resolve.push((url.to_string(), parsed));
                }
            }
            Err(_) => verdicts.push(RelayVerdict::refused(url, "(unparseable)", Vec::new())),
        }
    }
    // Phase 2, concurrent: resolve all names with a per-lookup timeout —
    // N URI relays cost the slowest lookup, not the sum, and a dead name
    // cannot park the connect dialog for the OS resolver's full patience.
    let futs = to_resolve.into_iter().map(|(url, parsed)| async move {
        let Some(host) = parsed.host_str() else {
            return RelayVerdict::refused(&url, "(no host)", Vec::new());
        };
        let key = normalize_host(host);
        // One ruleset: only wss dials (D51's routable_relay check 3). A
        // pasted ws:// relay would otherwise hand the wallet/bunker
        // transport a cleartext socket the IP screen never sees.
        if parsed.scheme() != "wss" {
            return RelayVerdict::refused(&url, "(not wss)", Vec::new());
        }
        // Resolve the normalized key: `host_str` keeps the brackets on
        // IPv6 literals, and `lookup_host` wants them bare — and the
        // ledger compares against the same normalized form.
        let screen = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::net::lookup_host((key.as_str(), 0)),
        )
        .await
        {
            Ok(Ok(addrs)) => {
                let refused: Vec<IpAddr> = addrs
                    .map(|a| a.ip())
                    .filter(|ip| !is_public_ip(ip))
                    .collect();
                if refused.is_empty() {
                    RelayScreen::Public
                } else {
                    // Fail closed on the whole name: a public/private mix
                    // never dials the private leg.
                    RelayScreen::Refused(refused)
                }
            }
            // No answer / resolver down / too slow: dial weather, not a
            // refusal.
            _ => RelayScreen::Public,
        };
        RelayVerdict {
            url,
            host: key,
            screen,
        }
    });
    verdicts.extend(futures_util::future::join_all(futs).await);
    verdicts
}

/// The connect dialog's banner line for refused relays (issue #11;
/// review Tier 2.2/2.3): names the relay by scheme+host, the address it
/// answered with, and the scope of the consent — allowing exempts the
/// host for every relay dial, not just this URI. House rule: no em
/// dashes in UI copy. Empty when nothing was refused.
pub fn relay_screen_notice(verdicts: &[RelayVerdict]) -> String {
    let mut lines = Vec::new();
    for v in verdicts {
        if let RelayScreen::Refused(addrs) = &v.screen {
            let shown = addrs
                .first()
                .map(|a| a.to_string())
                .unwrap_or_else(|| "a non-public address".into());
            let target = scheme_host(&v.url).unwrap_or_else(|| v.url.clone());
            let host_part = if v.host == shown {
                target
            } else {
                format!("{target} ({})", v.host)
            };
            lines.push(format!(
                "URI relay {host_part} resolves to {shown}, a private address. Allowing exempts it from relay screening for every dial."
            ));
        }
    }
    lines.join(" ")
}

/// scheme://host[:port] of a relay URL — the notice names the relay
/// without its path noise.
fn scheme_host(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?;
    let mut s = format!("{}://{}", u.scheme(), host);
    if let Some(port) = u.port() {
        s.push_str(&format!(":{port}"));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- the authority: v4 ------------------------------------------------

    #[test]
    fn v4_screen() {
        // Public unicast passes.
        assert!(is_public_ip(&"8.8.8.8".parse().unwrap()));
        assert!(is_public_ip(&"1.1.1.1".parse().unwrap()));
        // Everything the guard exists for dies.
        for bad in [
            "127.0.0.1",       // loopback
            "10.0.0.1",        // RFC1918
            "192.168.1.1",     // RFC1918
            "172.16.0.1",      // RFC1918
            "169.254.169.254", // cloud metadata
            "100.64.0.1",      // CGNAT
            "0.0.0.0",         // this-network
            "192.0.2.1",       // documentation (TEST-NET-1)
            "198.51.100.7",    // documentation (TEST-NET-2)
            "203.0.113.9",     // documentation (TEST-NET-3)
            "198.18.0.1",      // benchmark
            "192.0.0.9",       // 192.0.0.0/24
            "224.0.0.1",       // multicast
            "240.0.0.1",       // reserved
            "255.255.255.255", // broadcast
        ] {
            assert!(
                !is_public_ip(&bad.parse().unwrap()),
                "{bad} must be refused"
            );
        }
    }

    // --- the authority: v6 (incl. transition mechanisms) ------------------

    #[test]
    fn v6_screen() {
        assert!(is_public_ip(&"2606:4700::1111".parse().unwrap()));
        for bad in [
            "::1",              // loopback
            "fe80::1",          // link-local
            "fc00::1",          // ULA
            "ff02::1",          // multicast
            "::ffff:127.0.0.1", // v4-mapped loopback
            "2001:db8::1",      // documentation
            // Transition mechanisms with embedded INTERNAL v4:
            "64:ff9b::192.0.2.9", // NAT64 → TEST-NET-1
            "64:ff9b::127.0.0.1", // NAT64 → loopback
            "2002:7f00:1::1",     // 6to4 → loopback
            "2002:c0a8:101::1",   // 6to4 → 192.168.1.1
            "2001:0:c0a8:101::1", // Teredo server 192.168.1.1
        ] {
            assert!(
                !is_public_ip(&bad.parse().unwrap()),
                "{bad} must be refused"
            );
        }
        // Teredo with an internal CLIENT v4 (obfuscated) also dies —
        // 192.168.1.1 XOR 0xffff = c6:57 → segments 6/7 = 0x39a7,0xfe9f.
        assert!(!is_public_ip(
            &"2001:0:6464:6464:0:0:39a7:fe9f".parse().unwrap()
        ));
        // Transition mechanisms with PUBLIC embedded v4 still pass.
        assert!(is_public_ip(&"64:ff9b::8.8.8.8".parse().unwrap()));
        assert!(is_public_ip(&"2002:0808:0808::1".parse().unwrap()));
    }

    // --- the paste-time URI screens (D51) ----------------------------------

    #[test]
    fn uri_screens_refuse_private_relays() {
        // The hostile spellings the routable screen catches, carried by a
        // real URI relay — the D51 list, plus the transition-mechanism
        // literals the loose screen used to let through (their embedded
        // IPv4 is internal; the authority sees it).
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
            "192.0.2.9",       // TEST-NET-1
            "198.18.0.7",      // benchmark
            "[::1]",
            "[fd00::1]",            // ULA
            "[fe80::1]",            // link-local
            "[64:ff9b::192.0.2.9]", // NAT64 → TEST-NET-1
            "[2002:c0a8:101::1]",   // 6to4 → 192.168.1.1
            "[2001:0:c0a8:101::1]", // Teredo server 192.168.1.1
        ];
        for host in hostile {
            assert!(
                !routable_relay(&format!("wss://{host}")),
                "host {host} must not be connectable"
            );
        }
    }

    #[test]
    fn uri_screens_pass_public_relays() {
        // A public name passes; a transition mechanism with a PUBLIC
        // embedded leg passes — the authority screens the embedded v4,
        // not the prefix.
        assert!(routable_relay("wss://relay.damus.io"));
        assert!(routable_relay("wss://[64:ff9b::8.8.8.8]"));
    }

    // --- the paste-time async screen (screen_uri_relays) --------------------

    #[tokio::test]
    async fn uri_screen_refuses_cleartext_and_unparseable() {
        // One ruleset means the scheme too: a pasted ws:// relay would
        // hand the wallet transport a cleartext socket the IP screen
        // never sees.
        let verdicts = screen_uri_relays(["ws://relay.example", "ftp://relay.example"]).await;
        assert_eq!(verdicts.len(), 2);
        for v in &verdicts {
            assert!(matches!(v.screen, RelayScreen::Refused(_)), "{v:?}");
            assert_eq!(v.host, "(not wss)");
        }
        let verdicts = screen_uri_relays(["not a url \\ ia"]).await;
        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].host, "(unparseable)");
        assert!(matches!(verdicts[0].screen, RelayScreen::Refused(_)));
    }

    #[test]
    fn refused_partition_is_the_one_home() {
        let verdicts = vec![
            RelayVerdict::refused("ws://a", "(not wss)", Vec::new()),
            RelayVerdict {
                url: "wss://b".into(),
                host: "b".into(),
                screen: RelayScreen::Public,
            },
            RelayVerdict {
                url: "wss://c".into(),
                host: "c".into(),
                screen: RelayScreen::Exempted,
            },
        ];
        let refused = refused_verdicts(&verdicts);
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].host, "(not wss)");
    }

    // --- dial-time resolution pinning --------------------------------------

    #[test]
    fn pinned_resolution_fails_closed() {
        // localhost never resolves to a public address — the name is
        // refused outright, whatever the answer list looks like.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let res = resolve_pinned_public("localhost", 8080).await;
            assert!(matches!(
                res,
                Err(NetGuardError::NonPublic(_)) | Err(NetGuardError::Resolve(_))
            ));
        });
    }

    /// The video-overlay crash (user report, D59 cycle): the guard ran on
    /// GPUI's executor, which has NO Tokio reactor — the first
    /// `tokio::net::lookup_host` panicked ("there is no reactor running")
    /// and the unwind under GStreamer's threads took the app down with a
    /// segfault. Poll the guard outside any runtime, the exact shape of
    /// the tap path: it must complete (localhost → refused), not panic.
    #[test]
    fn resolution_without_a_runtime_does_not_panic() {
        use std::task::{Context, Poll};
        let mut fut = Box::pin(resolve_pinned_public("localhost", 8080));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(res) => assert!(matches!(
                res,
                Err(NetGuardError::NonPublic(_)) | Err(NetGuardError::Resolve(_))
            )),
            // The no-runtime path is a blocking resolve — first poll
            // completes it; Pending would mean we regressed into an
            // async lookup we can't drive here.
            Poll::Pending => panic!("no-runtime resolve must not pend"),
        }
    }

    // --- the user-relay exemption ------------------------------------------

    #[test]
    fn user_relay_exemption_roundtrip() {
        let _env = crate::test_env::EnvGuard::new("netguard");
        let url = "wss://umbrel.local:7777";
        assert!(!user_chosen_relay(url));
        allow_direct_relay("UMBREL.local");
        assert!(user_chosen_relay(url), "normalization folds case");
        disallow_direct_relay("umbrel.local");
        assert!(!user_chosen_relay(url));
        // Garbage URLs are never "user chosen".
        assert!(!user_chosen_relay("not a url"));
    }

    // --- paste-time URI relay screening (issue #11) ----------------------

    #[test]
    fn uri_relay_screen_refuses_private_dial_targets() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // IP-literal hosts: lookup_host parses them without DNS, so
            // these verdicts are device-independent.
            let vs = screen_uri_relays([
                "wss://127.0.0.1:7777",
                "wss://10.0.0.7:9735",
                "wss://[::1]:8080",
                "wss://100.64.0.9",
                "wss://8.8.8.8",
            ])
            .await;
            assert_eq!(vs.len(), 5);
            assert!(matches!(vs[0].screen, RelayScreen::Refused(_)), "loopback");
            assert!(matches!(vs[1].screen, RelayScreen::Refused(_)), "RFC1918");
            assert!(
                matches!(vs[2].screen, RelayScreen::Refused(_)),
                "v6 loopback"
            );
            assert!(matches!(vs[3].screen, RelayScreen::Refused(_)), "CGNAT");
            assert_eq!(vs[4].screen, RelayScreen::Public, "public unicast");
            assert_eq!(vs[0].host, "127.0.0.1");

            // The v4-mapped form screens through the v4 rules, same as the
            // dial paths.
            let vs = screen_uri_relays(["wss://[::ffff:192.168.1.1]"]).await;
            assert!(matches!(vs[0].screen, RelayScreen::Refused(_)));
        });
    }

    #[test]
    fn uri_relay_screen_honors_the_consent_ledger() {
        let _env = crate::test_env::EnvGuard::new("netguard");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let url = "wss://127.0.0.1:7777";
            let vs = screen_uri_relays([url]).await;
            assert!(matches!(vs[0].screen, RelayScreen::Refused(_)));
            // The dialog's allow affordance writes the SAME ledger the
            // relay allowlist uses; the retry classifies Exempted.
            allow_direct_relay("127.0.0.1");
            let vs = screen_uri_relays([url]).await;
            assert_eq!(vs[0].screen, RelayScreen::Exempted);
            disallow_direct_relay("127.0.0.1");
        });
    }

    #[test]
    fn relay_screen_notice_names_host_and_address() {
        let vs = vec![RelayVerdict::refused(
            "wss://home-lan.example:7777",
            "home-lan.example",
            vec!["192.168.1.1".parse().unwrap()],
        )];
        let n = relay_screen_notice(&vs);
        // Tier 2.3: the notice carries the relay (scheme+host), not just
        // the host; Tier 2.2: the pool-wide scope, one clause, no em dash.
        assert!(n.contains("wss://home-lan.example:7777"), "{n}");
        assert!(n.contains("home-lan.example"), "{n}");
        assert!(n.contains("192.168.1.1"), "{n}");
        assert!(n.contains("every dial"), "{n}");
        assert!(!n.contains('\u{2014}'), "no em dashes in UI copy: {n}");
        // An IP-literal target doesn't repeat itself.
        let vs = vec![RelayVerdict::refused(
            "wss://192.168.1.10",
            "192.168.1.10",
            vec!["192.168.1.10".parse().unwrap()],
        )];
        let n = relay_screen_notice(&vs);
        assert!(n.contains("wss://192.168.1.10"), "{n}");
        // An IP-literal host gets no redundant "(ip)" parenthetical.
        assert!(!n.contains("(192.168.1.10)"), "{n}");
        // Nothing refused -> empty notice.
        assert_eq!(relay_screen_notice(&[]), "");
    }

    #[test]
    fn ledger_persists_and_lists() {
        let _env = crate::test_env::EnvGuard::new("netguard");
        // The in-memory set is process-global (OnceLock) and other tests
        // hold ENV_LOCK while mutating it, so assert on OUR host only.
        assert!(!direct_relays().iter().any(|h| h == "persist-test.local"));
        allow_direct_relay("persist-test.local");
        assert!(direct_relays().iter().any(|h| h == "persist-test.local"));
        // The file is the persistence: the same host is on disk.
        let hosts = read_ledger_file();
        assert!(hosts.contains("persist-test.local"));
        disallow_direct_relay("persist-test.local");
        assert!(!direct_relays().iter().any(|h| h == "persist-test.local"));
        assert!(!read_ledger_file().contains("persist-test.local"));
    }

    #[test]
    fn allow_refuses_sentinel_hosts() {
        let _env = crate::test_env::EnvGuard::new("netguard");
        allow_direct_relay("(unparseable)");
        allow_direct_relay("(no host)");
        allow_direct_relay("");
        assert!(
            !direct_relays().iter().any(|h| h.starts_with('(')),
            "sentinels must not be written"
        );
        assert!(!read_ledger_file().iter().any(|h| h.starts_with('(')));
    }
}
