//! The write path: notes, articles, media, reactions, lists, relays.

use super::*;

impl Core {
    // --- write path ---------------------------------------------------------------

    /// Post a note — a NIP-10 reply, a NIP-18 quote, or plain. Signed, then
    /// held for the undo window with a backstop commit; events stream on
    /// the channel.
    pub fn post_note(
        self: &Arc<Self>,
        content: String,
        reply_to: Option<Event>,
        mentions: Vec<PublicKey>,
        quote: Option<Event>,
        imetas: Vec<Vec<String>>,
    ) -> PublishRx {
        self.stream_task(move |this, tx| {
            Box::pin(async move {
                this.post_note_inner(tx, content, reply_to, mentions, quote, imetas)
                    .await
            })
        })
    }

    /// Tag + content building for a note — shared by the immediate path
    /// (post_note) and the scheduled path (schedule_note), so a scheduled
    /// note's tags are byte-identical to an instant one. `imetas` (the
    /// uploaded NIP-92 tag arrays from upload.rs, element 0 = "imeta")
    /// ride the kind-1 as plain imeta tags — publish.ts signNote's
    /// `for (const m of imeta) tags.push(m)`; an attached image or video
    /// is media IN a note, not a picture event (issue #3 WS1).
    /// (pub(crate)-crossing: the composer's preview builds its synthetic
    /// event from the SAME builder — the preview shows what publish signs.)
    /// A note with attachments is a NOTE (issue #3 WS1): note_tags carries
    /// the NIP-92 imeta arrays as kind-1 tags (signNote's
    /// `for (const m of imeta) tags.push(m)`), alongside the reply tags —
    /// the reply-with-attachment case keeps its thread context.
    pub fn note_tags(
        content: String,
        reply_to: Option<Event>,
        mentions: Vec<PublicKey>,
        quote: Option<Event>,
        imetas: &[Vec<String>],
        me: PublicKey,
    ) -> (String, Vec<Tag>) {
        let mut tags: Vec<Tag> = Vec::new();
        // NIP-92 media metadata rides right after content-warning's slot
        // in the TS (signNote pushes it before the quote/reference tags).
        for m in imetas {
            if m.len() >= 2 && m[0] == "imeta" {
                tags.push(Tag::custom("imeta", m[1..].iter()));
            }
        }
        // The NIP-18 quote rides FIRST: a q tag (id, relay hint, author) and
        // the nostr: reference folded into the text the reader sees.
        let content = if let Some(q) = &quote {
            // The relay hint rides the author's most-recent seen relay
            // (actions.ts:118); empty when we've never seen them (NIP-18:
            // the hint is optional).
            tags.push(Tag::custom(
                "q",
                [
                    q.id.to_string(),
                    seen::first(&q.pubkey).unwrap_or_default(),
                    q.pubkey.to_string(),
                ],
            ));
            format!(
                "{}\n\nnostr:{}",
                content,
                q.id.to_bech32().unwrap_or_default()
            )
        } else {
            content
        };
        if let Some(parent) = &reply_to {
            // NIP-10 marked tags; root = parent's root if marked, else parent itself.
            let mut root_id: Option<EventId> = None;
            for tag in parent.tags.clone().to_vec() {
                let parts = tag.as_slice();
                if parts.len() >= 2 && parts[0] == "e" {
                    if parts.len() >= 4 && parts[3] == "root" {
                        root_id = Some(EventId::from_str(&parts[1]).ok()).flatten();
                    }
                    break;
                }
            }
            let root = root_id.unwrap_or(parent.id);
            let empty = "";
            tags.push(Tag::custom(
                "e",
                [root.to_string(), empty.to_string(), "root".to_string()],
            ));
            tags.push(Tag::custom(
                "e",
                [
                    parent.id.to_string(),
                    empty.to_string(),
                    "reply".to_string(),
                ],
            ));
            tags.push(Tag::public_key(parent.pubkey));
            if root != parent.id {
                // Root author belongs in p too when different (NIP-10).
                tags.push(Tag::custom("p", [root.to_string()]));
            }
        }
        // @-mentions (NIP-10 p tags — what fires notifications). The
        // parent's own p's (root/parent authors) take precedence.
        let mut seen_p: Vec<String> = tags
            .iter()
            .filter(|t| t.as_slice().first().map(String::as_str) == Some("p"))
            .filter_map(|t| t.as_slice().get(1).cloned())
            .collect();
        for who in mentions {
            let hex = who.to_string();
            if hex != me.to_string() && !seen_p.contains(&hex) {
                seen_p.push(hex.clone());
                tags.push(Tag::public_key(who));
            }
        }
        (Self::content_with_media_urls(content, imetas), tags)
    }

    /// NIP-92's shape is a URL in the CONTENT plus a matching imeta tag,
    /// and clients MAY ignore an imeta that matches no content URL — the
    /// strict parsers (dark-wisp-class) rendered an attached image as
    /// nothing at all (the Nov 13 2026 interop report: the announcement
    /// note's screenshot visible in Satori, invisible elsewhere). Each
    /// imeta's url joins the text on its own trailing line unless the
    /// author already pasted it; the reader's media pass dedupes it
    /// against the imeta asset (content.rs media_assets) and the body
    /// folds the link paragraph into the gallery, so nothing doubles.
    pub(super) fn content_with_media_urls(mut content: String, imetas: &[Vec<String>]) -> String {
        for m in imetas {
            if m.len() >= 2 && m[0] == "imeta" {
                for entry in &m[1..] {
                    if let Some(url) = entry.strip_prefix("url ")
                        && !url.is_empty()
                        && !content.contains(url)
                    {
                        content.push_str("\n\n");
                        content.push_str(url);
                    }
                }
            }
        }
        content
    }

    pub(super) async fn post_note_inner(
        self: &Arc<Self>,
        tx: &mpsc::UnboundedSender<PublishEvent>,
        content: String,
        reply_to: Option<Event>,
        mentions: Vec<PublicKey>,
        quote: Option<Event>,
        imetas: Vec<Vec<String>>,
    ) -> Result<()> {
        let active = self.active().await?;
        let me = active.me();
        let (content, tags) = Self::note_tags(content, reply_to, mentions, quote, &imetas, me);
        let unsigned = UnsignedEvent::new(me, Timestamp::now(), Kind::TextNote, tags, content);
        let signed = active.sign_report(unsigned).await?;

        match active.undo.hold_duration() {
            Some(hold) => {
                let token = self.hold_seq.fetch_add(1, Ordering::SeqCst);
                let _ = tx.send(PublishEvent::Held {
                    token,
                    event: signed.clone(),
                    hold_secs: hold.as_secs(),
                });
                self.hold_and_commit(Arc::new(active.clone()), token, signed, hold, tx.clone())
                    .await;
            }
            None => Self::commit(&active, &signed, tx).await?,
        }
        Ok(())
    }

    /// Schedule a note for later broadcast (scheduled.ts holdScheduled):
    /// signed AT COMPOSE with `created_at` == the scheduled time — it
    /// reads as ≈now when the sweep fires it, and relays don't reject
    /// future-dating. Held in scheduled.json (0600, synchronous flush),
    /// published by the 30s sweep through the SAME funnel as an instant
    /// post (store → outbox write-ahead → publish → dequeue). No undo
    /// window on the schedule itself — undoing IS cancelling
    /// (scheduled_cancel). Reports on ScheduleEvent.
    #[allow(clippy::too_many_arguments)]
    pub fn schedule_note(
        self: &Arc<Self>,
        title: String,
        content: String,
        reply_to: Option<Event>,
        mentions: Vec<PublicKey>,
        quote: Option<Event>,
        imetas: Vec<Vec<String>>,
        post: PostKind,
        scheduled_at: u64,
    ) -> mpsc::UnboundedReceiver<ScheduleEvent> {
        self.schedule_task(scheduled_at, move |active| {
            Box::pin(async move {
                let me = active.me();
                // The composer's tab decides: a photo post schedules as the
                // SAME kind-20 picture event an instant post makes
                // (picture_tags' build — the composer's title rides, blank
                // derives from the caption like the instant path); a video
                // post likewise rides NIP-71 kind 22 (video_tags, issue
                // #18); a note with attachments schedules as kind 1 + imeta
                // (note_tags' build), matching the instant path's routing
                // (issue #3 WS1).
                let (kind, content, tags) = match post {
                    PostKind::Photo if !imetas.is_empty() => {
                        let tags = Self::picture_tags(&title, &content, &imetas, None);
                        (Kind::Custom(20), content, tags)
                    }
                    PostKind::Video if !imetas.is_empty() => {
                        let tags = Self::video_tags(&title, &content, &imetas, None);
                        (Kind::Custom(22), content, tags)
                    }
                    _ => {
                        let (content, tags) =
                            Self::note_tags(content, reply_to, mentions, quote, &imetas, me);
                        (Kind::TextNote, content, tags)
                    }
                };
                let unsigned = UnsignedEvent::new(
                    me,
                    Timestamp::from_secs(scheduled_at),
                    kind,
                    tags,
                    content,
                );
                let signed = active.sign_report(unsigned).await?;
                crate::scheduled::hold(&me, signed, scheduled_at).ok_or_else(|| {
                    LoginError::Connect(format!(
                        "you have reached the maximum of {} scheduled posts; cancel some to schedule more",
                        crate::scheduled::MAX_PER_USER
                    ))
                })
            })
        })
    }

