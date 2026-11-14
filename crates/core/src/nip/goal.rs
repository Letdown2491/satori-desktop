//! NIP-75 zap goals (kind 9041) — the parse + sum side of a fundraising
//! goal. The goal is an addressable event (a `d` tag) whose `amount` tag
//! states the target in millisats and whose optional `closed_at` ends the
//! ask; zaps directed at it carry the goal's coordinate in their `a` tag
//! (zap request AND receipt), so the raised total is a sum over the
//! receipts that name it. Display (the feed card) lives in app-gpui.

use nostr::prelude::*;

use crate::notifications::parse_zap_receipt;

/// The goal kind (NIP-75): addressable, replaceable.
pub const KIND: u16 = 9041;

fn tag_value(ev: &Event, name: &str) -> Option<String> {
    ev.tags
        .iter()
        .find(|t| t.as_slice().first().map(String::as_str) == Some(name))
        .and_then(|t| t.as_slice().get(1).cloned())
}

/// The goal's own tags, validated (a goal without an amount is not one).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GoalData {
    /// The target, in millisats (the `amount` tag's unit).
    pub amount_msat: u64,
    /// Past this unix time the goal is closed (absent = open until met).
    pub closed_at: Option<u64>,
}

pub fn parse(ev: &Event) -> Option<GoalData> {
    let amount_msat = tag_value(ev, "amount")?.parse().ok()?;
    Some(GoalData {
        amount_msat,
        closed_at: tag_value(ev, "closed_at").and_then(|c| c.parse().ok()),
    })
}

/// The goal's coordinate ("9041:author:d") — what zap requests and
/// receipts carry in their `a` tag to name this goal. `None` without a
/// `d` tag: the event isn't addressable and can't be zapped AS a goal.
pub fn coordinate(ev: &Event) -> Option<String> {
    let d = tag_value(ev, "d")?;
    Some(format!("{KIND}:{}:{d}", ev.pubkey))
}

/// Raised so far, in msat: every receipt naming THIS goal (its `a` tag),
/// each amount bolt11-authoritative (`parse_zap_receipt` validates the
/// embedded 9734 and reads what was actually PAID — anonymous zaps
/// count, forged requests don't).
pub fn raised_msat(goal: &Event, receipts: &[Event]) -> u64 {
    let Some(coord) = coordinate(goal) else {
        return 0;
    };
    receipts
        .iter()
        .filter(|r| tag_value(r, "a").as_deref() == Some(coord.as_str()))
        .map(|r| parse_zap_receipt(r).sats * 1000)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(keys: &Keys, kind: u16, tags: Vec<Tag>, content: &str) -> Event {
        keys.sign_event(UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            Kind::from_u16(kind),
            tags,
            content,
        ))
        .unwrap()
    }

    /// A real NIP-57 zap receipt for `goal`: a signed 9734 request (a tag
    /// = the goal coordinate) embedded in the description, the paid
    /// amount readable from the bolt11.
    fn receipt(zapper: &Keys, a_value: &str, bolt11: &str) -> Event {
        let req = sign(
            zapper,
            9734,
            vec![
                Tag::custom("p", [zapper.public_key().to_string()]),
                Tag::custom("a", [a_value]),
                Tag::custom("amount", ["21000"]),
            ],
            "for the goal",
        );
        sign(
            zapper,
            9735,
            vec![
                Tag::custom("p", [zapper.public_key().to_string()]),
                Tag::custom("a", [a_value]),
                Tag::custom("bolt11", [bolt11]),
                Tag::custom("description", [serde_json::to_string(&req).unwrap()]),
            ],
            "",
        )
    }

    #[test]
    fn parse_reads_amount_and_closed_at() {
        let author = Keys::generate();
        let goal = sign(
            &author,
            KIND,
            vec![
                Tag::custom("d", ["new-laptop"]),
                Tag::custom("amount", ["50000000"]),
                Tag::custom("closed_at", ["1700000000"]),
            ],
            "help me buy a laptop",
        );
        let data = parse(&goal).expect("a d+amount goal parses");
        assert_eq!(data.amount_msat, 50_000_000);
        assert_eq!(data.closed_at, Some(1_700_000_000));
        // No amount: not a goal.
        assert!(
            parse(&sign(
                &author,
                KIND,
                vec![Tag::custom("d", ["x"])],
                "no ask"
            ))
            .is_none()
        );
        // No d tag: not addressable, coordinate refuses.
        assert!(
            coordinate(&sign(
                &author,
                KIND,
                vec![Tag::custom("amount", ["1"])],
                "no d"
            ))
            .is_none()
        );
    }

    #[test]
    fn coordinate_is_the_a_tag_value() {
        let author = Keys::generate();
        let goal = sign(
            &author,
            KIND,
            vec![Tag::custom("d", ["roof-fund"])],
            "a new roof",
        );
        let expected = format!("9041:{}:roof-fund", author.public_key());
        assert_eq!(coordinate(&goal).as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn raised_sums_receipts_naming_the_goal() {
        let author = Keys::generate();
        let zapper = Keys::generate();
        let goal = sign(
            &author,
            KIND,
            vec![Tag::custom("d", ["roof-fund"])],
            "a new roof",
        );
        let coord = coordinate(&goal).unwrap();
        // 21 sats each, paid (bolt11-authoritative).
        let paid = receipt(&zapper, &coord, "lnbc210n1xyz");
        // A receipt naming a DIFFERENT goal (or carrying no a tag at all)
        // never counts toward this one.
        let elsewhere = receipt(&zapper, "9041:other:goal", "lnbc210n1xyz");
        let stray = sign(
            &zapper,
            9735,
            vec![Tag::custom("bolt11", ["lnbc210n1xyz"])],
            "",
        );
        assert_eq!(
            raised_msat(&goal, &[paid.clone(), elsewhere, stray]),
            21_000
        );
        // A goal without a d tag can't be zapped as one: raised is 0 even
        // with receipts in hand.
        let naked = sign(&author, KIND, vec![], "no d");
        assert_eq!(raised_msat(&naked, &[paid]), 0);
    }
}
