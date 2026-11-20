//! NIP-A3 payment targets (kind 10133, the `payto` tag) — D93.
//!
//! TODO(port): amethyst commons `PaymentTargetTypes.kt` + dark-wisp
//! `NipA3.kt` — 0.11.0 (D93). A kind-10133 replaceable event whose
//! `["payto", <type>, <authority>]` tags declare the author's payment
//! addresses for rails the lightning zap doesn't cover (venmo, iban,
//! monero, ...). Clients assemble hand-off URIs from each tag.
//!
//! The rules both reference implementations agree on (their consensus is
//! the spec's intent):
//!
//! - **Chips carry no amount.** A venmo or iban target cannot send 1000
//!   sats and there is no FX anywhere in the app; the URI never emits
//!   `amount=`, the amount is named in the external app.
//! - **The gate is capability, not symmetry.** A target renders when the
//!   client can act on it; the author's and reader's protocol support
//!   are unrelated.
//! - **Type strings are user input** — lowercase, `^[a-z0-9-]+$`, and
//!   the space is open-ended. Only known aliases collapse; everything
//!   else passes through and falls back to `payto://` (RFC 8905).
//!
//! Parsing and URI assembly only here; the chip layer lives in app-gpui.

use nostr::prelude::*;

/// The NIP-A3 event kind (replaceable, one per author).
pub const KIND: u16 = 10133;
/// The tag name: `["payto", <type>, <authority>]`.
const TAG_NAME: &str = "payto";

/// One declared payment target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentTarget {
    /// The normalized rail type (`venmo`, `bitcoin`, `iban`, ...).
    pub target_type: String,
    /// The address or handle on that rail, verbatim.
    pub authority: String,
}

/// Rail aliases that mean the SAME rail, so matching a sender's `btc`
/// against a recipient's `bitcoin` succeeds while `monero` and `bitcoin`
/// stay apart. `cashapp` predates the spec settling on `cashme`.
const ALIASES: &[(&str, &str)] = &[
    ("btc", "bitcoin"),
    ("onchain", "bitcoin"),
    ("ln", "lightning"),
    ("lnurl", "lightning"),
    ("eth", "ethereum"),
    ("xmr", "monero"),
    ("zec", "zcash"),
    ("bch", "bitcoincash"),
    ("ltc", "litecoin"),
    ("doge", "dogecoin"),
    ("sol", "solana"),
    ("trx", "tron"),
    ("xno", "nano"),
    ("cashapp", "cashme"),
];

/// Lowercase, trim, and collapse known aliases onto one family name.
/// The result is the NIP-A3 spelling where the spec lists one, so it is
/// also what a new `payto` tag should carry.
pub fn canonical(raw_type: &str) -> String {
    let t = raw_type.trim().to_lowercase();
    for (from, to) in ALIASES {
        if *from == t {
            return (*to).to_string();
        }
    }
    t
}

/// Rails the lightning zap already owns: a chip for these beside the
/// bolt would just draw a second bolt for the same money. (Amethyst
/// also excludes bitcoin-family types — their app carries an on-chain
/// wallet; ours does not.)
pub fn is_wallet_covered(raw_type: &str) -> bool {
    matches!(canonical(raw_type).as_str(), "lightning" | "lnurl")
}

