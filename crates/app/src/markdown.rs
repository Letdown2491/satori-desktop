//! Markdown → Pango markup — port of `src/nostr/markdown.ts` (subset):
//! headings, bold/italic, inline code + fenced blocks, blockquotes, lists,
//! links. Escape-first: user text never becomes markup.

use crate::content::esc;

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

/// Inline markdown → Pango markup (escapes first).
pub fn inline(src: &str) -> String {
    let mut s = esc(src);

    // Links [text](url) — process FIRST so their URLs don't get altered.
    let mut out = String::new();
    let mut rest = s.as_str();
    while let Some(open) = rest.find('[') {
        let close = rest[open..].find("](");
        if let Some(close_off) = close {
            let close_paren = rest[open + close_off..].find(')');
            if let Some(paren_off) = close_paren {
                let text = &rest[open + 1..open + close_off];
                let url_start = open + close_off + 2;
                let url = &rest[url_start..url_start + paren_off - 2];
                out.push_str(&rest[..open]);
                out.push_str(&format!(r#"<a href="{}">{}</a>"#, url, text));
                rest = &rest[url_start + paren_off..];
                continue;
            }
        }
        out.push_str(&rest[..open + 1]);
        rest = &rest[open + 1..];
    }
    out.push_str(rest);
    s = out;

    // **bold** then *italic* / _italic_ (marker pairs).
    s = wrap_pairs(&s, "**", "<b>", "</b>");
    s = wrap_pairs(&s, "__", "<i>", "</i>");
    // `code` → <tt> (paired).
    s = wrap_pairs(&s, "`", "<tt>", "</tt>");
    s
}

/// Wrap marker-delimited segments (odd indices = inside).
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

fn wrap_pairs(s: &str, marker: &str, open: &str, close: &str) -> String {
    let parts: Vec<&str> = s.split(marker).collect();
    if parts.len() < 3 || parts.len().is_multiple_of(2) {
        return s.to_string();
    }
    parts
        .iter()
        .enumerate()
        .map(|(i, seg)| {
            if i % 2 == 1 {
                format!("{open}{seg}{close}")
            } else {
                seg.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_before_markup() {
        let m = inline("a <b> & **bold**");
        assert!(m.starts_with("a &lt;b&gt; &amp; "));
        assert!(m.contains("<b>bold</b>"));
    }

    #[test]
    fn links_and_code() {
        let m = inline("see [the tao](https://x.test) and `code`");
        assert!(m.contains(r#"<a href="https://x.test">the tao</a>"#));
        assert!(m.contains("<tt>code</tt>"));
    }

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
}