    /// The active account's pending scheduled posts, soonest first (the
    /// composer's schedule list). Empty when logged out.
    pub fn scheduled_list(&self) -> Vec<crate::scheduled::ScheduledPost> {
        self.active_pubkey_opt()
            .map(|me| crate::scheduled::list(&me))
            .unwrap_or_default()
    }

    /// Schedule an article (schedule_note's discipline, NIP-23-shaped):
    /// signed AT COMPOSE with `created_at` == the scheduled time, held in
    /// scheduled.json, published by the 30s sweep through the same funnel.
    pub fn schedule_article(
        self: &Arc<Self>,
        draft: crate::nip::article::ArticleDraft,
        scheduled_at: u64,
    ) -> mpsc::UnboundedReceiver<ScheduleEvent> {
        self.schedule_task(scheduled_at, move |active| {
            Box::pin(async move {
                if draft.title.trim().is_empty() || draft.body.trim().is_empty() {
                    return Err(LoginError::Other("an article needs a title and a body".into()));
                }
                let me = active.me();
                let unsigned = UnsignedEvent::new(
                    me,
                    Timestamp::from_secs(scheduled_at),
                    Kind::Custom(30023),
                    draft.tags(scheduled_at),
                    draft.body.clone(),
                );
                let signed = active.sign_report(unsigned).await?;
                crate::scheduled::hold(&me, signed, scheduled_at).ok_or_else(|| {
                    LoginError::Connect(format!(
                        "you have reached the maximum of {} scheduled posts; cancel some to schedule more",
                        crate::scheduled::MAX_PER_USER
                    ))
                })
            })
        })
    }

    /// Cancel a scheduled post before its time (scheduled.ts
    /// cancelScheduled): only the author's own token goes, synchronously
    /// flushed so a confirmed cancel can't resurrect after a restart.
    pub fn scheduled_cancel(&self, token: &str) -> bool {
        let Some(me) = self.active_pubkey_opt() else {
            return false;
        };
        crate::scheduled::cancel(&me, token)
    }

    /// The scheduled sweep (scheduled.ts startScheduledSweep, 30s): ONE
    /// task per process, armed on the first bootstrap next to the outbox
    /// drainer. Its first tick catches anything already due at boot — the
    /// restart catch-up: the store is the only state, there are no
    /// per-post timers. Due entries publish through the same funnel as
    /// instant posting (Self::commit's order: store save → outbox
    /// write-ahead → publish → dequeue on acceptance) and are removed only
    /// on acceptance — PUBLISH-BEFORE-MARK, see the scheduled.rs module
    /// doc. A session-less stretch just sleeps; entries persist on disk.
    pub(super) fn spawn_scheduled_sweep(self: &Arc<Self>) {
        if self.scheduled_started.swap(true, Ordering::SeqCst) {
            return;
        }
        eprintln!("[core] scheduled: sweep armed");
        let weak = Arc::downgrade(self);
        self.rt.spawn(async move {
            loop {
                let Some(core) = weak.upgrade() else { break }; // app closing
                let now = crate::scheduled::now_secs();
                let Some(due) = crate::scheduled::take_due(now) else {
                    drop(core);
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    continue;
                };
                let mut settled: Vec<String> = Vec::new();
                if let Ok(active) = core.try_active().await {
                    for p in due {
                        // Same funnel as Self::commit (the publish-before-
                        // mark discipline: acceptance is the durability
                        // decision; removal rides the debounced flusher).
                        let _ = active.pool.store.save(&p.event).await;
                        crate::outbox_queue::enqueue(&p.event);
                        let targets =
                            crate::routing::targets(&active.pool, &active.write_relays, &p.event)
                                .await;
                        let report = active.pool.publish(&p.event, &targets).await;
                        for r in &report.ok {
                            crate::relay_latency::record_write_result(r, true);
                        }
                        for (r, _m) in &report.failed {
                            crate::relay_latency::record_write_result(r, false);
                        }
                        if report.accepted() {
                            crate::outbox_queue::remove(&p.event);
                            settled.push(p.token);
                        } else if crate::scheduled::give_up(now, p.scheduled_at) {
                            eprintln!("[core] scheduled: giving up on {}", p.token);
                            settled.push(p.token);
                        }
                        // else: kept; the next tick retries (relay may be down)
                    }
                }
                // Logged out: nothing settles, nothing drops — the sweep
                // just spins quietly until a session returns.
                crate::scheduled::finish_sweep(&settled);
                drop(core);
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });
    }

    /// A random 9-char option id (nip88.ts generateOptionId): xorshift off
    /// the clock and the option index — no rand dep for nine chars.
    pub(super) fn poll_option_id(n: usize) -> String {
        const ID_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let mut seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0)
            ^ (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut id = String::new();
        for _ in 0..9 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            id.push(ID_CHARS[(seed % ID_CHARS.len() as u64) as usize] as char);
        }
        id
    }

    /// Publish a NIP-88 poll (kind:1068): question in content, `option`
    /// tags with random 9-char ids, polltype, optional endsAt (seconds
    /// from now) + relay hints. nip88.ts buildPollTags shape. Same undo
    /// hold as notes.
    pub fn post_poll(
        self: &Arc<Self>,
        question: String,
        options: Vec<String>,
        multi: bool,
        ends_in_secs: Option<u64>,
    ) -> PublishRx {
        let core = Arc::clone(self);
        self.publish_task(move |active, tx| {
            Box::pin(async move {
                const ID_CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
                let mut tag_list: Vec<Tag> = Vec::new();
                for (n, label) in options.iter().enumerate() {
                    tag_list.push(Tag::custom(
                        "option",
                        [Self::poll_option_id(n), label.as_str().to_string()],
                    ));
                }
                tag_list.push(Tag::custom(
                    "polltype",
                    [if multi {
                        "multiplechoice"
                    } else {
                        "singlechoice"
                    }],
                ));
                if let Some(secs) = ends_in_secs {
                    let ends_at = Timestamp::now().as_secs() + secs;
                    tag_list.push(Tag::custom("endsAt", [ends_at.to_string()]));
                }
                for url in &active.read_relays {
                    tag_list.push(Tag::custom("relay", [url.as_str()]));
                }
                let unsigned = UnsignedEvent::new(
                    active.me(),
                    Timestamp::now(),
                    Kind::Custom(1068),
                    tag_list,
                    question,
                );
                let signed = active.sign_report(unsigned).await?;
                match active.undo.hold_duration() {
                    Some(hold) => {
                        let token = core.hold_seq.fetch_add(1, Ordering::SeqCst);
                        let _ = tx.send(PublishEvent::Held {
                            token,
                            event: signed.clone(),
                            hold_secs: hold.as_secs(),
                        });
                        core.hold_and_commit(
                            Arc::new(active.clone()),
                            token,
                            signed,
                            hold,
                            tx.clone(),
                        )
                        .await;
                    }
                    None => Self::commit(active, &signed, tx).await?,
                }
                Ok(())
            })
        })
    }

    /// Post a NIP-68 picture (kind:20). `imeta` carries one or more full NIP-92
    /// imeta tags (from `upload::blossom_upload_all`) — a picture needs at least
    /// one. Same undo window as notes. Port of `publish.ts signPicture`.
    pub fn post_picture(
        self: &Arc<Self>,
        title: String,
        content: String,
        imeta: Vec<Vec<String>>,
        content_warning: Option<String>,
    ) -> PublishRx {
        self.stream_task(move |this, tx| {
            Box::pin(async move {
                this.post_picture_inner(tx, title, content, imeta, content_warning)
                    .await
            })
        })
    }

    /// Post a NIP-71 video (kind 22, the general video; kind 21 is the
    /// short-vertical sibling, not built here) — the Video tab's publish
    /// (issue #18), the D49 routing discipline's deliberate-video twin of
    /// the Photo tab's kind 20: a video POST is its own kind; a video
    /// attached to a note stays kind 1 + imeta (issue #3 WS1). imeta shape
    /// is the uploader's (url/m/dim/x + the D38 poster `image`).
    pub fn post_video(
        self: &Arc<Self>,
        title: String,
        content: String,
        imeta: Vec<Vec<String>>,
        content_warning: Option<String>,
    ) -> PublishRx {
        self.stream_task(move |this, tx| {
            Box::pin(async move {
                this.post_video_inner(tx, title, content, imeta, content_warning)
                    .await
            })
        })
    }

