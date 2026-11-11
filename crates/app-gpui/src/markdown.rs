//! Markdown → layout blocks + styled spans — port of `crates/app/src/markdown.rs`
//! (itself a subset of `src/nostr/markdown.ts`): headings (ATX + setext),
//! bold/italic, inline code + fenced blocks, blockquotes, ordered/unordered
//! lists, links (+ titles), autolinks, images, rules, `<br>`.
//!
//! The GTK original emitted Pango markup; GPUI has no markup strings, so the
//! inline half is `spans()` instead — a flat run of styled segments the word
//! flow renders directly. Escape-first isn't needed: GPUI text is painted, not
//! interpreted.

/// Render a whole markdown document into a list of blocks.
/// Port of parseBlocks (markdown.ts).
pub fn blocks(src: &str) -> Vec<Block> {
    let src = src.replace("\r\n", "\n");
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
        let t = trim.trim_end();

        if t.is_empty() {
            flush_para(&mut out, &mut para);
            continue;
        }

        if let Some(rest) = trim.strip_prefix("```") {
            flush_para(&mut out, &mut para);
            let _lang = rest.trim();
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

        // ATX heading: #{1,6} + space (markdown.ts HEADING).
        let hashes = t.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            flush_para(&mut out, &mut para);
            out.push(Block::Heading(hashes, t[hashes..].trim().to_string()));
            continue;
        }

        // Rule: ---, ***, ___ (3+) on its own line (markdown.ts HR).
        if is_hr(t) {
            flush_para(&mut out, &mut para);
            out.push(Block::Rule);
            continue;
        }

        // A lone <br> — authors use it for extra spacing (markdown.ts BR_ONLY).
        if is_br_line(t) {
            flush_para(&mut out, &mut para);
            out.push(Block::Break);
            continue;
        }

        // Own-line image: ![alt](url) with the whitespace/title tolerance
        // (markdown.ts IMG_ONLY).
        if let Some((url, _alt)) = img_only(t) {
            flush_para(&mut out, &mut para);
            out.push(Block::Image { url });
            continue;
        }

        // Blockquote: `>` with optional single space (markdown.ts /^>\s?/).
        if trim.starts_with('>') {
            flush_para(&mut out, &mut para);
            let mut quote = vec![
                trim.trim_start_matches('>')
                    .trim_start_matches(' ')
                    .to_string(),
            ];
            while lines
                .peek()
                .map(|l| l.trim_start().starts_with('>'))
                .unwrap_or(false)
            {
                let l = lines.next().unwrap();
                quote.push(
                    l.trim_start()
                        .trim_start_matches('>')
                        .trim_start_matches(' ')
                        .to_string(),
                );
            }
            out.push(Block::Quote(quote.join("\n")));
            continue;
        }

        // List item: -, *, +, or 1. (markdown.ts LIST). Loose continuation
        // (a non-item line) falls through to the paragraph — same as the TS
        // (the item loop just stops). GitHub task-list markers ([ ]/[x])
        // ride the item as `task`.
        if is_list(trim) {
            flush_para(&mut out, &mut para);
            let ordered = is_ordered(trim);
            let strip = |l: &str| -> ListItem {
                let l = l.trim_start();
                let body = if is_ordered(l) {
                    // Up to and including the first space after "N.".
                    let dot = l.find('.').unwrap_or(0);
                    l[dot + 1..].trim_start()
                } else {
                    &l[2..]
                };
                task_of(body.trim_start())
            };
            let mut items = vec![strip(trim)];
            while lines
                .peek()
                .map(|l| is_list(l.trim_start()))
                .unwrap_or(false)
            {
                items.push(strip(lines.next().unwrap()));
            }
            out.push(Block::List { items, ordered });
            continue;
        }

        // Table: a `|`-bearing line over a separator row (`| --- | :--: |`),
        // body rows until a non-cell line. Checked BEFORE the setext
        // underline (a separator row is dashes but never a bare `-=`
        // underline) and before the paragraph fallback. Pragmatic: inline
        // escapes (`\|`) aren't wired.
        if t.contains('|') && lines.peek().map(|l| is_table_sep(l)).unwrap_or(false) {
            flush_para(&mut out, &mut para);
            let head = table_cells(t);
            let aligns = aligns_of(lines.next().unwrap());
            let mut rows: Vec<Vec<String>> = Vec::new();
            while let Some(l) = lines.peek() {
                let l = l.trim();
                let starter = l.is_empty()
                    || l.starts_with("```")
                    || l.starts_with('>')
                    || is_list(l.trim_start())
                    || is_hr(l)
                    || (l.starts_with('#') && l[1..].starts_with(' '));
                if starter || !l.contains('|') {
                    break;
                }
                rows.push(table_cells(lines.next().unwrap()));
            }
            out.push(Block::Table { head, rows, aligns });
            continue;
        }

        // Setext heading: a text line underlined by `=` (H1) or `-` (H2) —
        // checked after the fence/ATX/HR/quote/list starters and before the
        // paragraph fallback, so no ambiguity with a standalone `----`
        // (markdown.ts Setext comment).
        if let Some(under) = lines.peek()
            && is_setext_underline(under)
        {
            flush_para(&mut out, &mut para);
            let level = if under.trim().starts_with('=') { 1 } else { 2 };
            lines.next();
            out.push(Block::Heading(level, t.to_string()));
            continue;
        }

        para.push(t.to_string());
    }
    flush_para(&mut out, &mut para);
    out
}

