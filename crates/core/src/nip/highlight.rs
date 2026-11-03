//! NIP-84 highlights — tag parsing, port of `src/manifest/highlight.ts`
//! (originalAuthors + sourceLink): the source citation is an `r` tag (the
//! `source`-marked one preferred, then a bare legacy `r`, never a
//! `mention`-marked one) or a nostr ref (`a` addressable, then `e` event);
//! attribution is the `p` tags minus `mention`/`editor` roles.

use nostr::prelude::*;

/// Where a highlight's passage came from: a web page (`r` tag) or a nostr
/// reference (the raw `a` coord / `e` id hex — the card renders it bech32).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Url(String),
    Nostr(String),
}

#[derive(Clone)]
pub struct HighlightData {
    /// The quoted text (event content).
    pub content: String,
    /// Surrounding context, if the highlighter included it.
    pub context: Option<String>,
    /// The highlighter's own words (NIP-84 `comment` tag): the quote-repost
    /// form, comment as the post, passage quoted beneath. First tag wins
    /// (highlight.ts `tag1`); the render trims and skips empties.
    pub comment: Option<String>,
    /// The source citation (first present: marked `r` > bare `r` > `a` > `e`).
    pub source: Option<Source>,
    /// The ORIGINAL author(s) (NIP-84 `p` tags), hex pubkeys — "by X".
    pub authors: Vec<String>,
}

pub fn parse(ev: &Event) -> HighlightData {
    let mut d = HighlightData {
        content: ev.content.clone(),
        context: None,
        comment: None,
        source: None,
        authors: Vec::new(),
    };
    let mut bare_r: Option<String> = None;
    let mut addr: Option<String> = None;
    let mut event_id: Option<String> = None;
    for tag in ev.tags.clone().to_vec() {
        let parts = tag.as_slice();
        match parts.first().map(String::as_str) {
            Some("context") if parts.len() >= 2 => d.context = Some(parts[1..].join(" ")),
            Some("comment") if parts.len() >= 2 && d.comment.is_none() => {
                d.comment = Some(parts[1..].join(" "))
            }
            Some("r") if parts.len() >= 2 && !parts[1].is_empty() => {
                match parts.get(2).map(String::as_str) {
                    // The highlighted page's url carries the `source` marker;
                    // `mention`-marked r tags are urls from the highlighter's
                    // comment, NOT the source (highlight.ts).
                    Some("source") => d.source = Some(Source::Url(parts[1].clone())),
                    Some("mention") => {}
                    _ => {
                        if bare_r.is_none() {
                            bare_r = Some(parts[1].clone());
                        }
                    }
                }
            }
            Some("a") if parts.len() >= 2 && !parts[1].is_empty() && addr.is_none() => {
                addr = Some(parts[1].clone());
            }
            Some("e") if parts.len() >= 2 && !parts[1].is_empty() && event_id.is_none() => {
                event_id = Some(parts[1].clone());
            }
            // originalAuthors (highlight.ts): a roleless `["p", pk]` or
            // `["p", pk, relay, "author"]` counts; a `mention` role (a
            // comment mention, not the author) and `editor` do not.
            // Deduped, order preserved.
            Some("p") if parts.len() >= 2 => {
                let role = parts.get(3).map(String::as_str);
                if role != Some("mention")
                    && role != Some("editor")
                    && !d.authors.contains(&parts[1])
                    && parts[1].parse::<PublicKey>().is_ok()
                {
                    d.authors.push(parts[1].clone());
                }
            }
            _ => {}
        }
    }
    if d.source.is_none() {
        if let Some(u) = bare_r {
            d.source = Some(Source::Url(u));
        } else if let Some(a) = addr {
            d.source = Some(Source::Nostr(a));
        } else if let Some(e) = event_id {
            d.source = Some(Source::Nostr(e));
        }
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(tags: Vec<(&str, Vec<&str>)>) -> Event {
        let keys = Keys::generate();
        let mut t = Tags::new();
        for (name, values) in tags {
            t.push(Tag::custom(name, values));
        }
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from_secs(1),
            Kind::Custom(9802),
            t,
            "the passage",
        );
        keys.sign_event(unsigned).unwrap()
    }

    #[test]
    fn marked_source_wins_and_mention_never_cites() {
        // `source`-marked r beats the bare legacy r; a mention-marked r is
        // the highlighter's comment url, never the citation.
        let e = ev(vec![
            ("r", vec!["https://b/track", "mention"]),
            ("r", vec!["https://b/page"]),
            ("r", vec!["https://b/exact", "source"]),
        ]);
        let d = parse(&e);
        assert_eq!(
            d.source,
            Some(Source::Url("https://b/exact".to_string()))
        );
        // No marked r: the bare r cites; the mention r still never does.
        let e = ev(vec![
            ("r", vec!["https://b/track", "mention"]),
            ("r", vec!["https://b/page"]),
        ]);
        assert_eq!(parse(&e).source, Some(Source::Url("https://b/page".to_string())));
        let e = ev(vec![("r", vec!["https://b/track", "mention"])]);
        assert_eq!(parse(&e).source, None);
    }

    #[test]
    fn nostr_refs_fallback_in_order_a_then_e() {
        let e = ev(vec![
            ("e", vec!["abc123"]),
            ("a", vec!["30023:abc:def"]),
        ]);
        let d = parse(&e);
        assert_eq!(d.source, Some(Source::Nostr("30023:abc:def".to_string())));
        let e = ev(vec![("e", vec!["abc123"])]);
        assert_eq!(parse(&e).source, Some(Source::Nostr("abc123".to_string())));
    }

    #[test]
    fn p_tags_drop_mention_and_editor_roles() {
        let e = ev(vec![
            ("p", vec!["460c25e682fda7832b52d1f22d3d22b3176d972f60dcdc3212ed8c92ef85065c"]),
            ("p", vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "", "mention"]),
            ("p", vec!["bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "", "author"]),
            ("p", vec!["460c25e682fda7832b52d1f22d3d22b3176d972f60dcdc3212ed8c92ef85065c", "", "author"]),
            ("p", vec!["cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc", "", "editor"]),
        ]);
        let d = parse(&e);
        assert_eq!(
            d.authors,
            vec![
                "460c25e682fda7832b52d1f22d3d22b3176d972f60dcdc3212ed8c92ef85065c".to_string(),
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
            ]
        );
    }

    #[test]
    fn comment_tag_takes_the_first_value() {
        let e = ev(vec![
            ("comment", vec!["worth a listen"]),
            ("comment", vec!["second one ignored (tag1)"]),
        ]);
        assert_eq!(parse(&e).comment.as_deref(), Some("worth a listen"));
        // An empty first comment renders as nothing (the card trims).
        let e = ev(vec![("comment", vec!["   "])]);
        assert_eq!(parse(&e).comment.as_deref(), Some("   "));
        let e = ev(vec![]);
        assert_eq!(parse(&e).comment, None);
    }

    #[test]
    fn context_tag_keeps_inner_spaces() {
        let e = ev(vec![("context", vec!["she wrote, and then", "some"])]);
        assert_eq!(parse(&e).context.as_deref(), Some("she wrote, and then some"));
    }
}
