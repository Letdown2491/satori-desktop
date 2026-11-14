//! Cross-NIP single-tag fields — the small render-side accessors that
//! several NIPs hang off one tag: NIP-36 `content-warning` (sensitive
//! content never renders raw), NIP-31 `alt` (a kind's self-description
//! for clients that don't implement it), NIP-14/NIP-24 `subject`/`title`
//! (a kind-1 row header), NIP-30 `emoji` (shortcode → image url).

use nostr::prelude::*;

/// The one tag-value kernel (audit dup: three drifted copies —
/// notifications' filtered empty, fields'/goal's raw). Returns the tag's
/// first value slot, trimmed; the EMPTY-VALUE policy belongs to the call
/// site (alt filters it out, a coordinate treats it as absent) so one
/// helper can serve both.
pub(crate) fn tag_value(ev: &Event, name: &str) -> Option<String> {
    ev.tags
        .iter()
        .find(|t| t.as_slice().first().map(String::as_str) == Some(name))
        .and_then(|t| t.as_slice().get(1).cloned())
        .map(|v| v.trim().to_string())
}

/// NIP-36: the content warning's reason (the tag's value may be absent
/// or empty — the veil renders a generic line then; the tag's PRESENCE
/// is the warning).
pub fn content_warning(ev: &Event) -> Option<String> {
    ev.tags
        .iter()
        .find(|t| {
            matches!(
                t.as_slice().first().map(String::as_str),
                Some("content-warning") | Some("cw")
            )
        })
        .map(|t| {
            t.as_slice()
                .get(1)
                .map(|r| r.trim().to_string())
                .unwrap_or_default()
        })
}

/// NIP-31: the `alt` tag — a one-line, human-readable description of
/// what the event is, written by its author for clients that don't
/// implement the kind. Empty values are no description.
pub fn alt_description(ev: &Event) -> Option<String> {
    tag_value(ev, "alt").filter(|a| !a.is_empty())
}

/// NIP-14 (and NIP-24's carryover): a kind-1 subject line. `subject` is
/// the spec spelling; `title` is what some clients send. Long-form and
/// media kinds carry real title fields (NIP-23 `title`, NIP-71 first
/// line) and never reach for this.
pub fn subject_title(ev: &Event) -> Option<String> {
    tag_value(ev, "subject")
        .or_else(|| tag_value(ev, "title"))
        .filter(|s| !s.is_empty())
}

/// NIP-30: the event's own `["emoji", <shortcode>, <url>]` tags, in tag
/// order. Shortcodes resolve per event: the tags ride the note, and the
/// kind-10030 profile set is the COMPOSER's palette (a separate surface).
pub fn emoji_tags(ev: &Event) -> Vec<(String, String)> {
    ev.tags
        .iter()
        .filter(|t| t.as_slice().first().map(String::as_str) == Some("emoji"))
        .filter_map(|t| {
            let code = t.as_slice().get(1)?.to_string();
            let url = t.as_slice().get(2)?.to_string();
            (!code.is_empty() && !url.is_empty()).then_some((code, url))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev_with(tags: Vec<Tag>) -> Event {
        let keys = Keys::generate();
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            Kind::TextNote,
            tags,
            String::new(),
        );
        keys.sign_event(unsigned).unwrap()
    }

    #[test]
    fn content_warning_reads_reason_and_tolerates_empty() {
        let ev = ev_with(vec![Tag::custom(
            "content-warning",
            ["graphic war footage".to_string()],
        )]);
        assert_eq!(content_warning(&ev).as_deref(), Some("graphic war footage"));
        // An empty reason is still a warning (the veil shows a generic
        // line), and the `cw` spelling some clients use counts.
        let bare = ev_with(vec![Tag::custom("content-warning", Vec::<String>::new())]);
        assert_eq!(content_warning(&bare).as_deref(), Some(""));
        let cw = ev_with(vec![Tag::custom("cw", ["sensitive".to_string()])]);
        assert_eq!(content_warning(&cw).as_deref(), Some("sensitive"));
        assert!(content_warning(&ev_with(vec![])).is_none());
    }

    #[test]
    fn alt_description_requires_non_empty() {
        let ev = ev_with(vec![Tag::custom(
            "alt",
            ["a kind-2222 reply: a riddle".to_string()],
        )]);
        assert_eq!(
            alt_description(&ev).as_deref(),
            Some("a kind-2222 reply: a riddle")
        );
        let bare = ev_with(vec![Tag::custom("alt", ["  ".to_string()])]);
        assert!(alt_description(&bare).is_none());
    }

    #[test]
    fn subject_prefers_subject_then_title() {
        let both = ev_with(vec![
            Tag::custom("title", ["the NIP-24 spelling".to_string()]),
            Tag::custom("subject", ["the NIP-14 spelling".to_string()]),
        ]);
        assert_eq!(subject_title(&both).as_deref(), Some("the NIP-14 spelling"));
        let titled = ev_with(vec![Tag::custom("title", ["fallback".to_string()])]);
        assert_eq!(subject_title(&titled).as_deref(), Some("fallback"));
        assert!(subject_title(&ev_with(vec![])).is_none());
    }

    #[test]
    fn emoji_tags_in_order_junk_dropped() {
        let ev = ev_with(vec![
            Tag::custom(
                "emoji",
                ["sox".to_string(), "https://a/sox.png".to_string()],
            ),
            Tag::custom("emoji", ["broken".to_string()]),
            Tag::custom("emoji", Vec::<String>::new()),
            Tag::custom("p", ["not".to_string(), "an emoji".to_string()]),
            Tag::custom(
                "emoji",
                ["bounce".to_string(), "https://a/bounce.png".to_string()],
            ),
        ]);
        let tags = emoji_tags(&ev);
        assert_eq!(
            tags,
            vec![
                ("sox".to_string(), "https://a/sox.png".to_string()),
                ("bounce".to_string(), "https://a/bounce.png".to_string()),
            ]
        );
    }
}
