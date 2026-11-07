# WoT & spam research: Dark Wisp Android and Amethyst

Sources: Dark Wisp Android (`barrydeen/dark-wisp-android`, shallow clone at
`/tmp/opencode/dark-wisp-android`; Kotlin paths below relative to
`app/src/main/kotlin/com/darkwisp/app/`) and Amethyst
(`greenart7c3/Amethyst`, shallow clone at `/tmp/opencode/amethyst`; Kotlin
paths relative to the repo root). Desktop citations are `crates/core/src/…`
in this repo. Conventions follow `notes/0.2-research.md`.

Constraint check: satori has NIP-51 mute machinery (kind 10000, private-by-default,
`manager.rs:366` `muted` set + `manager.rs:768` `is_muted`, chokepoint gating at
`manager.rs:2464/2486/2607`, D18 "mutes gate at both phases"), event seen-on
(`seen.rs`), author-keyed relay hints (`hints.rs`: `record_provenance`,
`record_tag_hints`, `for_pubkey`), and learned relay profiles
(`relay_latency.rs`). Everything below was checked against that inventory; no
idea re-recommends what exists.

---

## 1. Web of trust: how each builds it

### Dark Wisp: two-hop qualified network (client-computed, threshold-based)

Dark Wisp builds a second-degree social graph entirely on-device:

- **Storage**: per-account SQLite table `followed_by(pubkey, follower)` —
  `repo/SocialGraphDb.kt` (schema at the `onCreate` block, ~line 20; per-account
  DB filename + one-time migration from a legacy shared DB, `migrateAndOpen`,
  ~line 35).
- **Build**: `repo/ExtendedNetworkRepository.kt` fetches each first-degree
  follow's kind-3 contact list (coverage target = 70% of first degree,
  ~line 177), then counts second-degree appearances: for each entry in each
  follow list it bumps `counts[pk]` and records `followedBy` rows + relay
  hints from the entry's relay field (`processFollowListEvent` loop,
  ~lines 225–270; batched SQLite inserts every 5000 rows to avoid OOM).
- **Qualification**: a pubkey is in the "qualified network" when **at least
  10 of your first-degree follows also follow them** (`THRESHOLD = 10`,
  `ExtendedNetworkRepository.kt:85`; filter at ~lines 273–277, which also
  excludes muted/blocked pubkeys). `isInQualifiedNetwork(pubkey)`
  (~line 504) is the membership query; `getFollowedBy(pubkey)` returns the
  set of your follows who follow someone (used for "followers-in-network"
  on profiles).
- **Caching/staleness**: the qualified set + relay set-cover persists as a
  JSON blob in SharedPreferences (`saveToPrefs`/`loadFromPrefs`, ~lines 536+),
  with a 24h staleness window and a 10% drift threshold
  (`STALE_HOURS`, `STALE_DRIFT_THRESHOLD`, lines 91–92). A `DiscoveryState`
  sealed class (lines 47–55) exposes build progress to the UI.
- **What it gates**: the optional "extended network" feed scope
  ("follows + follows-of-follows", README §Safety) — the expanded relay pool
  for it comes from a greedy set-cover over the qualified pubkeys' kind
  10002s (`computeRelaySetCover`, ~line 360; `MAX_EXTENDED_RELAYS = 100`,
  `MAX_AUTHORS_PER_RELAY = 300`). It is a *feed scoping* mechanism, not a
  trust score: no ranking, no distances beyond exactly two hops.

No NIP-85, no trust scores, no provider in Dark Wisp.

### Amethyst: two parallel systems

**A. Local follow-graph score (`WoTService`)** — `commons/src/commonMain/kotlin/com/vitorpamplona/amethyst/commons/wot/WoTService.kt`:

- Score(pubkey X) = **count of the active user's follows who also follow X**
  (class doc, lines 39–41). One hop beyond your follow set, counted, not
  distance-based.
- Built incrementally from kind-3 events of your follows only
  (`applyKind3`, lines 122–140; events from non-follows are ignored),
  maintained via a reverse index (target → set of my-follows who follow
  them) and per-follower follow-set snapshots for diff-based updates when a
  follower republishes their list (lines 85–88, `recompute` writes at
  ~line 297: `if (n > 0) _scores[target] = n else _scores.remove(target)`).
