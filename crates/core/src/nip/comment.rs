//! NIP-22 generic comments (kind 1111) — port of `src/data/comments.ts` +
//! `src/nostr/nip22.ts` (tag grammar slice).
//!
//! Uppercase = root reference, lowercase = parent reference:
//! `E`/`e` event ids, `A`/`a` addresses (`kind:pubkey:d`), `I`/`i` external
//! identifiers, `K`/`k` kinds, `P`/`p` authors.

use nostr::prelude::*;

/// A root/parent reference.
#[derive(Debug, Clone, PartialEq)]
pub enum Ref {
    Event(EventId),
    Address {
        kind: u16,
        author: PublicKey,
        d: String,
    },
    Url(String),
}

impl Ref {
    pub fn from_tag(letter: char, value: &str) -> Option<Ref> {
        match letter {
            'e' | 'E' => Some(Ref::Event(EventId::from_hex(value).ok()?)),
            'a' | 'A' => {
                let (kind, rest) = value.split_once(':')?;
                let (author, d) = rest.split_once(':')?;
                Some(Ref::Address {
                    kind: kind.parse().ok()?,
                    author: PublicKey::from_hex(author).ok()?,
                    d: d.to_string(),
                })
            }
            'i' | 'I' => Some(Ref::Url(value.to_string())),
            _ => None,
        }
    }

    pub fn tag_letter(&self, root: bool) -> char {
        match self {
            Ref::Event(_) => {
                if root {
                    'E'
                } else {
                    'e'
                }
            }
            Ref::Address { .. } => {
                if root {
                    'A'
                } else {
                    'a'
                }
            }
            Ref::Url(_) => {
                if root {
                    'I'
                } else {
                    'i'
                }
            }
        }
    }

    pub fn value(&self) -> String {
        match self {
            Ref::Event(id) => id.to_string(),
            Ref::Address { kind, author, d } => format!("{kind}:{author}:{d}"),
            Ref::Url(u) => u.clone(),
        }
    }
}

/// The root/parent references a comment carries.
#[derive(Debug, Clone, Default)]
pub struct CommentRefs {
    pub root: Option<Ref>,
    pub parent: Option<Ref>,
    pub root_kind: Option<u16>,
    pub parent_kind: Option<u16>,
    pub root_author: Option<PublicKey>,
    pub parent_author: Option<PublicKey>,
}

/// Parse a comment's refs (single-letter tags; lowercase = parent).
pub fn parse_refs(ev: &Event) -> CommentRefs {
    let mut refs = CommentRefs::default();
    for tag in ev.tags.clone().to_vec() {
        let parts = tag.as_slice();
        let Some(first) = parts.first() else { continue };
        let mut chars = first.chars();
        let (Some(letter), None) = (chars.next(), chars.next()) else {
            continue; // exactly one letter
        };
        let Some(value) = parts.get(1) else { continue };
        match letter {
            'e' | 'E' | 'a' | 'A' | 'i' | 'I' => {
                if let Some(r) = Ref::from_tag(letter, value) {
                    if letter.is_lowercase() {
                        refs.parent = Some(r);
                    } else {
                        refs.root = Some(r);
                    }
                }
            }
            'k' => refs.parent_kind = value.parse().ok(),
            'K' => refs.root_kind = value.parse().ok(),
            'p' => refs.parent_author = PublicKey::from_hex(value).ok(),
            'P' => refs.root_author = PublicKey::from_hex(value).ok(),
            _ => {}
        }
    }
    refs
}

/// What a comment thread hangs on.
#[derive(Debug, Clone)]
pub enum CommentTarget {
    /// An immutable event root (e.g. highlights).
    Event {
        id: EventId,
        kind: u16,
        author: PublicKey,
    },
    /// An addressable root (articles, wikis) — stable across edits.
    Address {
        kind: u16,
        author: PublicKey,
        d: String,
    },
}

