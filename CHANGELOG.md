# Changelog

Releases are tagged; the CI builds the deb, rpm, apk, and flatpak from the
tag. Notes are written here first: at tagging, the new version's paragraph
becomes the `<release>` entry in
`packaging/io.github.Letdown2491.satori.metainfo.xml` (Keep this source of
truth, not the other way around).

## 0.14.0 - 2026-11-27

Spam replies no longer ring the bell. Someone none of your follows follow
now counts as outside your network once enough of your follows' contact
lists are loaded, so a brand new throwaway account replying to you lands
in the hidden list instead of your notifications. Mentions from outside
your network are held the same way, the bell dot no longer lights for
anything the notifications page would hide, and zaps always come through.

A new Filters tab in Settings. Mute words, phrases, hashtags and regular
expressions; matching notes collapse with the reason and a Show button,
in timelines, threads and search, and move to the hidden list in
notifications. Words and hashtags are saved privately in your mute list,
so other clients can use them. Satori also spots disguised spam on its
own: notes that swap in lookalike letters from other alphabets, or the
same message sent by several accounts, from people outside your network.
Muted accounts no longer show up in threads or search results.

Settings tabs are reordered: General, Content, Filters, Trust,
Notifications, Composing, Relays, Wallet. The duplicate-content filter
and its list of caught spammers moved to Filters.

Threads and notifications are tidier. Reply counts skip replies the
thread hides, so a note whose only reply is spam no longer shows a 1.
Hidden and collapsed replies sink to the bottom of the thread. The
notifications page opens with the newest items already in place, and
clicking the bell while you are on it loads what just arrived. Reaction
rows now show which of your notes was reacted to.

Faster, quieter startup. The timeline shows placeholder rows while it
loads instead of "connecting to the relays" in two places, and says
"Still reaching your relays" only if they take more than a few seconds.
With a remote signer, the fresh timeline no longer waits for your
private lists to decrypt. Satori starts connecting to your relays while
the saved timeline is still loading, and the saved timeline itself
loads in a fraction of a second right after a reboot, down from several
seconds. Nothing heavy runs before your timeline appears anymore.

On btrfs (the Fedora default), Satori now keeps its local store out of
copy-on-write, which made the first launch after a reboot very slow. An
existing store is copied once in the background while you use the app,
and the new copy takes over at your next launch.

Store maintenance stays out of your way. The occasional store
compaction now copies in the background during a session and hands
over in about a second at the next launch. Before, a compaction that
couldn't finish in the background rebuilt the whole store before the
window appeared, which could take minutes. The daily cleanup of old
notes also removes them in batches instead of one disk write each.

Avatars and images now stay cached between launches. Until now, every
launch emptied the image cache and downloaded everything again.

Settings, General has a new Text group: pick a font for the whole app,
and a size for notes, replies and articles. Any installed font works.

Smaller fixes: scrolling the gif picker no longer scrolls the timeline
behind the composer, and the Settings tabs are evenly spaced.

## 0.13.0 - 2026-11-27

Gifs animate on hover again: since 0.12.0 every hover preview was being
refused, so the gif tiles stayed still. Two quick taps on different videos
now always play the one you tapped last.

Sturdier on disk. Your session, preferences, zap history and wallet
connection are written atomically, so a crash or power loss mid-save can
no longer sign you out or reset your settings. Older stores are recognized
correctly for future format upgrades.

The window appears immediately at launch, even while your keyring is
still unlocking. Screen readers can now see and press the main controls:
the note actions, the sidebar, the composer and the sign-in doors.

Threads open faster: the replies you already have show at once, and
the rest arrive in two quick rounds instead of four slow ones, without
jumping you back to the top. Replies are also asked for where they are
most likely to be, the original author's own inbox relays. Clicking a
reply now opens that reply, with the posts it answers shown above it as
readable posts, and a link to the full thread. Reply previews and quoted
notes in the timeline read in full-strength text instead of faded grey.

