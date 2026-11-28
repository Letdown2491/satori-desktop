//! Search (NIP-50), hashtags, profile pages, and threads.

use super::*;

impl Core {
    // --- search (NIP-50) ----------------------------------------------------------

    /// Note search across the read set, gated by NIP-11 capability
    /// (relay-info.ts:126's nip50Capable): relays KNOWN to lack NIP-50
    /// search are never asked (they can't answer a search filter);
    /// unknown relays stay (absence of evidence isn't refusal); never
    /// empty — all-known-incapable asks everyone, search degrades
    /// instead of breaking.
    pub fn search(self: &Arc<Self>, query: String) -> FeedRx {
        self.feed_task(move |active| {
            Box::pin(async move {
                let filter = Filter::new().kind(Kind::TextNote).search(&query).limit(50);
                let mut relays: Vec<String> = active
                    .pool
                    .read_relays()
                    .into_iter()
                    .filter(|url| crate::nip11::search(url) != Some(false))
                    .collect();
                if relays.is_empty() {
                    relays = active.pool.read_relays();
                }
                // The fan-out path (get_from): per-relay queries with
                // latency attribution — and the gate above decides WHICH
                // relays hear the query at all.
                let mut results: Vec<Event> = active
                    .pool
                    .get_from(&relays, &filter)
                    .await
                    .into_iter()
                    .collect();
                // (no explicit save: the SDK persists what it receives)
                // Mutes and deletions gate search like every feed page
                // (search never applied them; D110).
                results
                    .retain(|e| !crate::filter::is_muted(&e.pubkey) && !deletions::is_deleted(e));
                results.sort_by_key(|e| std::cmp::Reverse(e.created_at.as_secs()));
                let profiles = Self::profiles_for(active, &results).await;
                Ok(FeedEvent::Search(results, profiles))
            })
        })
    }

    // --- profiles ---------------------------------------------------------------

    /// A hashtag timeline (Phase 2 §4.2): the `#t` filter across the
    /// visible catalog, any author — the profile() shape with a tag filter
    /// instead of an author set. Relay fetch into the store, then project
    /// straight off the fetch (the store's matcher isn't trusted for tags);
    /// mutes and deletions gate like every feed page.
    pub fn hashtag(self: &Arc<Self>, tag: String) -> FeedRx {
        let this = Arc::clone(self);
        self.feed_task(move |active| {
            Box::pin(async move {
                // t tags are conventionally lowercase (NIP-24); the tap
                // keeps its raw case for display, the filter normalizes.
                let filter = Filter::new()
                    .kinds(crate::store::visible_kinds())
                    .custom_tag(SingleLetterTag::LOWERCASE_T, tag.to_lowercase())
                    .limit(PAGE + 1);
                let mut notes: Vec<Event> = active
                    .pool
                    .get(filter)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                // (no explicit save: the SDK persists what it receives)
                {
                    let muted = this.muted.lock();
                    notes.retain(|ev| !muted.contains(&ev.pubkey));
                }
                let mut notes = this.drop_deleted(notes);
                notes.sort_by_key(|e| std::cmp::Reverse(e.created_at.as_secs()));
                let profiles = Self::profiles_for(active, &notes).await;
                Ok(FeedEvent::Loaded(FeedPage { notes, profiles }))
            })
        })
    }

    /// The hashtag timeline's older page — the home feed's scroll
    /// pagination (older()), with the tag filter standing in for the
    /// follow set. A full page keeps `has_more` lit; the next probe
    /// refetches around the oldest shown.
    pub fn hashtag_older(self: &Arc<Self>, tag: String, until: u64) -> FeedRx {
        let this = Arc::clone(self);
        self.feed_task(move |active| {
            Box::pin(async move {
                let filter = Filter::new()
                    .kinds(crate::store::visible_kinds())
                    .custom_tag(SingleLetterTag::LOWERCASE_T, tag.to_lowercase())
                    .until(Timestamp::from(until))
                    .limit(PAGE + 1);
                let mut notes: Vec<Event> = active
                    .pool
                    .get(filter)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                // (no explicit save: the SDK persists what it receives)
                {
                    let muted = this.muted.lock();
                    notes.retain(|ev| !muted.contains(&ev.pubkey));
                }
                let mut notes = this.drop_deleted(notes);
                notes.sort_by_key(|e| std::cmp::Reverse(e.created_at.as_secs()));
                let has_more = notes.len() > PAGE;
                notes.truncate(PAGE);
                let profiles = Self::profiles_for(active, &notes).await;
                Ok(FeedEvent::Older(FeedPage { notes, profiles }, has_more))
            })
        })
    }