impl CommentTarget {
    pub fn for_event(ev: &Event) -> CommentTarget {
        let kind = u16::from(ev.kind);
        // NIP-68/71: pictures (20), videos (21/22) are parameterized
        // replaceable — when a d tag is present their address survives
        // edits and other clients hang comment threads off the a-tag
        // address, so the address is the correct target.
        let parameterized = matches!(kind, 20..=22)
            && ev
                .tags
                .clone()
                .to_vec()
                .iter()
                .any(|t| t.as_slice().first().map(String::as_str) == Some("d"));
        if (kind >= 30000 && kind != 9802) || parameterized {
            // Addressable: prefer the address (survives edits).
            let d = ev
                .tags
                .clone()
                .to_vec()
                .iter()
                .find(|t| t.as_slice().first().map(String::as_str) == Some("d"))
                .and_then(|t| t.as_slice().get(1).cloned())
                .unwrap_or_default();
            CommentTarget::Address {
                kind,
                author: ev.pubkey,
                d,
            }
        } else {
            CommentTarget::Event {
                id: ev.id,
                kind,
                author: ev.pubkey,
            }
        }
    }

    /// Filters for fetching this target's comments (#e / #a exact matches).
    pub fn fetch_filters(&self) -> Vec<Filter> {
        let kind_filter = Filter::new().kind(Kind::Custom(1111));
        match self {
            CommentTarget::Event { id, .. } => {
                vec![kind_filter.custom_tag(SingleLetterTag::LOWERCASE_E, id.to_string())]
            }
            CommentTarget::Address { kind, author, d } => {
                vec![
                    kind_filter
                        .custom_tag(SingleLetterTag::LOWERCASE_A, format!("{kind}:{author}:{d}")),
                ]
            }
        }
    }

    /// Root tags for a top-level comment (parent == root).
    pub fn root_tags(&self) -> Vec<Tag> {
        match self {
            CommentTarget::Event { id, kind, author } => vec![
                Tag::custom("E", [id.to_string()]),
                Tag::custom("e", [id.to_string()]),
                Tag::custom("K", [kind.to_string()]),
                Tag::custom("k", [kind.to_string()]),
                Tag::custom("P", [author.to_string()]),
                Tag::custom("p", [author.to_string()]),
            ],
            CommentTarget::Address { kind, author, d } => {
                let addr = CommentTarget::Address {
                    kind: *kind,
                    author: *author,
                    d: d.clone(),
                };
                let value = match &addr {
                    CommentTarget::Address { .. } => Ref::Address {
                        kind: *kind,
                        author: *author,
                        d: d.clone(),
                    }
                    .value(),
                    _ => unreachable!(),
                };
                vec![
                    Tag::custom("A", [value.clone()]),
                    Tag::custom("a", [value]),
                    Tag::custom("K", [kind.to_string()]),
                    Tag::custom("k", [kind.to_string()]),
                    Tag::custom("P", [author.to_string()]),
                    Tag::custom("p", [author.to_string()]),
                ]
            }
        }
    }
}

/// Tags for a reply to another comment: the ROOT preserved from the
/// parent (NIP-22 — a comment-on-comment keeps the thread's original
/// root, uppercase E/A + K + P riding along; rebuilding the root from the
/// parent comment itself would make every nested reply its own thread and
/// break cross-client placement). The passed target is the FALLBACK for a
/// parent that carries no parseable root (a top-level comment's parent is
/// the content itself, whose refs are the target).
pub fn reply_tags(parent_comment: &Event, fallback_root: &CommentTarget) -> Vec<Tag> {
    let refs = parse_refs(parent_comment);
    let mut tags: Vec<Tag> = match &refs.root {
        Some(root) => {
            let letter = root.tag_letter(true);
            let mut t = vec![Tag::custom(letter.to_string(), [root.value()])];
            if let Some(k) = refs.root_kind {
                t.push(Tag::custom("K", [k.to_string()]));
            }
            if let Some(a) = refs.root_author {
                t.push(Tag::custom("P", [a.to_string()]));
            }
            t
        }
        None => root_tags_of(fallback_root),
    };
    // Parent = the comment itself.
    tags.push(Tag::custom("e", [parent_comment.id.to_string()]));
    tags.push(Tag::custom("k", ["1111".to_string()]));
    tags.push(Tag::custom("p", [parent_comment.pubkey.to_string()]));
    tags
}

