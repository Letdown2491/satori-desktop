//! The composer: text, multi-image attach, undo window, draft persistence.
//!
//! TODO(port): crates/app/src/composer.rs (Adw dialog) — P2. Same write path:
//! attachments upload through `blossom_upload` one by one, then the note goes
//! out via `post_note` (text/reply) or `post_picture` (kind 20 with imetas),
//! pops on Held — the countdown toast lives in the shell. Draft semantics
//! match the GTK composer: a top-level compose restores whatever was stashed,
//! and closing with un-published text stashes it again (replies get neither).

use std::sync::Arc;

use gpui::{
    actions, div, img, prelude::*, px, rgb, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Global, Render, SharedString, Window,
};
use nostr::prelude::*;
use satori_core::manager::{Core, Profile, PublishEvent};

use crate::{input::Input, media, theme};
use super::nav::Nav;

actions!(composer, [Publish]);

/// `ctrl-enter` publishes — the web composer's keybinding (composer.rs:307).
/// Bound on the "Input" context, where the composer's focus lives; the action
/// bubbles up the focus path to this view.
pub fn bind_composer_keys(cx: &mut gpui::App) {
    cx.bind_keys([gpui::KeyBinding::new("ctrl-enter", Publish, Some("Input"))]);
}

/// The stashed top-level draft (composer.rs's DRAFT thread-local, session
/// scope like the web app's note.ts saveDraft — one draft at a time).
/// v1.4: the stash carries the uploaded attachment imetas too, so the
/// instant reopen restores the gallery, not just the words.
#[derive(Default)]
pub struct Draft(pub Option<SessionDraft>);
impl Global for Draft {}

/// What a session-stashed draft holds (v1.4): the buffer + the ready
/// imeta arrays (the blobs are on Blossom; nothing re-uploads).
#[derive(Default)]
pub struct SessionDraft {
    pub text: String,
    pub attachments: Vec<Vec<String>>,
}

fn draft_take(cx: &mut App) -> Option<SessionDraft> {
    cx.default_global::<Draft>().0.take()
}

/// The synced-draft candidate: (reply parent, quote id, buffer, the ready
/// attachment imetas) — save_draft's arguments, basically.
pub type DraftCandidate = (Option<EventId>, Option<EventId>, String, Vec<Vec<String>>);

struct Attachment {
    /// Stable within the composer — an upload completion finds its tile
    /// by id (the X button can have removed an earlier one meanwhile).
    id: u64,
    name: String,
    /// Raw bytes while composing — the tile preview, and the video
    /// poster grab's source. None on a draft restore (v1.4): no bytes
    /// ride the synced draft; the tile shows the CDN copy through the
    /// media cache instead.
    bytes: Option<Vec<u8>>,
    /// The tile preview: the ORIGINAL encoded bytes handed to gpui (it
    /// decodes on its own asset thread) — the note's gallery, live.
    image: Option<gpui::Image>,
    /// The Blossom imeta once uploaded (v1.4); None while the attach-time
    /// upload chain is still working on it. The synced draft and the
    /// publish both take this verbatim.
    imeta: Option<Vec<String>>,
    /// NIP-68 `alt` — set in the composer's alt editor, written INTO the
    /// imeta (the tag is the single source of truth; drafts, publish and
    /// the render side's badge all read it from there).
    alt: Option<String>,
}

impl Attachment {
    /// Rebuild a tile from a synced draft's imeta (v1.4's restore path):
    /// name/mime parsed from the tag, preview left to the media cache
    /// (fetch kicks in attachment_row's render).
    fn from_imeta(imeta: Vec<String>) -> Option<Self> {
        let url = imeta
            .iter()
            .find(|e| e.starts_with("url "))
            .map(|e| e[4..].to_string())?;
        let name = url.rsplit('/').next().unwrap_or("attachment").to_string();
        let alt = imeta
            .iter()
            .find(|e| e.starts_with("alt "))
            .map(|e| e[4..].to_string());
        Some(Self {
            id: 0,
            name,
            bytes: None,
            image: None,
            alt,
            imeta: Some(imeta),
        })
    }

    /// The CDN url the media cache keys on (the tile's restored preview).
    fn url(&self) -> Option<String> {
        self.imeta
            .as_ref()?
            .iter()
            .find(|e| e.starts_with("url "))
            .map(|e| e[4..].to_string())
    }
}

/// The compose tab: a note (kind 1), a poll (NIP-88, kind 1068), or an
/// article (NIP-23, kind 30023). Replies carry no tabs — they're
/// notes/comments by nature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Note,
    Poll,
    Article,
}

/// The compose field's resting height, in lines of text: present without
/// dominating (note.rs composer.rs:114 — the reference's 8 rows grew up).
const LINES_MIN: f32 = 10.0;

/// Wrapped-line estimate for a buffer — the Input element fills its parent
/// (request_layout relative(1)), so intrinsic growth is computed here:
/// one line per break, plus a wrap every `width` characters. Slightly
/// overestimating only adds margin.
/// A quiet field label (the settings group title's voice, 12px semibold
/// muted) — the poll panel's two groups.
fn poll_label(text: &str) -> gpui::Div {
    div()
        .text_size(px(theme::T_SMALL))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(theme::muted()))
        .child(text.to_string())
}

fn est_lines(text: &str, width: f32) -> f32 {
    text.split('\n')
        .map(|l| 1.0 + (l.chars().count() as f32 / width).floor())
        .sum()
}

pub struct Composer {
    core: Arc<Core>,
    input: Entity<Input>,
    tab: Tab,
    /// Poll tab: the question, the options (min two), multiple-choices.
    poll_question: Entity<Input>,
    poll_options: Vec<Entity<Input>>,
    poll_multi: bool,
    /// Article tab (NIP-23): the structural fields. The BODY is the main
    /// note buffer (tab switches keep its text — a half-typed body stays);
    /// topics ride one comma-separated input (the web app's shape).
    art_title: Entity<Input>,
    art_summary: Entity<Input>,
    /// Cover image URL — pasted, or the Blossom blob the upload button
    /// writes back when it lands.
    art_cover: Entity<Input>,
    art_cover_busy: bool,
    art_topics: Entity<Input>,
    /// @-mentions: the pubkeys whose p tags ride the publish (content
    /// keeps the human-readable @Name).
    mentions: Vec<PublicKey>,
    /// The live @-suggestions (strip open when non-empty) + the token
    /// being completed.
    suggest: Vec<Profile>,
    mention_token: Option<String>,
    /// The `:`-emoji typeahead (WS4): the table hits for the token under
    /// the caret, the token, the arrow-key selection.
    emoji_suggest: Vec<&'static crate::emoji::Emoji>,
    emoji_token: Option<String>,
    emoji_sel: usize,
    reply_to: Option<Event>,
    /// The NIP-18 quote: folded into the content + a q tag at publish; the
    /// preview above the field says what's being quoted.
    quote: Option<Event>,
    attachments: Vec<Attachment>,
    /// Attachment id supply (monotonic; upload completions find their
    /// tile by this).
    next_att: u64,
    /// The open alt-text editor: which attachment id it edits + the one
    /// shared single-line field (research §4.2 — AltTextEditorDialog).
    alt_edit: Option<u64>,
    alt_input: Entity<Input>,
    /// WS4 schedule: the affordance on/off, the when field, the pending
    /// list's disclosure.
    sched_on: bool,
    sched_at: Entity<Input>,
    sched_list_open: bool,
    posting: bool,
    status: String,
    autofocused: bool,
    /// The tab whose field currently holds the caret — a tab switch hands
    /// focus to the new tab's field (the note field's handle dangles when
    /// the poll tab renders, and a dangling focus eats keystrokes).
    focused_tab: Option<Tab>,
    focus: FocusHandle,
}

impl EventEmitter<Nav> for Composer {}

