//! Article reader page — the markdown port (TODO(port): crates/app/src/cards.rs
//! `article_reader` — the GTK reader pushed an adw page; here it's a pane).
//! Head: title, byline (avatar, author, published, reading time), cover.
//! Then the markdown blocks: paragraphs through the word flow (entities
//! resolve, markdown styles ride along), headings, quotes, ordered/unordered
//! lists, code, rules, own-line images. The blocks ride a `list` (the feed's
//! scroll machinery — plain overflow divs don't take wheel input), one item
//! per block, built from data inside the item closure. Comments (NIP-22)
//! stay with the GTK original for now — no comments section yet.

use std::{collections::HashMap, sync::Arc};

use gpui::{
    div, img, list, prelude::*, px, relative, rgb, AnyElement, Context, Div, Entity,
    EventEmitter, FontWeight, ListAlignment, ListState, Render, SharedString, Window,
};
use nostr::prelude::*;
use satori_core::manager::{Core, FeedEvent, FeedRx, Profile};
use satori_core::nip::article;

use crate::{markdown, media, theme};
use super::{content, nav::Nav};

pub struct ArticleView {
    core: Arc<Core>,
    ev: Event,
    data: article::ArticleData,
    profiles: HashMap<String, Profile>,
    list: ListState,
}

impl EventEmitter<Nav> for ArticleView {}

impl ArticleView {
    pub fn new(core: Arc<Core>, ev: Event, cx: &mut Context<Self>) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        crate::timefmt::watch(cx); // repaint on mode/height change
        let data = article::parse(&ev).unwrap_or_else(|| article::ArticleData {
            title: "Untitled".into(),
            summary: String::new(),
            image: None,
            published_at: None,
            slug: String::new(),
        });
        let view = Self {
            core,
            ev,
            data,
            profiles: HashMap::new(),
            list: ListState::new(0, ListAlignment::Top, px(400.)),
        };
        // Mentions in the body resolve to @names, same as the feed and
        // thread panes (TODO(port): content.ts mentionPubkeys) — and the
        // author, for the byline.
        view.want_names(cx);
        view.want_media(cx);
        view
    }

    /// Hydrate names for the byline + mentioned pubkeys — otherwise the
    /// byline degrades to @npub1… (thread.rs want_names, same seam).
    fn want_names(&self, cx: &mut Context<Self>) {
        let mut want = vec![self.ev.pubkey];
        want.extend(content::mention_pks(&self.ev.content));
        let mut seen = std::collections::HashSet::new();
        let missing: Vec<PublicKey> = want
            .into_iter()
            .filter(|pk| !self.profiles.contains_key(&pk.to_string()) && seen.insert(pk.to_string()))
            .collect();
        if !missing.is_empty() {
            self.pump(self.core.ensure_profiles(missing), cx);
        }
    }

    /// Queue fetches for the cover + every own-line image in the body.
    fn want_media(&self, cx: &mut Context<Self>) {
        let mut urls = content::media_urls_cached(&self.ev).iter().cloned().collect::<Vec<_>>();
        if let Some(image) = &self.data.image {
            urls.push(image.clone());
        }
        for block in markdown::blocks(&self.ev.content) {
            if let markdown::Block::Image { url } = block {
                urls.push(url);
            }
        }
        for url in urls {
            media::fetch(&self.core, url, cx.entity_id(), cx);
        }
    }

    fn pump(&self, mut rx: FeedRx, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |this, cx| this.on_event(ev, cx));
            }
        })
        .detach();
    }

    fn on_event(&mut self, ev: FeedEvent, cx: &mut Context<Self>) {
        if let FeedEvent::Profiles(profiles) = ev {
            for p in profiles {
                self.profiles.insert(p.pubkey.clone(), p);
            }
            cx.notify();
        }
    }

    fn name_for(&self, pk: &PublicKey) -> String {
        self.profiles
            .get(&pk.to_string())
            .map(|p| p.display())
            .unwrap_or_else(|| format!("{}…", &pk.to_string()[..8]))
    }
}

/// One list item: the reading column's width + the block's bottom rhythm
/// (list items carry no gaps of their own).
fn block_item(el: Div, pad: f32) -> Div {
    div()
        .w_full()
        .max_w(px(660.0))
        .pb(px(pad))
        .child(el)
}

