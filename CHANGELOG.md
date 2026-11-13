# Changelog

Releases are tagged; the CI builds the deb, rpm, apk, and flatpak from the
tag. Notes are written here first: at tagging, the new version's paragraph
becomes the `<release>` entry in
`packaging/io.github.Letdown2491.satori.metainfo.xml` (Keep this source of
truth, not the other way around).

## 0.7.0 - 2026-11-13

The interoperability release: NIP-faithful surfaces where the client was
thin, each landed against real published-event fixtures. Comments became
a surface: kind-1111 threads render and compose on articles (the reader
grew a comments section with a reply affordance), comment counts ride
the feed bubble, and the core preserves roots and addressable targets
through nested fetches. Replies reach who they're for: publishes now
additionally target the parent author's and tagged users' inbox relays
(their kind-10002 read sets) and where the parent was recently seen, so
a reply no longer vanishes for an author who shares no relay with us.
The notifications pane gained an accurate tally strip: counts come from
the classified rows themselves, so a chip's number and its tap's row
list agree by construction, scoped by a 24h/7d/30d dropdown that
remembers its choice.

The wallet got paste-time protection (a merged outside contribution):
relays inside nostr+walletconnect:// and bunker:// URIs are screened
before any socket opens, private-address relays ask first, consents
live in the settings pane for revocation, and the wallet client is shut
down properly when a second URI replaces it. Cleartext ws:// relays are
refused outright.

Media discipline: every pane pins exactly what is on screen, so images
stop disappearing while a page idles and RAM stops ballooning on photo
profiles. The pin registry is owner-keyed with a per-owner byte cap, so
no surface can hold the whole photo budget hostage, and closing a pane
releases its pins. The reader caps comment fetches and windows their
avatars. Big pictures load now: the media path's total-request timeout
is gone in favor of connect and idle-read bounds. The flatpak now
carries the PulseAudio permission, so inline video plays its sound (the
audio socket was masked, so inline video played silently).

Under the hood: the pre-release five-lens audit (performance, system
usage, memory, security, duplication) fixed its verified findings, the
consent ledger is written atomically with 0600 permissions, preferences
sync defers to newer local edits (feed settings no longer revert at
login), the engagement engine lives in one implementation, and clicking
a user in a thread opens their profile.

## 0.6.0 - 2026-11-12

The replies-and-repairs release. Replies became surfaces, not a page: the
composer sheet opens from any action row, thread included (the full form
with attachments, alt text, preview and schedule in a card over the
current pane, replying-to line and post snippet on top), and the zap
sheet became a proper overlay with its own card. Notifications wear
their glyph anatomy: a reaction leads with its emoji, and reposts and
quotes join the pane. Follow and Mute sit grouped on the profile, and
the undo toast tells the truth past its deadline.

Signing got its biggest reliability pass yet: the first sign after login
works against strict signers (the session wears the handshake-completed
client), the first sign after a restart carries the pairing secret the
URI issued, and signing out and back in no longer strands the app on
the login doors (the shell swap now has its listener attached to every
login screen, not just the startup one).

Under the hood: net_guard is the one IP authority (M5 WP1, decision
D57). The pre-release four-lens audit (performance, resources, memory,
security) fixed ten verified findings: zap receipts no longer run a
signature verification per visible row per frame, repost payloads are
parsed once and shared, a closed pane leaves no resident pump tasks
behind, abandoned signer transports are shut down instead of dropped,
the upload client and the video player respect the IP authority,
signer responses log method names only (the log file is now 0600), the
single-instance socket read is capped, and the shell's navigation
dispatch lives in one place. A flaky web-of-trust test stopped wiping
shared state mid-assert.

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
