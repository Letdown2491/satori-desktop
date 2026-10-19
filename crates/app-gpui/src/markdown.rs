//! Markdown → layout blocks + styled spans — port of `crates/app/src/markdown.rs`
//! (itself a subset of `src/nostr/markdown.ts`): headings, bold/italic, inline
//! code + fenced blocks, blockquotes, lists, links, images, rules.
//!
//! The GTK original emitted Pango markup; GPUI has no markup strings, so the
//! inline half is `spans()` instead — a flat run of styled segments the word
//! flow renders directly. Escape-first isn't needed: GPUI text is painted, not
//! interpreted.

/// Render a whole markdown document into a list of (kind, text) blocks.
pub fn blocks(src: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut lines = src.lines().peekable();
    let mut para: Vec<String> = Vec::new();

    fn flush_para(out: &mut Vec<Block>, para: &mut Vec<String>) {
        if !para.is_empty() {
            out.push(Block::Para(para.join("\n")));
            para.clear();
        }
    }

    while let Some(line) = lines.next() {
        let trim = line.trim_start();

        if trim.starts_with("```") {
            flush_para(&mut out, &mut para);
            let mut body = Vec::new();
            for l in lines.by_ref() {
                if l.trim_start().starts_with("```") {
                    break;
                }
                body.push(l.to_string());
            }
            out.push(Block::Code(body.join("\n")));
            continue;
        }

        if trim.starts_with("### ") || trim.starts_with("## ") || trim.starts_with("# ") {
            flush_para(&mut out, &mut para);
            let level = trim.chars().take_while(|c| *c == '#').count();
            out.push(Block::Heading(level.min(3), trim[level..].trim().to_string()));
            continue;
        }

        if trim.starts_with("> ") || trim == ">" {
            flush_para(&mut out, &mut para);
            let mut quote = vec![trim.trim_start_matches("> ").to_string()];
            while lines
                .peek()
                .map(|l| l.trim_start().starts_with("> "))
                .unwrap_or(false)
            {
                let l = lines.next().unwrap();
                quote.push(l.trim_start()[2..].to_string());
            }
            out.push(Block::Quote(quote.join("\n")));
            continue;
        }

        if trim.starts_with("- ") || trim.starts_with("* ") {
            flush_para(&mut out, &mut para);
            let mut items = vec![trim[2..].to_string()];
            while lines
                .peek()
                .map(|l| l.trim_start().starts_with("- ") || l.trim_start().starts_with("* "))
                .unwrap_or(false)
            {
                let l = lines.next().unwrap().trim_start().to_string();
                items.push(l[2..].to_string());
            }
            out.push(Block::List(items));
            continue;
        }

        // Own-line image: ![alt](url) or a bare image URL.
        if let Some(url) = image_line(trim) {
            flush_para(&mut out, &mut para);
            out.push(Block::Image(url));
            continue;
        }

        if trim == "---" {
            flush_para(&mut out, &mut para);
            out.push(Block::Rule);
            continue;
        }

        if trim.is_empty() {
            flush_para(&mut out, &mut para);
            continue;
        }

        para.push(trim.to_string());
    }
    flush_para(&mut out, &mut para);
    out
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Para(String),
    Heading(usize, String),
    Quote(String),
    List(Vec<String>),
    Code(String),
    Image(String),
    Rule,
}

/// One styled run of inline markdown. `link` spans render as one clickable
/// unit (they wrap as a whole); everything else flows word by word.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MdSpan {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link: Option<String>,
}