fn root_tags_of(root: &CommentTarget) -> Vec<Tag> {
    root.root_tags()
        .into_iter()
        .filter(|t| {
            let first = t.as_slice().first().map(String::as_str).unwrap_or("");
            // Keep only the uppercase (root) halves; reply_tags appends parent.
            first
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Keys {
        Keys::generate()
    }

    fn sign(me: &Keys, kind: Kind, tags: Tags, content: &str) -> Event {
        me.sign_event(UnsignedEvent::new(
            me.public_key(),
            Timestamp::now(),
            kind,
            tags,
            content.to_string(),
        ))
        .unwrap()
        .into()
    }

    /// NIP-22 root preservation (#32's cross-client threading gap): a
    /// reply to a comment must carry the ORIGINAL thread's root tags —
    /// rebuilding the root from the parent comment itself would make
    /// every nested reply its own thread.
    #[test]
    fn reply_to_comment_preserves_root() {
        let author = keys();
        let replier = keys();
        // The article root, addressed (kind:pubkey:d).
        let d = "my-article";
        let target = CommentTarget::Address {
            kind: 30023,
            author: author.public_key(),
            d: d.to_string(),
        };
        // A top-level comment on it (root_tags carries A + a + K + k + P + p).
        let top = sign(
            &author,
            Kind::Custom(1111),
            Tags::from_list(target.root_tags()),
            "top-level comment",
        );
        // A reply to THAT comment: the fallback target is wrong on purpose
        // (the composer's for_event(parent) names the parent comment) —
        // the preservation must win.
        let nested_parent = sign(
            &replier,
            Kind::Custom(1111),
            Tags::from_list(reply_tags(&top, &CommentTarget::for_event(&top))),
            "nested reply",
        );
        let tags = reply_tags(&nested_parent, &CommentTarget::for_event(&nested_parent));
        let refs = parse_refs(&sign(
            &replier,
            Kind::Custom(1111),
            Tags::from_list(tags),
            "depth two",
        ));
        match refs.root {
            Some(Ref::Address { kind, d: got, .. }) => {
                assert_eq!(kind, 30023);
                assert_eq!(got, d, "root is still the ARTICLE, not the parent comment");
            }
            other => panic!("address root preserved, got {other:?}"),
        }
        // The parent ref is the comment replied to.
        assert_eq!(refs.parent, Some(Ref::Event(nested_parent.id)));
    }

    /// NIP-68: pictures (kind 20) are parameterized replaceable — with a
    /// d tag the comment target is the ADDRESS (other clients hang
    /// threads off the a-tag), not the event id.
    #[test]
    fn picture_with_d_targets_the_address() {
        let author = keys();
        let mut t = Tags::new();
        t.push(Tag::custom("d", ["im-the-d"]));
        let pic = sign(&author, Kind::Custom(20), t, "caption");
        match CommentTarget::for_event(&pic) {
            CommentTarget::Address { kind, d, .. } => {
                assert_eq!(kind, 20);
                assert_eq!(d, "im-the-d");
            }
            other => panic!("addressable picture target, got {other:?}"),
        }
        // A d-tag-less (nonconformant) picture falls back to the event id.
        let pic = sign(&author, Kind::Custom(20), Tags::new(), "caption");
        assert!(matches!(
            CommentTarget::for_event(&pic),
            CommentTarget::Event { kind: 20, .. }
        ));
    }
}