impl Composer {
    pub fn new(
        core: Arc<Core>,
        reply_to: Option<Event>,
        quote: Option<Event>,
        cx: &mut Context<Self>,
    ) -> Self {
        crate::theme::watch(cx); // repaint on theme change
        // The nsec strip repaints when a paste trips the wire (input.rs's
        // set_global wakes this).
        cx.observe_global::<crate::input::NsecWarn>(|_, cx| cx.notify())
            .detach();
        let input = cx.new(Input::new);
        let focus = input.read(cx).focus_handle(cx);
        // D17 tripwire: the note field and the alt field are not places a
        // private key belongs — a pasted nsec is intercepted with a strip.
        input.update(cx, |i, _| i.tripwire = true);        // Restored attachments (stash or synced entry) build up here.
        let mut attachments: Vec<Attachment> = Vec::new();
        // Draft restore: a top-level compose reopens with whatever was typed
        // and closed before (composer.rs:114 — the take spends the stash).
        // A quote compose carries the quote, so it skips the stash too.
        if reply_to.is_none() && quote.is_none()
            && let Some(stash) = draft_take(cx)
            && !stash.text.trim().is_empty()
        {
            input.update(cx, |i, cx| i.set_text(&stash.text, cx));
            attachments = stash
                .attachments
                .iter()
                .filter_map(|m| Attachment::from_imeta(m.clone()))
                .collect();
        } else if reply_to.is_none() && quote.is_none() {
            // D38: no session stash — the synced one (NIP-78 kind 30078)
            // survives restarts; the take spends it (a close without a send
            // saves it right back). The newest top-level entry wins — that
            // includes the web app's imported wraps (id-marked, v1.1).
            let synced = core
                .own_draft_list()
                .into_iter()
                .filter(|d| d.matches(None, None))
                .max_by_key(|d| d.at);
            if let Some(d) = synced.filter(|d| !d.text.trim().is_empty()) {
                // Fire and forget: the publish rides the outbox; a failed
                // save leaves the slot intact (the next close re-syncs).
                match d.id {
                    // An import converts: the wrap dies (NIP-37's delete)
                    // and the text becomes the composer's — its close saves
                    // a Satori slot entry.
                    Some(id) => {
                        let _ = core.drop_draft(id);
                    }
                    None => {
                        let _ = core.save_draft(None, None, String::new(), Vec::new(), false);
                    }
                }
                input.update(cx, |i, cx| i.set_text(&d.text, cx));
                attachments = d
                    .attachments
                    .iter()
                    .filter_map(|m| Attachment::from_imeta(m.clone()))
                    .collect();
            }
        }
        // @-mention detection: watch the buffer, find an active `@token`.
        cx.observe(&input, |this, _input, cx| this.detect_mention(cx))
            .detach();
        let poll_question = cx.new(Input::new);
        let poll_options = (0..2)
            .map(|_| cx.new(Input::new))
            .collect();
        // The schedule field and the alt editor are single-line.
        let sched_at = cx.new(|cx| {
            let mut i = Input::new(cx);
            i.multiline = false;
            i.placeholder("YYYY-MM-DD HH:MM (your time)")
        });
        let alt_input = cx.new(|cx| {
            let mut i = Input::new(cx);
            i.multiline = false;
            i.tripwire = true;
            i.placeholder("describe the image, for readers who can't see it")
        });
        // Restored attachments get real ids too (the upload chain only
        // ever targets fresh picks, but the invariant stays simple: ids
        // are unique within a composer).
        for (i, a) in attachments.iter_mut().enumerate() {
            a.id = i as u64;
        }
        Self {
            core,
            input,
            tab: Tab::Note,
            poll_question,
            poll_options,
            poll_multi: false,
            art_title: cx.new(Input::new),
            art_summary: cx.new(Input::new),
            art_cover: cx.new(Input::new),
            art_cover_busy: false,
            art_topics: cx.new(Input::new),
            mentions: vec![],
            suggest: vec![],
            mention_token: None,
            reply_to,
            quote,
            next_att: attachments.len() as u64,
            attachments,
            alt_edit: None,
            alt_input,
            sched_on: false,
            sched_at,
            sched_list_open: false,
            emoji_suggest: vec![],
            emoji_token: None,
            emoji_sel: 0,
            posting: false,
            status: String::new(),
            autofocused: false,
            focused_tab: None,
            focus,
        }
    }