fn is_hr(t: &str) -> bool {
    match t.chars().next() {
        Some(c @ ('-' | '*' | '_')) => t.len() >= 3 && t.chars().all(|x| x == c),
        _ => false,
    }
}

fn is_setext_underline(line: &str) -> bool {
    let t = line.trim();
    (!t.is_empty() && t.chars().all(|c| c == '=') && !t.is_empty())
        || (!t.is_empty() && t.chars().all(|c| c == '-') && !t.is_empty())
}

/// A line that is only a bare `<br>` (no attributes) — markdown.ts BR_ONLY.
fn is_br_line(t: &str) -> bool {
    matches!(t.to_lowercase().as_str(), "<br>" | "<br/>" | "<br />")
}

/// List starter: `- `, `* `, `+ `, or `N. ` (markdown.ts LIST).
fn is_list(trim: &str) -> bool {
    if matches!(trim.get(..2), Some("- " | "* " | "+ ")) {
        return true;
    }
    is_ordered(trim)
}

/// Ordered starter: `N. ` (markdown.ts `\d+\.\s+`).
fn is_ordered(trim: &str) -> bool {
    let digits: usize = trim.chars().take_while(|c| c.is_ascii_digit()).count();
    digits > 0 && trim[digits..].starts_with(". ")
}

/// A GitHub task-list item: `[ ]` / `[x]` / `[X]` (a space after the
/// bracket, or nothing after it at all). Anything else is plain text.
fn task_of(body: &str) -> ListItem {
    let (mark, rest) = if let Some(r) = body.strip_prefix("[ ] ") {
        (Some(false), r)
    } else if let Some(r) = body
        .strip_prefix("[x] ")
        .or_else(|| body.strip_prefix("[X] "))
    {
        (Some(true), r)
    } else if body == "[ ]" {
        (Some(false), "")
    } else if body.eq_ignore_ascii_case("[x]") {
        (Some(true), "")
    } else {
        (None, body)
    };
    ListItem {
        text: rest.to_string(),
        task: mark,
    }
}

/// A table separator row: optional pipes around cells of `:?-+:?`
/// (`| --- | :--: |`). At least one cell, at least one dash.
fn is_table_sep(line: &str) -> bool {
    let t = line.trim();
    if !t.contains('|') {
        return false;
    }
    let cells: Vec<&str> = t.trim_matches('|').split('|').map(str::trim).collect();
    !cells.is_empty()
        && cells.iter().all(|c| {
            !c.is_empty()
                && c.trim_start_matches(':')
                    .trim_end_matches(':')
                    .contains('-')
                && c.chars().all(|ch| ch == ':' || ch == '-')
        })
}

/// A row's cells: trim, drop the outer pipes, split on `|`.
fn table_cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
}

/// Column alignment from the separator cells' colons.
fn aligns_of(sep: &str) -> Vec<Align> {
    table_cells(sep)
        .iter()
        .map(|c| match (c.starts_with(':'), c.ends_with(':')) {
            (true, true) => Align::Center,
            (true, false) => Align::Left,
            (false, true) => Align::Right,
            _ => Align::Left,
        })
        .collect()
}