/// Inline markdown → styled spans (the Pango `inline()` half, reshaped for
/// GPUI): links [text](url), **bold**, __italic_, `code`. Marker pairs only —
/// an unclosed marker stays literal text (same rule as the GTK `wrap_pairs`).
pub fn spans(src: &str) -> Vec<MdSpan> {
    let mut out: Vec<MdSpan> = vec![MdSpan::default()];
    let push = |out: &mut Vec<MdSpan>, text: &str, bold: bool, italic: bool, code: bool, link: Option<String>| {
        if text.is_empty() {
            return;
        }
        // Merge into the previous span when the style continues.
        if let Some(last) = out.last_mut()
            && last.bold == bold
            && last.italic == italic
            && last.code == code
            && last.link == link
        {
            last.text.push_str(text);
        } else {
            out.push(MdSpan { text: text.to_string(), bold, italic, code, link });
        }
    };

    let mut rest = src;
    while !rest.is_empty() {
        // The earliest marker wins; nothing before it is plain text.
        let mut best: Option<(usize, &'static str)> = None;
        for m in ["**", "__", "`", "["] {
            if let Some(i) = rest.find(m)
                && best.is_none_or(|(bi, _)| i < bi)
            {
                best = Some((i, m));
            }
        }
        let Some((i, marker)) = best else {
            push(&mut out, rest, false, false, false, None);
            break;
        };
        push(&mut out, &rest[..i], false, false, false, None);
        rest = &rest[i..];

        match marker {
            "[" => {
                // [text](url) — malformed brackets stay text.
                let Some(close) = rest.find("](") else {
                    push(&mut out, "[", false, false, false, None);
                    rest = &rest[1..];
                    continue;
                };
                let Some(paren) = rest[close..].find(')') else {
                    push(&mut out, "[", false, false, false, None);
                    rest = &rest[1..];
                    continue;
                };
                let text = &rest[1..close];
                let url = &rest[close + 2..close + paren];
                push(&mut out, text, false, false, false, Some(url.to_string()));
                rest = &rest[close + paren + 1..];
            }
            "`" => {
                let Some(end) = rest[1..].find('`') else {
                    push(&mut out, "`", false, false, false, None);
                    rest = &rest[1..];
                    continue;
                };
                push(&mut out, &rest[1..1 + end], false, false, true, None);
                rest = &rest[end + 2..];
            }
            m => {
                // ** / __: an empty match is a literal marker.
                let Some(end) = rest[2..].find(m) else {
                    push(&mut out, m, false, false, false, None);
                    rest = &rest[2..];
                    continue;
                };
                if end == 0 {
                    push(&mut out, m, false, false, false, None);
                    rest = &rest[2..];
                    continue;
                }
                let (bold, italic) = (m == "**", m == "__");
                push(&mut out, &rest[2..2 + end], bold, italic, false, None);
                rest = &rest[end + 4..];
            }
        }
    }
    out.retain(|s| !s.text.is_empty());
    out
}

fn image_line(line: &str) -> Option<String> {
    let is_img_ext = |u: &str| {
        let path = u.split(['?', '#']).next().unwrap_or("");
        [".png", ".jpg", ".jpeg", ".webp", ".gif"]
            .iter()
            .any(|ext| path.ends_with(ext))
    };
    if line.starts_with("![") {
        let close = line.find("](")?;
        let url = line[close + 2..].strip_suffix(')')?;
        return is_img_ext(url).then(|| url.to_string());
    }
    if line.starts_with("https://") && is_img_ext(line) {
        return Some(line.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_parse() {
        let src = "# Title\n\npara one\n\n> quote\n- item\n```\ncode\n```";
        let b = blocks(src);
        assert_eq!(b[0], Block::Heading(1, "Title".into()));
        assert_eq!(b[1], Block::Para("para one".into()));
        assert_eq!(b[2], Block::Quote("quote".into()));
        assert_eq!(b[3], Block::List(vec!["item".into()]));
        assert_eq!(b[4], Block::Code("code".into()));
    }

    #[test]
    fn spans_bold_code() {
        let s = spans("a **bold** and `code` end");
        assert_eq!(s[0].text, "a ");
        assert!(s.iter().any(|x| x.bold && x.text == "bold"));
        assert!(s.iter().any(|x| x.code && x.text == "code"));
    }

    #[test]
    fn spans_link() {
        let s = spans("see [the tao](https://x.test) now");
        assert_eq!(s[1].link.as_deref(), Some("https://x.test"));
        assert_eq!(s[1].text, "the tao");
    }

    #[test]
    fn unclosed_marker_stays_literal() {
        let s = spans("a ** b");
        assert_eq!(s.iter().map(|x| x.text.as_str()).collect::<String>(), "a ** b");
        assert!(!s.iter().any(|x| x.bold));
    }

    #[test]
    fn image_line_variants() {
        assert_eq!(image_line("![x](https://a.test/p.png)").as_deref(), Some("https://a.test/p.png"));
        assert_eq!(image_line("https://a.test/p.jpg").as_deref(), Some("https://a.test/p.jpg"));
        assert_eq!(image_line("https://a.test/p.mp4"), None);
    }
}