    fn pick_attachments(&mut self, cx: &mut Context<Self>) {
        if self.posting {
            return;
        }
        self.status = "choosing files…".into();
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            // Blocking portal dialog on the background executor; one thread
            // parked while the user picks is fine.
            let picked = cx
                .background_executor()
                .spawn(async move {
                    rfd::FileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif"])
                        .pick_files()
                })
                .await;
            // v1.4: attachments upload NOW, at attach time — the blobs
            // land on Blossom while the user keeps typing, the publish
            // embeds the finished imetas, and the synced draft (which
            // carries no bytes) can reference them. The chain stays
            // sequential within a batch (one blob at a time, the
            // publish-time discipline); two overlapping pick batches run
            // two chains — a deliberate user act, still bounded.
            let mut fresh: Vec<(u64, String, Vec<u8>)> = Vec::new();
            let _ = this.update(cx, |this, cx| {
                this.status.clear();
                for path in picked.into_iter().flatten() {
                    let ext = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    let content_type = match ext.as_str() {
                        "png" => "image/png",
                        "jpg" | "jpeg" => "image/jpeg",
                        "webp" => "image/webp",
                        "gif" => "image/gif",
                        _ => continue,
                    };
                    match std::fs::read(&path) {
                        Ok(bytes) => {
                            let format = match content_type {
                                "image/png" => gpui::ImageFormat::Png,
                                "image/jpeg" => gpui::ImageFormat::Jpeg,
                                "image/webp" => gpui::ImageFormat::Webp,
                                _ => gpui::ImageFormat::Gif,
                            };
                            let image = gpui::Image::from_bytes(format, bytes.clone());
                            let id = this.next_att;
                            this.next_att += 1;
                            this.attachments.push(Attachment {
                                id,
                                name: path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default(),
                                bytes: Some(bytes.clone()),
                                image: Some(image),
                                imeta: None,
                                alt: None,
                            });
                            fresh.push((id, content_type.into(), bytes));
                        }
                        Err(e) => this.status = format!("couldn't read {}: {e}", path.display()),
                    }
                }
                cx.notify();
            });
            // The chain: per attachment — upload, then the video poster
            // grab rides its OWN upload (the D38 order, moved here from
            // the publish); a failed blob removes the tile with a word.
            for (id, content_type, bytes) in fresh {
                let mut rx = core.blossom_upload(bytes.clone(), content_type.clone());
                let mut imeta: Option<Vec<String>> = None;
                while let Some(ev) = rx.recv().await {
                    match ev {
                        PublishEvent::Uploaded { imeta: m, .. } => {
                            imeta = Some(m);
                            break;
                        }
                        PublishEvent::Failed { error, .. } => {
                            let _ = this.update(cx, |this, cx| {
                                this.attachments.retain(|a| a.id != id);
                                this.status = format!("upload failed: {error}");
                                cx.notify();
                            });
                            break;
                        }
                        _ => {}
                    }
                }
                let Some(mut imeta) = imeta else { continue };
                // D38: a video carries its first frame as a second blob —
                // the imeta gains `image` + `blurhash` so every client (and
                // our own tile) paints it with no runtime sniff. The grab
                // runs on the background pool (a GStreamer demux has no
                // place on the UI thread); a failed grab or poster upload
                // DEGRADES to the bare imeta — never blocks the attach.
                if content_type.starts_with("video/") {
                    let grabbed = cx
                        .background_executor()
                        .spawn(async move { crate::poster::grab(&bytes) })
                        .await;
                    if let Some((png, hash)) = grabbed {
                        let mut rx = core.blossom_upload(png, "image/png".to_string());
                        let mut poster_url: Option<String> = None;
                        while let Some(ev) = rx.recv().await {
                            match ev {
                                PublishEvent::Uploaded { url, .. } => {
                                    poster_url = Some(url);
                                    break;
                                }
                                PublishEvent::Failed { .. } => break,
                                _ => {}
                            }
                        }
                        if let Some(poster_url) = poster_url {
                            imeta.push(format!("image {poster_url}"));
                            imeta.push(format!("blurhash {hash}"));
                        }
                    }
                }
                let _ = this.update(cx, |this, cx| {
                    if let Some(a) = this.attachments.iter_mut().find(|a| a.id == id) {
                        a.imeta = Some(imeta);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// The @-token under the caret, if any: `@handle` with no whitespace
    /// between the @ and the cursor.
    fn mention_under_caret(&self, cx: &mut Context<Self>) -> Option<String> {
        let i = self.input.read(cx);
        let text = &i.content;
        let cursor = i.cursor_offset().min(text.len());
        let before = &text[..cursor];
        let at = before.rfind('@')?;
        let token = &before[at + 1..];
        (token.len() <= 30 && !token.contains(char::is_whitespace))
            .then(|| token.to_string())
    }

    fn detect_mention(&mut self, cx: &mut Context<Self>) {
        let token = self.mention_under_caret(cx);
        match &token {
            Some(q) if !q.is_empty() => {
                let candidates = self.core.mention_candidates(q);
                if candidates.is_empty() {
                    self.mention_token = None;
                    self.suggest.clear();
                } else {
                    self.mention_token = Some(q.clone());
                    self.suggest = candidates;
                }
            }
            _ => {
                self.mention_token = None;
                self.suggest.clear();
            }
        }
        self.detect_emoji(cx);
        cx.notify();
    }

    /// A suggestion was picked: replace the @token in the buffer with the
    /// chosen name, remember the pubkey for the p tags.
    fn pick_mention(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(profile) = self.suggest.get(idx).cloned() else {
            return;
        };
        if let Some(q) = self.mention_token.clone() {
            let i = self.input.read(cx);
            let text = i.content.clone();
            let cursor = i.cursor_offset().min(text.len());
            let needle = format!("@{q}");
            let at = text[..cursor].rfind(&needle).unwrap_or(cursor);
            let mut next = String::with_capacity(text.len());
            next.push_str(&text[..at]);
            next.push_str(&format!("@{} ", profile.display()));
            next.push_str(&text[cursor..]);
            self.input.update(cx, |i, cx| i.set_text(&next, cx));
        }
        let pk_hex = profile.pubkey.clone();
        if let Ok(pk) = PublicKey::from_hex(&pk_hex)
            && !self.mentions.contains(&pk)
        {
            self.mentions.push(pk);
        }
        self.suggest.clear();
        self.mention_token = None;
        cx.notify();
    }

    /// The `:token` under the caret, if any — no whitespace, no `/` (a
    /// URL's `://` must not open the strip), bounded length.
    fn emoji_under_caret(&self, cx: &mut Context<Self>) -> Option<String> {
        let i = self.input.read(cx);
        let text = &i.content;
        let cursor = i.cursor_offset().min(text.len());
        let before = &text[..cursor];
        let colon = before.rfind(':')?;
        let token = &before[colon + 1..];
        (token.len() <= 24
            && !token.contains(char::is_whitespace)
            && !token.contains('/')
            && !token.contains(':'))
            .then(|| token.to_string())
    }

    /// The emoji side of the same buffer watch: the strip's candidates,
    /// and Enter's allegiance while it's open (the typeahead inserts the
    /// shortcode; the field must not add a newline under it).
    fn detect_emoji(&mut self, cx: &mut Context<Self>) {
        let token = self.emoji_under_caret(cx);
        match &token {
            Some(t) => {
                self.emoji_suggest = crate::emoji::candidates(t);
                self.emoji_token = Some(t.clone());
            }
            None => {
                self.emoji_suggest.clear();
                self.emoji_token = None;
                self.emoji_sel = 0;
            }
        }
        let open = !self.emoji_suggest.is_empty();
        self.input.update(cx, |i, _| i.suppress_newline = open);
        cx.notify();
    }

    /// A suggestion was picked (click, Enter, Tab): replace the `:token`
    /// with the full `:shortcode:` text (NIP-30-style shortcode in the
    /// content — the webapp's withEmoji left unknown codes literal, so
    /// only real shortcodes go in).
    fn pick_emoji(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(e) = self.emoji_suggest.get(idx).copied() else {
            return;
        };
        let Some(q) = self.emoji_token.clone() else {
            return;
        };
        let i = self.input.read(cx);
        let text = i.content.clone();
        let cursor = i.cursor_offset().min(text.len());
        let needle = format!(":{q}");
        let Some(at) = text[..cursor].rfind(&needle) else {
            return;
        };
        let mut next = String::with_capacity(text.len());
        next.push_str(&text[..at]);
        next.push_str(&format!(":{}: ", e.name));
        next.push_str(&text[cursor..]);
        self.input.update(cx, |i, cx| i.set_text(&next, cx));
        self.emoji_suggest.clear();
        self.emoji_token = None;
        self.emoji_sel = 0;
        self.input.update(cx, |i, _| i.suppress_newline = false);
        cx.notify();
    }

    /// Schedule parse — honest validation: `YYYY-MM-DD HH:MM` local time
    /// (or a bare date, noon), at least a minute ahead. Ambiguous DST
    /// times are refused (chrono's `single()`), not guessed.
    fn parse_schedule(s: &str) -> Option<u64> {
        use chrono::TimeZone as _;
        let t = s.trim();
        let dt = chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M")
            .ok()
            .or_else(|| {
                chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d")
                    .ok()
                    .and_then(|d| d.and_hms_opt(12, 0, 0))
            })?;
        let secs = chrono::Local.from_local_datetime(&dt).single()?.timestamp();
        let secs = u64::try_from(secs).ok()?;
        // At least a minute ahead (scheduled.rs refuses to date-past).
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        (secs > now + 60).then_some(secs)
    }

    /// The scheduled publish: sign-at-compose (core schedule_note), the
    /// sweep fires it. Replies and quotes ride along; polls don't
    /// schedule (v1, like drafting).
    fn schedule(&mut self, cx: &mut Context<Self>) {
        if self.posting {
            return;
        }
        let text = self.input.read(cx).content.trim().to_string();
        if text.is_empty() && self.attachments.is_empty() {
            self.status = "nothing to say yet".into();
            cx.notify();
            return;
        }
        let when_field = self.sched_at.read(cx).content.clone();
        let Some(at) = Self::parse_schedule(&when_field) else {
            self.status =
                "when? enter a future date and time, YYYY-MM-DD HH:MM, at least a minute ahead"
                    .into();
            cx.notify();
            return;
        };
        self.posting = true;
        self.status.clear();
        let reply = self.reply_to.clone();
        let quote = self.quote.clone();
        let mentions = std::mem::take(&mut self.mentions);
        // Pending uploads keep the schedule from sealing, same as an
        // instant post — the imetas must be final before signing.
        if self.attachments.iter().any(|a| a.imeta.is_none()) {
            self.posting = false;
            self.status = "waiting for the upload to finish…".into();
            cx.notify();
            return;
        }
        let imetas: Vec<Vec<String>> = self.ready_imetas();
        let core = self.core.clone();
        cx.notify();
        let mut rx = core.schedule_note(text, reply, mentions, quote, imetas, false, at);
        cx.spawn(async move |this, cx| {
            // Every ScheduleEvent settles the pane (Scheduled pops, Failed
            // stays with a word) — one receipt, no pump.
            if let Some(ev) = rx.recv().await {
                match ev {
                    satori_core::manager::ScheduleEvent::Scheduled { .. } => {
                        let _ = this.update(cx, |_this, cx| cx.emit(Nav::Pop));
                    }
                    satori_core::manager::ScheduleEvent::Failed { error } => {
                        let _ = this.update(cx, |this, cx| {
                            this.posting = false;
                            this.status = format!("couldn't schedule: {error}");
                            cx.notify();
                        });
                    }
                }
            }
        })
        .detach();
    }

    /// The pending scheduled notes (soonest first), the "Scheduled"
    /// surface's rows. Empty when logged out (core gates on the session).
    fn scheduled(&self) -> Vec<satori_core::scheduled::ScheduledPost> {
        self.core.scheduled_list()
    }

    /// Write an attachment's alt into its imeta (NIP-68): one `alt` entry,
    /// replaced on edit, dropped when emptied. The imeta stays the single
    /// source of truth — publish, drafts and the reader-side badge read it.
    fn save_alt(&mut self, id: u64, cx: &mut Context<Self>) {
        let text = self.alt_input.read(cx).content.trim().to_string();
        if let Some(a) = self.attachments.iter_mut().find(|a| a.id == id) {
            if let Some(imeta) = a.imeta.as_mut() {
                imeta.retain(|e| !e.starts_with("alt "));
                if !text.is_empty() {
                    imeta.push(format!("alt {text}"));
                }
            }
            a.alt = (!text.is_empty()).then_some(text);
        }
        self.alt_edit = None;
        self.alt_input.update(cx, |i, cx| i.clear(cx));
        cx.notify();
    }

    /// Switch tabs (note / poll): buffers keep their text; the mention
    /// strip belongs to the note buffer.
    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            self.suggest.clear();
            self.mention_token = None;
            cx.notify();
        }
    }

    /// The poll publish: question + at least two options (NIP-88).
    /// No attachments, no mentions, no draft — v1 keeps the surface small.
    fn post_poll(&mut self, cx: &mut Context<Self>) {
        let question = self.poll_question.read(cx).content.trim().to_string();
        let options: Vec<String> = self
            .poll_options
            .iter()
            .map(|e| e.read(cx).content.trim().to_string())
            .filter(|o| !o.is_empty())
            .collect();
        if question.is_empty() || options.len() < 2 {
            self.status = "a poll needs a question and two options".into();
            cx.notify();
            return;
        }
        self.posting = true;
        self.status.clear();
        cx.notify();
        let core = self.core.clone();
        let mut rx = core.post_poll(question, options, self.poll_multi, None);
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                match ev {
                    // Held → the pane pops NOW (publish.ts: the composer
                    // closes and a toast floats the countdown over the
                    // feed). The pump dies with the pane; core finishes the
                    // window regardless — the shell's subscribe_hold toast
                    // carries Undo and the outcome.
                    PublishEvent::Held { .. } => {
                        let _ = this.update(cx, |_this, cx| cx.emit(Nav::Pop));
                        return;
                    }
                    PublishEvent::Undone { .. } => {
                        let _ = this.update(cx, |this, cx| {
                            this.posting = false;
                            this.status = "undone, nothing was sent".into();
                            cx.notify();
                        });
                        return;
                    }
                    PublishEvent::Committed { .. } => {
                        let _ = this.update(cx, |_this, cx| cx.emit(Nav::Pop));
                        return;
                    }
                    PublishEvent::Failed { error, .. } => {
                        let _ = this.update(cx, |this, cx| {
                            this.posting = false;
                            this.status = format!("failed: {error}");
                            cx.notify();
                        });
                        return;
                    }
                    _ => {}
                }
            }
        })
        .detach();
    }

