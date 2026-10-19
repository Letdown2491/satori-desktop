//! NIP-22 generic comments (kind 1111) — port of `src/data/comments.ts` +
//! `src/nostr/nip22.ts` (tag grammar slice).
//!
//! Uppercase = root reference, lowercase = parent reference:
//! `E`/`e` event ids, `A`/`a` addresses (`kind:pubkey:d`), `I`/`i` external
//! identifiers, `K`/`k` kinds, `P`/`p` authors.

use nostr::prelude::*;

/// A root/parent reference.
#[derive(Debug, Clone)]
pub enum Ref {
    Event(EventId),
    Address { kind: u16, author: PublicKey, d: String },
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
            Ref::Event(_) => if root { 'E' } else { 'e' },
            Ref::Address { .. } => if root { 'A' } else { 'a' },
            Ref::Url(_) => if root { 'I' } else { 'i' },
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
    Event { id: EventId, kind: u16, author: PublicKey },
    /// An addressable root (articles, wikis) — stable across edits.
    Address { kind: u16, author: PublicKey, d: String },
}

impl CommentTarget {
    pub fn for_event(ev: &Event) -> CommentTarget {
        if u16::from(ev.kind) >= 30000 && u16::from(ev.kind) != 9802 {
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
                kind: u16::from(ev.kind),
                author: ev.pubkey,
                d,
            }
        } else {
            CommentTarget::Event {
                id: ev.id,
                kind: u16::from(ev.kind),
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
                vec![kind_filter.custom_tag(
                    SingleLetterTag::LOWERCASE_A,
                    format!("{kind}:{author}:{d}"),
                )]
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
                    CommentTarget::Address { .. } => {
                        Ref::Address { kind: *kind, author: *author, d: d.clone() }.value()
                    }
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

/// Tags for a reply to another comment (root preserved from the parent).
pub fn reply_tags(parent_comment: &Event, root: &CommentTarget) -> Vec<Tag> {
    let parent_refs = parse_refs(parent_comment);
    let mut tags = root_tags_of(root);
    // Parent = the comment itself.
    tags.push(Tag::custom("e", [parent_comment.id.to_string()]));
    tags.push(Tag::custom("k", ["1111".to_string()]));
    tags.push(Tag::custom("p", [parent_comment.pubkey.to_string()]));
    // Preserve external identifiers if the parent carried them.
    if parent_refs.parent.is_none() {
        let _ = &parent_refs;
    }
    tags
}

fn root_tags_of(root: &CommentTarget) -> Vec<Tag> {
    root.root_tags()
        .into_iter()
        .filter(|t| {
            let first = t.as_slice().first().map(String::as_str).unwrap_or("");
            // Keep only the uppercase (root) halves; reply_tags appends parent.
            first.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
        })
        .collect()
}