Pictures and videos display whole again. Some pictures had their bottom
cut off in the timeline (tall photos especially, and any post whose size
tag didn't match the picture), and videos with unusual widths played
sheared diagonally with green edges.

Your mute and bookmark lists are safe with other clients. Muting or
bookmarking in Satori used to rewrite entries it didn't understand: muted
words and hashtags from another client came back as broken key entries,
bookmarked articles and links were mangled, and a list carrying relay hints
could be read as empty and overwritten. Every entry now survives untouched,
and a list Satori can't decrypt is never overwritten.

Safer QR sign-in: the nostrconnect library (0.45.3) now rejects a signer
that answers without the connection secret, which could have let a
stranger on the relay pose as your signer.

Under the hood: the renderer moves to gpui-pre 0.3.14 (wgpu 30), the file
picker to rfd 0.18, a crash in one background task can no longer cascade
into the whole app, and profile and engagement lookups no longer build a
text key for every visible note on every frame.

## 0.12.0 - 2026-11-21

Gifs, done properly. The composer has a gif button: type to search, hover
to preview, click to pick. The pick publishes as a real attachment, an
imeta-tagged gif that every client renders, not just this one. In the
feed, gif posts animate when you hover and play with controls when you
click, and a click always plays: even if you crossed three gifs on the
way there, none of them can hold your tap hostage.

Storage you control. Settings gains retention levers: keep notes,
pictures and the rest for a span you choose (a week to ten years), and
the store vacuums what aged out on its own schedule.

Under the hood, the store learned versioning. The on-disk format now
carries a schema stamp and a migration runner, so a future format change
converts the store in place at boot instead of asking for a fresh login,
and a downgrade is refused with a quiet screen before anything opens.

Bunker honesty. When a NIP-46 signer stops answering, an amber pill reads
"Signer unreachable, reads still work" where the outbox pill lives: no
dismiss, no button, gone the moment the signer returns.

Around the house: Settings gains a Content tab for the feed's kind
catalog, sign out moved into the avatar menu, and a fresh start opens
three columns. A relay that failed three writes in a row used to sit out
of the publish fan-out forever; it now comes back on its own within ten
minutes, and when the fan-out does skip a relay, the log says so.

As with every release, a five-lens audit (performance, system usage,
memory, security, duplication) ran over the batch before the tag. Its
densest vein was the gif picker's hover machinery: hover is not intent,
so a preview never opens a browser tab, a sweep across the grid builds
nothing, and a build nobody wants any more is dropped rather than parked
looping. It also caught a store stamp that could desync from the
generation it describes, a self-heal path one rename away from stranding
a recovery, and smaller finds, all fixed.

## 0.11.1 - 2026-11-20

Reading and pictures settle down. Note text now keeps the spacing its
author typed: single line breaks stay tight, and a blank line between
paragraphs actually shows one, instead of everything reading as one
evenly-spaced run. Profiles' about text picks up the same rhythm, and
keeps its paragraph breaks where they used to be dropped. Big pictures
arrive pre-sized for the screen: oversized originals are re-encoded once
on fetch, so a couple of huge photos can no longer push the picture
cache over its memory budget and flash everything on screen blank while
it refetches, in single column, in multicolumn, and worst on the
articles page.

## 0.11.0 - 2026-11-20

Zaps grow up. The zap sheet is a real dialog now: the amount is a big
typeable number (pick a preset pill to seed it, type over it), a message
can ride the zap (256 characters, delivered with the payment), and the
preset strip is yours to shape: save any amount plus message as a pill
from the sheet, remove one with a right-click, or manage the whole strip
in Settings under Wallet. Two new preferences live in the sheet itself:
a zap can be sent anonymously (signed with a throwaway key, so the
receipt carries no name; the receipt publishes through neutral public
relays rather than your own, which would give the anonymity away), and
the quick tap on a note's bolt can be turned off, so tapping opens the
sheet instead of instantly sending. The tap amount is configurable too,
in the same Settings pane.

Profiles can be zapped. A profile's header carries a Zap button between
follow and mute, and the zap lands as a profile-level payment with no
note attached.

Other ways to pay. Some people publish their payment addresses from
other services (Venmo, Bitcoin, Monero and friends) as NIP-A3 payment
targets. Where an author publishes them, Satori shows quiet chips on
their profile and in the zap sheet: one click opens the address in the
service's app or page, right-click copies it. Chips never attach an
amount, and they appear even when no wallet is connected, so the
payment affordance has no dead end.

Zap messages also surface where the zaps do: a notification for a zap
that carried a message shows the message.

As with every release, a five-lens audit (performance, system usage,
memory, security, duplication) ran over the batch before the tag. It
caught two real bugs: profile zaps could never receive their final
confirmation (the receipt check looked for a marker no receipt carries,
so the bolt stayed amber forever after a successful payment), and
anonymous zaps leaked the identifying relay list described above. Both
are fixed, along with smaller finds: a stale receipt can no longer
confirm a new zap, and oversized zap messages can no longer silently
fail a payment.

## 0.10.0 - 2026-11-19

Editing your lists can no longer wipe them. Follow, mute, and bookmark
lists were rebuilt from what the session had in memory when you toggled,
so a change made in the first seconds after launch (before your lists
finished loading), or one made from another client mid-session, could
publish a shorter list over the relay's full one. Every list publish now
starts from the newest copy of the list Satori knows (the local store,
with one fallback fetch when it has none), keeps the parts of the old
event a toggle does not touch (a follow list's relay hints, a mute
list's public entries), and refuses to publish a removal when it has
nothing to remove from. This is the same shape Amethyst ships, proven there by years of daily
use. A second five-lens audit of the whole release then caught three
last holes: an un-bookmark skipped the same protection, an unmute
could silently come back the next time you muted someone else, and
the background store rebuild held too much in memory at once on large
stores. All three are fixed, and a failed store swap can no longer
delete the backup copy it was recovering from.

Your relationships live on one page now. Your profile picture at the top
of the rail opens the me menu, and its new Network row opens a page with
two segments: Following (people you follow) and Muted (people you have
silenced, which until now you could not see anywhere). Every row carries
the actions that fit the relationship: follow or unfollow, mute or
unmute, right there in the list, no more visiting each profile to manage
who you hear from. (An early build also tried a Followers segment: Nostr
has no global followers list, the reverse lookup undersold the real
number by an order of magnitude, and a confidently wrong list is worse
than its absence, so it did not ship.)

The event store keeps itself lean. The store's file only ever grows,
even after notes are deleted or pruned: the space stays allocated until
the store is rebuilt. Satori has shipped an explicit offline rebuild
(the `--vacuum` flag) since 0.5.0; the app now runs the same verified
rebuild automatically when the store has grown half again past its size
at the last rebuild (at least 64 MiB of growth, so small stores never
churn) — and the rebuild runs in the background while you use the app,
never in front of it: the slimmed store swaps in at a later launch, so
no launch waits on the rebuild.

Alpine packages are signed by a persistent key: one keypair signs every
release, on both architectures, so the "UNTRUSTED signature" warning apk
showed on first install is fixed for good. Trust the .pub once and every
future version verifies clean. From this release on:

    wget https://github.com/Letdown2491/satori-desktop/releases/download/v0.10.0/satori-desktop-0.10.0-<ARCH>.pub
    sudo cp satori-desktop-0.10.0-<ARCH>.pub /etc/apk/keys/
    apk add ./satori-desktop-0.10.0-<ARCH>.apk

(If you were sideloading with `--allow-untrusted`, stop needing it.)

Connections come back faster. A full audit of the relay connection
lifecycle surfaced real gaps in how Satori recovers: the reconnect
nudge after sleep was a no-op in exactly the case it existed for (every
relay asleep-disconnected), so recovery rode the SDK's own timers, up
to a minute or more. The nudge now force-retries each dead relay the
moment the machine wakes, short lid-closes included; relays added for
routed reads actually connect now (some followed-author reads could
sit empty for a whole session); the live bell's re-arm no longer has
terminal failure paths; the wire column and engagement counts survive
a relay restarting under them; and a relay that answered brownout
queries with nothing can earn its way back into the read rotation.

## 0.9.2 - 2026-11-16

@mentions inside note bodies now show people's names everywhere, not just
in the notes and articles timelines. A mention in a note is a raw npub
token that the renderer turns into a name by looking the person up; the
page fetches only ever looked up note AUTHORS, so the same note showed
@name in the timeline and @npub1… in the notifications page or on a
profile (it worked sometimes, when another pane happened to have fetched
that person). The page builders now include everyone a body mentions, and
the notifications and profile views patch names in as they land. Fixing
this also un-broke the notifications page's receipt names, whose update
channel was silently dropped.

Notification rows carry the copy-the-event-id seal that notes and
articles already have: it sits on the timestamp and copies the note on
screen (the boosted note for a repost; receipts that show no note get no
seal).

The Library has left the sidebar, and then the Library itself left. Your
profile picture at the top of the rail now opens a small menu: View
profile, Bookmarks, Drafts, and Lists, and each one is its own page with
its own name and back button. One fewer icon in the rail, and your
things live in one obvious place: under you.

Threads stop flashing their images. A thread pinned every picture it
contained against memory pressure, and on photo-heavy threads that pin
silently ran out partway down: the pictures on screen lost their
protection and blinked out and back as you scrolled. Threads now protect
exactly what is on screen, like every other page, and the reply list no
longer re-animates itself when names and context land in the background.

## 0.9.1 - 2026-11-15

The live notification subscriptions survive relay CLOSEDs. A relay that
answers CLOSED for one of the live bell's subscriptions (subscription
pressure, a relay restart, an auth flip) used to lose that subscription for
good: the SDK removes a CLOSED subscription and never re-sends it on
reconnect, so the bell dot and the new-notes pill starved silently for the
rest of the session. The live loop now re-arms both subscriptions when a
relay takes one away (debounced to one re-arm per minute), and the routed
relay trim no longer skips its own job: stale relays are evicted for real
(a silent no-op let the client's relay set grow unbounded), while relays
still hosting subscriptions are re-stamped instead of evicted, so the
trimming can never kill the live bell.

Relays that refuse a query for a reason we can satisfy are no longer
treated as read-denials. purplepag.es (the profile and graph relay)
answered our wider queries with "limit too high: 1000 (max 500)", and
every refusal stamped the relay read-dead for 12 hours, cutting profile
fetches off from the one relay best at them. A limit rejection or a
NIP-42 auth challenge now leaves the relay in the read rotation (the
query simply gets no contribution from it, or the challenge is answered
with the session identity); genuine refusals (paywalls, restrictions,
bans) still stamp the relay read-dead, and refusal log lines now include
the relay's reason text. Our own network queries now cap at 500 events
(the strictest mainstream relay limit), so strict relays answer them
instead of refusing them.

The routed fan-out no longer dials other people's personal firehose
endpoints. A relay list entry like `wss://filter.nostr.wine/<npub>` is a
per-customer subscription: only the key in the path can answer its auth
challenge, so a follow's endpoint was an endless
authenticate-and-fail loop for us. Those entries now screen out of the
routed set (our own, if we ever list one, still works).

Auth flows gained observability: a relay refusing our signed auth event
(unpaid tier, an unwelcome key) now logs as "auth rejected" instead of
vanishing, and a mock auth-gated relay test proves the full
challenge-sign-accept dance delivers (so a challenge line that repeats
forever pins the failure to the signer leg, e.g. a bunker that won't
sign kind 22242).

The notifications page now verifies that each row is actually about you.
The mention query asks relays for notes carrying your key in a p tag, but
the answers were trusted as-is: a relay that mis-serves the filter could
paint strangers' notes as "mentioned you" (the @geek report). Every row
is now checked client-side: your key in a p tag, your key in an
addressed a tag, or a reference to one of your recent notes. Zaps,
reactions, and reposts all clear this check naturally; nothing a relay
injects does. The live bell already verified exactly this and is
unchanged.

## 0.9.0 - 2026-11-15

Video tiles earned their picture. A note's video now previews its first
frame instead of a bare box: the sniff pipeline finally reads the frame it
prerolls, big files answer a ranged head request, and files whose decode
tables live at the file's end (moov-at-end mp4s, every GIF-to-MP4
conversion) ride the same guarded range-proxy inline playback uses to pull
exactly the bytes the decoder asks for. GIF-encoded-as-mp4 clips play
inline now too: the vendored video player accepts the zero-framerate caps
those converters emit and no longer drops every frame that arrives without
a duration. The head-sniff's download cap tightened from 2MB to 1MB (files
that need more fall through to ranged fetching rather than over-fetching
blind).