- **Hard bounds**: WoT shuts off entirely above `MAX_FOLLOWS = 2000`
  (line 302; disable path at ~line 203), follow lists are capped at
  `MAX_FOLLOWS_PER_EVENT = 5000` against hostile publishers (line 305).
  Single-writer coroutine serializes mutations (doc §Concurrency).
- Seeded by one batch kind-3 REQ over the follow set; readiness marked on
  first EOSE or a startup timeout (`markReadyOnce`, ~line 148).

**B. NIP-85 trust-provider network (rank assertions from a third party)** — `commons/.../wot/network/`:

- The user designates a **trust provider** via kind 10040
  (`TrustLabels.kt` describes kind 10040 designations: `30382:rank` user-rank
  assertions, Trusted List kind 30392, D-list assistant 39998). The provider
  (e.g. a Brainstorm-style rank publisher) publishes per-user rank cards.
- **Sync**: `TrustNetworkSource.kt` / `TrustNetworkState` — a dedicated
  relay client (so hundreds of thousands of cards never enter the app's
  `LocalCache`) downloads the provider's full index once, then reconciles
  with **NIP-77 negentropy** (`reconcile` in the `TrustNetworkConnection`
  interface, ~line 31; "Null when the relay cannot reconcile (no NIP-77)"),
  with checkpoint/resume for partial downloads and a weekly full re-check
  (documented in `geode/src/test/.../TrustNetworkSyncTest.kt` class doc).
- **Verdicts**: `TrustVerdicts.kt` (`explain`, ~lines 40–52) — SELF,
  FOLLOW (in your kind-3), TRUSTED (provider rank ≥ user's **minimum-score**
  slider), BELOW_MIN_SCORE, NOT_IN_NETWORK, NO_NETWORK (feature off).
  Newer cards seen between syncs override the loaded index
  (`TrustOverlayCard` + `rankIn`, `TrustNetworkModels.kt` ~lines 47–55 /
  `TrustVerdicts.kt` `rankIn`).