/// Own-line image: `![alt]( url "title"? )` — whitespace-tolerant, optional
/// title (markdown.ts IMG_ONLY). Returns (url, alt).
fn img_only(t: &str) -> Option<(String, String)> {
    let rest = t.strip_prefix("![")?;
    let close = rest.find("]")?;
    let alt = rest[..close].to_string();
    let inner = rest[close + 1..].trim();
    let inner = inner.strip_prefix("(")?.strip_suffix(")")?;
    let url = inner.split_whitespace().next()?.to_string();
    Some((url, alt))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Para(String),
    Heading(usize, String),
    Quote(String),
    List {
        items: Vec<ListItem>,
        ordered: bool,
    },
    /// A markdown table: the header cells, the body rows, per-column
    /// alignment from the separator row's colons.
    Table {
        head: Vec<String>,
        rows: Vec<Vec<String>>,
        aligns: Vec<Align>,
    },
    Code(String),
    Image {
        url: String,
    },
    Break,
    Rule,
}

/// One list item; `task` marks a GitHub-style checkbox (`- [ ]` / `- [x]`),
/// the bool = checked.
#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    pub text: String,
    pub task: Option<bool>,
}

/// Column alignment from the separator row's colons (`:---`, `:---:`, `---:`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// One styled run of inline markdown. `link` spans render as one clickable
/// unit (they wrap as a whole); everything else flows word by word. A `br`
/// span becomes a full-width spacer in the flow (a real line break).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MdSpan {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
    pub br: bool,
    pub link: Option<String>,
}