    /// The article fields off the inputs (the body is the note buffer).
    fn article_draft(&self, cx: &Context<Self>) -> satori_core::nip::article::ArticleDraft {
        let cover = self.art_cover.read(cx).content.trim().to_string();
        satori_core::nip::article::ArticleDraft {
            title: self.art_title.read(cx).content.trim().to_string(),
            summary: self.art_summary.read(cx).content.trim().to_string(),
            image: (!cover.is_empty()).then_some(cover),
            topics: self
                .art_topics
                .read(cx)
                .content
                .split(',')
                .map(|t| t.trim().trim_start_matches('#').to_string())
                .filter(|t| !t.is_empty())
                .collect(),
            body: self.input.read(cx).content.clone(),
            identifier: String::new(), // fresh publishes mint a random d
        }
    }

    fn article_valid(&mut self, cx: &mut Context<Self>) -> bool {
        let draft = self.article_draft(cx);
        if draft.title.is_empty() || draft.body.trim().is_empty() {
            self.status = "an article needs a title and a body".into();
            cx.notify();
            return false;
        }
        true
    }

    /// The article publish (NIP-23): pump mirrors the poll's — Held pops
    /// the pane (the toast carries Undo), the rest report in place. No
    /// synced draft (v1) — nothing to spend on a commit.
    fn post_article(&mut self, cx: &mut Context<Self>) {
        if !self.article_valid(cx) {
            return;
        }
        self.posting = true;
        self.status.clear();
        cx.notify();
        let core = self.core.clone();
        let draft = self.article_draft(cx);
        let mut rx = core.post_article(draft);
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                match ev {
                    PublishEvent::Held { .. } => {
                        let _ = this.update(cx, |_this, cx| cx.emit(Nav::Pop));
                        return;
                    }
                    PublishEvent::Undone { .. } => {
                        let _ = this.update(cx, |this, cx| {
                            this.posting = false;
                            this.status = "undone, nothing was sent".into();
                            cx.notify();
                        });
                        return;
                    }
                    PublishEvent::Committed { .. } => {
                        let _ = this.update(cx, |_this, cx| cx.emit(Nav::Pop));
                        return;
                    }
                    PublishEvent::Failed { error, .. } => {
                        let _ = this.update(cx, |this, cx| {
                            this.posting = false;
                            this.status = format!("couldn't post: {error}");
                            cx.notify();
                        });
                        return;
                    }
                    _ => {}
                }
            }
        })
        .detach();
    }

    /// The scheduled article: sign-at-compose (core schedule_article), the
    /// sweep fires it — the note schedule's discipline, article-shaped.
    fn schedule_article(&mut self, cx: &mut Context<Self>) {
        if self.posting {
            return;
        }
        if !self.article_valid(cx) {
            return;
        }
        let when_field = self.sched_at.read(cx).content.clone();
        let Some(at) = Self::parse_schedule(&when_field) else {
            self.status =
                "when? enter a future date and time, YYYY-MM-DD HH:MM, at least a minute ahead"
                    .into();
            cx.notify();
            return;
        };
        self.posting = true;
        self.status.clear();
        cx.notify();
        let draft = self.article_draft(cx);
        let core = self.core.clone();
        let mut rx = core.schedule_article(draft, at);
        cx.spawn(async move |this, cx| {
            if let Some(ev) = rx.recv().await {
                match ev {
                    satori_core::manager::ScheduleEvent::Scheduled { .. } => {
                        let _ = this.update(cx, |_this, cx| cx.emit(Nav::Pop));
                    }
                    satori_core::manager::ScheduleEvent::Failed { error } => {
                        let _ = this.update(cx, |this, cx| {
                            this.posting = false;
                            this.status = format!("couldn't schedule: {error}");
                            cx.notify();
                        });
                    }
                }
            }
        })
        .detach();
    }

    /// The cover's Blossom upload: pick one image, the blob URL lands in
    /// the cover field when the upload completes (the imeta half is
    /// unused — the article carries the bare `image` tag).
    fn upload_cover(&mut self, cx: &mut Context<Self>) {
        if self.posting || self.art_cover_busy {
            return;
        }
        self.status = "choosing a cover…".into();
        cx.spawn(async move |this, cx| {
            let picked = cx
                .background_executor()
                .spawn(async {
                    rfd::FileDialog::new()
                        .add_filter("Images", &["png", "jpg", "jpeg", "webp"])
                        .pick_file()
                })
                .await;
            let Some(path) = picked else {
                let _ = this.update(cx, |this, cx| {
                    this.status.clear();
                    cx.notify();
                });
                return;
            };
            let content_type = match path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase()
                .as_str()
            {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "webp" => "image/webp",
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.status = "that's not an image".into();
                        cx.notify();
                    });
                    return;
                }
            };
            let Ok(bytes) = std::fs::read(&path) else {
                let _ = this.update(cx, |this, cx| {
                    this.status = format!("couldn't read {}", path.display());
                    cx.notify();
                });
                return;
            };
            let Ok((_, mut rx)) = this.update(cx, |this, cx| {
                this.art_cover_busy = true;
                this.status = "uploading the cover…".into();
                cx.notify();
                let core = this.core.clone();
                let rx = core.blossom_upload(bytes, content_type.into());
                (core, rx)
            }) else {
                return;
            };
            while let Some(ev) = rx.recv().await {
                let _ = this.update(cx, |this, cx| match ev {
                    PublishEvent::Uploaded { url, .. } => {
                        this.art_cover_busy = false;
                        this.status.clear();
                        this.art_cover.update(cx, |i, cx| i.set_text(&url, cx));
                        cx.notify();
                    }
                    PublishEvent::Failed { error, .. } => {
                        this.art_cover_busy = false;
                        this.status = format!("cover upload failed: {error}");
                        cx.notify();
                    }
                    _ => {}
                });
            }
        })
        .detach();
    }

    fn post(&mut self, cx: &mut Context<Self>) {
        if self.posting {
            return;
        }
        // The poll tab publishes a poll, not the note buffer.
        if self.tab == Tab::Poll {
            self.post_poll(cx);
            return;
        }
        // The article tab publishes an article: the buffer IS the
        // markdown body, the fields panel carries the structural tags.
        // The schedule affordance reroutes it like a note's.
        if self.tab == Tab::Article {
            if self.sched_on {
                self.schedule_article(cx);
            } else {
                self.post_article(cx);
            }
            return;
        }
        // The schedule affordance reroutes the seal: same content path,
        // signed now, broadcast later (scheduled.rs).
        if self.sched_on {
            self.schedule(cx);
            return;
        }
        let text = self.input.read(cx).content.trim().to_string();
        if text.is_empty() && self.attachments.is_empty() {
            self.status = "nothing to say yet".into();
            cx.notify();
            return;
        }
        self.posting = true;
        self.status.clear();
        let reply = self.reply_to.clone();
        let quote = self.quote.clone();
        // A top-level publish consumes the buffer: the draft is spent
        // (composer.rs:476). If the send fails, `draft_text` re-stashes on
        // close.
        if reply.is_none() {
            cx.default_global::<Draft>().0 = None;
        }
        let core = self.core.clone();
        let draft_key = (
            self.reply_to.as_ref().map(|e| e.id),
            self.quote.as_ref().map(|e| e.id),
        );
        // v1.4: the attachments uploaded at ATTACH time — the publish
        // embeds the finished imetas. Anything still in flight keeps the
        // note unposted for the moment (the seal re-arms when the chain
        // lands; a failed blob already removed its tile with a word).
        if self.attachments.iter().any(|a| a.imeta.is_none()) {
            self.posting = false;
            self.status = "waiting for the upload to finish…".into();
            cx.notify();
            return;
        }
        let this_mentions = std::mem::take(&mut self.mentions);
        let imetas: Vec<Vec<String>> = self
            .attachments
            .iter()
            .filter_map(|a| a.imeta.clone())
            .collect();
        cx.notify();

        cx.spawn(async move |this, cx| {
            // Publish: a note with media IS a note (issue #3 WS1) — kind 1
            // + imeta via post_note, signNote's semantics; the imetas ride
            // comments (kind 1111) the same way when the root isn't a
            // note. kind 20 (post_picture) belongs to the Photo tab (WS2),
            // never to "a note happened to carry an attachment". Amethyst's
            // migration rule still routes replies: kind-1 roots keep
            // kind-1 replies + NIP-10 tags (every client reads those);
            // every other root (articles, pictures, highlights…) goes out
            // as a NIP-22 kind-1111 comment with scope tags.
            let mentions = this_mentions;
            let mut rx = if reply.as_ref().is_some_and(|r| u16::from(r.kind) != 1) {
                let r = reply.clone().expect("checked");
                core.comment(
                    text,
                    satori_core::nip::comment::CommentTarget::for_event(&r),
                    Some(r),
                    mentions,
                    imetas,
                )
            } else {
                core.post_note(text, reply, mentions, quote, imetas)
            };
            while let Some(ev) = rx.recv().await {
                match ev {
                    // Held → the pane pops NOW (publish.ts: the composer
                    // closes and a toast floats the countdown over the
                    // feed). The pump dies with the pane; core finishes the
                    // window regardless — the shell's subscribe_hold toast
                    // carries Undo and the outcome.
                    PublishEvent::Held { .. } => {
                        // D38: the synced slot is spent — the note will go
                        // out unless undone (then the composer stays open
                        // and a close re-saves the draft).
                        let _ = core.save_draft(draft_key.0, draft_key.1, String::new(), Vec::new(), false);
                        let _ = this.update(cx, |_this, cx| cx.emit(Nav::Pop));
                        return;
                    }
                    PublishEvent::Undone { .. } => {
                        let _ = this.update(cx, |this, cx| {
                            // Kept on the stack with a quiet word — the draft
                            // is still there if the user wants it.
                            this.posting = false;
                            this.status = "undone, nothing was sent".into();
                            cx.notify();
                        });
                        return;
                    }
                    PublishEvent::Committed { .. } => {
                        // D38: the synced draft slot is spent with the send.
                        let _ = core.save_draft(draft_key.0, draft_key.1, String::new(), Vec::new(), false);
                        let _ = this.update(cx, |_this, cx| cx.emit(Nav::Pop));
                        return;
                    }
                    PublishEvent::Failed { error, .. } => {
                        let _ = this.update(cx, |this, cx| {
                            this.posting = false;
                            this.status = format!("failed: {error}");
                            cx.notify();
                        });
                        return;
                    }
                    _ => {}
                }
            }
        })
        .detach();
    }

    /// Stash candidate, GTK close-handler semantics (composer.rs:509): the
    /// whole buffer of a top-level compose that never sent — possibly empty,
    /// so closing empty clears a stale draft. None = don't touch the draft
    /// (replies, a send in flight, an undo window pending). v1.4: the
    /// session stash carries the ready attachment imetas too.
    pub fn draft_candidate(&self, cx: &App) -> Option<SessionDraft> {
        if self.reply_to.is_some() || self.quote.is_some() || self.posting
            || self.tab == Tab::Article
        {
            // The article tab keeps no draft (v1, like the poll): its
            // body is the note buffer, and an article-shaped restore is
            // not wired.
            return None;
        }
        Some(SessionDraft {
            text: self.input.read(cx).content.trim().to_string(),
            attachments: self.ready_imetas(),
        })
    }

    /// The SYNCED draft candidate (D38): (reply parent, quote, buffer,
    /// attachment imetas) for any compose that isn't mid-send — replies and
    /// quotes sync keyed, not just the top-level slot. Polls don't draft
    /// (v1 keeps their surface small — and their empty note buffer must not
    /// wipe the top-level slot). v1.4: the imetas ride the entry (the
    /// blobs are on Blossom since attach time).
    pub fn draft_sync_candidate(&self, cx: &App) -> Option<DraftCandidate> {
        // Polls don't draft (v1 keeps their surface small — and their
        // empty note buffer must not wipe the top-level slot); the article
        // tab joins that exclusion (its buffer IS the note buffer).
        if self.posting || self.tab != Tab::Note {
            return None;
        }
        Some((
            self.reply_to.as_ref().map(|e| e.id),
            self.quote.as_ref().map(|e| e.id),
            self.input.read(cx).content.trim().to_string(),
            self.ready_imetas(),
        ))
    }

    /// The uploaded imetas, verbatim — the publish's gallery, the synced
    /// draft's attachment list (pending tiles contribute nothing).
    fn ready_imetas(&self) -> Vec<Vec<String>> {
        self.attachments
            .iter()
            .filter_map(|a| a.imeta.clone())
            .collect()
    }

    /// D38: fill the buffer from a reopened Library draft — overwrites
    /// whatever the session stash restored (the synced slot was spent at
    /// tap time; the close saves whatever the user does with it).
    /// v1.4: the entry's attachment imetas restore with it.
    pub fn prefill(
        &mut self,
        text: &str,
        attachments: Vec<Vec<String>>,
        cx: &mut Context<Self>,
    ) {
        self.input.update(cx, |i, cx| i.set_text(text, cx));
        for m in attachments.into_iter() {
            if let Some(mut a) = Attachment::from_imeta(m) {
                a.id = self.next_att;
                self.next_att += 1;
                self.attachments.push(a);
            }
        }
        cx.notify();
    }
}