Every note timestamp gained a copy button: one tap puts a `nostr:nevent1…`
(or `naddr1…` for addressable kinds) URI on the clipboard, with a short
flash to confirm the copy.

Notes with a plain link now show a preview card: the page's og: metadata
(site, title, description, thumbnail) fetched when the note scrolls into
view, capped and parsed defensively, with the thumbnail riding the ordinary
image cache. Strict Tor keeps previews off entirely. Links in note bodies
are cleaned up too: the well-known tracking params (fbclid, gclid, the
utm_* family and friends) are stripped before display, before click-through,
and before any preview fetch, so your clicks don't share their tags with
trackers.

Clicking the "new notes" chip is fast now: the notes the live feed already
saved paint immediately, and the relay round deepens in the background,
while the chip reads "Loading new notes…" until they land. In multicolumn
view the chip appears on the notes timeline only (it used to double up on
the articles column). Feeds start up faster for the same reason: page
landings no longer wait on a metadata round trip for authors the profile
cache has never seen; names and avatars patch in as they resolve. Behind
the scenes, the trust-graph's list fetch fires once per launch instead of
once per pane, and link previews load when a note scrolls into view rather
than for every note the relays push. Preview cards now appear for
extension-less web pages (a wiki slug, an App Store link used to render as
a broken picture tile that suppressed its own card), and flaky preview
fetches give up after five tries instead of retrying forever.