/// Render one markdown block (the free fn stands behind the list's item
/// closure, which rebuilds items from data on each visible row).
fn render_block(
    block: markdown::Block,
    idx: usize,
    ctx: &content::Body,
    handle: &Entity<ArticleView>,
) -> AnyElement {
    match block {
        markdown::Block::Para(text) => {
            block_item(md_flow(&text, 15.0, false, ctx, handle), 14.0).into_any_element()
        }
        markdown::Block::Heading(level, text) => {
            // The ladder sits UNDER the 22px title (H1 just below it, deeper
            // levels stepping down toward body size); a little air ABOVE each
            // heading separates it from the paragraph it introduces — the
            // reading rhythm hangs on that gap.
            let size = [20.0, 18.0, 16.0, 15.0, 14.5, 14.0][(level - 1).clamp(0, 5)];
            let top = [10.0, 8.0, 6.0, 4.0, 2.0, 0.0][(level - 1).clamp(0, 5)];
            let head = div()
                .w_full()
                .pt(px(top))
                .font_family(theme::SERIF)
                .text_size(px(size))
                .line_height(relative(1.2))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme::ink()))
                .child(text);
            block_item(head, 8.0).into_any_element()
        }
        markdown::Block::Quote(text) => {
            let q = div()
                .w_full()
                .pl_3()
                .py_1()
                .border_l_2()
                .border_color(rgb(theme::hairline()))
                .child(md_flow(&text, 14.0, true, ctx, handle));
            block_item(q, 14.0).into_any_element()
        }
        markdown::Block::List { items, ordered } => {
            let mut list_el = div().w_full().flex().flex_col().gap_1();
            for (n, li) in items.into_iter().enumerate() {
                let marker = if ordered {
                    format!("{}.", n + 1)
                } else {
                    "·".to_string()
                };
                list_el = list_el.child(
                    div()
                        .w_full()
                        .flex()
                        .gap_2()
                        .child(
                            div()
                                .font_family(theme::SERIF)
                                .text_size(px(15.0))
                                .text_color(rgb(theme::muted()))
                                .child(marker),
                        )
                        .child(md_flow(&li, 15.0, false, ctx, handle)),
                );
            }
            block_item(list_el, 14.0).into_any_element()
        }
        markdown::Block::Code(text) => {
            let code = div()
                .id(SharedString::from(format!("code-{idx}")))
                .overflow_x_scroll()
                .w_full()
                .p_3()
                .rounded(px(4.))
                .bg(rgb(theme::panel()))
                .font_family(theme::MONO)
                .text_size(px(12.5))
                .line_height(relative(1.35))
                .text_color(rgb(theme::ink()))
                .child(text);
            block_item(div().w_full().child(code), 14.0).into_any_element()
        }
        markdown::Block::Break => {
            // An author's own <br>: a quiet spacer, not a rule.
            block_item(div().w_full().h(px(6.0)), 0.0).into_any_element()
        }
        markdown::Block::Rule => {
            let rule = div()
                .w_full()
                .py_1()
                .border_b_1()
                .border_color(rgb(theme::hairline()));
            block_item(rule, 14.0).into_any_element()
        }
        markdown::Block::Image { url } => {
            block_item(image_tile(&url, ctx.media), 14.0).into_any_element()
        }
    }
}

/// One markdown paragraph through the word flow: markdown spans carry the
/// styling; plain words classify as entities (urls, mentions) exactly like
/// note bodies. Link spans wrap as one unit — a link is one word to the
/// reader's eye anyway. (Line height 1.35: long-form serif needs the air —
/// the note cards' 1.2 is for short bursts.)
fn md_flow(
    para: &str,
    size: f32,
    quiet: bool,
    ctx: &content::Body,
    handle: &Entity<ArticleView>,
) -> Div {
    let mut flow = div().w_full().flex().flex_wrap().gap_1();
    for span in markdown::spans(para) {
        // A <br> span: a full-width spacer — flex-wrap pushes the rest of
        // the flow to the next line (the web's literal line break).
        if span.br {
            flow = flow.child(div().w_full().h(px(2.0)));
            continue;
        }
        let is_link = span.link.is_some();
        let label = span.text.clone();
        let styled = |text: String| {
            div()
                .font_family(if span.code { theme::MONO } else { theme::SERIF })
                .text_size(px(if span.code { size * 0.88 } else { size }))
                .line_height(relative(1.35))
                .text_color(rgb(if quiet || is_link {
                    theme::muted()
                } else {
                    theme::ink()
                }))
                .when(span.bold, |d| d.font_weight(FontWeight::BOLD))
                .when(span.italic, |d| d.italic())
                .when(is_link || span.code, |d| d.underline())
                .child(text)
        };
        match span.link {
            Some(url) => {
                let id = SharedString::from(format!("mdlink-{label}-{url}"));
                flow = flow.child(
                    styled(label)
                        .id(id)
                        .cursor_pointer()
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                );
            }
            None => {
                // Plain words ride the entity classifier: bare urls and
                // npub mentions stay clickable inside styled runs.
                for word in span.text.split_whitespace() {
                    let (core, punct) = content::split_trailing(word);
                    match content::classify_token(core) {
                        content::Tok::Word(w) => {
                            flow = flow.child(styled(if punct.is_empty() {
                                w
                            } else {
                                format!("{w}{punct}")
                            }));
                        }
                        tok => {
                            flow =
                                flow.child(content::token_el(tok, punct, core.len(), ctx, handle));
                        }
                    }
                }
            }
        }
    }
    flow
}

/// One image tile: the media frame at a fixed reading height — media_tile
/// semantics, fed by the shared cache.
fn image_tile(url: &str, snap: &media::Snapshot) -> Div {
    let png = snap.png(url);
    // Aspect-correct once the bytes are in (reading column ~660 minus
    // padding); a provisional box until then.
    let height = snap
        .size(url)
        .filter(|(w, _)| *w > 0)
        .map_or(360.0, |(w, h)| (628.0 * h as f32 / w as f32).clamp(80.0, 560.0));
    let frame = div()
        .w_full()
        .h(px(height))
        .flex_none()
        .overflow_hidden()
        .rounded(px(4.))
        .border_1()
        .border_color(rgb(theme::hairline()))
        .bg(rgb(theme::panel()));
    match png {
        Some(png) => frame.child(img(png).size_full()),
        None => frame,
    }
}

