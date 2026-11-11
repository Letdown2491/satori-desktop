//! Notification helpers — port of `src/data/notifications.ts` semantics.
//!
//! The stream itself lives in manager.rs `notifications()` (replies +
//! mentions, kind 1/1111 #p; zaps 9735 #p, verified; poll votes 1018
//! #e poll ids; reactions 7 behind the reactionNotifs pref, dislikes
//! never). Bell dot = anything newer than the read high-water, muted
//! actors excluded. This module carries the zap-receipt parser: the
//! zapper identity AND the claimed amount both live in the embedded
//! kind:9734 request (the `description` tag), which ANYONE can forge in
//! a receipt tagged to you — so the request's signature and its p/e/a
//! tags are verified, and the amount comes from the bolt11 invoice
//! (what was actually paid). An unverifiable request yields an
//! ANONYMOUS zap — real sats from the invoice, no name, never a
//! spoofed identity.

use nostr::prelude::*;

/// Who zapped, and how much (sats). `sender: None` = anonymous — the
/// receipt didn't carry a verifiable 9734.
#[derive(Debug, Clone, Default)]
pub struct ZapInfo {
    pub sender: Option<PublicKey>,
    pub sats: u64,
}

fn tag_value(ev: &Event, name: &str) -> Option<String> {
    ev.tags
        .iter()
        .find(|t| t.as_slice().first().map(String::as_str) == Some(name))
        .and_then(|t| t.as_slice().get(1).cloned())
        .filter(|v| !v.is_empty())
}

/// Millisats from a bolt11 invoice's human-readable part
/// (notifications.ts msatsFromBolt11): `lnbc<digits>[munp]1…`.
/// Also the receipt-side amount authority for the wallet's LNURL check (D20).
pub(crate) fn msats_from_bolt11(invoice: &str) -> u64 {
    let s = invoice.to_lowercase();
    let Some(rest) = s.strip_prefix("lnbc") else {
        return 0;
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return 0;
    }
    let after = &rest[digits.len()..];
    let amount: u64 = digits.parse().unwrap_or(0);
    if amount == 0 {
        return 0;
    }
    // Multiplier in msats (BOLT-11: m = 10^-3 BTC, u = 10^-6, n = 10^-9,
    // p = 10^-12; 1 BTC = 10^11 msat). Pico needs the divide, not a multiply.
    let (mult, tail) = match after.chars().next() {
        Some('m') => (100_000_000u64, &after[1..]),
        Some('u') => (100_000, &after[1..]),
        Some('n') => (100, &after[1..]),
        Some('p') => (0, &after[1..]),
        _ => (100_000_000_000, after), // bare = whole BTC
    };
    // The signature separator must follow (the regex's `1`).
    if !tail.starts_with('1') {
        return 0;
    }
    if mult == 0 {
        amount / 10
    } else {
        amount.checked_mul(mult).unwrap_or(0)
    }
}

/// Sender + sats from a kind:9735 receipt, the reference's verifyZap:
/// the embedded 9734 must verify and its p/e/a tags must match the
/// receipt's; the bolt11 (actually paid) amount is authoritative over
/// the request's self-declared one.
pub fn parse_zap_receipt(ev: &Event) -> ZapInfo {
    let paid = tag_value(ev, "bolt11")
        .map(|b| msats_from_bolt11(&b))
        .unwrap_or(0);
    let desc = tag_value(ev, "description");
    let mut info = ZapInfo::default();
    if let Some(desc) = desc
        && let Ok(req) = serde_json::from_str::<Event>(&desc)
    {
        let matches = |name: &str| match tag_value(ev, name) {
            Some(on) => tag_value(&req, name).as_deref() == Some(on.as_str()),
            None => true,
        };
        if u16::from(req.kind) == 9734
            && req.verify().is_ok()
            && matches("p")
            && matches("e")
            && matches("a")
        {
            info.sender = Some(req.pubkey);
            info.sats = tag_value(&req, "amount")
                .and_then(|a| a.parse::<u64>().ok())
                .unwrap_or(0);
        }
    }
    if paid > 0 {
        info.sats = paid / 1000;
    } else {
        info.sats /= 1000;
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bolt11_amounts() {
        // 21 sats = lnbc210n (nano-BTC × 100 msat).
        assert_eq!(msats_from_bolt11("lnbc210n1xyz"), 21_000);
        // 1 mBTC = 100k sats = 10^8 msat; 1 µBTC = 100 sats = 10^5 msat.
        assert_eq!(msats_from_bolt11("lnbc1m1xyz"), 100_000_000);
        assert_eq!(msats_from_bolt11("lnbc1u1xyz"), 100_000);
        // 2100 pico-BTC = 210 msat.
        assert_eq!(msats_from_bolt11("lnbc2100p1xyz"), 210);
        assert_eq!(msats_from_bolt11("lnbc1234567890"), 0); // no separator
        assert_eq!(msats_from_bolt11("not-an-invoice"), 0);
    }

    #[test]
    fn forged_request_is_anonymous() {
        // A receipt whose embedded 9734 has a junk signature (not signed
        // by anyone real) parses to anonymous, 0 sats (no bolt11).
        let keys = Keys::generate();
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            Kind::Custom(9735),
            [
                Tag::custom("p", [keys.public_key().to_string()]),
                Tag::custom(
                    "description",
                    [
                        r#"{"kind":9734,"sig":"junk","pubkey":"0000000000000000000000000000000000000000000000000000000000000000","tags":[["amount","2100"]]}"#,
                    ],
                ),
            ],
            "",
        );
        let receipt = keys.sign_event(unsigned).unwrap();
        let info = parse_zap_receipt(&receipt);
        assert!(info.sender.is_none());
        assert_eq!(info.sats, 0);
    }
}