- **CLI parity**: the same verdict logic is exercised headlessly by
  `cli/.../commands/trust/TrustCheck.kt` ("Known or stranger, and why…
  the verdict the DM tabs, Curated notifications and collapsed replies use").

Amethyst also has **GrapeRank operator tooling** (a crawler/score/publish
pipeline for producing the 30382 cards themselves:
`cli/.../commands/graperank/GrapeRankCrawl.kt|GrapeRankScore.kt|GrapeRankPublish.kt`)
— that is provider-side infrastructure, not client work.

**Seeding/update summary**: Dark Wisp re-crawls follow lists on a 24h staleness
window and on demand; Amethyst A updates incrementally per kind-3 arrival,
Amethyst B syncs on a schedule with NIP-77 delta reconciliation plus overlay
cards for freshness between syncs.

## 2. Spam protection

### Dark Wisp

- **nspam ML classifier (on-device LightGBM)**: `ml/NSpamClassifier.kt`
  (raw margin → sigmoid → piecewise-linear calibration, whole file),
  `ml/NSpamWeights.kt` (weights shipped in app assets,
  `loadFromAssets` at `viewmodel/FeedViewModel.kt:298`),
  `ml/NSpamFeatures.kt` (feature hashing: 131072 char + 131072 word
  MurmurHash3 buckets + 17 structural + 6 group features, constants at
  lines 12–15; structural features include URL counts, mention/hashtag
  patterns, emoji, punctuation, invisible-char-stripped "body keys" for
  duplicate-ish detection, ~lines 39–101), `ml/MurmurHash3.kt`,
  `ml/LightGbmModel.kt`, `ml/NSpamPreprocessor.kt`.
  **Author-level, not event-level**: the classifier scores the author's
  last ≤10 cached kind-1 notes (`NotificationRepository.warmSpamScore`,
  ~lines 720–745: skips follows, safelisted pubkeys, non-replies; regex
  feature extraction deliberately runs OUTSIDE the repo lock — see the
  method doc's 5s-ANR war story). Score cached per author in
  `repo/SpamAuthorCache.kt` (LRU 2000; re-scored once the author's cached
  note count crosses `RESCORE_THRESHOLD = 5`).
- **Gate**: notifications only — a reply from an author scoring **≥ 0.7**
  is dropped from the notification stream (`mergeReply`,
  `repo/NotificationRepository.kt` ~line 759: `if (cached != null && cached >= 0.7f) return false`).
  The main feed uses the social-graph scope and mutes, not the ML score.
- **Mutes**: NIP-51 kind 10000 pubkey mutes synced via Nostr plus local
  muted words (#tag-exact or substring) and muted threads —
  `repo/MuteRepository.kt` (word/hashtag matcher doc at lines 93–96,
  persistence to SharedPreferences at ~line 163). Blocked relays via
  NIP-51 kind 10006 (README §Safety).
- **PoW (NIP-13), outgoing**: `viewmodel/PowManager.kt` mines configurable
  per-kind difficulty via its own `nostr/Nip13.kt` (`PowStatus.Mining`
  progress states), settings in `ui/screen/PowSettingsScreen.kt` +
  `repo/PowPreferences.kt`. This is spam-*etiquette* (making your own posts
  cost something), not inbound filtering.
- Client-side everything; no third-party services, no relay-side filtering
  beyond picking your relays.

### Amethyst

- **Duplicate-content heuristics (ingest-time)**:
  `commons/.../model/cache/AntiSpamFilter.kt` — hashes `31 * content.hashCode() + tags`
  (so identical replies to different people don't collide, ~line 47), keeps
  LRU 2000 of recent event ids/addressables + LRU 1000 spammer records;
  events < 60 chars are exempt ("GM" spam false-positive, ~line 57), and
  short `nostr:`-prefixed messages (bot commands) are exempt while repeated
  invoices/phishing URLs are not (~line 63). A repeat offender accrues
  duplicated ids/addresses and is hidden at **5 duplicates**
  (`Spammer.shouldHide()`, line 38). Duplicates are reported per-relay
  (`relayStats.newSpam`, ~line 115) so a relay flooding duplicates is
  visible, and a `flowSpam` stream drives a **SpammingUsersScreen**
  settings surface (`commonsUI/.../settings/SpammingUsersScreen.kt`).
  Wired at every cache-consume site: `EventCache.kt` lines 1288, 1360,
  1405, 1450, 1493 (`if (verified && event is BaseNoteEvent && antiSpam.isSpam(event, relay))`).
- **WoT gating of streams**: `CuratedTrustFilter.kt`
  (`Account.curatedHidesByTrust`, ~lines 33–50) — notifications from
  authors *outside* the trust network are hidden, with a single shared
  exemption list (all zap kinds cost money; chess/community/stream/chat
  messages governed by their own membership; DMs use the known-chatroom
  rule). The same `TrustVerdicts.isOutside` drives thread-reply collapsing
  and chat rows (§3).
- **Hidden users**: `model/nip51Lists/HiddenUsersState.kt` — NIP-51 hidden
  users plus "transient" hidden users (in-session hides that aren't yet
  published), assembled into a `LiveHiddenUsers`.
- **PoW (NIP-13), outgoing, policy-driven**:
  `commons/.../service/pow/` — `PoWPolicy.kt` groups kinds into
  user-facing categories (`PoWCategory`: short notes/comments/reports/
  long-form default ON; reposts/reactions/chat/gift-wraps OFF, lines
  34–53), `PoWEstimator.kt`, `PoWPublishQueue.kt` (mining before publish),
  `PoWJobPersistence.kt` + `PowJobRestorer.kt` (mining survives process
  death), `PoWReplay.kt`.
- **App-authenticity trust (the "dot/verification" work)**:
  `commons/.../walletScrutiny/BuildVerdict.kt` — reproduci­ble-build
  verdicts (TRUSTED / FAILED / CAUTION / UNKNOWN, `verdictToneOf`) rendered
  as verification cards for wallet apps. Client-side display of a
  third-party verifier's assertions; related assertion-decryption caches
  live under `model/nip85TrustedAssertions/`
  (`UserAssertionDecryptionCache.kt`, `TrustProviderListDecryptionCache.kt` —
  NIP-85 private-list decryption).
- **Hashtag spam filter (desktop)**: designed, not yet in the app —
  `docs/plans/2026-06-29-feat-desktop-hashtag-spam-filter-plan.md`
  (collapse spammy hashtags, exempt self+follows, slider threshold,
  `rememberSaveable` reveal state).
- **Relay-side**: none trusted. Amethyst runs its own relay
  (`geode/`, incl. `server/HttpAdmission.kt`, `config/RateLimits.kt`,
  `mirror/MirrorWorker.kt`) but that is infrastructure for the NIP-85
  provider/mirror, not a client spam dependency.

## 3. How trust is surfaced in the UI

**Dark Wisp** keeps it nearly invisible:

- The safety surface is a settings tab: `ui/screen/SafetyScreen.kt` /
  `SafetyFiltersTab.kt` (spam filter toggle, safelist, keyword mutes,
  social-graph scope). Spam-filtered replies just never appear in
  notifications — no badge, no score shown.
- The extended-network build has a progress surface
  (`ui/screen/SocialGraphScreen.kt`, fed by `DiscoveryState`), and profiles
  show "followers in network" counts (`getFollowedBy`).
- PoW mining shows a status (`PowStatus.Mining(kind, attempts, difficulty)`).

**Amethyst** surfaces trust explicitly and verbosely:

- **Profile trust badge**: `commonsUI/.../profile/header/ProfileTrustBadge.kt`
  — a pill under the name: "In your network · Score 42" / "Outside your
  network · Not scored" / "You follow" (`TrustVerdictBadge`, ~lines 65–90),
  tinted by verdict, tapping opens the WoT settings. It even asks the
  provider for the person's card on profile view so a fresher score shows
  (`observeUserAssertionsScore`, ~line 58).
- **Auto-collapse**: thread replies from authors outside the network start
  collapsed, descendants of a collapsed reply are hidden and counted —
  `commonsUI/.../threadview/ThreadFeedView.kt` lines 271–314
  (`collapsedByDefault`, hidden-count folding "as gitworkshop folds it").
- **Notifications**: outside-network authors dropped from Curated
  notifications entirely (`CuratedTrustFilter.kt`), not demoted.
- **Chats**: outside-network public-chat rows render a muted placeholder
  with a "show" affordance — `commonsUI/.../chats/feed/OutsideNetworkChat.kt`
  (exemptions: stream host/speakers, relay-group members, channel creator
  vouch via the room's observed state, doc ~lines 50–55).
- **Settings + onboarding**: `WebOfTrustScreen.kt` (provider, minimum-score
  slider, sync status including per-outcome reasons from
  `TrustNetworkModels.kt`'s `TrustNetworkOutcome`), a first-run **nudge**
  (`wot/nudge/WebOfTrustNudge.kt` + `WebOfTrustNudgeRules.shouldOffer`),
  and a per-user drill-down (`TrustCardParts.kt`, `TrustedList.kt`).
- **Spam surfaced as data, not labels**: the SpammingUsersScreen lists
  caught spammers with per-relay duplicate reports; spammy notes are simply
  absent from caches (no collapsed-with-reveal treatment yet — the planned
  hashtag filter adds that pattern).

## 4. Comparison, and what satori could adopt

### Honest comparison

**Dark Wisp** — strengths: everything runs on-device (ML included), the
qualified-network threshold is cheap, explainable ("followed by ≥10 of your
follows"), and doubles as a relay-routing substrate (set-cover over the
extended network — a genuinely clever reuse). Weaknesses: exactly two hops,
no scores/ranking; the ML path is notifications-only with a hard 0.7 cut, no
user visibility into why something was dropped; SQLite social graph is heavy
for a desktop with LMDB already in place; the trust story and the spam story
don't connect (graph scope ≠ spam filter).

**Amethyst** — strengths: the layered verdict model (SELF/FOLLOW/TRUSTED/
BELOW_MIN_SCORE/NOT_IN_NETWORK) is the most honest WoT UX in the space —
every gate can *explain itself*, feature-off is a first-class state
("not known yet never reads as no network"), and bounds (MAX_FOLLOWS,
per-event caps, dedicated sync client, NIP-77 delta sync) show production
hardening. The duplicate-content AntiSpamFilter is simple, cheap, and
per-relay-attributed. Weaknesses: the local WoT score is a raw count that
only works under 2000 follows and is noisy (one popular follow pollutes
scores); the NIP-85 system outsources judgment to a provider (Brainstorm/
GrapeRank), adding a trusted third party satori's §1.1 posture would resist;
two parallel WoT systems with overlapping vocabulary is real complexity;
duplicate-based spam detection only catches lazy spam.

### Candidates for satori (ranked)

Pure-core unless noted; satori's `crates/core` already owns mutes
(`manager.rs`), provenance (`seen.rs`), hints (`hints.rs`), and learned relay
profiles (`relay_latency.rs`).

**Quick wins**

1. **Author-keyed duplicate-content filter at the pool chokepoint** —
   Amethyst's `AntiSpamFilter.kt` model (content+tags hash, LRU caps,
   ≥5 duplicates → drop, short-content exemptions, per-relay attribution)
   slots directly into D34's `consume_and_filter` in `pool.rs`; per-relay
   duplicate counts ride the existing `relay_latency.rs` sidecar pattern.
   *Pure-core, ~150 lines + tests. Highest spam-per-line-of-code of anything here.*
2. **Two-hop "known" set as a mute-complement, not a feed algorithm** —
   Dark Wisp's `followed_by` count with a threshold (their `THRESHOLD = 10`)
   computed from the kind-3s satori's manager already ingests for the
   follows feed; store as a D33-discipline JSON sidecar (bounded, LRU).
   Gates: notification stream only (D18's "mutes gate at both phases" gains
   an optional "from outside your extended network" tier, default OFF per
   §1.3 quiet-by-default). *Pure-core, ~200 lines. Touches routing only if
   reused for relay selection.*
3. **Extended-network relay set-cover (feeds the existing routing)** —
   Dark Wisp's greedy cover over 2nd-degree kind-10002s
   (`ExtendedNetworkRepository.computeRelaySetCover`) is a natural second
   consumer of the set in #2 and lands in `pool.rs`'s existing shard
   builder as one more candidate source alongside `seen.rs`/`hints.rs`.
   *Touches routing, ~100 lines, strictly additive behind a pref.*
4. **Explainable verdicts as a core enum** — port Amethyst's
   `TrustVerdict` shape (SELF/FOLLOW/TRUSTED/BELOW_MIN_SCORE/NOT_IN_NETWORK/
   NO_NETWORK) into core as the single vocabulary every future gate uses,
   with "feature off" as a state, never null-as-false. *Pure-core, ~50
   lines, prevents re-litigating semantics per feature.*

**Structural work**

5. **Thread-reply auto-collapse from WoT** — Amethyst's
   `ThreadFeedView` pattern (outside-network replies start collapsed,
   descendants folded, hidden-count chip). Needs the verdict enum (#4) plus
   UI state in `app-gpui` thread view; respects satori's "unknown kinds
   degrade honestly" ethos since collapsed ≠ deleted. *Touches UI +
   core lookup, ~1–2 days.*
6. **On-device author spam scoring (notifications tier)** — Dark Wisp's
   nspam architecture is the right shape (author-level score from last N
   notes, cache with re-score threshold, regex features off the chokepoint
   thread, ≥threshold drops from notifications only) but the LightGBM
   weights are a maintenance liability; a satori version would start with
   the *structural* features only (duplicate bodies, URL/mention ratios —
   their `NSpamFeatures` structural block) as a hand-tuned score before
   any model. *Pure-core, ~2–3 days for the structural version.*
7. **NIP-85 trusted assertions (read-only)** — Amethyst's
   kind-10040-provider → rank-cards → NIP-77 reconcile pipeline is the only
   way to get real ranked scores, but it introduces a trusted provider and a
   sync client (their dedicated-client rule — keep hundreds of thousands of
   cards out of the main cache — is the load-bearing detail). Defer until a
   provider satori's maintainer actually trusts exists; the verdict enum
   (#4) keeps the door open. *Touches core sync + UI, ~a week.*
8. **Outgoing NIP-13 PoW per-category policy** — both clients do this
   (Dark Wisp `PowManager.kt`, Amethyst `PoWPolicy.kt`+`PoWPublishQueue.kt`);
   satori's `publish.rs` + `outbox_queue.rs` would host it. Low value until
   relays satori users choose start demanding it. *Touches publish path,
   ~1 day, P5-era.*

**Not recommended**: Amethyst's local raw-count score as a *displayed*
number (noisy, fragile past 2000 follows); any third-party spam API
(neither client uses one; satori's no-telemetry rule forbids it); Dark
Wisp's SQLite graph (LMDB/sidecars already cover satori's needs); relay-side
filtering trust (both clients agree with satori: relays are untrusted pipes).