impl Render for ArticleView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let media_snap = media::Snapshot::new(cx);
        let quotes: content::Quotes = HashMap::new();
        let handle = cx.entity();

        // Data the item closure rebuilds elements from (AnyElement isn't
        // Clone — the thread pane's closure does the same dance).
        let title = self.data.title.clone();
        let author_pk = self.ev.pubkey;
        let author = self.name_for(&author_pk);
        let author_pic = self
            .profiles
            .get(&author_pk.to_string())
            .map(|p| p.picture.clone())
            .unwrap_or_default();
        // Evicted media comes back on render (thread.rs render, same
        // shape — fetch is a silent no-op for entries the cache holds, and
        // only a landing notifies, so no render loop): the byline avatar,
        // the cover, and every body image. A long read outlives the
        // cache's photo budget; without the sweep an evicted mid-article
        // image stayed dark for good (want_media only ran at construction).
        // The markdown parses ONCE: the sweep and the render list share
        // the same blocks (the double parse ran the full block scan twice
        // per render).
        let cover = content::article_cover(&self.ev);
        let blocks = markdown::blocks(&self.ev.content);
        if !author_pic.is_empty() {
            media::fetch(&self.core, author_pic.clone(), cx.entity_id(), cx);
        }
        if let Some(url) = &cover {
            media::fetch(&self.core, url.clone(), cx.entity_id(), cx);
        }
        for url in content::media_urls_cached(&self.ev).iter().cloned() {
            media::fetch(&self.core, url, cx.entity_id(), cx);
        }
        for block in &blocks {
            if let markdown::Block::Image { url } = block {
                media::fetch(&self.core, url.clone(), cx.entity_id(), cx);
            }
        }
        // Mode-aware stamp; ensure refines Block mode from the bridge
        // (render-path spawns are silent no-ops when cached — media.rs's
        // eviction re-request is the same pattern).
        let published = self.data.published_at.map(|ts| {
            crate::timefmt::ensure(ts, cx);
            crate::timefmt::stamp(ts, cx)
        });
        // Reading time at ~220 wpm (content.ts's readingMinutes rate).
        let minutes = self.ev.content.split_whitespace().count() / 220 + 1;
        let profiles = self.profiles.clone();
        let media = media_snap;
        let block_count = blocks.len();

        // Head items (title, byline, cover) then one per markdown block.
        let head_count = 2 + cover.is_some() as usize;
        let item_count = head_count + block_count;
        if self.list.item_count() != item_count {
            self.list.reset(item_count);
        }

        div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .justify_center()
                    .child(
                        // list() has no intrinsic height — it must be told to
                        // fill its container or it lays out at 0px and paints
                        // nothing (the feed carries the same comment).
                        list(self.list.clone(), move |idx, _w, _cx| {
                            let body_ctx = content::Body {
                                media: &media,
                                quotes: &quotes,
                                profiles: &profiles,
                            };
                            match idx {
                                0 => block_item(
                                    div()
                                        .w_full()
                                        .font_family(theme::SERIF)
                                        .text_size(px(22.0))
                                        .line_height(relative(1.25))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(theme::ink()))
                                        .child(title.clone()),
                                    10.0,
                                )
                                .into_any_element(),
                                1 => block_item(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(media::avatar(
                                            &media,
                                            &author_pic,
                                            &author,
                                            24.0,
                                        ))
                                        .child(
                                            div()
                                                .flex()
                                                .items_baseline()
                                                .gap_2()
                                                .text_size(px(12.5))
                                                .text_color(rgb(theme::muted()))
                                                .child(
                                                    div()
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .text_color(rgb(theme::ink()))
                                                        .child(author.clone()),
                                                )
                                                .children(
                                                    published.clone().map(|t| {
                                                        // The timestamp is
                                                        // the third mode
                                                        // toggle (timefmt.rs):
                                                        // tap cycles.
                                                        div()
                                                            .id("reader-published")
                                                            .cursor_pointer()
                                                            .on_click(|_, _, cx| {
                                                                crate::timefmt::cycle(cx)
                                                            })
                                                            .child(format!("· {t}"))
                                                    }),
                                                )
                                                .child(format!("· {minutes} min read")),
                                        ),
                                    18.0,
                                )
                                .into_any_element(),
                                2 if cover.is_some() => block_item(
                                    image_tile(cover.as_deref().unwrap_or(""), &media),
                                    18.0,
                                )
                                .into_any_element(),
                                i => render_block(
                                    blocks[i - head_count].clone(),
                                    i - head_count,
                                    &body_ctx,
                                    &handle,
                                ),
                            }
                        })
                        .w_full()
                        .max_w(px(660.0))
                        .h_full(),
                    ),
            )
    }
}