    /// The NIP-71 kind-22 tag build, shared by the instant path and the
    /// scheduled path (byte-identical, the picture_tags discipline) and by
    /// the composer's preview. (pub: the composer's preview uses it.)
    pub fn video_tags(
        title: &str,
        content: &str,
        imetas: &[Vec<String>],
        content_warning: Option<&str>,
    ) -> Vec<Tag> {
        // NIP-71: a `title` tag when there is one; a blank composer title
        // derives from the content's first non-empty line (picture_tags'
        // rule). Everything else — url/m/dim/x/image — rides the imeta.
        let mut tags: Vec<Tag> = vec![Tag::custom(
            "title",
            [if title.trim().is_empty() {
                first_caption_line(content)
            } else {
                title.trim().to_string()
            }],
        )];
        if let Some(cw) = content_warning {
            tags.push(Tag::custom("content-warning", [cw]));
        }
        for m in imetas {
            if m.len() >= 2 && m[0] == "imeta" {
                tags.push(Tag::custom("imeta", m[1..].iter()));
            }
        }
        tags
    }

    pub(super) async fn post_video_inner(
        self: &Arc<Self>,
        tx: &mpsc::UnboundedSender<PublishEvent>,
        title: String,
        content: String,
        imeta: Vec<Vec<String>>,
        content_warning: Option<String>,
    ) -> Result<()> {
        if imeta.is_empty() {
            return Err(LoginError::Other(
                "a video post needs at least one video".into(),
            ));
        }
        let active = self.active().await?;
        let me = active.me();
        let tags = Self::video_tags(&title, &content, &imeta, content_warning.as_deref());
        let unsigned = UnsignedEvent::new(me, Timestamp::now(), Kind::Custom(22), tags, content);
        let signed = active.sign_report(unsigned).await?;

        match active.undo.hold_duration() {
            Some(hold) => {
                let token = self.hold_seq.fetch_add(1, Ordering::SeqCst);
                let _ = tx.send(PublishEvent::Held {
                    token,
                    event: signed.clone(),
                    hold_secs: hold.as_secs(),
                });
                self.hold_and_commit(Arc::new(active.clone()), token, signed, hold, tx.clone())
                    .await;
            }
            None => Self::commit(&active, &signed, tx).await?,
        }
        Ok(())
    }

    /// The NIP-68 kind-20 tag build, shared by the instant path
    /// (post_picture_inner) and the scheduled path (schedule_note) so a
    /// scheduled picture's tags are byte-identical to an instant one.
    /// (pub: the composer's preview uses the same builder.)
    pub fn picture_tags(
        title: &str,
        content: &str,
        imetas: &[Vec<String>],
        content_warning: Option<&str>,
    ) -> Vec<Tag> {
        // NIP-68: a picture event carries a `title` tag (structural). A blank
        // composer title derives one from the caption's first non-empty line.
        let mut tags: Vec<Tag> = vec![Tag::custom(
            "title",
            [if title.trim().is_empty() {
                first_caption_line(content)
            } else {
                title.trim().to_string()
            }],
        )];
        if let Some(cw) = content_warning {
            tags.push(Tag::custom("content-warning", [cw]));
        }
        for m in imetas {
            if m.len() >= 2 && m[0] == "imeta" {
                tags.push(Tag::custom("imeta", m[1..].iter()));
            }
        }
        // NIP-68 also surfaces each image's hash (`x`) + mime (`m`) as TOP-LEVEL
        // tags (relay/client filtering), deduped across the imetas.
        let mut xs: Vec<String> = Vec::new();
        let mut mimes: Vec<String> = Vec::new();
        for m in imetas {
            for v in m.iter().skip(1) {
                if let Some(x) = v.strip_prefix("x ")
                    && !xs.contains(&x.to_string())
                {
                    xs.push(x.to_string());
                }
                if let Some(mm) = v.strip_prefix("m ")
                    && !mimes.contains(&mm.to_string())
                {
                    mimes.push(mm.to_string());
                }
            }
        }
        for x in &xs {
            tags.push(Tag::custom("x", [x.as_str()]));
        }
        for m in &mimes {
            tags.push(Tag::custom("m", [m.as_str()]));
        }
        tags
    }

    pub(super) async fn post_picture_inner(
        self: &Arc<Self>,
        tx: &mpsc::UnboundedSender<PublishEvent>,
        title: String,
        content: String,
        imeta: Vec<Vec<String>>,
        content_warning: Option<String>,
    ) -> Result<()> {
        if imeta.is_empty() {
            return Err(LoginError::Other(
                "a picture needs at least one image".into(),
            ));
        }
        let active = self.active().await?;
        let me = active.me();
        let tags = Self::picture_tags(&title, &content, &imeta, content_warning.as_deref());
        // TODO(port): src/nostr/content.ts mention tokenization → p tags (the
        // note path carries the same deferral).
        let unsigned = UnsignedEvent::new(me, Timestamp::now(), Kind::Custom(20), tags, content);
        let signed = active.sign_report(unsigned).await?;

        match active.undo.hold_duration() {
            Some(hold) => {
                let token = self.hold_seq.fetch_add(1, Ordering::SeqCst);
                let _ = tx.send(PublishEvent::Held {
                    token,
                    event: signed.clone(),
                    hold_secs: hold.as_secs(),
                });
                self.hold_and_commit(Arc::new(active.clone()), token, signed, hold, tx.clone())
                    .await;
            }
            None => Self::commit(&active, &signed, tx).await?,
        }
        Ok(())
    }

    /// Post a NIP-23 article (kind:30023) — port of `signArticle` +
    /// the article route's publish (article.ts). Top-level only (no
    /// reply/quote; the reader's comments are NIP-22 kind-1111s). The
    /// `d` identifier is random when the draft doesn't carry one. Same
    /// undo window as notes/pictures.
    pub fn post_article(self: &Arc<Self>, draft: crate::nip::article::ArticleDraft) -> PublishRx {
        let core = Arc::clone(self);
        self.publish_task(move |active, tx| {
            Box::pin(async move {
                if draft.title.trim().is_empty() {
                    return Err(LoginError::Other("an article needs a title".into()));
                }
                if draft.body.trim().is_empty() {
                    return Err(LoginError::Other("an article needs a body".into()));
                }
                let me = active.me();
                let now = Timestamp::now().as_secs();
                let unsigned = UnsignedEvent::new(
                    me,
                    Timestamp::from_secs(now),
                    Kind::Custom(30023),
                    draft.tags(now),
                    draft.body.clone(),
                );
                let signed = active.sign_report(unsigned).await?;
                // Same undo window as notes/pictures (hold_and_commit).
                match active.undo.hold_duration() {
                    Some(hold) => {
                        let token = core.hold_seq.fetch_add(1, Ordering::SeqCst);
                        let _ = tx.send(PublishEvent::Held {
                            token,
                            event: signed.clone(),
                            hold_secs: hold.as_secs(),
                        });
                        core.hold_and_commit(
                            Arc::new(active.clone()),
                            token,
                            signed,
                            hold,
                            tx.clone(),
                        )
                        .await;
                    }
                    None => Self::commit(active, &signed, tx).await?,
                }
                Ok(())
            })
        })
    }