impl Focusable for Composer {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.tab {
            Tab::Poll => self.poll_question.read(cx).focus_handle(cx),
            Tab::Article => self.art_title.read(cx).focus_handle(cx),
            Tab::Note => self.focus.clone(),
        }
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.autofocused {
            self.autofocused = true;
            window.focus(&self.focus, cx);
            self.focused_tab = Some(Tab::Note);
        }
        if self.focused_tab != Some(self.tab) {
            // A tab switch moves the caret: the poll's question, the
            // article's title, or back to the note field. (The note handle
            // dangles on the other tabs — its div isn't rendered — and a
            // dangling focus eats keys.)
            self.focused_tab = Some(self.tab);
            match self.tab {
                Tab::Poll => window
                    .focus(&self.poll_question.read(cx).focus_handle(cx), cx),
                Tab::Article => window
                    .focus(&self.art_title.read(cx).focus_handle(cx), cx),
                Tab::Note => window.focus(&self.focus, cx),
            }
        }
        let replying = self.reply_to.is_some();
        let quote_prev = self.quote.as_ref().map(|q| {
            let prof = self.core.profile_of(&q.pubkey);
            (
                prof.as_ref()
                    .map(|p| p.display())
                    .unwrap_or_else(|| format!("{}…", &q.pubkey.to_string()[..8])),
                q.content
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(240)
                    .collect::<String>(),
                prof.map(|p| p.picture).unwrap_or_default(),
            )
        });
        let status = self.status.clone();
        let input = self.input.clone();
        let posting = self.posting;
        let suggest = self.suggest.clone();
        let tab = self.tab;
        let poll_question = self.poll_question.clone();
        let poll_options = self.poll_options.clone();
        let poll_multi = self.poll_multi;
        let art_title = self.art_title.clone();
        let art_summary = self.art_summary.clone();
        let art_cover = self.art_cover.clone();
        let art_topics = self.art_topics.clone();
        let art_cover_busy = self.art_cover_busy;
        let emoji_suggest = self.emoji_suggest.clone();
        let emoji_sel = self.emoji_sel;
        let sched_on = self.sched_on;
        let sched_list_open = self.sched_list_open;
        let scheduled = self.scheduled();
        let nsec_warn = cx
            .try_global::<crate::input::NsecWarn>()
            .map(|w| w.0)
            .unwrap_or(false);
        let field_lines = est_lines(input.read(cx).content.as_str(), 72.0);
        let question_lines =
            est_lines(&poll_question.read(cx).content, 72.0);
        let quote_snap = media::Snapshot::new(cx);
        // The quote card's avatar comes back if the cache evicted it
        // (fetch is a silent no-op when the cache still holds it).
        if let Some((_, _, picture)) = quote_prev.as_ref()
            && !picture.is_empty()
        {
            media::fetch(&self.core, picture.clone(), cx.entity_id(), cx);
        }