    /// A profile page's projection: their recent notes + their metadata.
    pub fn profile(self: &Arc<Self>, who: PublicKey) -> FeedRx {
        self.feed_task(move |active| {
            Box::pin(async move {
                // Their notes: relay fetch into the store, then project.
                let filter = Filter::new()
                    .kinds(crate::store::visible_kinds())
                    .author(who)
                    .limit(50);
                // (no explicit save: the SDK persists what it receives)
                let _ = active.pool.get(filter).await;
                let notes = active
                    .pool
                    .store
                    .feed_page(&[who], None, PAGE)
                    .await
                    .unwrap_or_default();
                let profiles = Self::profiles_for(active, &notes).await;
                Ok(FeedEvent::Loaded(FeedPage { notes, profiles }))
            })
        })
    }

    /// The profile's next older page (endless scroll): author-scoped mirror
    /// of `older` — the store page behind the anchor, then a relay top-up
    /// for the author only (deletions ride along), then re-project.
    pub fn profile_older(self: &Arc<Self>, who: PublicKey, until: u64) -> FeedRx {
        self.feed_task(move |active| {
            Box::pin(async move {
                let mut notes = active
                    .pool
                    .store
                    .feed_page(&[who], Some(until), PAGE)
                    .await
                    .unwrap_or_default();
                if notes.len() <= PAGE {
                    let filter = Filter::new()
                        .kinds(crate::store::visible_kinds())
                        .author(who)
                        .until(Timestamp::from(until))
                        .limit(TOPUP_DEPTH);
                    // (no explicit save: the SDK persists what it receives)
                    let _ = active.pool.get(filter).await.unwrap_or_default();
                    let del_filter = Filter::new()
                        .kind(Kind::EventDeletion)
                        .author(who)
                        .until(Timestamp::from(until))
                        .limit(crate::outbox::QUERY_LIMIT);
                    let _ = active.pool.get(del_filter).await;
                    notes = active
                        .pool
                        .store
                        .feed_page(&[who], Some(until), PAGE)
                        .await
                        .unwrap_or_default();
                }
                let has_more = notes.len() > PAGE;
                notes.truncate(PAGE);
                let profiles = Self::profiles_for(active, &notes).await;
                Ok(FeedEvent::Older(FeedPage { notes, profiles }, has_more))
            })
        })
    }

    // --- threads ----------------------------------------------------------------

