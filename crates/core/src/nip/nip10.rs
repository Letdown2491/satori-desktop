//! NIP-10 threading — port of `src/nostr/nip10.ts` (reply_parent; the thread
//! view anchors by id, so reply_root isn't needed yet).

use std::collections::HashSet;

use nostr::prelude::*;

/// The ids this event `q`-quotes (NIP-18) — references, NOT the thing being
/// replied to (tags.ts quotedIds).
fn quoted_ids(ev: &Event) -> HashSet<String> {
    ev.tags
        .iter()
        .filter_map(|t| {
            let v = t.clone().to_vec();
            match v.first().map(String::as_str) {
                Some("q") => v.get(1).cloned(),
                _ => None,
            }
        })
        .collect()
}

/// If a note is a reply, return the parent (id, relay hints) — the
/// "reply"-marked e-tag, else "root", else (legacy, no markers) the last
/// unmarked e-tag. "mention"-marked e-tags and quoted ids are references,
/// not reply targets; counting them would misread a quote as a reply.
pub fn reply_parent(ev: &Event) -> Option<(String, Vec<String>)> {
    let quoted = quoted_ids(ev);
    let mut e_tags: Vec<Vec<String>> = Vec::new();
    for tag in ev.tags.clone().to_vec() {
        let parts = tag.as_slice();
        if parts.first().map(String::as_str) != Some("e") {
            continue;
        }
        let Some(id) = parts.get(1).filter(|v| !v.is_empty()).cloned() else {
            continue;
        };
        if parts.get(3).map(String::as_str) == Some("mention") || quoted.contains(&id) {
            continue;
        }
        e_tags.push(parts.to_vec());
    }
    let pick = |marker: &str| {
        e_tags
            .iter()
            .find(|t| t.get(3).map(String::as_str) == Some(marker))
    };
    let tag = pick("reply")
        .or_else(|| pick("root"))
        .or_else(|| e_tags.last())
        .cloned()?;
    let relays = tag
        .get(2)
        .filter(|r| !r.is_empty())
        .map(|r| vec![r.clone()])
        .unwrap_or_default();
    Some((tag[1].clone(), relays))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(tags: Vec<(&str, &str, &str, &str)>) -> Event {
        let keys = Keys::generate();
        let tags = tags
            .into_iter()
            .map(|(a, c, d, e)| Tag::custom(a, [c, d, e]))
            .collect::<Tags>();
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            Kind::TextNote,
            tags,
            "x",
        );
        keys.sign_event(unsigned).unwrap()
    }

    #[test]
    fn reply_marker_wins() {
        let e = ev(vec![
            ("e", "rootid", "", "root"),
            ("e", "parentid", "", "reply"),
            ("p", "who", "", ""),
        ]);
        assert_eq!(reply_parent(&e).unwrap().0, "parentid");
    }

    #[test]
    fn root_fallback_when_no_reply_marker() {
        let e = ev(vec![("e", "rootid", "", "root"), ("p", "who", "", "")]);
        assert_eq!(reply_parent(&e).unwrap().0, "rootid");
    }

    #[test]
    fn legacy_last_etag_is_the_parent() {
        let e = ev(vec![("e", "olderid", "", ""), ("e", "parentid", "", "")]);
        assert_eq!(reply_parent(&e).unwrap().0, "parentid");
    }

    #[test]
    fn quotes_and_mentions_are_not_replies() {
        let e = ev(vec![
            ("q", "quoteid", "", ""),
            ("e", "mentionid", "", "mention"),
        ]);
        assert!(reply_parent(&e).is_none());
    }

    #[test]
    fn no_e_tags_no_reply() {
        let e = ev(vec![("p", "who", "", "")]);
        assert!(reply_parent(&e).is_none());
    }
}