    /// Upload a blob to your Blossom servers (upload.ts). Emits
    /// `PublishEvent::Uploaded` with the blob URL + NIP-92 imeta tag; feed that
    /// imeta to `post_picture`.
    pub fn blossom_upload(self: &Arc<Self>, bytes: Vec<u8>, content_type: String) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let res: std::result::Result<(), String> = async {
                if bytes.len() as u64 > crate::upload::MAX_BYTES {
                    return Err(format!(
                        "that file is too large (max {} MB)",
                        crate::upload::MAX_BYTES / (1024 * 1024)
                    ));
                }
                let active = this.active().await.map_err(|e| e.to_string())?;
                let me = active.me();
                // Your kind:10063 media-server list, newest wins (upload.ts
                // fetchBlossomServers); out-of-box default when unset.
                let filter = Filter::new()
                    .author(me)
                    .kind(Kind::Custom(crate::upload::KIND_BLOSSOM_LIST))
                    .limit(1);
                let server = active
                    .pool
                    .get(filter)
                    .await
                    .ok()
                    .and_then(|events| {
                        events
                            .into_iter()
                            .max_by_key(|ev| ev.created_at.as_secs())
                            .and_then(|ev| {
                                ev.tags
                                    .iter()
                                    .find(|t| {
                                        t.as_slice().first().map(String::as_str) == Some("server")
                                    })
                                    .and_then(|t| t.as_slice().get(1).cloned())
                            })
                    })
                    .unwrap_or_else(|| crate::upload::DEFAULT_BLOSSOM_SERVER.to_string());
                let servers: Vec<String> = vec![server];
                let hash = crate::upload::sha256_hex(&bytes);
                let unsigned = crate::upload::blossom_auth_template(me, &hash);
                let signed = active
                    .sign_report(unsigned)
                    .await
                    .map_err(|e| e.to_string())?;
                let up = crate::upload::blossom_upload_all(
                    &servers,
                    bytes,
                    &content_type,
                    &hash,
                    &signed,
                )
                .await?;
                let _ = tx.send(PublishEvent::Uploaded {
                    url: up.url,
                    imeta: up.imeta,
                });
                Ok(())
            }
            .await;
            if let Err(e) = res {
                let _ = tx.send(PublishEvent::Failed { token: 0, error: e });
            }
        });
        rx
    }

    /// Fetch your kind:10063 media-server list for the settings pane. Emits
    /// `FeedEvent::MediaServers` (empty list = the out-of-box default applies).
    pub fn fetch_media_servers(self: &Arc<Self>) -> FeedRx {
        self.feed_task(move |active| {
            Box::pin(async move {
                let me = active.me();
                let filter = Filter::new()
                    .author(me)
                    .kind(Kind::Custom(crate::upload::KIND_BLOSSOM_LIST))
                    .limit(1);
                let list = active
                    .pool
                    .get(filter)
                    .await
                    .ok()
                    .and_then(|events| events.into_iter().max_by_key(|ev| ev.created_at.as_secs()))
                    .map(|ev| server_tags(&ev))
                    .unwrap_or_default();
                Ok(FeedEvent::MediaServers(list))
            })
        })
    }

    /// Publish the media-server list (kind:10063, BUD-03) to your write
    /// relays — routes/settings.ts postMedia + upload.ts serverListTemplate.
    pub fn set_media_servers(self: &Arc<Self>, servers: Vec<String>) -> PublishRx {
        self.publish_task(move |active, tx| {
            Box::pin(async move {
                let me = active.me();
                let tags: Vec<Tag> = servers
                    .iter()
                    .map(|s| Tag::custom("server", [s.trim_end_matches('/').to_string()]))
                    .collect();
                let unsigned = UnsignedEvent::new(
                    me,
                    Timestamp::now(),
                    Kind::Custom(crate::upload::KIND_BLOSSOM_LIST),
                    tags,
                    "",
                );
                let signed = active.sign_report(unsigned).await?;
                Self::commit(active, &signed, tx).await?;
                Ok(())
            })
        })
    }

    /// The undo window: sleep-or-cancel, then the backstop commit (ports
    /// undo.ts semantics; disk-persistence of holds lands in M1).
    pub(super) async fn hold_and_commit(
        self: &Arc<Self>,
        active: Arc<Active>,
        token: u64,
        event: Event,
        hold: std::time::Duration,
        tx: mpsc::UnboundedSender<PublishEvent>,
    ) {
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        self.holds.lock().insert(token, cancel_tx);
        self.mirror_hold(&PublishEvent::Held {
            token,
            event: event.clone(),
            hold_secs: hold.as_secs(),
        });
        tokio::spawn({
            let core = Arc::clone(self);
            async move {
                tokio::select! {
                    _ = tokio::time::sleep(hold + crate::undo::BACKSTOP_GRACE) => {
                        if let Err(e) = Self::commit(&active, &event, &tx).await {
                            core.mirror_hold(&PublishEvent::Failed { token, error: e.to_string() });
                            let _ = tx.send(PublishEvent::Failed { token, error: e.to_string() });
                        } else {
                            core.mirror_hold(&PublishEvent::Committed { token });
                            let _ = tx.send(PublishEvent::Committed { token });
                        }
                    }
                    _ = cancel_rx => {
                        core.mirror_hold(&PublishEvent::Undone { token });
                        let _ = tx.send(PublishEvent::Undone { token });
                    }
                }
                core.holds.lock().remove(&token);
            }
        });
    }

    /// Cancel a held publish.
    pub fn undo(&self, token: u64) {
        if let Some(cancel) = self.holds.lock().remove(&token) {
            let _ = cancel.send(());
        }
    }

    /// THE publish funnel (D36 full outbox): every outgoing event —
    /// notes, reactions, deletions, relay lists, restores — comes through
    /// here. The order IS the contract:
    /// 1. save to the store FIRST (local-first: the event exists even
    ///    fully offline),
    /// 2. enqueue in the outbox (write-ahead, synchronously persisted)
    ///    so a crash mid-publish still retries later,
    /// 3. publish to the current write set (slash-normalized, write-dead
    ///    filtered), learning per-relay outcomes for the fan-out,
    /// 4. ≥1 relay ACK → dequeue + Ok (the UI gets Committed); zero →
    ///    the item STAYS queued and Err flows to the UI as Failed. The
    ///    composer shows Committed only on first acceptance; a later
    ///    outbox success is logged, not re-toasted (D36).
    pub(super) async fn commit(
        active: &Active,
        event: &Event,
        _tx: &mpsc::UnboundedSender<PublishEvent>,
    ) -> Result<()> {
        let _ = active.pool.store.save(event).await;
        crate::outbox_queue::enqueue(event);
        let targets = crate::routing::targets(&active.pool, &active.write_relays, event).await;
        Self::publish_targets(active, event, targets).await
    }

    /// The EVERYWHERE-known publish (NIP-65 phase 2): own profile and
    /// relay list ride every relay the session knows (write set + read
    /// set), not just the write set — a profile that exists only where we
    /// publish CONTENT is invisible on relays we only read. Personal
    /// kinds never ride it: `routing::broadcastable` is the allowlist
    /// (the opt-out sweep), refusing loudly rather than leaking.
    pub(super) async fn commit_broadcast(active: &Active, event: &Event) -> Result<()> {
        if !crate::routing::broadcastable(u16::from(event.kind)) {
            return Err(LoginError::Connect(format!(
                "kind {} is not broadcastable (personal kinds skip the everywhere publish)",
                u16::from(event.kind)
            )));
        }
        let _ = active.pool.store.save(event).await;
        crate::outbox_queue::enqueue(event);
        let targets =
            crate::routing::broadcast_targets(&active.write_relays, &active.pool.read_relays());
        Self::publish_targets(active, event, targets).await
    }

    /// Commit's tail (both funnels): the publish, the write-latency
    /// mirror, dequeue on ≥1 ACK (zero → the item STAYS queued and Err
    /// flows to the UI as Failed).
    pub(super) async fn publish_targets(
        active: &Active,
        event: &Event,
        targets: Vec<String>,
    ) -> Result<()> {
        let report = active.pool.publish(event, &targets).await;
        for r in &report.ok {
            crate::relay_latency::record_write_result(r, true);
        }
        for (r, _m) in &report.failed {
            crate::relay_latency::record_write_result(r, false);
        }
        if report.accepted() {
            crate::outbox_queue::remove(event);
            Ok(())
        } else {
            let n = report.failed.len();
            Err(LoginError::Connect(format!(
                "no relay accepted the event ({n} tried); queued for retry"
            )))
        }
    }

    /// The outbox drainer (D36): ONE task owns every retry, spawned once
    /// per process on the first bootstrap with a session. Serialized
    /// publishes mean a login drain can never double-fire against an
    /// in-flight backoff attempt (per-item loops could). Items survive
    /// restarts (the queue file loads on first touch), so drain-on-login
    /// is simply the drainer finding due items; logged-out stretches
    /// just push each item's next try out 60s at a time.
    pub(super) fn spawn_outbox_drain(self: &Arc<Self>) {
        if self.outbox_started.swap(true, Ordering::SeqCst) {
            return;
        }
        eprintln!("[core] outbox: drainer armed");
        let weak = Arc::downgrade(self);
        self.rt.spawn(async move {
            loop {
                let Some(core) = weak.upgrade() else { break }; // app closing
                let Some(item) = crate::outbox_queue::next_due(crate::outbox_queue::now_secs())
                else {
                    drop(core);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                };
                let Ok(active) = core.active().await else {
                    crate::outbox_queue::reschedule(
                        &item.event,
                        crate::outbox_queue::now_secs() + 60,
                    );
                    continue;
                };
                let targets =
                    crate::routing::targets(&active.pool, &active.write_relays, &item.event).await;
                let report = active.pool.publish(&item.event, &targets).await;
                for r in &report.ok {
                    crate::relay_latency::record_write_result(r, true);
                }
                for (r, _m) in &report.failed {
                    crate::relay_latency::record_write_result(r, false);
                }
                if report.accepted() {
                    crate::outbox_queue::remove(&item.event);
                    let short = &item.event.id.to_string()[..8];
                    eprintln!(
                        "[core] outbox: {short} delivered after {} queued attempt(s)",
                        item.attempts
                    );
                } else {
                    crate::outbox_queue::mark_attempt(&item.event);
                }
            }
        });
    }

    /// Like (NIP-25). No undo window on reactions.
    /// React to `target` (NIP-25 kind:7). `content` is the reaction body:
    /// `"+"` (the heart) or one of the row picker's positive unicode
    /// glyphs. Negative reactions stay unsent, unshown, unnotified —
    /// the house posture, deliberately kept.
    pub fn react(self: &Arc<Self>, target: &Event, content: &str) -> PublishRx {
        let core = Arc::clone(self);
        let target = target.clone();
        let content = content.to_string();
        self.publish_task(move |active, tx| {
            Box::pin(async move {
                let unsigned = UnsignedEvent::new(
                    active.me(),
                    Timestamp::now(),
                    Kind::Reaction,
                    [
                        seen::hint_tag(target.id, &target.pubkey),
                        Tag::public_key(target.pubkey),
                    ],
                    content,
                );
                let signed = active.sign_report(unsigned).await?;
                Self::commit(active, &signed, tx).await?;
                core.own_liked.lock().insert(target.id);
                Ok(())
            })
        })
    }

    /// Bookmark (NIP-51). Private by default: kind 10003 with the id in a
    /// nip44-encrypted tag list — the web app's private-default semantics.
    /// Public scope (D38): kind 30001 d="bookmarks", the id as a plain e-tag
    /// — which list the star writes is the settings pref's job.
    pub fn bookmark(self: &Arc<Self>, target: &Event, public: bool) -> PublishRx {
        let core = Arc::clone(self);
        let target = target.clone();
        self.publish_task(move |active, tx| {
            Box::pin(async move {
                let me = active.me();
                if !public {
                    // Private: the content array is the list (nip44); the
                    // stored event's other tags ride along (a plain e-tag
                    // the user's other client wrote must survive the RMW).
                    // The id set is the newest known event's DECRYPTED
                    // list unioned with the mirror (D89): a bookmark made
                    // elsewhere lands in the store through subscriptions
                    // even when the mirror has not been refreshed.
                    let stored =
                        Self::latest_list_event(active, me, Kind::Custom(10003), None).await;
                    let mirror: Vec<String> = core
                        .own_bookmarked
                        .lock()
                        .iter()
                        .map(|id| id.to_hex())
                        .collect();
                    // A decrypt failure aborts (the star rolls back):
                    // publishing the mirror alone would drop every entry
                    // it lacks, other clients' included.
                    let entries = match &stored {
                        Some(ev) if !ev.content.is_empty() => parse_list_entries(
                            &active
                                .signer
                                .nip44_decrypt_self(&me, ev.content.as_str())
                                .await?,
                        ),
                        _ => Vec::new(),
                    };
                    let entries = list_rmw(entries, "e", mirror, &target.id.to_hex(), true);
                    let plain = list_json(&entries);
                    let content = active.signer.nip44_encrypt_self(&me, &plain).await?;
                    let unsigned = UnsignedEvent::new(
                        me,
                        Timestamp::now(),
                        Kind::Custom(10003),
                        stored
                            .map(|ev| ev.tags.clone().to_vec())
                            .unwrap_or_default(),
                        content,
                    );
                    let signed = active.sign_report(unsigned).await?;
                    Self::commit(active, &signed, tx).await?;
                } else {
                    // Public: NIP-51 kind 30001 d="bookmarks" — the e-tags
                    // ARE the list (my event only — others' 30001s are
                    // different lists). The d and any other markers ride
                    // along; the push dedupes. (D89: the cold-start wire
                    // fetch guards a fresh install's first bookmark.)
                    let stored =
                        Self::latest_list_event(active, me, Kind::Custom(30001), Some("bookmarks"))
                            .await;
                    let mut raw: Vec<Tag> = stored
                        .map(|ev| ev.tags.clone().to_vec())
                        .unwrap_or_default();
                    let hex = target.id.to_string();
                    let present = raw
                        .iter()
                        .any(|t| t.kind() == "e" && t.content() == Some(hex.as_str()));
                    if !present {
                        raw.push(Tag::event(target.id));
                    }
                    let unsigned =
                        UnsignedEvent::new(me, Timestamp::now(), Kind::Custom(30001), raw, "");
                    let signed = active.sign_report(unsigned).await?;
                    Self::commit(active, &signed, tx).await?;
                }
                core.own_bookmarked.lock().insert(target.id);
                Ok(())
            })
        })
    }

    /// Un-bookmark (D38): RMW BOTH lists — the private 10003 (content array
    /// plus any plain e-tags it carries) and the public 30001
    /// d="bookmarks" — publishing only a list that actually held the id.
    pub fn unbookmark(self: &Arc<Self>, id: EventId) -> PublishRx {
        let core = Arc::clone(self);
        self.publish_task(move |active, tx| {
            Box::pin(async move {
                let me = active.me();
                let hex = id.to_string();
                // Private list: decrypt, drop the id, re-encrypt — only if
                // it was there (content array OR a plain e-tag). (D89: the
                // cold-start wire fetch guards a fresh install's first
                // un-bookmark, same as bookmark's.)
                let stored = Self::latest_list_event(active, me, Kind::Custom(10003), None).await;
                if let Some(ev) = stored {
                    // A decrypt failure aborts: it used to read as an
                    // empty list and publish that, wiping the private
                    // bookmarks whenever the id also sat in a plain tag.
                    let entries = if ev.content.is_empty() {
                        Vec::new()
                    } else {
                        parse_list_entries(
                            &active
                                .signer
                                .nip44_decrypt_self(&me, ev.content.as_str())
                                .await?,
                        )
                    };
                    let in_content = list_values(&entries, "e").any(|s| s == hex);
                    let in_tags = ev
                        .tags
                        .clone()
                        .to_vec()
                        .iter()
                        .any(|t| t.kind() == "e" && t.content() == Some(hex.as_str()));
                    if in_content || in_tags {
                        let entries = list_rmw(entries, "e", [], &hex, false);
                        let plain = list_json(&entries);
                        let content = active.signer.nip44_encrypt_self(&me, &plain).await?;
                        // The stored event's non-removed tags ride along
                        // (plain e-tags from another client, client marks).
                        let mut raw: Vec<Tag> = ev.tags.clone().to_vec();
                        raw.retain(|t| !(t.kind() == "e" && t.content() == Some(hex.as_str())));
                        let unsigned = UnsignedEvent::new(
                            me,
                            Timestamp::now(),
                            Kind::Custom(10003),
                            raw,
                            content,
                        );
                        let signed = active.sign_report(unsigned).await?;
                        Self::commit(active, &signed, tx).await?;
                    }
                }
                // Public list: rebuild the stored tags without the id's
                // e-tag (the d rides along) — only if it was there. (D89:
                // same cold-start guard.)
                let stored =
                    Self::latest_list_event(active, me, Kind::Custom(30001), Some("bookmarks"))
                        .await;
                if let Some(ev) = stored {
                    let mut raw: Vec<Tag> = ev.tags.clone().to_vec();
                    let before = raw.len();
                    raw.retain(|t| !(t.kind() == "e" && t.content() == Some(hex.as_str())));
                    if raw.len() < before {
                        let unsigned =
                            UnsignedEvent::new(me, Timestamp::now(), Kind::Custom(30001), raw, "");
                        let signed = active.sign_report(unsigned).await?;
                        Self::commit(active, &signed, tx).await?;
                    }
                }
                core.own_bookmarked.lock().remove(&id);
                Ok(())
            })
        })
    }

    /// The own bookmarks hydrated (D38): the merged id set (private 10003 +
    /// public 30001) resolved from the store — expired (NIP-40) and deleted
    /// (NIP-09) dropped at the same gates the whisper uses — with missing
    /// ids fetched from the indexers by a targeted ids query. One shot: the
    /// list is finite, so the page carries has_more = false (the feed never
    /// orders an Older round for this mode).
    pub fn bookmarked_notes(self: &Arc<Self>) -> FeedRx {
        let this = Arc::clone(self);
        self.feed_task(move |active| {
            Box::pin(async move {
                let want: Vec<EventId> = this.own_bookmarked.lock().iter().copied().collect();
                let mut events: Vec<Event> = Vec::new();
                for id in &want {
                    if let Some(ev) = active.pool.store.event_by_id(*id).await.ok().flatten()
                        && !nip40::is_expired(&ev)
                        && !deletions::is_deleted(&ev)
                    {
                        events.push(ev);
                    }
                }
                let have: HashSet<EventId> = events.iter().map(|e| e.id).collect();
                let missing: Vec<EventId> = want
                    .iter()
                    .filter(|id| !have.contains(id))
                    .cloned()
                    .collect();
                if !missing.is_empty() {
                    // feeds.ts fetchEventsByIds: a TARGETED query on the
                    // indexers (a bookmarked note's author may be outside
                    // the follows — seen-on can't narrow this one). Split
                    // at the REQ ceiling (WS1: OutboxRouter.kt
                    // MAX_ETAGS_PER_FILTER).
                    let fallback: Vec<String> = INDEXERS.iter().map(|s| s.to_string()).collect();
                    for id_shard in outbox::chunk(&missing, outbox::MAX_ETAGS_PER_FILTER) {
                        let n = id_shard.len();
                        let filter = Filter::new().ids(id_shard).limit(n);
                        let fetched: Vec<Event> = active
                            .pool
                            .get_from(&fallback, &filter)
                            .await
                            .into_iter()
                            .collect();
                        // Explicit saves (v1.2): the SDK's saves are async and
                        // once raced a reader — a fetched bookmark should BE
                        // in the store before this task ends.
                        for ev in &fetched {
                            let _ = active.pool.store.save(ev).await;
                        }
                        events.extend(fetched.into_iter().filter(|ev| !deletions::is_deleted(ev)));
                    }
                }
                events.sort_by_key(|e| std::cmp::Reverse(e.created_at));
                let profiles = Self::profiles_for(active, &events).await;
                Ok(FeedEvent::Loaded(FeedPage {
                    notes: events,
                    profiles,
                }))
            })
        })
    }

    /// Save a composer draft (D38): replace-or-insert by slot — the
    /// (reply, quote) pair; an empty text DELETES the slot (closing a
    /// compose empty clears a stale draft, the session stash's semantics).
    /// `attachments` (v1.4) ride the entry: the uploaded imeta arrays,
    /// verbatim — the blobs are already on Blossom (upload-at-attach).
    /// `photo_post` (issue #3 WS1) rides too: the composer tab the draft
    /// was written in decides the publish kind on restore.
    /// The whole array re-encrypts into the one NIP-78 event and rides
    /// commit() (30078 is coordinate-keyed, the outbox retries it).
    /// Optimistic (v1.1): the in-memory array spends immediately so the
    /// composer/Library never wait on the relay round; a failed publish
    /// restores what was spent.
    pub fn save_draft(
        self: &Arc<Self>,
        reply_to: Option<EventId>,
        quote: Option<EventId>,
        text: String,
        attachments: Vec<Vec<String>>,
        photo_post: bool,
    ) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        // Optimistic (v1.1): the in-memory array updates NOW — the
        // composer open and the Library rows react instantly — and the
        // publish rides behind; a failed publish RESTORES the spent
        // entries (D38's "failed save leaves the slot intact", just
        // without the wait).
        let key = (
            reply_to.map(|e| e.to_string()),
            quote.map(|e| e.to_string()),
        );
        let (plain, spent) = {
            let mut drafts = this.own_drafts.lock().clone();
            // Satori's own slot entries replace by (reply, quote);
            // imported (id-marked) entries only ever leave through
            // drop_draft (their wrap is a separate event).
            let mut spent: Vec<DraftEntry> = drafts
                .iter()
                .filter(|d| d.id.is_none() && d.matches(key.0.as_deref(), key.1.as_deref()))
                .cloned()
                .collect();
            drafts.retain(|d| d.id.is_some() || !d.matches(key.0.as_deref(), key.1.as_deref()));
            if !text.trim().is_empty() {
                let entry = DraftEntry {
                    text,
                    reply_to: key.0,
                    quote: key.1,
                    id: None,
                    attachments,
                    photo_post,
                    at: Timestamp::now().as_secs(),
                };
                spent.push(entry.clone());
                drafts.push(entry);
            }
            let plain = serde_json::to_string(&drafts).unwrap_or_default();
            *this.own_drafts.lock() = drafts;
            (plain, spent)
        };
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let me = active.me();
                let content = active.signer.nip44_encrypt_self(&me, &plain).await?;
                let unsigned = UnsignedEvent::new(
                    me,
                    Timestamp::now(),
                    Kind::Custom(30078),
                    [Tag::custom("d", ["satori/drafts"])],
                    content,
                );
                let signed = active.sign_report(unsigned).await?;
                Self::commit(&active, &signed, &tx).await?;
                Ok(())
            }
            .await;
            if let Err(e) = res {
                // Restore: the spent entries come back unless a newer
                // save already re-filled the slot.
                let mut drafts = this.own_drafts.lock().clone();
                for d in spent {
                    if !drafts.contains(&d) {
                        drafts.push(d);
                    }
                }
                *this.own_drafts.lock() = drafts;
                let _ = tx.send(PublishEvent::Failed {
                    token: 0,
                    error: e.to_string(),
                });
            } else {
                let _ = tx.send(PublishEvent::Committed { token: 0 });
            }
        });
        rx
    }

    /// Drop a draft entry (D38 v1.1): remove it from the array and
    /// republish the stash; an imported entry's kind 31234 wrap is ALSO
    /// republished with empty content — NIP-37's delete (the empty shell
    /// sits on the relays until its NIP-40 expiry purges it).
    pub fn drop_draft(self: &Arc<Self>, id: String) -> PublishRx {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        // Optimistic (v1.1): the entry leaves the in-memory array NOW —
        // the row vanishes the moment the tap lands — and the publishes
        // (the 30078 stash, then the wrap's NIP-37 empty-content delete)
        // ride behind; a failed publish restores the entry.
        let (plain, spent) = {
            let mut drafts = this.own_drafts.lock().clone();
            let spent: Vec<DraftEntry> = drafts
                .iter()
                .filter(|d| d.id.as_deref() == Some(id.as_str()))
                .cloned()
                .collect();
            drafts.retain(|d| d.id.as_deref() != Some(id.as_str()));
            let plain = serde_json::to_string(&drafts).unwrap_or_default();
            *this.own_drafts.lock() = drafts;
            (plain, spent)
        };
        self.rt.spawn(async move {
            let res: Result<()> = async {
                let active = this.active().await?;
                let me = active.me();
                let content = active.signer.nip44_encrypt_self(&me, &plain).await?;
                let unsigned = UnsignedEvent::new(
                    me,
                    Timestamp::now(),
                    Kind::Custom(30078),
                    [Tag::custom("d", ["satori/drafts"])],
                    content,
                );
                let signed = active.sign_report(unsigned).await?;
                Self::commit(&active, &signed, &tx).await?;
                // The wrap itself: NIP-37's delete — republish with empty
                // content (relays keep the empty shell until NIP-40 expiry).
                if let Ok(Some(wrap)) = active
                    .pool
                    .store
                    .latest_by_kind_d(me, Kind::Custom(31234), &id)
                    .await
                {
                    let unsigned = UnsignedEvent::new(
                        me,
                        Timestamp::now(),
                        Kind::Custom(31234),
                        wrap.tags.clone().to_vec(),
                        "",
                    );
                    let signed = active.sign_report(unsigned).await?;
                    Self::commit(&active, &signed, &tx).await?;
                }
                Ok(())
            }
            .await;
            if let Err(e) = res {
                let mut drafts = this.own_drafts.lock().clone();
                for d in spent {
                    if !drafts.contains(&d) {
                        drafts.push(d);
                    }
                }
                *this.own_drafts.lock() = drafts;
                let _ = tx.send(PublishEvent::Failed {
                    token: 0,
                    error: e.to_string(),
                });
            } else {
                let _ = tx.send(PublishEvent::Committed { token: 0 });
            }
        });
        rx
    }

    /// The synced drafts, snapshot (the Library's Drafts tab reads it at
    /// render — pane switches notify on every path that changes it).
    pub fn own_draft_list(&self) -> Vec<DraftEntry> {
        self.own_drafts.lock().clone()
    }

    /// One note by id (D38: the Drafts tab restores a draft's reply/quote
    /// context): the store first, then a targeted ids query on the
    /// indexers — the ParentNote resolution's pattern, with the same
    /// NIP-40/NIP-09 gates. None when nowhere.
    pub fn note_by_id(
        self: &Arc<Self>,
        id: EventId,
    ) -> tokio::sync::mpsc::UnboundedReceiver<Option<Event>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let ev = async {
                let active = this.active().await?;
                let stored = active.pool.store.event_by_id(id).await.ok().flatten();
                let ev = match stored {
                    Some(ev) if !nip40::is_expired(&ev) && !deletions::is_deleted(&ev) => Some(ev),
                    _ => {
                        let filter = Filter::new().ids(vec![id]).limit(1);
                        let fallback: Vec<String> =
                            INDEXERS.iter().map(|s| s.to_string()).collect();
                        active
                            .pool
                            .get_from(&fallback, &filter)
                            .await
                            .into_iter()
                            .find(|ev| !deletions::is_deleted(ev))
                    }
                };
                Ok::<Option<Event>, LoginError>(ev)
            }
            .await;
            let _ = tx.send(ev.unwrap_or(None));
        });
        rx
    }

    /// Current NIP-65 relay split (settings pane projection).
    pub fn current_relays(&self) -> (Vec<String>, Vec<String>) {
        self.active
            .lock()
            .as_ref()
            .map(|a| (a.read_relays.clone(), a.write_relays.clone()))
            .unwrap_or_default()
    }

    /// The learned per-relay health (D33/D36: latency EWMA, read-dead,
    /// write-dead) for the settings relay-health pane. A free function on
    /// the type — the data is a process-wide sidecar, not per-account.
    pub fn relay_health() -> Vec<crate::relay_latency::RelayHealth> {
        crate::relay_latency::snapshot()
    }

    /// Remembered NIP-11 capabilities (nip11.rs): what each relay
    /// ADVERTISES it can do, for honest degradation whispers. A relay we
    /// never fetched is absent or `search: None` — unknown, not refused.
    pub fn relay_capabilities() -> Vec<crate::nip11::RelayCaps> {
        crate::nip11::snapshot()
    }

    /// NIP-11 capability sweep for the read set (nip11.rs refresh_stale):
    /// TTL-gated (7d), background, fire-and-forget — capabilities are
    /// whispers, not routing. Spawned wherever the read set joins the pool.
    pub(super) fn spawn_relay_info(&self, reads: &[String]) {
        let mut urls: Vec<String> = crate::pool::INDEXERS
            .iter()
            .map(|s| s.to_string())
            .collect();
        urls.extend(reads.iter().cloned());
        urls.sort();
        urls.dedup();
        self.rt.spawn(async move {
            crate::nip11::refresh_stale(&urls).await;
        });
    }

    /// Pending outbox items (D36): notes signed and saved but not yet ACKed
    /// by any relay. The shell's "queued for delivery" honesty state.
    pub fn outbox_pending() -> usize {
        crate::outbox_queue::len()
    }

    /// The newest known list event, for read-modify-write (D89). The
    /// store's copy is continuously fed by the pool's saves (the role
    /// Amethyst's reactive LocalCache plays) and persists across
    /// sessions (the role their AccountSettings backup plays), so it is
    /// the base every list publish derives from. A store miss fires ONE
    /// wire fetch — the cold-start guard: a toggle in the first seconds
    /// of a fresh install must not replace the relay's list with a
    /// single-entry one built from an unloaded mirror.
    pub(super) async fn latest_list_event(
        active: &Active,
        me: PublicKey,
        kind: Kind,
        d: Option<&str>,
    ) -> Option<Event> {
        let local = match d {
            Some(d) => active
                .pool
                .store
                .latest_by_kind_d(me, kind, d)
                .await
                .ok()
                .flatten(),
            None => active
                .pool
                .store
                .latest_by_kind(me, kind)
                .await
                .ok()
                .flatten(),
        };
        if local.is_some() {
            return local;
        }
        let mut filter = Filter::new().kind(kind).author(me).limit(1);
        if let Some(d) = d {
            filter = filter.custom_tag(SingleLetterTag::LOWERCASE_D, d);
        }
        let _ = active.pool.get(filter).await;
        match d {
            Some(d) => active
                .pool
                .store
                .latest_by_kind_d(me, kind, d)
                .await
                .ok()
                .flatten(),
            None => active
                .pool
                .store
                .latest_by_kind(me, kind)
                .await
                .ok()
                .flatten(),
        }
    }

    /// Publish the NIP-65 relay list (kind:10002 RMW) and swap it in live:
    /// new reads join the pool, removed writes stop receiving publishes.
    ///
    /// Deliberately NOT the D89 store-RMW shape: this is a whole-set
    /// editor (the settings form's read/write boxes ARE the user's full
    /// intent — unioning tags the form doesn't show would resurrect
    /// relays the user just removed). Membership lists (follows, mutes,
    /// bookmarks) are toggles, and they derive from the newest known
    /// event below.
    pub fn set_relays(self: &Arc<Self>, read: Vec<String>, write: Vec<String>) -> PublishRx {
        let core = Arc::clone(self);
        self.publish_task(move |active, _tx| {
            Box::pin(async move {
                let me = active.me();
                let mut tags: Vec<Tag> = Vec::new();
                for url in &read {
                    let marker = if write.contains(url) {
                        None
                    } else {
                        Some("read")
                    };
                    tags.push(relay_tag(url, marker));
                }
                for url in &write {
                    if read.contains(url) {
                        continue; // both-capability tags carry no marker
                    }
                    tags.push(relay_tag(url, Some("write")));
                }
                let unsigned = UnsignedEvent::new(me, Timestamp::now(), Kind::RelayList, tags, "");
                let signed = active.sign_report(unsigned).await?;
                Self::commit_broadcast(active, &signed).await?;
                {
                    let mut a = core.active.lock();
                    if let Some(a) = a.as_mut() {
                        a.read_relays = read.clone();
                        a.write_relays = write.clone();
                    }
                }
                // New reads join the pool; reads the user dropped are
                // retired live (they used to drain only on restart).
                active.pool.set_reads(&read).await;
                core.spawn_relay_info(&read);
                Ok(())
            })
        })
    }

    /// Follow / unfollow (NIP-02 kind:3 read-modify-write).
    /// Follow / unfollow (NIP-02 kind:3, D89's store-RMW): the new
    /// event derives from the newest known list event — its content
    /// (the NIP-02 legacy relay map) and non-p tags (relay hints,
    /// petnames) ride along untouched, and only the ONE p-tag the
    /// toggle speaks for is added or removed. The mirror unions in on
    /// every publish (a key anything knows about is never dropped), an
    /// empty known list plus unfollow is a no-op (Amethyst's refusal
    /// rule: never publish an empty replacement), and a follow with
    /// nothing known anywhere starts a single-entry list (their
    /// createFromScratch).
    pub fn set_following(self: &Arc<Self>, who: PublicKey, follow: bool) -> PublishRx {
        let core = Arc::clone(self);
        self.publish_task(move |active, tx| {
            Box::pin(async move {
                let me = active.me();
                let stored = Self::latest_list_event(active, me, Kind::ContactList, None).await;
                let tags: Vec<Tag> = stored
                    .as_ref()
                    .map(|ev| ev.tags.clone().to_vec())
                    .unwrap_or_default();
                let content = stored
                    .as_ref()
                    .map(|ev| ev.content.clone())
                    .unwrap_or_default();
                let Some(tags) = contact_list_tags(tags, &active.follows, who, follow) else {
                    return Ok(()); // the refusal rule: nothing to publish
                };
                let unsigned =
                    UnsignedEvent::new(me, Timestamp::now(), Kind::ContactList, tags, content);
                let signed = active.sign_report(unsigned).await?;
                Self::commit(active, &signed, tx).await?;
                {
                    let mut a = core.active.lock();
                    if let Some(a) = a.as_mut() {
                        if follow {
                            if !a.follows.contains(&who) {
                                a.follows.push(who);
                            }
                        } else {
                            a.follows.retain(|f| f != &who);
                        }
                    }
                }
                Ok(())
            })
        })
    }

    /// Mute / unmute (NIP-51 kind:10000, content encrypted to self;
    /// D89's store-RMW): the entry set is the newest known event's
    /// union (decrypted content + its public p-tags) with the session
    /// mirror, re-encrypted whole into the content. Public p-tags other
    /// clients wrote stay public (they ride along untouched — a new
    /// mute is never published as a public tag: announcing whom you
    /// silenced is the one thing this list must not do). An empty known
    /// set plus unmute is a no-op (the refusal rule).
    pub fn set_muted(self: &Arc<Self>, who: PublicKey, mute: bool) -> PublishRx {
        let core = Arc::clone(self);
        self.publish_task(move |active, tx| {
            Box::pin(async move {
                let me = active.me();
                let hex = who.to_string();
                let stored = Self::latest_list_event(active, me, Kind::Custom(10000), None).await;
                let mut mirror: Vec<String> = core.muted.lock().iter().map(PublicKey::to_hex).collect();
                let mut tags: Vec<Tag> = stored
                    .as_ref()
                    .map(|ev| ev.tags.clone().to_vec())
                    .unwrap_or_default();
                let mut entries: Vec<Vec<String>> = Vec::new();
                if let Some(ev) = &stored {
                    for pk in ev.tags.clone().public_keys() {
                        mirror.push(pk.to_hex());
                    }
                    if !ev.content.is_empty() {
                        match active
                            .signer
                            .nip44_decrypt_self(&me, ev.content.as_str())
                            .await
                        {
                            Ok(plain) => entries = parse_list_entries(&plain),
                            Err(e) => eprintln!(
                                "[core] mute publish: stored content decrypt failed ({e}); publishing the union of what is known"
                            ),
                        }
                    }
                }
                if !mute
                    && mirror.is_empty()
                    && list_values(&entries, "p").next().is_none()
                {
                    // Nothing known anywhere to unmute from.
                    return Ok(());
                }
                let entries = list_rmw(entries, "p", mirror, &hex, mute);
                if !mute {
                    // A public mute of the toggled key goes quiet with it.
                    tags.retain(|t| !(t.kind() == "p" && t.content() == Some(hex.as_str())));
                }
                let plain = list_json(&entries);
                let content = active.signer.nip44_encrypt_self(&me, &plain).await?;
                let unsigned =
                    UnsignedEvent::new(me, Timestamp::now(), Kind::Custom(10000), tags, content);
                let signed = active.sign_report(unsigned).await?;
                Self::commit(active, &signed, tx).await?;
                // The mirror becomes the published key set (D89 audit): it
                // never shrank before, so an unmuted key survived it and
                // the next unrelated toggle's union re-published it.
                *core.muted.lock() = list_values(&entries, "p")
                    .filter_map(|h| PublicKey::from_hex(h).ok())
                    .collect();
                // Keep the WoT qualification's mute screen in step.
                core.push_wot_mutes();
                Ok(())
            })
        })
    }

    /// The own kind:0 content string, RAW (D38: the edit form prefills
    /// from it — the parsed Profile drops display_name/website, and a form
    /// save writes every displayed field, so a missing prefill would
    /// silently clobber them). Empty string when none (fresh account).
    pub fn own_metadata(self: &Arc<Self>) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let content = async {
                let active = this.active().await?;
                let me = active.me();
                let raw = active
                    .pool
                    .store
                    .latest_by_kind(me, Kind::Metadata)
                    .await
                    .ok()
                    .flatten()
                    .map(|e| e.content)
                    .unwrap_or_default();
                Ok::<String, LoginError>(raw)
            }
            .await;
            let _ = tx.send(content.unwrap_or_default());
        });
        rx
    }

    /// The own parsed Profile, from the store's freshest kind:0 (D38: the
    /// edit form's save lands a new kind 0 — the shell refreshes the rail's
    /// copy and the profile pane underneath with it; the profiles service's
    /// cache holds a 12h TTL, far too long to lean on). None when none is
    /// stored.
    pub fn own_profile(self: &Arc<Self>) -> tokio::sync::mpsc::UnboundedReceiver<Option<Profile>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let p = async {
                let active = this.active().await?;
                let me = active.me();
                Ok::<Option<Profile>, LoginError>(
                    active
                        .pool
                        .store
                        .latest_by_kind(me, Kind::Metadata)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|e| Profile::from_event(&e)),
                )
            }
            .await;
            let _ = tx.send(p.unwrap_or(None));
        });
        rx
    }

    /// Edit profile (NIP-01 kind:0 read-modify-write): the patch's Some
    /// fields overwrite, Some("") clears, None leaves the stored value;
    /// fields the app doesn't edit (lud06/lud16 and friends) ride along
    /// untouched. Published through commit_broadcast (phase 2: the
    /// profile rides every known relay, outbox retries it — kind 0 is
    /// coordinate-keyed, D36).
    pub fn set_profile(self: &Arc<Self>, patch: ProfileEdit) -> PublishRx {
        self.publish_task(move |active, _tx| {
            Box::pin(async move {
                let me = active.me();
                let stored = active
                    .pool
                    .store
                    .latest_by_kind(me, Kind::Metadata)
                    .await
                    .ok()
                    .flatten();
                let content = apply_profile_patch(
                    stored.as_ref().map(|e| e.content.as_str()).unwrap_or(""),
                    &patch,
                );
                let unsigned =
                    UnsignedEvent::new(me, Timestamp::now(), Kind::Metadata, [], content);
                let signed = active.sign_report(unsigned).await?;
                Self::commit_broadcast(active, &signed).await?;
                Ok(())
            })
        })
    }

    /// Delete own post (NIP-09 kind:5) + local tombstone.
    pub fn delete(self: &Arc<Self>, target: &Event) -> PublishRx {
        let target = target.clone();
        self.publish_task(move |active, tx| {
            Box::pin(async move {
                let unsigned = UnsignedEvent::new(
                    active.me(),
                    Timestamp::now(),
                    Kind::EventDeletion,
                    [seen::hint_tag(target.id, &target.pubkey)],
                    "",
                );
                let signed = active.sign_report(unsigned).await?;
                Self::commit(active, &signed, tx).await?;
                // Own deletion accepted: PIN the tombstone (deletions.ts
                // recordDeletion(ev, pin=true)) — immune to relay-fed
                // eviction, so the deleted note can't resurrect through
                // embeds, re-fetches, or a stranger's tag-spam churn.
                // (Audit: this was never wired; the pinned maps were dead.)
                deletions::record(&signed, true);
                let _ = active.pool.store.delete_event(target.id).await;
                Ok(())
            })
        })
    }

    /// Listen for push signals (bell dot, new-notes pill). Broadcast —
    /// every open pane keeps its own receiver.
    pub fn subscribe_live(&self) -> tokio::sync::broadcast::Receiver<Live> {
        self.live.subscribe()
    }

    /// The live wire (D68's relay column, live-wire v1 — owner call):
    /// subscribe ONE relay directly, kinds note + long-form, since NOW —
    /// no history, no store-attribution change (the real version is
    /// documented as per-event source-relay indexing; the wire proves
    /// the metaphor first). The note stream is account-independent, so
    /// this lives on the manager, not the session: the pool rides
    /// whatever session is active at arm time, and the wire's lifetime
    /// is the VIEW's receiver, not the login's.
    ///
    /// The URL passes the ONE ruleset (`net_guard::routable_relay`:
    /// wss only, public addresses only) at this CORE door — a typed or
    /// prefs-restored relay can never aim the wire transport at a
    /// private endpoint, and every future caller inherits the screen
    /// (the D61 no-second-ruleset lesson). Refusal returns a closed
    /// receiver: nothing dials, the view's pump exits quietly.
    ///
    /// When the view dies the broadcast's receiver count falls to zero;
    /// the drain task notices on its next send OR its 10s idle tick —
    /// whichever comes first — and unsubscribes. A closed column stops
    /// paying for its socket even when the relay has nothing to say.
    pub fn subscribe_relay_wire(
        self: &Arc<Self>,
        url: String,
    ) -> tokio::sync::broadcast::Receiver<Event> {
        use futures_util::StreamExt as _;
        let (tx, rx) = tokio::sync::broadcast::channel(128);
        if !crate::net_guard::routable_relay(&url) {
            eprintln!(
                "[core] wire: {} refused (not a routable relay)",
                crate::pool::clean_reason(&url)
            );
            return rx;
        }
        let this = self.clone();
        self.rt.spawn(async move {
            let Ok(active) = this.active().await else {
                return;
            };
            let pool = active.pool.clone();
            let filter = Filter::new()
                .kinds([Kind::TextNote, Kind::Custom(30023)])
                .since(Timestamp::now());
            let Ok(mut output) = pool
                .client()
                .subscribe(nostr_sdk::client::ReqTarget::manual(vec![(
                    url.as_str(),
                    // Kept alive for the CLOSED re-arm below (D85).
                    vec![filter.clone()],
                )]))
                .await
            else {
                eprintln!(
                    "[core] wire: subscribe to {} failed",
                    crate::pool::clean_reason(&url)
                );
                return;
            };
            let mut notifications = pool.client().notifications();
            // CLOSED re-arm state (D85): debounce + scheduled retry (a
            // CLOSED skipped for the cooldown, or a failed re-arm, must
            // retry on a timer — the removed sub can never CLOSE again).
            let mut last_rearm: Option<std::time::Instant> = None;
            let mut pending_rearm: Option<tokio::time::Instant> = None;
            // The idle sweep: teardown needs no event to arrive — the
            // receiver count is the truth about whether anyone watches.
            let mut idle = tokio::time::interval(std::time::Duration::from_secs(10));
            idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let n = tokio::select! {
                    _ = async {
                        match pending_rearm {
                            Some(at) => tokio::time::sleep_until(at).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        pending_rearm = None;
                        match pool
                            .client()
                            .subscribe(nostr_sdk::client::ReqTarget::manual(vec![(
                                url.as_str(),
                                vec![filter.clone()],
                            )]))
                            .await
                        {
                            Ok(o) => {
                                let _ = pool.client().unsubscribe(output.id()).await;
                                output = o;
                            }
                            Err(e) => {
                                eprintln!("[core] wire: re-arm failed ({e})");
                                pending_rearm = Some(
                                    tokio::time::Instant::now() + Self::REARM_COOLDOWN,
                                );
                            }
                        }
                        continue;
                    }
                    _ = idle.tick() => {
                        if tx.receiver_count() == 0 {
                            eprintln!(
                                "[core] wire: {} unwatched; unsubscribing",
                                crate::pool::clean_reason(&url)
                            );
                            break;
                        }
                        continue;
                    }
                    n = notifications.next() => n,
                };
                let event = match n {
                    Some(nostr_sdk::client::ClientNotification::Event {
                        subscription_id,
                        event,
                        ..
                    }) if output.id() == &subscription_id => *event,
                    Some(nostr_sdk::client::ClientNotification::Message { message, .. })
                        if matches!(
                            &*message,
                            nostr::message::RelayMessage::Closed { subscription_id, .. }
                                if output.id() == subscription_id.as_ref()
                        ) =>
                    {
                        // A CLOSED for the wire sub (relay restart,
                        // subscription pressure): the SDK has removed it
                        // and no reconnect replays a removed id (D85 —
                        // the bell got this arm in 0.9.1, the wire never
                        // did; one CLOSED froze the column for the
                        // session). Re-issue on the same filter, debounced.
                        if last_rearm.is_some_and(|t| t.elapsed() < Self::REARM_COOLDOWN) {
                            pending_rearm =
                                Some(tokio::time::Instant::now() + Self::REARM_COOLDOWN);
                            continue;
                        }
                        last_rearm = Some(std::time::Instant::now());
                        eprintln!(
                            "[core] wire: closed by {}; re-arming",
                            crate::pool::clean_reason(&url)
                        );
                        match pool
                            .client()
                            .subscribe(nostr_sdk::client::ReqTarget::manual(vec![(
                                url.as_str(),
                                vec![filter.clone()],
                            )]))
                            .await
                        {
                            Ok(o) => {
                                let _ = pool.client().unsubscribe(output.id()).await;
                                output = o;
                            }
                            Err(e) => {
                                eprintln!("[core] wire: re-arm failed ({e})");
                                pending_rearm =
                                    Some(tokio::time::Instant::now() + Self::REARM_COOLDOWN);
                            }
                        }
                        continue;
                    }
                    Some(nostr_sdk::client::ClientNotification::Shutdown) => break,
                    Some(_) => continue,
                    None => break,
                };
                // The chokepoint's kind-5/expiry arms (pool.ts onevent):
                // tombstones and expired copies never reach a view.
                if u16::from(event.kind) == 5 || nip40::is_expired(&event) {
                    continue;
                }
                // Nobody listening (the column closed): tear the sub down.
                if tx.send(event).is_err() {
                    let _ = pool.client().unsubscribe(output.id()).await;
                    return;
                }
            }
            let _ = pool.client().unsubscribe(output.id()).await;
        });
        rx
    }
}