        div()
            .id("composer")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Publish, _, cx| this.post(cx)))
            // The `:`-typeahead's keys: up/down move the selection, Enter
            // (or Tab) inserts, Escape closes. Fires in the bubble phase —
            // the Input has had its chance (its newline is suppressed
            // while the strip is open); stop_propagation keeps the
            // keystroke from going further up.
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                if this.emoji_suggest.is_empty() {
                    return;
                }
                let n = this.emoji_suggest.len();
                match ev.keystroke.key.as_str() {
                    "up" => {
                        this.emoji_sel = this.emoji_sel.saturating_sub(1);
                        cx.notify();
                    }
                    "down" => {
                        this.emoji_sel = (this.emoji_sel + 1).min(n - 1);
                        cx.notify();
                    }
                    "enter" | "tab" => {
                        let sel = this.emoji_sel;
                        this.pick_emoji(sel, cx);
                    }
                    "escape" => {
                        this.emoji_suggest.clear();
                        this.emoji_token = None;
                        this.emoji_sel = 0;
                        this.input.update(cx, |i, _| i.suppress_newline = false);
                        cx.notify();
                    }
                    _ => {}
                }
            }))
            // (No root click hand-off: every Input focuses itself on mouse
            // down now — a pane-wide handler would steal the caret back
            // from the poll's question and options.)
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .font_family(theme::SANS)
            .text_color(rgb(theme::ink()))
            .gap_2()
            .p_4()
            .when_some(quote_prev, |d, (name, snippet, picture)| {
                // The quoted note as a card: the same frame a quote wears
                // out in the feed, so the composer answers "quoting WHOM,
                // about WHAT" before a word is typed.
                d.child(
                    div()
                        .flex_col()
                        .gap_1()
                        .rounded(px(theme::R_EMBED))
                        .border_1()
                        .border_color(rgb(theme::hairline()))
                        .bg(rgb(theme::washi()))
                        .p_2()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(media::avatar(&quote_snap, &picture, &name, 20.0))
                                .child(
                                    div()
                                        .text_size(px(theme::T_UI))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(theme::ink()))
                                        .child(name),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(theme::muted()))
                                .child(snippet),
                        ),
                )
            })
            .when(tab == Tab::Article, |d| {
                // The NIP-23 structural fields; the body rides the note
                // buffer's box below. Single-line fields wear the poll's
                // washi row (definite height — the Input fills its parent).
                let art_field = |input: Entity<Input>, window: &Window| -> gpui::Div {
                    div()
                        .flex()
                        .h(window.line_height() + px(theme::S5))
                        .rounded(px(theme::R_EMBED))
                        .border_1()
                        .border_color(rgb(theme::hairline()))
                        .bg(rgb(theme::washi()))
                        .p_2()
                        .child(input)
                };
                d.child(
                    div()
                        .rounded(px(theme::R_EMBED))
                        .border_1()
                        .border_color(rgb(theme::hairline()))
                        .bg(rgb(theme::panel()))
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(poll_label("Title"))
                        .child(art_field(art_title, window))
                        .child(poll_label("Summary (optional)"))
                        .child(art_field(art_summary, window))
                        .child(poll_label("Cover image (optional)"))
                        .child(
                            div().flex().gap_2().child(
                                art_field(art_cover, window).flex_1(),
                            )
                            .child(
                                div()
                                    .id("cover-upload")
                                    .cursor_pointer()
                                    .when(!art_cover_busy, |d| d.hover(|s| s.bg(rgb(theme::washi()))))
                                    .flex()
                                    .items_center()
                                    .px_3()
                                    .rounded(px(theme::R_EMBED))
                                    .border_1()
                                    .border_color(rgb(theme::hairline()))
                                    .bg(rgb(theme::panel()))
                                    .text_size(px(theme::T_UI))
                                    .text_color(rgb(if art_cover_busy {
                                        theme::muted()
                                    } else {
                                        theme::ink()
                                    }))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.upload_cover(cx);
                                    }))
                                    .child(if art_cover_busy {
                                        "uploading…"
                                    } else {
                                        "upload"
                                    }),
                            ),
                        )
                        .child(poll_label("Topics (optional, comma separated)"))
                        .child(art_field(art_topics, window)),
                )
            })
            .when(tab != Tab::Poll, |d| {
                d.child(
                    div()
                        .id("composer-scroll")
                        // Ten quiet lines of breathing room; it grows
                        // with the text and scrolls when it outgrows the
                        // pane — the window never fills with an empty box.
                        .h(window.line_height() * field_lines.max(LINES_MIN))
                        .max_h_full()
                        .overflow_y_scroll()
                        .border_1()
                        .border_color(rgb(theme::hairline()))
                        .bg(rgb(theme::panel()))
                        .p_2()
                        .flex()
                        .flex_col()
                        // Tabs on top square the box's shoulders; a reply
                        // (no tabs) keeps the full rounding.
                        .when(replying, |d| d.rounded(px(theme::R_EMBED)))
                        .when(!replying, |d| d.rounded_b(px(theme::R_EMBED)))
                        .child(input),
                )
            })
            .when(tab == Tab::Poll, |d| {
                d.child(
                    div()
                        .id("poll-scroll")
                        // Content-sized: the panel is as tall as the poll,
                        // not as tall as the window (scrolls when it must).
                        .max_h_full()
                        .min_h_0()
                        .overflow_y_scroll()
                        .rounded_b(px(theme::R_EMBED))
                        .border_1()
                        .border_color(rgb(theme::hairline()))
                        .bg(rgb(theme::panel()))
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(poll_label("Question"))
                        .child(
                            // The question wears the same washi box as the
                            // options — an unboxed bare input reads as blank
                            // panel until typed. Definite height: the Input
                            // fills its parent; auto would collapse it.
                            div()
                                .flex()
                                .h(window.line_height() * question_lines.max(1.0) + px(theme::S5))
                                .rounded(px(theme::R_EMBED))
                                .border_1()
                                .border_color(rgb(theme::hairline()))
                                .bg(rgb(theme::washi()))
                                .p_2()
                                .child(poll_question),
                        )
                        .child(poll_label("Options"))
                        .children(poll_options.iter().enumerate().map(|(i, opt)| {
                            let opt_lines =
                                est_lines(&opt.read(cx).content, 60.0).max(1.0);
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                // The choice mark says what kind of poll this
                                // is before the toggle needs reading: a ring
                                // picks one, a square picks many.
                                .child(if poll_multi {
                                    div()
                                        .size(px(13.))
                                        .rounded(px(theme::R_EMBED))
                                        .border_1()
                                        .border_color(rgb(theme::hairline()))
                                        .flex_shrink_0()
                                } else {
                                    div()
                                        .size(px(13.))
                                        .rounded_full()
                                        .border_1()
                                        .border_color(rgb(theme::hairline()))
                                        .flex_shrink_0()
                                })
                                .child(
                                    div()
                                        .flex_1()
                                        .flex()
                                        .h(window.line_height() * (opt_lines + 0.8))
                                        .rounded(px(theme::R_EMBED))
                                        .border_1()
                                        .border_color(rgb(theme::hairline()))
                                        .bg(rgb(theme::washi()))
                                        .p_2()
                                        .child(opt.clone()),
                                )
                                .when(i >= 2, |d| {
                                    d.child(
                                        div()
                                            .id(SharedString::from(format!("rm-opt-{i}")))
                                            .cursor_pointer()
                                            .text_size(px(theme::T_BODY))
                                            .text_color(rgb(theme::muted()))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if !this.posting && this.poll_options.len() > 2 {
                                                    this.poll_options.remove(i);
                                                    cx.notify();
                                                }
                                            }))
                                            .child("×"),
                                    )
                                })
                        }))
                        .child(
                            div()
                                .id("add-option")
                                .cursor_pointer()
                                .flex()
                                .items_center()
                                .gap_2()
                                .pl(px(21.))
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(theme::muted()))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if !this.posting && this.poll_options.len() < 6 {
                                        this.poll_options.push(cx.new(Input::new));
                                        cx.notify();
                                    }
                                }))
                                .child("+ add another option"),
                        )
                        .child(
                            div()
                                .id("poll-multi")
                                .cursor_pointer()
                                .flex()
                                .items_center()
                                .gap_2()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(if poll_multi {
                                    theme::vermilion()
                                } else {
                                    theme::muted()
                                }))
                                .child(
                                    div()
                                        .size(px(13.))
                                        .rounded(px(theme::R_EMBED))
                                        .border_1()
                                        .border_color(rgb(if poll_multi {
                                            theme::vermilion()
                                        } else {
                                            theme::hairline()
                                        }))
                                        .flex_shrink_0(),
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.poll_multi = !this.poll_multi;
                                    cx.notify();
                                }))
                                .child(if poll_multi {
                                    "multiple choices ✓"
                                } else {
                                    "multiple choices"
                                }),
                        ),
                )
            })
            // The tabs ride the box's top edge: the active one wears the
            // panel's wash and frame (no bottom border — the 1px overlap
            // sands the seam into the field below).
            .when(!replying, |d| {
                d.child(
                    div().flex().gap_1().pl_2().mb(px(-1.))
                        .child(
                            div()
                                .id("tab-note")
                                .cursor_pointer()
                                .px_3()
                                .py_1()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(if tab == Tab::Note {
                                    theme::ink()
                                } else {
                                    theme::muted()
                                }))
                                .when(tab == Tab::Note, |d| {
                                    d.font_weight(gpui::FontWeight::SEMIBOLD)
                                        .bg(rgb(theme::panel()))
                                        .rounded_t(px(theme::R_EMBED))
                                        .border_t_1()
                                        .border_l_1()
                                        .border_r_1()
                                        .border_color(rgb(theme::hairline()))
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_tab(Tab::Note, cx)
                                }))
                                .child("Note"),
                        )
                        .child(
                            div()
                                .id("tab-poll")
                                .cursor_pointer()
                                .px_3()
                                .py_1()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(if tab == Tab::Poll {
                                    theme::ink()
                                } else {
                                    theme::muted()
                                }))
                                .when(tab == Tab::Poll, |d| {
                                    d.font_weight(gpui::FontWeight::SEMIBOLD)
                                        .bg(rgb(theme::panel()))
                                        .rounded_t(px(theme::R_EMBED))
                                        .border_t_1()
                                        .border_l_1()
                                        .border_r_1()
                                        .border_color(rgb(theme::hairline()))
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_tab(Tab::Poll, cx)
                                }))
                                .child("Poll"),
                        )
                        .child(
                            div()
                                .id("tab-article")
                                .cursor_pointer()
                                .px_3()
                                .py_1()
                                .text_size(px(theme::T_UI))
                                .text_color(rgb(if tab == Tab::Article {
                                    theme::ink()
                                } else {
                                    theme::muted()
                                }))
                                .when(tab == Tab::Article, |d| {
                                    d.font_weight(gpui::FontWeight::SEMIBOLD)
                                        .bg(rgb(theme::panel()))
                                        .rounded_t(px(theme::R_EMBED))
                                        .border_t_1()
                                        .border_l_1()
                                        .border_r_1()
                                        .border_color(rgb(theme::hairline()))
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_tab(Tab::Article, cx)
                                }))
                                .child("Article"),
                        ),
                )
            })
            .when(!suggest.is_empty(), |d| {
                d.child(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .children(suggest.iter().enumerate().map(|(i, p)| {
                            div()
                                .id(SharedString::from(format!("mention-{i}")))
                                .cursor_pointer()
                                .px_2()
                                .py_0p5()
                                .rounded(px(theme::R_EMBED))
                                .border_1()
                                .border_color(rgb(theme::hairline()))
                                .text_size(px(theme::T_SMALL))
                                .text_color(rgb(theme::ink()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_mention(i, cx);
                                }))
                                .child(format!("@{}", p.display()))
                        })),
                )
            })
            .when(!emoji_suggest.is_empty(), |d| {
                // The `:`-typeahead strip (dark-wisp EmojiShortcodePopup):
                // glyph + shortcode, the selection ringed in vermilion —
                // the seal marks the engaged row, arrows move it.
                d.child(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .children(emoji_suggest.iter().enumerate().map(|(i, e)| {
                            let on = i == emoji_sel;
                            div()
                                .id(SharedString::from(format!("emoji-{i}")))
                                .cursor_pointer()
                                .px_2()
                                .py_0p5()
                                .rounded(px(theme::R_EMBED))
                                .border_1()
                                .border_color(rgb(if on {
                                    theme::vermilion()
                                } else {
                                    theme::hairline()
                                }))
                                .text_size(px(theme::T_SMALL))
                                .text_color(rgb(theme::ink()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_emoji(i, cx);
                                }))
                                .child(format!("{} :{}:", e.char, e.name))
                        })),
                )
            })
            .when(nsec_warn, |d| {
                // D17 tripwire strip (dark-wisp NsecPasteGuard): the value
                // is never shown, never logged — only the fact of it.
                d.child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .rounded(px(theme::R_EMBED))
                        .border_1()
                        .border_color(rgb(theme::err()))
                        .px_3()
                        .py_2()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(theme::T_SMALL))
                                .text_color(rgb(theme::err()))
                                .child(
                                    "That looks like a private key (nsec…). Nothing was inserted, a pasted key here could sign as its owner.",
                                ),
                        )
                        .child(
                            div()
                                .id("nsec-dismiss")
                                .cursor_pointer()
                                .text_size(px(theme::T_SMALL))
                                .text_color(rgb(theme::muted()))
                                .hover(|s| s.text_color(rgb(theme::ink())))
                                .on_click(|_, _, cx| {
                                    // set_global wakes every observing view
                                    // (ThemeSignal's discipline) — no direct
                                    // entity notify from a plain closure.
                                    cx.set_global(crate::input::NsecWarn(false));
                                })
                                .child("×"),
                        ),
                )
            })
            .when(tab == Tab::Note, |d| d.child(self.attachment_row(cx)))
            .when(tab != Tab::Poll && sched_on, |d| {
                // The schedule affordance: the when field + the pending
                // list. Honest validation lives in parse_schedule; the
                // status line carries its refusals.
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(theme::T_SMALL))
                                .text_color(rgb(theme::muted()))
                                .child("broadcast at:"),
                        )
                        .child(
                            div()
                                .id("sched-when")
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .h(px(34.))
                                .px_2()
                                .pt_1()
                                .rounded(px(theme::R_EMBED))
                                .bg(rgb(theme::washi()))
                                .border_1()
                                .border_color(rgb(theme::hairline()))
                                .cursor(gpui::CursorStyle::IBeam)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    let focus = this.sched_at.read(cx).focus_handle(cx);
                                    window.focus(&focus, cx);
                                }))
                                .child(self.sched_at.clone()),
                        ),
                )
            })
            .when(tab != Tab::Poll && !scheduled.is_empty(), |d| {
                // The "Scheduled" surface: pending notes, soonest first,
                // each with its time and a cancel (cancelScheduled).
                d.child(
                    div()
                        .id("sched-list-toggle")
                        .cursor_pointer()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sched_list_open = !this.sched_list_open;
                            cx.notify();
                        }))
                        .child(format!(
                            "{} scheduled, {} waiting",
                            if sched_list_open { "▾" } else { "▸" },
                            scheduled.len()
                        )),
                )
            })
            .when(tab != Tab::Poll && sched_list_open, |d| {
                d.child(div().w_full().flex().flex_col().gap_1().children(
                    scheduled.iter().map(|p| {
                        let token = p.token.clone();
                        let when = fmt_sched_time(p.scheduled_at);
                        let snippet: String = p
                            .event
                            .content
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ")
                            .chars()
                            .take(80)
                            .collect();
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(theme::T_SMALL))
                                    .text_color(rgb(theme::muted()))
                                    .child(when),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_size(px(theme::T_SMALL))
                                    .text_color(rgb(theme::ink()))
                                    .whitespace_nowrap()
                                    .child(snippet),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("sched-cancel-{}", p.token)))
                                    .cursor_pointer()
                                    .text_size(px(theme::T_SMALL))
                                    .child(theme::pressable(div().child("cancel")))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if this.core.scheduled_cancel(&token) {
                                            cx.notify();
                                        }
                                    })),
                            )
                    }),
                ))
            })
            .child(div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .when(tab != Tab::Article, |d| {
                                d.child(
                                    div()
                                        .id("attach")
                                        .cursor_pointer()
                                        .px_3()
                                        .py_1()
                                        .text_size(px(theme::T_UI))
                                        .text_color(rgb(theme::muted()))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.pick_attachments(cx);
                                        }))
                                        .child("+ attach images"),
                                )
                            })
                            .when(tab != Tab::Poll, |d| {
                                d.child(
                                    div()
                                        .id("schedule-toggle")
                                        .cursor_pointer()
                                        .px_3()
                                        .py_1()
                                        .rounded(px(theme::R_EMBED))
                                        .border_1()
                                        .border_color(rgb(if sched_on {
                                            theme::vermilion()
                                        } else {
                                            theme::hairline()
                                        }))
                                        .text_size(px(theme::T_UI))
                                        .text_color(rgb(if sched_on {
                                            theme::vermilion()
                                        } else {
                                            theme::muted()
                                        }))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.sched_on = !this.sched_on;
                                            cx.notify();
                                        }))
                                        .child(if sched_on { "schedule ✓" } else { "schedule" }),
                                )
                            }),
                    )
                    .child(
                        div()
                            .id("send")
                            .cursor_pointer()
                            .px_4()
                            .py_1()
                            .rounded(px(theme::R_EMBED))
                            .border_1()
                            .border_color(rgb(theme::vermilion()))
                            .text_size(px(theme::T_BODY))
                            .text_color(rgb(theme::vermilion()))
                            .on_click(cx.listener(|this, _, _, cx| this.post(cx)))
                            .child(if posting {
                                "…"
                            } else if sched_on {
                                "Schedule"
                            } else if replying {
                                "Reply"
                            } else {
                                "Publish"
                            }),
            ))
            .when(!status.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(theme::T_SMALL))
                        .text_color(rgb(theme::muted()))
                        .child(status),
                )
            })
    }
}

