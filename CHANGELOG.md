# Changelog

Releases are tagged; the CI builds the deb, rpm, apk, and flatpak from the
tag. Notes are written here first: at tagging, the new version's paragraph
becomes the `<release>` entry in
`packaging/io.github.Letdown2491.satori.metainfo.xml` (Keep this source of
truth, not the other way around).

## Unreleased

- Replies open the composer sheet from any action row, thread included:
  the full form (attachments, alt text, preview, schedule) in a card over
  the current pane, replying-to line and post snippet on top. Publishing
  closes the card; the pane underneath stays where it is.
- The zap sheet became a proper overlay with its own card, like replies.
- The toast that rides the publish undo window tells the truth past its
  deadline ("sending…" instead of a fake "publishing in 1s"), and its Undo
  button centers its label.
- Follow and Mute sit grouped on the profile, not flung to opposite ends
  of the identity row.
- Fixed a flaky web-of-trust test (test cleanup was wiping shared state
  mid-assert).

## 0.5.0 - 2026-11-11

An iteration release with no theme: the composer grew a Video tab (a
deliberate video post is its own NIP-71 event, instant or scheduled,
videos only), thread notes now carry the full action row the timeline has
(reactions, repost, quote, reply, delete, zap), and the gallery carousel's
arrows and position dots page the gallery instead of opening the thread.

Under the hood: paste-time screening refuses wallet and bunker URIs whose
relays point at private addresses, the publish path refuses expired events
and records its own deletions as tombstones at the door, and dependencies
are current (cargo audit reports zero vulnerabilities). A CI workflow now
gates every pull request, with a cargo-deny pass beside the audit.

New in the final cut: a store vacuum (satori-desktop --vacuum rebuilds the
LMDB store offline, recovering the space deletions left behind) and an
Alpine package beside the deb, rpm and flatpak - the first musl build.

## 0.4.0 - 2026-11-08

The media and long-form release: attachments publish right (a note with an
image is a kind-1 note with imeta; replies keep their thread), the Photo
tab returns as its own picture-post surface, a full article composer
(title, summary, cover upload, topics, markdown body) with strikethrough,
task lists and tables in the reader, and hardenened media fetching
(DNS-rebinding-safe resolution, search only asks search-capable relays).

The composer grew a polish pass of its own: previews on every tab (the
article flips to the reader-faithful view, the rest show the draft as its
feed card), session drafts for every post type with a two-step escape (the
first escape saves and stays), a real editor (word motion, word deletion,
line-wise home and end, a body that scrolls), fields that wrap and grow
instead of clipping, video attachments in notes, a live reading time on
the article, the type selector as a segmented control, and one filled
action bar where Publish is unmistakable. A tag bug that kept satori's own
polls from rendering as ballots is fixed.

## 0.3.0 - 2026-11-07

The trust release: web of trust (in your network / outside, always
annotated, never hidden), a duplicate-content spam filter with receipts,
an auditable notification tier, and the four-lens audit fixes.

## 0.2.0 - 2026-11-07

The polish release: live feed engagement, the relay hint model, scheduled
posts, a full visual pass, and the quick wins (keyboard nav, emoji
typeahead, multi-image carousel, alt text, relay health pane).

## 0.1.0 - 2026-11-02

The first release: the whole client, from the follows timeline to inline
video and zaps.