/// Valid `payto` type: lowercase letters, digits, dashes.
fn type_is_valid(t: &str) -> bool {
    !t.is_empty()
        && t.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Valid authority: non-blank, no whitespace, no control characters
/// (the tags are user input and travel in URIs later).
fn authority_is_valid(a: &str) -> bool {
    !a.is_empty() && !a.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Parse the `payto` tags off a kind-10133 event. Junk tags drop out
/// (the tags are attacker-writable); elements past index 2 are reserved
/// for future RFC-8905 features and ignored; duplicates collapse.
/// Non-reusable lightning (bolt11 invoices, lnurl callbacks) is refused:
/// invoices expire and a target list is an address, not a payment.
pub fn parse(ev: &Event) -> Vec<PaymentTarget> {
    if u16::from(ev.kind) != KIND {
        return Vec::new();
    }
    let mut out: Vec<PaymentTarget> = Vec::new();
    for t in ev.tags.iter() {
        let tag = t.as_slice();
        if tag.first().map(String::as_str) != Some(TAG_NAME) || tag.len() < 3 {
            continue;
        }
        let target_type = canonical(&tag[1]);
        let authority = tag[2].trim();
        if !type_is_valid(&target_type) || !authority_is_valid(authority) {
            continue;
        }
        if !is_reusable_target(&target_type, authority) {
            continue;
        }
        let candidate = PaymentTarget {
            target_type,
            authority: authority.to_string(),
        };
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// A `lightning`-family target must be a reusable address: a bolt11
/// invoice or lnurl payload expires, and a target list is an address,
/// not a payment (BOLT11/LNURL don't belong in a replaceable list; a
/// BIP-21 payload makes the mistake easy). Every other rail passes.
fn is_reusable_target(target_type: &str, authority: &str) -> bool {
    if target_type != "lightning" {
        return true;
    }
    let a = authority.to_lowercase();
    !(a.starts_with("lnbc")
        || a.starts_with("lntb")
        || a.starts_with("lnbcrt")
        || a.starts_with("lnurl"))
}

/// The hand-off URI for a target. Recognized crypto rails use their
/// native scheme (almost no desktop wallet handles `payto://`); the big
/// web services get their page; everything else falls back to RFC 8905
/// `payto://<type>/<authority>` (the URI scheme exists precisely so the
/// open-ended tail stays resolvable). No `amount=` is ever emitted.
pub fn uri_for(target: &PaymentTarget) -> String {
    match target.target_type.as_str() {
        "bitcoin" => format!("bitcoin:{}", target.authority),
        "bitcoincash" => format!("bitcoincash:{}", target.authority),
        "dash" => format!("dash:{}", target.authority),
        "ethereum" => format!("ethereum:{}", target.authority),
        "litecoin" => format!("litecoin:{}", target.authority),
        "monero" => format!("monero:{}", target.authority),
        "nano" => format!("nano:{}", target.authority),
        "solana" => format!("solana:{}", target.authority),
        "zcash" => format!("zcash:{}", target.authority),
        // Web pages: any browser resolves them, so they are never gated.
        "cashme" => format!("https://cash.app/{}", target.authority),
        "venmo" => format!("https://venmo.com/{}", target.authority),
        "paypal" => format!("https://paypal.me/{}", target.authority),
        "revolut" => format!("https://revolut.me/{}", target.authority),
        // BIP-352 silent payments ride a BIP-321 bitcoin: query field.
        "bip352" => format!("bitcoin:?sp={}", target.authority),
        _ => format!("payto://{}/{}", target.target_type, target.authority),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payto_event(tags: &[&str]) -> Event {
        let keys = Keys::parse("e1d4a5b33a9d2f0c8b7e6f5a4d3c2b1a0918273645f4e3d2c1b0a99887766554")
            .unwrap();
        let tags: Vec<Tag> = tags
            .iter()
            .map(|t| {
                let parts: Vec<&str> = t.split('|').collect();
                Tag::custom(
                    parts[0].to_string(),
                    parts[1..].iter().map(|s| s.to_string()),
                )
            })
            .collect();
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            Kind::Custom(KIND),
            tags,
            String::new(),
        );
        keys.sign_event(unsigned).unwrap()
    }

    #[test]
    fn parses_tags_and_canonicalizes() {
        let ev = payto_event(&[
            "payto|venmo|alice-smith",
            "payto|monero|48jewB..",
            "payto|BTC|bc1qxyz",
            "payto|CashApp|$alice",
        ]);
        let targets = parse(&ev);
        assert_eq!(
            targets.len(),
            4,
            "aliases collapse per tag, not across tags"
        );
        assert_eq!(targets[0].target_type, "venmo");
        assert_eq!(targets[2].target_type, "bitcoin", "btc canonicalizes");
        assert_eq!(targets[2].authority, "bc1qxyz");
        assert_eq!(targets[3].target_type, "cashme", "cashapp canonicalizes");
    }

    #[test]
    fn junk_tags_drop_out() {
        let ev = payto_event(&[
            "payto|only-two-parts",
            "payto|bad type|handle",
            "payto|venmo|has space",
            "payto|venmo|",
            "payto|LIGHTNING|lnbc1invoice", // an invoice is not an address
            "note|whatever",
        ]);
        assert!(parse(&ev).is_empty());
    }

    #[test]
    fn other_kinds_parse_empty() {
        let keys = Keys::generate();
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            Kind::TextNote,
            Vec::new(),
            String::new(),
        );
        let ev = keys.sign_event(unsigned).unwrap();
        assert!(parse(&ev).is_empty());
    }

    #[test]
    fn duplicates_collapse() {
        let ev = payto_event(&["payto|venmo|alice", "payto|venmo|alice"]);
        assert_eq!(parse(&ev).len(), 1);
    }

    #[test]
    fn wallet_covered_is_lightning_only() {
        assert!(is_wallet_covered("lightning"));
        assert!(is_wallet_covered("ln"));
        assert!(is_wallet_covered("lnurl"));
        // No on-chain wallet in the app: a bitcoin chip is a real hand-off.
        assert!(!is_wallet_covered("bitcoin"));
        assert!(!is_wallet_covered("btc"));
        assert!(!is_wallet_covered("venmo"));
    }

    #[test]
    fn uris_use_native_schemes_and_payto_fallback() {
        let mk = |t: &str, a: &str| PaymentTarget {
            target_type: canonical(t),
            authority: a.to_string(),
        };
        assert_eq!(uri_for(&mk("monero", "48jewB..")), "monero:48jewB..");
        assert_eq!(uri_for(&mk("btc", "bc1qxyz")), "bitcoin:bc1qxyz");
        assert_eq!(
            uri_for(&mk("venmo", "alice-smith")),
            "https://venmo.com/alice-smith"
        );
        assert_eq!(uri_for(&mk("paypal", "alice")), "https://paypal.me/alice");
        assert_eq!(uri_for(&mk("bip352", "sp1xyz")), "bitcoin:?sp=sp1xyz");
        // The open-ended tail rides RFC 8905.
        assert_eq!(uri_for(&mk("iban", "DE4211")), "payto://iban/DE4211");
        assert_eq!(
            uri_for(&mk("swish", "0701234567")),
            "payto://swish/0701234567"
        );
    }
}