    /// Replies to a note (store first, relay top-up).
    /// Streamed (Nov 27 2026): the replies already in the store paint
    /// first (near-instant), then ONE relay round (kind-1 replies and
    /// NIP-22 comments together), then a second (nested comments and the
    /// NIP-09 deletion check together), each re-sending the whole merged
    /// set. The old shape ran four relay rounds back to back, each up to
    /// READ_BUDGET against every connected relay, and sent nothing until
    /// the last: 8 to 16 s of an empty pane even when the store already
    /// held every reply.
    pub fn thread(self: &Arc<Self>, id: EventId) -> FeedRx {
        /// The anchor author's inbox relays asked (their read list's
        /// first few; a long list is mostly redundancy).
        const INBOX_RELAYS: usize = 6;
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let active = match this.active().await {
                Ok(a) => a,
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                    return;
                }
            };
            // 1. The store paint.
            let local = match Self::thread_local(&active, id).await {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(FeedEvent::Failed(e.to_string()));
                    return;
                }
            };
            if !local.is_empty() {
                let _ = tx.send(Self::thread_page(&active, local).await);
            }
            // 2. Relay top-up: kind-1 replies (the SDK saves what it
            // receives; the store read re-applies its filters), the
            // top-level NIP-22 comments, and the anchor author's NIP-65
            // INBOX (read) relays, concurrently. The outbox model sends a
            // reply to the replied-to author's read relays, so that is
            // where replies are most complete, and often the fastest
            // answer (one targeted REQ per relay, latency-budgeted, vs
            // the client-wide read that waits on the slowest relay).
            let reply_filter = Filter::new().kind(Kind::TextNote).event(id).limit(200);
            let comment_filter = Filter::new().kind(Kind::Custom(1111)).event(id).limit(200);
            let author = active
                .pool
                .store
                .event_by_id(id)
                .await
                .ok()
                .flatten()
                .map(|e| e.pubkey);
            let inbox = async {
                // The anchor not stored yet (a cold open by id): the
                // broad reads still run; the inbox ask just sits out.
                let Some(pk) = author else {
                    return Vec::new();
                };
                let lists = active.pool.relay_lists(&[pk]).await;
                let mut urls = lists
                    .get(&pk.to_string())
                    .map(|l| l.read.clone())
                    .unwrap_or_default();
                urls.truncate(INBOX_RELAYS);
                if urls.is_empty() {
                    return Vec::new();
                }
                let f = Filter::new()
                    .kinds([Kind::TextNote, Kind::Custom(1111)])
                    .event(id)
                    .limit(200);
                active.pool.get_from(&urls, &f).await.into_iter().collect()
            };
            let (_, fetched_comments, from_inbox) = futures_util::future::join3(
                active.pool.get(reply_filter),
                active.pool.get(comment_filter),
                inbox,
            )
            .await;
            let mut replies = Self::thread_local(&active, id).await.unwrap_or_default();
            replies.extend(fetched_comments.unwrap_or_default());
            // Inbox kind-1 rows ride the store re-read above (saved on
            // receipt, filtered there); the comments merge directly.
            replies.extend(
                from_inbox
                    .into_iter()
                    .filter(|e: &Event| u16::from(e.kind) == 1111),
            );
            Self::thread_tidy(&mut replies);
            let _ = tx.send(Self::thread_page(&active, replies.clone()).await);
            // 3. NIP-22 nesting (comment-on-comment: a nested reply's
            // lowercase e names its parent COMMENT, the root rides
            // uppercase E, so the anchor's #e filter reaches top-level
            // comments only) and the NIP-09 deletions, targeted by id
            // (replies come from beyond the follows, so the sync can't
            // cover them; the chokepoint records the kind:5s it fetches
            // and `is_deleted` enforces deleter == author), concurrently.
            // A nested comment fetched in this same round is screened by
            // the tombstones already known, not by this round's query.
            let comment_ids: Vec<EventId> = replies
                .iter()
                .filter(|e| u16::from(e.kind) == 1111)
                .map(|e| e.id)
                .take(100)
                .collect();
            let reply_ids: Vec<EventId> = replies.iter().map(|e| e.id).take(200).collect();
            if comment_ids.is_empty() && reply_ids.is_empty() {
                return;
            }
            let nested = async {
                if comment_ids.is_empty() {
                    return Vec::new();
                }
                let mut f = Filter::new().kind(Kind::Custom(1111)).limit(200);
                for cid in comment_ids.iter() {
                    f = f.event(*cid);
                }
                let mut out = active
                    .pool
                    .store
                    .comments_of(comment_ids.clone())
                    .await
                    .unwrap_or_default();
                out.extend(active.pool.get(f).await.unwrap_or_default());
                out
            };
            let deletions = async {
                if reply_ids.is_empty() {
                    return;
                }
                let mut f = Filter::new().kind(Kind::EventDeletion).limit(200);
                for rid in reply_ids.iter() {
                    f = f.event(*rid);
                }
                // (no explicit save: the SDK persists what it receives)
                let _ = active.pool.get(f).await;
            };
            let (nested, ()) = futures_util::future::join(nested, deletions).await;
            let before: Vec<EventId> = replies.iter().map(|e| e.id).collect();
            replies.extend(nested);
            Self::thread_tidy(&mut replies);
            // Unchanged (no nested comments, nothing deleted): stay quiet,
            // the pane already shows this exact set.
            if replies.iter().map(|e| e.id).ne(before.iter().copied()) {
                let _ = tx.send(Self::thread_page(&active, replies).await);
            }
        });
        rx
    }

    /// Walk a focused note's parents up to the root (thread option B,
    /// Nov 27 2026): each hop follows the NIP-10/NIP-22 parent tag
    /// through `resolve_event` (store first, then the parent author's
    /// relays + hints, then the indexers), so a reply whose client wrote
    /// no root tag still reaches the true root. Sends the chain (root
    /// first) after every hop; stops at the root, an unreachable parent
    /// (the gap flag), a cycle, or MAX_ANCESTORS hops.
    pub fn ancestors(self: &Arc<Self>, focus: Event) -> FeedRx {
        const MAX_ANCESTORS: usize = 24;
        let (tx, rx) = mpsc::unbounded_channel();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let Ok(active) = this.active().await else {
                return;
            };
            let mut chain: Vec<Event> = Vec::new();
            let mut seen: HashSet<EventId> = HashSet::from([focus.id]);
            let mut cur = focus;
            let mut gap = false;
            for _ in 0..MAX_ANCESTORS {
                let Some((pid, hints)) = crate::nip::nip10::reply_parent(&cur) else {
                    break;
                };
                let Ok(pid) = EventId::from_hex(&pid) else {
                    break;
                };
                if !seen.insert(pid) {
                    break;
                }
                let author = parent_author_hint(&cur, &pid);
                match this.resolve_event(&active, pid, hints, author).await {
                    Some(parent) => {
                        chain.insert(0, parent.clone());
                        let page = Self::thread_page(&active, chain.clone()).await;
                        if let FeedEvent::Replies(c, p) = page
                            && tx.send(FeedEvent::Ancestors(c, p, false)).is_err()
                        {
                            return; // the pane closed
                        }
                        cur = parent;
                    }
                    None => {
                        gap = true;
                        break;
                    }
                }
            }
            if gap {
                let page = Self::thread_page(&active, chain).await;
                if let FeedEvent::Replies(c, p) = page {
                    let _ = tx.send(FeedEvent::Ancestors(c, p, true));
                }
            }
        });
        rx
    }

    /// The store's view of a thread: kind-1 replies, the NIP-22 comments
    /// on the note, and the stored comments on those comments, tidied.
    async fn thread_local(active: &Active, id: EventId) -> Result<Vec<Event>> {
        let mut replies = active
            .pool
            .store
            .replies(id)
            .await
            .map_err(|e| LoginError::Resume(e.to_string()))?;
        replies.extend(active.pool.store.comments(id).await.unwrap_or_default());
        let comment_ids: Vec<EventId> = replies
            .iter()
            .filter(|e| u16::from(e.kind) == 1111)
            .map(|e| e.id)
            .take(100)
            .collect();
        if !comment_ids.is_empty() {
            replies.extend(
                active
                    .pool
                    .store
                    .comments_of(comment_ids)
                    .await
                    .unwrap_or_default(),
            );
        }
        Self::thread_tidy(&mut replies);
        Ok(replies)
    }

    /// Drop the deleted and the muted, dedupe by id, oldest first. (Muted
    /// authors' replies used to render in threads: every feed page
    /// dropped them, the thread read never did. D110.)
    fn thread_tidy(replies: &mut Vec<Event>) {
        replies.retain(|e| !deletions::is_deleted(e) && !crate::filter::is_muted(&e.pubkey));
        let mut seen: HashSet<EventId> = HashSet::new();
        replies.retain(|e| seen.insert(e.id));
        replies.sort_by_key(|e| e.created_at.as_secs());
    }

    /// One Replies landing: the set plus its authors' cached profiles.
    async fn thread_page(active: &Active, replies: Vec<Event>) -> FeedEvent {
        let authors: Vec<PublicKey> = replies.iter().map(|e| e.pubkey).collect();
        let cached = active
            .pool
            .store
            .profiles(&authors)
            .await
            .unwrap_or_default();
        let profiles: Vec<Profile> = cached.iter().filter_map(Profile::from_event).collect();
        FeedEvent::Replies(replies, profiles)
    }
}