The disappearing-image class is closed for its remaining roots. Preview
thumbnails now come back when memory pressure drops them (they used to go
dark for the whole session once evicted), the notifications pane re-asks
for evicted images as its rows render (it only fetched at arrival before,
so anything evicted never recovered), and the relay wire pane does the
same for its rows.

The multicolumn workspace's add-column button moved from the end of the
column row (where adding a column meant scrolling the row first) into the
sidebar, above Settings. It shows while the workspace is up and the menu
it opens sits beside the rail.

## 0.8.0 - 2026-11-14

The interop pass continues on two fronts: what a note can ask for, and
where a publish goes.

Zaps learned arithmetic. A note with a zap split (NIP-57) now pays every
receiver: weights are relative, an unweighted receiver on a partially
weighted note gets nothing, a receiver with no lightning pointer fails
alone instead of failing the whole split, and a retry replays every
pending record. Zap goals (NIP-75) render as cards with live progress, a
raised-of target line, and a closed marker once the ask's deadline
passes.

Comments learned addresses. A comment on an article, picture, video or
wiki now whispers "on {name}'s note" in the notifications pane and opens
the parent by its coordinate (NIP-22): the thread fetches the
addressable anchor, learns its id, and rides from there; long-form
parents land in the reader.

The feed grew surfaces for the kinds it already carried: content
warnings veil until revealed (NIP-36), emoji shortcodes render their
images (NIP-30), subject lines headline the row (NIP-14/24), and an
unknown kind that carries an alt tag renders that description instead
of vanishing (NIP-31). Picture metadata parsing is pinned against real
client fixtures (NIP-68).

