//! Notification stream — port of `src/data/notifications.ts` semantics.
//!
//! Replies + mentions (kind 1/1111 #p), zaps (9735, verified), poll votes (1018),
//! optional reactions (off by default, dislikes never shown), private replies
//! merged from the DM cache. No counts, no like/repost-of-yours noise. Bell dot =
//! anything newer than the read high-water, muted actors excluded.

// TODO(port): M2.
pub struct Notifications;
