//! NIP-45 event counts: the `COUNT` verb's response parsing, multi-relay
//! merging, and the HyperLogLog estimate.
//!
//! Spec: <https://nips.nostr.com/45> (draft, optional). No TS reference —
//! the web app predated COUNT. A relay answers `["COUNT", <id>,
//! {"count": N, "approximate"?: B, "hll"?: <512-hex>}]`; a refusal is a
//! `CLOSED`; silence or an error means the relay has no COUNT support.
//!
//! Merging across relays (the spec's whole point): when two or more
//! answers carry HLL registers, merge register-wise (max) and take the
//! estimate; otherwise the merged count is the max of the plain counts
//! (never a sum — the same event is usually on every answering relay).
//!
//! Fixture shapes come from `examples/count_probe.rs` (live COUNT frames to
//! primal — the only app read relay that answers today — plus the NIP's
//! own documented examples).

use serde_json::Value;

/// HLL register count: one byte per register, 256 total (the spec's
/// `hll` field is their 512-char hex concatenation).
pub const REGISTERS: usize = 256;

/// One relay's answer to a COUNT.
#[derive(Debug, Clone, PartialEq)]
pub struct CountResult {
    /// The relay's count (exact or the probabilistic estimate's
    /// current best).
    pub count: u64,
    /// The relay flagged the count as probabilistic.
    pub approximate: bool,
    /// HyperLogLog registers (256 bytes), when the relay computes them.
    /// Boxed: a bare 256-byte array made the parse Outcome enum carry
    /// it inline (clippy's size-difference lint — the count result rides
    /// options and channels; the registers stay one indirection away).
    pub hll: Option<Box<[u8; REGISTERS]>>,
}

/// What one wire frame told us: a count, a refusal (`CLOSED`, the spec's
/// only sanctioned refusal), or nothing usable (silence, garbage, a
/// mismatched id).
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Count(CountResult),
    Refused,
    Unsupported,
}

/// Decode a wire frame `["COUNT", <id>, {..}]` / `["CLOSED", <id>, ..]`.
/// Only the verb and payload matter here; callers match the subscription
/// id before calling.
pub fn parse_frame(frame: &Value) -> Outcome {
    let Some(verb) = frame
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
    else {
        return Outcome::Unsupported;
    };
    match verb {
        "COUNT" => match frame.get(2) {
            Some(obj) => match parse_result(obj) {
                Some(r) => Outcome::Count(r),
                None => Outcome::Unsupported,
            },
            None => Outcome::Unsupported,
        },
        "CLOSED" => Outcome::Refused,
        _ => Outcome::Unsupported,
    }
}

/// Parse a COUNT payload object: `count` required, `approximate` optional
/// (default false), `hll` optional (512 hex chars → 256 registers; a
/// malformed one is dropped, the plain count still stands).
pub fn parse_result(obj: &Value) -> Option<CountResult> {
    let count = obj.get("count")?.as_u64()?;
    let approximate = obj
        .get("approximate")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let hll = obj
        .get("hll")
        .and_then(|v| v.as_str())
        .and_then(decode_hll)
        .map(Box::new);
    Some(CountResult {
        count,
        approximate,
        hll,
    })
}

fn decode_hll(hex: &str) -> Option<[u8; REGISTERS]> {
    let bytes = hex.as_bytes();
    if bytes.len() != REGISTERS * 2 {
        return None;
    }
    let mut out = [0u8; REGISTERS];
    for (i, reg) in out.iter_mut().enumerate() {
        let hi = (bytes[i * 2] as char).to_digit(16)?;
        let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
        *reg = (hi as u8) << 4 | lo as u8;
    }
    Some(out)
}