Publishing learned phase 2 of the NIP-65 routing: the app keeps
per-event seen-on memory, deletion checks ask the relays where the
original lived, profiles and relay lists publish to every known relay
so they exist where we only read, personal kinds (mute lists,
bookmarks, drafts) never broadcast, and a foreign inbox relay visits
for one publish and leaves instead of joining the pool forever.

Composing got friendlier. The emoji picker inserts the unicode
character instead of the :shortcode:; the reaction row's heart grows a
right-click quick-set (five positive glyphs plus the heart) with
tooltips; and emoji render in color at last: a vendored gpui patch
exempts the known emoji fonts from the primary-font gate, and the
composer field splits its text runs so emoji clusters ride the color
face.

Mentions learned people. The typeahead matches display name, handle,
NIP-05 and pubkey hex, and finds anyone you interact with, not just
accounts you follow; the profile mirror is seeded from the disk cache
at launch, so names resolve on the first keystroke after a restart.

The screen learned to work in columns. A rail toggle (or the c key)
opens a TweetDeck-style workspace: up to six stream columns pin side
by side, each live, each scrolling on its own; the streams are Home,
Articles, Notifications, Search, any hashtag, an author (pinned from
a profile's + Column button), or a relay (pinned from the + menu,
live only: what that relay pushes while the column is open, no
history). Drilling into a thread, article
or profile still takes the whole width, and Esc hands you back to the
columns exactly as you left them. The layout persists across
restarts: which columns, in what order, how wide. Columns reorder by
dragging their header and resize from their right edge, the rail
keeps out of the way, retiring the button of a stream already pinned,
and h/l hop the focus between columns, scrolling the target into
view. The columns never narrow: the row scrolls horizontally when
they overflow.
Two follow-up fixes from the first days with it: repost rows carry
the boosted note's action bar (the envelope used to render bare), and
the columns' add and remove buttons read at full ink.