/// The parent's author, when the reply names it: NIP-10's optional
/// fifth e-tag element, or (kind 1111) NIP-22's lowercase `p`, which is
/// the parent's author by spec. Feeds `resolve_event`'s author-relay
/// race; None falls back to the hints and the indexers.
fn parent_author_hint(ev: &Event, parent: &EventId) -> Option<PublicKey> {
    let hex = parent.to_hex();
    let tagged = |name: &str| {
        ev.tags.iter().find_map(|t| {
            let parts = t.as_slice();
            (parts.first().map(String::as_str) == Some(name)).then_some(parts)
        })
    };
    let from_e = ev.tags.iter().find_map(|t| {
        let parts = t.as_slice();
        (parts.first().map(String::as_str) == Some("e") && parts.get(1) == Some(&hex))
            .then(|| parts.get(4))
            .flatten()
            .and_then(|pk| PublicKey::from_hex(pk).ok())
    });
    from_e.or_else(|| {
        (u16::from(ev.kind) == 1111)
            .then(|| tagged("p"))
            .flatten()
            .and_then(|parts| parts.get(1))
            .and_then(|pk| PublicKey::from_hex(pk).ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_author_hint_reads_nip10_and_nip22() {
        let keys = Keys::generate();
        let parent = EventBuilder::new(Kind::TextNote, "p")
            .finalize(&keys)
            .unwrap()
            .id;
        let author = Keys::generate().public_key();
        // NIP-10: the fifth e-tag element names the parent's author.
        let reply = EventBuilder::new(Kind::TextNote, "x")
            .tag(Tag::custom(
                "e",
                [
                    parent.to_hex(),
                    String::new(),
                    "reply".into(),
                    author.to_hex(),
                ],
            ))
            .finalize(&keys)
            .unwrap();
        assert_eq!(parent_author_hint(&reply, &parent), Some(author));
        // NIP-22: a comment's lowercase p IS the parent's author.
        let comment = EventBuilder::new(Kind::Custom(1111), "x")
            .tag(Tag::custom("e", [parent.to_hex()]))
            .tag(Tag::public_key(author))
            .finalize(&keys)
            .unwrap();
        assert_eq!(parent_author_hint(&comment, &parent), Some(author));
        // A kind-1 p tag is a mention, not the parent's author.
        let plain = EventBuilder::new(Kind::TextNote, "x")
            .tag(Tag::custom("e", [parent.to_hex()]))
            .tag(Tag::public_key(author))
            .finalize(&keys)
            .unwrap();
        assert_eq!(parent_author_hint(&plain, &parent), None);
    }
}