/// Merge several relays' answers into one (count, approximate). Two or
/// more HLLs → register-wise max, then the estimate (the merged count is
/// inherently approximate). Otherwise the max plain count — the same
/// events sit on every answering relay, so a sum double-counts; the max
/// is the best-known true count. With no answers at all: (0, false).
pub fn merge(results: &[CountResult]) -> (u64, bool) {
    let hlls: Vec<&[u8; REGISTERS]> = results.iter().filter_map(|r| r.hll.as_deref()).collect();
    if hlls.len() >= 2 {
        let mut merged = *hlls[0];
        for h in &hlls[1..] {
            for (m, r) in merged.iter_mut().zip(h.iter()) {
                *m = (*m).max(*r);
            }
        }
        return (hll_estimate(&merged), true);
    }
    let mut count = results.first().map(|r| r.count).unwrap_or(0);
    let mut approximate = results.first().is_some_and(|r| r.approximate);
    for r in results.iter().skip(1) {
        if r.count > count {
            count = r.count;
        }
        approximate = approximate || r.approximate;
    }
    (count, approximate)
}

/// The HyperLogLog estimate over 256 uint8 registers — the "methods I
/// don't dare to describe here in English" (NIP-45), i.e. the standard
/// Flajolet estimator with the usual corrections:
///
/// - raw estimate E = αm·m² / Σ 2⁻ʳ where αm = 0.7213 / (1 + 1.079/m);
/// - small range (E ≤ 2.5m) with empty registers V: linear counting,
///   E = m·ln(m/V);
/// - large range (E > 2³²/30): bias correction, E = −2³²·ln(1 − E/2³²).
pub fn hll_estimate(registers: &[u8; REGISTERS]) -> u64 {
    const M: f64 = REGISTERS as f64;
    let alpha = 0.7213 / (1.0 + 1.079 / M);
    let sum: f64 = registers.iter().map(|&r| 2.0f64.powi(-(r as i32))).sum();
    let raw = alpha * M * M / sum;
    let empty = registers.iter().filter(|&&r| r == 0).count();
    let estimate = if raw <= 2.5 * M && empty > 0 {
        // Linear counting: the small range, dominated by empty registers.
        M * (M / empty as f64).ln()
    } else {
        let large = 2.0f64.powi(32) / 30.0;
        if raw > large {
            // Large-range correction (32-bit-hash HLL heritage). The
            // ratio's guard keeps the log in domain: a fully saturated
            // register set pushes raw past 2^32, and ln of a negative
            // is NaN — clamp to just-under-saturation instead.
            let ratio = (raw / 2.0f64.powi(32)).min(1.0 - 1e-6);
            -2.0f64.powi(32) * (1.0 - ratio).ln()
        } else {
            raw
        }
    };
    estimate.round().max(0.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A real frame: relay.primal.net answering a live COUNT for replies
    /// to a note (examples/count_probe.rs, Nov 13 2026). Plain count —
    /// no `approximate`, no `hll`.
    #[test]
    fn parses_a_plain_live_count() {
        let frame: Value =
            serde_json::from_str(r#"["COUNT","probe-replies",{"count":9}]"#).unwrap();
        match parse_frame(&frame) {
            Outcome::Count(r) => {
                assert_eq!(r.count, 9);
                assert!(!r.approximate);
                assert_eq!(r.hll, None);
            }
            other => panic!("expected a count, got {other:?}"),
        }
    }

    /// Also real (same probe): a follower count. 6987 — primal answers
    /// the NIP's canonical follower query.
    #[test]
    fn parses_a_live_follower_count() {
        let frame: Value =
            serde_json::from_str(r#"["COUNT","probe-followers",{"count":6987}]"#).unwrap();
        match parse_frame(&frame) {
            Outcome::Count(r) => assert_eq!(r.count, 6987),
            other => panic!("expected a count, got {other:?}"),
        }
    }

    /// The NIP's documented hll example (the reaction-count frame):
    /// approximate absent, hll present, 256 registers.
    #[test]
    fn parses_the_spec_hll_example() {
        let hll_hex = "01ef070505060806050508060707070706090d080b0605090607070b07090606060b0705070709050807080805080407060906080707080507070805060509040a0b06060704060405070706080607050907070b08060808080b080607090a06060805060604070908050607060805050d05060906090809080807050e0705070507060907060606070708080b0807070708080706060609080705060604060409070a0808050a0506050b0810060a0908070709080b0a07050806060508060607080606080707050806080c0a0707070a080808050608080f070506070706070a0908090c080708080806090508060606090906060d07050708080405070708";
        let frame = json!(["COUNT", "x", {"count": 2044, "hll": hll_hex}]);
        match parse_frame(&frame) {
            Outcome::Count(r) => {
                assert_eq!(r.count, 2044);
                assert!(!r.approximate);
                let regs = r.hll.as_deref().expect("registers decode");
                assert_eq!(regs[0], 0x01);
                assert_eq!(regs[1], 0xef);
                assert_eq!(regs[255], 0x08);
                // The spec's own example estimate hangs together: the
                // registers say "a few thousand distinct actors".
                let (merged, approx) = merge(&[r]);
                assert!(!approx);
                assert!(
                    (500..=50_000).contains(&merged),
                    "estimate {merged} implausible for the spec's 2044-reaction example"
                );
            }
            other => panic!("expected a count, got {other:?}"),
        }
    }

    /// `approximate: true` survives the parse (the NIP's "count notes
    /// approximately" example: a global-ish count no relay computes
    /// exactly).
    #[test]
    fn parses_approximate_flag() {
        let frame = json!(["COUNT", "x", {"count": 93412452, "approximate": true}]);
        match parse_frame(&frame) {
            Outcome::Count(r) => {
                assert_eq!(r.count, 93412452);
                assert!(r.approximate);
            }
            other => panic!("expected a count, got {other:?}"),
        }
    }

    /// The NIP's refusal example: relays that won't count send CLOSED
    /// (here: other people's DMs), never a bogus count.
    #[test]
    fn parses_a_refusal() {
        let frame = json!([
            "CLOSED",
            "x",
            "auth-required: cannot count other people's DMs"
        ]);
        assert_eq!(parse_frame(&frame), Outcome::Refused);
    }

    /// Silence (no COUNT support), a NOTICE (snort's real answer to a
    /// live COUNT: "Unknown message type: COUNT"), or a countless object
    /// is not an answer — the merge must skip it.
    #[test]
    fn treats_silence_and_garbage_as_unsupported() {
        let frame: Value =
            serde_json::from_str(r#"["NOTICE","Unknown message type: COUNT"]"#).unwrap();
        assert_eq!(parse_frame(&frame), Outcome::Unsupported);
        assert_eq!(parse_frame(&json!([])), Outcome::Unsupported);
        assert_eq!(
            parse_frame(&json!(["NOTICE", "nope"])),
            Outcome::Unsupported
        );
        assert_eq!(
            parse_frame(&json!(["COUNT", "x", {"approximate": true}])),
            Outcome::Unsupported
        );
        assert_eq!(
            parse_frame(&json!(["COUNT", "x", {"count": "lots"}])),
            Outcome::Unsupported
        );
    }

    /// A malformed hll (wrong length) is dropped, the plain count stands:
    /// one broken relay must not cost the whole tally.
    #[test]
    fn drops_a_malformed_hll_keeps_the_count() {
        let frame = json!(["COUNT", "x", {"count": 7, "hll": "0bad"}]);
        match parse_frame(&frame) {
            Outcome::Count(r) => {
                assert_eq!(r.count, 7);
                assert_eq!(r.hll, None);
            }
            other => panic!("expected a count, got {other:?}"),
        }
    }

    /// The committed fixture FILES replay: everything the probe captured
    /// from live relays must still parse the way it did in the field —
    /// primal's counts count, snort's NOTICE doesn't.
    #[test]
    fn replays_the_captured_fixtures() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nip45");
        let mut counted = 0;
        for file in std::fs::read_dir(&dir).unwrap() {
            let file = file.unwrap();
            let name = file.file_name().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(file.path()).unwrap();
            // The probe may capture a lead-up NOTICE before the answer:
            // the LAST frame is the decision.
            let outcome = text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| parse_frame(&serde_json::from_str(l).unwrap()))
                .last()
                .expect("fixture has frames");
            if name.starts_with("relay.primal.net") {
                let Outcome::Count(r) = outcome else {
                    panic!("{name}: primal's answer must parse as a count");
                };
                assert_eq!(r.hll, None, "{name}: primal sends no registers");
                counted += 1;
            } else if name.starts_with("relay.snort.social") {
                assert_eq!(
                    outcome,
                    Outcome::Unsupported,
                    "{name}: a NOTICE is not a count"
                );
            } else {
                panic!("unclassified fixture: {name}");
            }
        }
        assert_eq!(counted, 5, "five primal counts expected");
    }

    /// Plain counts merge by MAX (never a sum — every relay counted the
    /// same events); approximate ORs.
    #[test]
    fn plain_counts_merge_by_max() {
        let mk = |c: u64, a: bool| CountResult {
            count: c,
            approximate: a,
            hll: None,
        };
        assert_eq!(merge(&[]), (0, false));
        assert_eq!(merge(&[mk(9, false)]), (9, false));
        assert_eq!(merge(&[mk(9, false), mk(6987, false)]), (6987, false));
        assert_eq!(
            merge(&[mk(9, true), mk(6987, false)]),
            (6987, true),
            "one approximate answer marks the merge"
        );
    }

    /// A single hll among plain counts can't merge register-wise —
    /// the max plain count still wins.
    #[test]
    fn one_hll_alone_falls_back_to_max() {
        let mk = |c: u64, h: Option<[u8; REGISTERS]>| CountResult {
            count: c,
            approximate: false,
            hll: h.map(Box::new),
        };
        let regs = [3u8; REGISTERS];
        assert_eq!(merge(&[mk(9, None), mk(20, Some(regs))]), (20, false));
    }

    /// Two hlls merge register-wise: the result's estimate is at least
    /// each relay's own (a union can only grow), and a bumped register
    /// moves the estimate up.
    #[test]
    fn hll_merge_is_register_max_then_estimate() {
        let mk = |c: u64, mut regs: [u8; REGISTERS]| {
            regs[0] = 5;
            CountResult {
                count: c,
                approximate: false,
                hll: Some(Box::new(regs)),
            }
        };
        let mut a = [0u8; REGISTERS];
        let mut b = [0u8; REGISTERS];
        for i in 0..100 {
            a[i] = 4 + (i % 3) as u8;
        }
        for i in 50..150 {
            b[i] = 5 + (i % 2) as u8;
        }
        let (merged, approx) = merge(&[mk(900, a), mk(800, b)]);
        assert!(approx, "a merged hll count is approximate by nature");
        let e_a = hll_estimate(&a);
        let e_b = hll_estimate(&b);
        assert!(
            merged >= e_a.max(e_b),
            "merged {merged} must cover both relays' own estimates ({e_a}, {e_b})"
        );
        // Push every register up hard (out of linear counting, where the
        // estimate only reads the empty-register count): the estimate
        // can only rise.
        let bumped = [8u8; REGISTERS];
        assert!(hll_estimate(&bumped) > e_a.max(e_b));
    }

    /// The estimator's corners: all-zero registers count zero; a
    /// saturated register set (every value at 255) far above the linear
    /// range lands in the large-range correction without wrapping.
    #[test]
    fn hll_estimate_corners() {
        let zeros = [0u8; REGISTERS];
        assert_eq!(hll_estimate(&zeros), 0);
        let sat = [255u8; REGISTERS];
        let huge = hll_estimate(&sat);
        assert!(
            huge > 10_000_000,
            "saturated registers: a huge set, got {huge}"
        );
    }
}