impl Composer {
    fn attachment_row(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let tiles = self.attachments.len();
        let alt_edit = self.alt_edit;
        // Restored attachments (v1.4) have no bytes — their preview is the
        // CDN copy through the media cache (fetch + Snapshot, the quote
        // card's pattern; fetch is a silent no-op while it's cached).
        let snap = media::Snapshot::new(cx);
        for a in &self.attachments {
            if a.bytes.is_none()
                && let Some(url) = a.url()
            {
                media::fetch(&self.core, url, cx.entity_id(), cx);
            }
        }
        let mut grid = div()
            .w_full()
            .flex()
            .flex_wrap()
            .gap_2()
            .children(self.attachments.iter().enumerate().map(|(i, a)| {
                let att_id = a.id;
                let has_alt = a.alt.is_some();
                let open = alt_edit == Some(att_id);
                let remove = cx.listener(move |this, _, _, cx| {
                    if !this.posting {
                        this.attachments.retain(|x| x.id != att_id);
                        cx.notify();
                    }
                });
                let toggle_alt = cx.listener(move |this, _, _, cx| {
                    this.alt_edit = if open { None } else { Some(att_id) };
                    if let Some(prev) = this
                        .attachments
                        .iter()
                        .find(|a| a.id == att_id)
                        .and_then(|a| a.alt.clone())
                    {
                        this.alt_input.update(cx, |inp, cx| inp.set_text(&prev, cx));
                    }
                    cx.notify();
                });
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .size(px(72.))
                            .rounded(px(theme::R_EMBED))
                            .border_1()
                            .border_color(rgb(theme::hairline()))
                            .overflow_hidden()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(rgb(theme::panel()))
                            .child(match &a.image {
                                Some(image) => img(std::sync::Arc::new(image.clone()))
                                    .size_full()
                                    .object_fit(gpui::ObjectFit::Cover)
                                    .into_any_element(),
                                // The restored tile: the CDN copy when it
                                // has landed, a quiet placeholder before.
                                None => match a.url().and_then(|u| snap.png(&u)) {
                                    Some(png) => img(png)
                                        .size_full()
                                        .object_fit(gpui::ObjectFit::Cover)
                                        .into_any_element(),
                                    None => div()
                                        .text_size(px(theme::T_SMALL))
                                        .text_color(rgb(theme::muted()))
                                        .child("…")
                                        .into_any_element(),
                                },
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .id(SharedString::from(format!("alt-{att_id}")))
                                    .cursor_pointer()
                                    .rounded(px(theme::R_EMBED))
                                    .border_1()
                                    .border_color(rgb(if has_alt {
                                        theme::ok()
                                    } else {
                                        theme::hairline()
                                    }))
                                    .px_1()
                                    .text_size(px(theme::T_SMALL))
                                    .text_color(rgb(if has_alt {
                                        theme::ok()
                                    } else {
                                        theme::muted()
                                    }))
                                    .on_click(toggle_alt)
                                    .child(if has_alt { "alt ✓" } else { "alt" }),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("rm-att-{i}")))
                                    .cursor_pointer()
                                    .text_size(px(theme::T_SMALL))
                                    .text_color(rgb(theme::muted()))
                                    .on_click(remove)
                                    .when(i < tiles, |d| {
                                        d.child(format!("× {}", truncate_name(&a.name)))
                                    }),
                            ),
                    )
            }));
        // The one shared alt editor opens under the tile that asked for
        // it (research §4.2's editor dialog, inline — no modal needed).
        if let Some(id) = alt_edit {
            grid = grid.child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("alt-field")
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .h(px(34.))
                            .px_2()
                            .pt_1()
                            .rounded(px(theme::R_EMBED))
                            .bg(rgb(theme::washi()))
                            .border_1()
                            .border_color(rgb(theme::hairline()))
                            .cursor(gpui::CursorStyle::IBeam)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let focus = this.alt_input.read(cx).focus_handle(cx);
                                window.focus(&focus, cx);
                            }))
                            .child(self.alt_input.clone()),
                    )
                    .child(
                        div()
                            .id("alt-save")
                            .cursor_pointer()
                            .text_size(px(theme::T_SMALL))
                            .child(theme::pressable(div().child("save")))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.save_alt(id, cx);
                            })),
                    ),
            );
        }
        grid
    }
}

/// The scheduled time, local: "Nov 8 09:30" (chrono Local; a schedule the
/// list can't render is a schedule the user can't trust).
fn fmt_sched_time(at: u64) -> String {
    use chrono::TimeZone as _;
    chrono::Local
        .timestamp_opt(at as i64, 0)
        .single()
        .map(|dt| dt.format("%b %-d %H:%M").to_string())
        .unwrap_or_else(|| format!("{at}"))
}

/// Trim a file name to a tile label without splitting a glyph.
fn truncate_name(name: &str) -> String {
    if name.chars().count() <= 18 {
        return name.to_string();
    }
    let cut: String = name.chars().take(15).collect();
    format!("{cut}…")
}