/// Inline markdown → styled spans — port of the INLINE regex's marker set
/// (markdown.ts): links `[text](url "title")`, `**bold**`, `__bold__`,
/// `*em*`, `_em_`, `code`, angle autolinks `<https://…>`, and bare `<br>`.
/// Marker pairs only — an unclosed marker stays literal text.
pub fn spans(src: &str) -> Vec<MdSpan> {
    let mut out: Vec<MdSpan> = vec![MdSpan::default()];
    let push = |out: &mut Vec<MdSpan>,
                text: &str,
                bold: bool,
                italic: bool,
                code: bool,
                strike: bool,
                br: bool,
                link: Option<String>| {
        if text.is_empty() && !br {
            return;
        }
        // Merge into the previous span when the style continues.
        if let Some(last) = out.last_mut()
            && last.bold == bold
            && last.italic == italic
            && last.code == code
            && last.strike == strike
            && last.br == br
            && last.link == link
        {
            last.text.push_str(text);
        } else {
            out.push(MdSpan {
                text: text.to_string(),
                bold,
                italic,
                code,
                strike,
                br,
                link,
            });
        }
    };
    let br_span = |out: &mut Vec<MdSpan>| {
        out.push(MdSpan {
            br: true,
            ..Default::default()
        })
    };

    let mut rest = src;
    while !rest.is_empty() {
        // The earliest marker wins; at a tie the LONGER marker does (** beats
        // *). Nothing before the winner is plain text.
        let mut best: Option<(usize, &str)> = None;
        for m in ["<br/>", "<br>", "~~", "**", "__", "*", "_", "`", "[", "!["] {
            if let Some(i) = rest.find(m)
                && best.is_none_or(|(bi, bm)| i < bi || (i == bi && m.len() > bm.len()))
            {
                best = Some((i, m));
            }
        }
        // Angle autolink: `<https://…>` with no `>`, space or newline inside
        // (markdown.ts group 4). The `<` competes with the text markers by
        // position like any other.
        let al = rest.find('<').and_then(|i| {
            let r = &rest[i..];
            let close = r[1..].find(|c: char| c == '>' || c.is_whitespace())?;
            (close > 0
                && r.as_bytes()[1 + close] == b'>'
                && (r.starts_with("<http://") || r.starts_with("<https://")))
            .then_some(i)
        });
        if let Some(i) = al
            && best.is_none_or(|(bi, _)| i < bi)
        {
            best = Some((i, "<"));
        }
        let Some((i, marker)) = best else {
            push(&mut out, rest, false, false, false, false, false, None);
            break;
        };
        push(
            &mut out,
            &rest[..i],
            false,
            false,
            false,
            false,
            false,
            None,
        );
        rest = &rest[i..];

        match marker {
            "<" => {
                let close = rest.find('>').unwrap();
                let url = &rest[1..close];
                push(
                    &mut out,
                    url,
                    false,
                    false,
                    false,
                    false,
                    false,
                    Some(url.to_string()),
                );
                rest = &rest[close + 1..];
            }
            "![" | "[" => {
                // [text](url) — malformed brackets stay text. An inline image
                // degrades to a link labeled by its alt.
                let text_start = marker.len();
                let Some(close) = rest.find("](") else {
                    push(
                        &mut out,
                        &rest[..text_start],
                        false,
                        false,
                        false,
                        false,
                        false,
                        None,
                    );
                    rest = &rest[text_start..];
                    continue;
                };
                let Some(paren) = rest[close..].find(')') else {
                    push(
                        &mut out,
                        &rest[..text_start],
                        false,
                        false,
                        false,
                        false,
                        false,
                        None,
                    );
                    rest = &rest[text_start..];
                    continue;
                };
                let text = rest[text_start..close].trim();
                let mut url = rest[close + 2..close + paren].trim().to_string();
                // Optional markdown title: (url "title") — drop the title.
                if let Some(sp) = url.find(" \"") {
                    url.truncate(sp);
                }
                let label = if text.is_empty() && marker == "![" {
                    "image"
                } else {
                    text
                };
                if label.is_empty() {
                    rest = &rest[close + paren + 1..];
                    continue;
                }
                push(
                    &mut out,
                    label,
                    false,
                    false,
                    false,
                    false,
                    false,
                    Some(url),
                );
                rest = &rest[close + paren + 1..];
            }
            "`" => {
                let Some(end) = rest[1..].find('`') else {
                    push(&mut out, "`", false, false, false, false, false, None);
                    rest = &rest[1..];
                    continue;
                };
                push(
                    &mut out,
                    &rest[1..1 + end],
                    false,
                    false,
                    true,
                    false,
                    false,
                    None,
                );
                rest = &rest[end + 2..];
            }
            "<br>" | "<br/>" => {
                br_span(&mut out);
                rest = &rest[marker.len()..];
            }
            "~~" => {
                // Strike: a pair with non-empty inside, like the em arms.
                let Some(end) = rest[2..].find("~~") else {
                    push(&mut out, "~~", false, false, false, false, false, None);
                    rest = &rest[2..];
                    continue;
                };
                if end == 0 {
                    push(&mut out, "~~", false, false, false, false, false, None);
                    rest = &rest[2..];
                    continue;
                }
                push(
                    &mut out,
                    &rest[2..2 + end],
                    false,
                    false,
                    false,
                    true,
                    false,
                    None,
                );
                rest = &rest[2 + end + 2..];
            }
            m => {
                // ** / __ / * / _: a pair with non-empty inside; an unclosed
                // or empty match is a literal marker.
                let Some(end) = rest[m.len()..].find(m) else {
                    push(&mut out, m, false, false, false, false, false, None);
                    rest = &rest[m.len()..];
                    continue;
                };
                if end == 0 {
                    push(&mut out, m, false, false, false, false, false, None);
                    rest = &rest[m.len()..];
                    continue;
                }
                let (bold, italic) = (m.len() == 2, m.len() == 1);
                push(
                    &mut out,
                    &rest[m.len()..m.len() + end],
                    bold,
                    italic,
                    false,
                    false,
                    false,
                    None,
                );
                rest = &rest[m.len() + end + m.len()..];
            }
        }
    }
    out.retain(|s| !s.text.is_empty() || s.br);
    out
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
        assert_eq!(
            b[3],
            Block::List {
                items: vec![ListItem {
                    text: "item".into(),
                    task: None
                }],
                ordered: false
            }
        );
        assert_eq!(b[4], Block::Code("code".into()));
    }

    #[test]
    fn setext_and_ordered() {
        let b = blocks("Book Title\n=====\n\n1. one\n2. two");
        assert_eq!(b[0], Block::Heading(1, "Book Title".into()));
        assert_eq!(
            b[1],
            Block::List {
                items: vec![
                    ListItem {
                        text: "one".into(),
                        task: None
                    },
                    ListItem {
                        text: "two".into(),
                        task: None
                    },
                ],
                ordered: true
            }
        );
    }

    #[test]
    fn task_lists() {
        let b = blocks("- [ ] milk\n- [x] eggs\n- plain\n- [ ]");
        let items = match &b[0] {
            Block::List { items, .. } => items,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            items[0],
            ListItem {
                text: "milk".into(),
                task: Some(false)
            }
        );
        assert_eq!(
            items[1],
            ListItem {
                text: "eggs".into(),
                task: Some(true)
            }
        );
        assert_eq!(
            items[2],
            ListItem {
                text: "plain".into(),
                task: None
            }
        );
        assert_eq!(
            items[3],
            ListItem {
                text: String::new(),
                task: Some(false)
            }
        );
        // A bracket that isn't a checkbox stays text.
        let b = blocks("- [y] not a task");
        assert_eq!(
            match &b[0] {
                Block::List { items, .. } => items[0].clone(),
                other => panic!("{other:?}"),
            },
            ListItem {
                text: "[y] not a task".into(),
                task: None
            }
        );
    }

    #[test]
    fn tables_parse() {
        let src = "| a | b |\n| :--- | ---: |\n| one | two |\n| three | four |\n\nafter";
        let b = blocks(src);
        assert_eq!(
            b[0],
            Block::Table {
                head: vec!["a".into(), "b".into()],
                rows: vec![
                    vec!["one".into(), "two".into()],
                    vec!["three".into(), "four".into()],
                ],
                aligns: vec![Align::Left, Align::Right],
            }
        );
        assert_eq!(b[1], Block::Para("after".into()));
        // A setext underline is not a separator; a heading with a pipe in
        // it is still a heading.
        let b = blocks("a | b\n=====");
        assert_eq!(b[0], Block::Heading(1, "a | b".into()));
        // A body row stops at a block starter.
        let src = "| a |\n| - |\n| x |\n# head |";
        let b = blocks(src);
        assert_eq!(
            b[0],
            Block::Table {
                head: vec!["a".into()],
                rows: vec![vec!["x".into()]],
                aligns: vec![Align::Left],
            }
        );
        assert_eq!(b[1], Block::Heading(1, "head |".into()));
    }

    #[test]
    fn strike_spans() {
        let s = spans("a ~~gone~~ b");
        assert!(s.iter().any(|x| x.strike && x.text == "gone"));
        assert!(!s.iter().any(|x| x.text.contains('~')));
        // Unclosed stays literal (merged into the plain run).
        let s = spans("a ~~ b");
        assert!(s.iter().all(|x| !x.strike));
        assert!(
            s.iter()
                .map(|x| x.text.as_str())
                .collect::<String>()
                .contains("~~")
        );
        // ~~ wins its position over * by length-at-tie.
        let s = spans("**bold ~~both~~**");
        assert!(s.iter().any(|x| x.bold && x.text.contains("both")));
    }

    #[test]
    fn hr_variants() {
        for line in ["---", "***", "___", "-----"] {
            assert_eq!(blocks(line), vec![Block::Rule], "{line}");
        }
    }

    #[test]
    fn italics_single_markers() {
        let s = spans("Hoppe's *The Book* and _the idea_ end");
        assert!(s.iter().any(|x| x.italic && x.text == "The Book"));
        assert!(s.iter().any(|x| x.italic && x.text == "the idea"));
        assert!(!s.iter().any(|x| x.text.contains('*')));
    }

    #[test]
    fn bold_double_markers() {
        let s = spans("**bold** __also bold__ end");
        assert!(s.iter().any(|x| x.bold && x.text == "bold"));
        assert!(s.iter().any(|x| x.bold && x.text == "also bold"));
    }

    #[test]
    fn link_with_title() {
        let s = spans("see [the tao]( https://x.test \"Tao\" ) now");
        assert_eq!(s[1].link.as_deref(), Some("https://x.test"));
        assert_eq!(s[1].text, "the tao");
    }

    #[test]
    fn angle_autolink() {
        let s = spans("at <https://x.test> now");
        assert!(
            s.iter()
                .any(|x| x.link.as_deref() == Some("https://x.test"))
        );
    }

    #[test]
    fn br_breaks() {
        let s = spans("one<br>two");
        assert!(s.iter().any(|x| x.br));
        let b = blocks("para\n\n<br>\n\nmore");
        assert!(b.contains(&Block::Break));
    }

    #[test]
    fn unclosed_marker_stays_literal() {
        let s = spans("a ** b");
        assert_eq!(
            s.iter().map(|x| x.text.as_str()).collect::<String>(),
            "a ** b"
        );
        assert!(!s.iter().any(|x| x.bold));
    }

    #[test]
    fn image_line_variants() {
        assert_eq!(
            img_only("![x](https://a.test/p.png)").map(|(u, _)| u),
            Some("https://a.test/p.png".into())
        );
        assert_eq!(
            img_only("![x]( https://a.test/p.png )").map(|(u, _)| u),
            Some("https://a.test/p.png".into())
        );
        assert_eq!(
            img_only("![x](https://a.test/p.png \"t\")").map(|(u, _)| u),
            Some("https://a.test/p.png".into())
        );
        assert_eq!(img_only("https://a.test/p.mp4"), None);
    }
}
