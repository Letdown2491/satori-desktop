//! Article reader page — the markdown port (TODO(port): crates/app/src/cards.rs
//! `article_reader` — the GTK reader pushed an adw page; here it's a pane).
//! Title, header image, then the markdown blocks: paragraphs through the
//! word flow (entities resolve, markdown styles ride along), headings, quotes,
//! lists, code, rules, own-line images. Comments (NIP-22) stay with the GTK
//! original for now — the pane has no comments section yet.

use std::{collections::HashMap, sync::Arc};

use gpui::{
    div, img, prelude::*, px, relative, rgb, Context, Div, Entity, EventEmitter, FontWeight,
    Render, SharedString, Window,
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
}

impl EventEmitter<Nav> for ArticleView {}

impl ArticleView {
    pub fn new(core: Arc<Core>, ev: Event, cx: &mut Context<Self>) -> Self {
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
        };
        // Mentions in the body resolve to @names, same as the feed and
        // thread panes (TODO(port): content.ts mentionPubkeys).
        view.want_names(cx);
        view.want_media(cx);
        view
    }

    /// Hydrate names for mentioned pubkeys — otherwise mentions degrade to
    /// @npub1… (thread.rs want_names, same seam).
    fn want_names(&self, cx: &mut Context<Self>) {
        let mut seen = std::collections::HashSet::new();
        let missing: Vec<PublicKey> = content::mention_pks(&self.ev.content)
            .into_iter()
            .filter(|pk| !self.profiles.contains_key(&pk.to_string()) && seen.insert(pk.to_string()))
            .collect();
        if !missing.is_empty() {
            self.pump(self.core.ensure_profiles(missing), cx);
        }
    }

    /// Queue fetches for the header image + every own-line image in the body.
    fn want_media(&self, cx: &mut Context<Self>) {
        let mut urls = content::media_urls(&self.ev);
        if let Some(image) = &self.data.image {
            urls.push(image.clone());
        }
        for block in markdown::blocks(&self.ev.content) {
            if let markdown::Block::Image(url) = block {
                urls.push(url);
            }
        }
        for url in urls {
            media::fetch(&self.core, url, cx, |_, _, _, cx| cx.notify());
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

    fn render_blocks(
        &self,
        blocks: Vec<markdown::Block>,
        ctx: &content::Body,
        handle: &Entity<Self>,
    ) -> Div {
        let mut col = div().w_full().flex().flex_col().gap_3();
        for block in blocks {
            match block {
                markdown::Block::Para(text) => {
                    col = col.child(self.md_flow(&text, 15.0, false, ctx, handle));
                }
                markdown::Block::Heading(level, text) => {
                    // The GTK sizes: 28000/23000/19000 Pango units.
                    let size = [28.0, 23.0, 19.0][(level - 1).clamp(0, 2)];
                    col = col.child(
                        div()
                            .w_full()
                            .font_family(theme::SERIF)
                            .text_size(px(size))
                            .line_height(relative(1.2))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(theme::INK))
                            .child(text),
                    );
                }
                markdown::Block::Quote(text) => {
                    col = col.child(
                        div()
                            .w_full()
                            .pl_3()
                            .py_1()
                            .border_l_2()
                            .border_color(rgb(theme::HAIRLINE))
                            .child(self.md_flow(&text, 14.0, true, ctx, handle)),
                    );
                }
                markdown::Block::List(items) => {
                    let mut list = div().w_full().flex().flex_col().gap_1();
                    for item in items {
                        list = list.child(
                            div()
                                .w_full()
                                .flex()
                                .gap_2()
                                .child(
                                    div()
                                        .font_family(theme::SERIF)
                                        .text_size(px(15.0))
                                        .text_color(rgb(theme::MUTED))
                                        .child("·"),
                                )
                                .child(self.md_flow(&item, 15.0, false, ctx, handle)),
                        );
                    }
                    col = col.child(list);
                }
                markdown::Block::Code(text) => {
                    col = col.child(
                        div()
                            .w_full()
                            .p_3()
                            .rounded(px(4.))
                            .bg(rgb(theme::PANEL))
                            .font_family(theme::MONO)
                            .text_size(px(12.5))
                            .line_height(relative(1.35))
                            .text_color(rgb(theme::INK))
                            .child(text),
                    );
                }
                markdown::Block::Rule => {
                    col = col.child(
                        div()
                            .w_full()
                            .py_1()
                            .border_b_1()
                            .border_color(rgb(theme::HAIRLINE)),
                    );
                }
                markdown::Block::Image(url) => {
                    col = col.child(image_tile(&url, ctx.media));
                }
            }
        }
        col
    }

    /// One markdown paragraph through the word flow: markdown spans carry the
    /// styling; plain words classify as entities (urls, mentions) exactly like
    /// note bodies. Link spans wrap as one unit — a link is one word to the
    /// reader's eye anyway.
    fn md_flow(
        &self,
        para: &str,
        size: f32,
        quiet: bool,
        ctx: &content::Body,
        handle: &Entity<Self>,
    ) -> Div {
        let mut flow = div().w_full().flex().flex_wrap().gap_1();
        for span in markdown::spans(para) {
            let is_link = span.link.is_some();
            let label = span.text.clone();
            let styled = |text: String| {
                div()
                    .font_family(if span.code { theme::MONO } else { theme::SERIF })
                    .text_size(px(if span.code { size * 0.88 } else { size }))
                    .line_height(relative(1.2))
                    .text_color(rgb(if quiet || is_link {
                        theme::MUTED
                    } else {
                        theme::INK
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
                        match content::classify_token(&core) {
                            content::Tok::Word(w) => {
                                flow = flow.child(styled(if punct.is_empty() {
                                    w
                                } else {
                                    format!("{w}{punct}")
                                }));
                            }
                            tok => {
                                flow = flow.child(content::token_el(
                                    tok,
                                    punct,
                                    core.len(),
                                    ctx,
                                    handle,
                                ));
                            }
                        }
                    }
                }
            }
        }
        flow
    }
}

/// One image tile: the media frame at a fixed reading height — media_tile
/// semantics, fed by the shared cache.
fn image_tile(url: &str, snap: &media::Snapshot) -> impl IntoElement {
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
        .border_color(rgb(theme::HAIRLINE))
        .bg(rgb(theme::PANEL));
    match png {
        Some(png) => frame.child(img(png).size_full()),
        None => frame,
    }
}

impl Render for ArticleView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let media_snap = media::Snapshot::new(cx);
        let quotes: content::Quotes = HashMap::new();
        let ctx = content::Body {
            media: &media_snap,
            quotes: &quotes,
            profiles: &self.profiles,
        };
        let handle = cx.entity();
        let blocks = markdown::blocks(&self.ev.content);
        div()
            .id("reader-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .justify_center()
            .child(
                // The centered reading column (the GTK reader's 660px).
                div()
                    .w_full()
                    .max_w(px(660.0))
                    .py_6()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .w_full()
                            .font_family(theme::SERIF)
                            .text_size(px(28.0))
                            .line_height(relative(1.2))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(theme::INK))
                            .child(self.data.title.clone()),
                    )
                    .children(self.data.published_at.map(|ts| {
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(theme::MUTED))
                            .child(super::feed::rel_time(ts))
                    }))
                    .children(self.data.image.as_ref().map(|url| {
                        image_tile(url, &media_snap).into_any_element()
                    }))
                    .child(self.render_blocks(blocks, &ctx, &handle)),
            )
    }
}