The render riders close the interop pass's last open surfaces.
Pictures and videos with a `d` tag now show comment counts on their
action row, and clicking one opens the comment thread by its address
(NIP-22) — the thread was unreachable by id before. Voice notes render
as a row with the author's waveform and duration, and play inline
(NIP-A0): one player at a time, the same guarded door as the video
player, strict Tor degrading to the browser link. Code snippets (NIP-C0)
render in a monospace card with the file's name, language, description
and license, and a tap copies the code. A kind the app has never heard
of now renders as a quiet "unrendered kind N" line instead of
vanishing; nothing arrives invisible.

Under the hood, the interop batch got its five-lens audit (the same
performance / system usage / memory / security / duplication pass as
the previous two cycles) and the findings landed: the feed's
parent-whisper and comment-count maps are capped so a long scrolling
session cannot grow them for the process lifetime; secrets (nsec,
bunker pairing and client keys, the wallet's NWC secret) are wiped from
freed heap; the single-instance socket verifies the connecting peer's
uid and fails closed; the auth-URL door only opens http and https
links; the mention scan was moved off the per-keystroke hot path; and
the wallet pump no longer reads its own entity mid-construction at
startup. The columns batch got its own five-lens pass, and the
findings landed: a closed relay column's socket used to leak (its
pump held the subscription open after the view died; the pump now
exits with the view and the relay task unsubscribes on an idle
check), the wire column snapshots its data like the feed's instead of
deep-copying it per frame, relay URLs pass the same screening
everywhere they are typed, restored or dialed, and five hand-copied
code shapes collapsed into shared kernels, and the render-riders cycle
collapsed the fourteen publish call sites and the two schedule paths
onto shared task kernels. The riders and the refactor got the same
five-lens audit, and its findings landed: a voice row's tap on a
non-https attacker URL no longer hands the link to a desktop handler;
the comment-count fetches batch instead of firing one subscription per
picture (unbounded on the bookmarks pane); a double-tap on a voice row
can no longer spawn two players mid-build; the relay column's quote
embeds and in-flight author ledger are capped so nothing rides outside
its bounds for the session's life; and the guard sequence both players
share, the FIFO trim, and the signed-event test fixture are one kernel
each instead of hand-copies.

## 0.7.1 - 2026-11-13

The attachment-interop fix, hours after 0.7.0: a note's attached image
or video now puts its URL in the event content beside the imeta tag.
NIP-92 lets a client ignore an imeta that matches no content URL, and
the strict ones did: the attachment showed in Satori and nowhere else.
Comments get the same treatment, and an author who pastes the URL
themselves gets no duplicate. Picture and video posts were already
self-contained by design.

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
