//! Content filters (D110): the user's own rules and the objective spam
//! signals, behind one question every surface asks, [`judge`].
//!
//! **User rules** (apply to everyone but yourself, follows included):
//! - words and phrases: NIP-51 kind:10000 `word` entries (synced through
//!   the mute list, private by default like the muted keys). A single
//!   word matches on word boundaries, a phrase as a substring, both
//!   case-insensitive and AFTER lookalike folding (so "SΟL" written with
//!   a Greek omicron still matches "sol").
//! - hashtags: kind:10000 `t` entries, matched against the event's `t`
//!   tags and inline `#tags`.
//! - regex: no NIP-51 slot exists, so the patterns live in prefs.json
//!   (`filter_regex`, synced over NIP-78). The `regex` crate runs in
//!   linear time (no catastrophic backtracking), and each pattern is
//!   length- and size-capped, so a bad pattern cannot hang the app.
//!
//! **Spam signals** (`spam_signals` pref, default ON; only for authors
//! who are not you, a follow, or Trusted by the graph):
//! - lookalike characters: words that mix Latin letters with Cyrillic,
//!   Greek, Cherokee or Armenian lookalikes ("ClaiᎷ yՕur frЕe"). Real
//!   multilingual text switches script per word, not inside one. Two or
//!   more mixed words flag the note.
//! - templates: the same message (lookalikes folded, URLs reduced to
//!   their host, digits and punctuation dropped) from
//!   [`TEMPLATE_AUTHORS_TO_FLAG`] or more distinct outside authors. The
//!   key-farm shape the per-author duplicate filter (dedup.rs) can't see.
//!
//! What a surface does with a verdict is the surface's call: feeds and
//! threads collapse the row with the reason (the NIP-36 veil's shape),
//! notifications move it to the receipts strip, the bell stays quiet.
//! Nothing is deleted. Core-clean: no UI deps.

use parking_lot::Mutex;
use regex::{RegexBuilder, RegexSet, RegexSetBuilder};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use unicode_script::{Script, UnicodeScript};

use nostr::prelude::*;

use crate::wot::{self, Verdict};

/// Most regex patterns kept (the D24 discipline: every list is bounded).
pub const MAX_PATTERNS: usize = 50;
/// Longest accepted pattern source, in bytes.
pub const MAX_PATTERN_LEN: usize = 300;
/// Compiled-program ceiling per pattern (the regex crate's size limit).
const REGEX_SIZE_LIMIT: usize = 1 << 20;
/// Mixed-script words before a note reads as lookalike spam.
pub const MIXED_WORDS_TO_FLAG: usize = 2;
/// Distinct outside authors sending one template before it is flagged.
pub const TEMPLATE_AUTHORS_TO_FLAG: usize = 3;
/// Letters a normalized message needs before it can be a template (short
/// greetings collide innocently; dedup.rs's 60-char floor, same idea).
const TEMPLATE_MIN_LETTERS: usize = 40;
/// Template keys tracked (oldest-inserted evicted).
const TEMPLATE_CAP: usize = 4000;
/// Authors remembered per template (only the count up to the flag
/// threshold matters; a few extra absorb churn).
const TEMPLATE_AUTHORS_KEPT: usize = 8;
/// Per-event verdict cache (cleared whole when full: rows re-judge
/// cheaply, and a render pass only ever touches a screenful).
const JUDGE_CACHE_CAP: usize = 8192;
/// How often the prefs-backed rules (regex, the spam switch) are
/// re-read. `judge` runs per rendered row, so it must not stat the
/// prefs file every call.
const PREFS_REFRESH: Duration = Duration::from_millis(500);

/// Why a note is held. `label` is the user-facing reason line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// A muted author (lists drop them; this is the veil for any surface
    /// that still renders one, an ancestor chain or a quote).
    Muted,
    Word(String),
    Hashtag(String),
    Pattern(String),
    Lookalike,
    Template,
}

impl Reason {
    /// The reason line a collapsed row or a receipt shows.
    pub fn label(&self) -> String {
        match self {
            Reason::Muted => MUTED_LABEL.to_string(),
            Reason::Word(w) => format!("{USER_RULE_PREFIX}word: {w}"),
            Reason::Hashtag(t) => format!("{USER_RULE_PREFIX}hashtag: #{t}"),
            Reason::Pattern(p) => format!("{USER_RULE_PREFIX}pattern: {p}"),
            Reason::Lookalike => "Lookalike characters".to_string(),
            Reason::Template => "Same message from many accounts".to_string(),
        }
    }
}

const USER_RULE_PREFIX: &str = "Filtered ";
const MUTED_LABEL: &str = "Muted account";

/// Is this receipt reason one of the spam signals?
pub fn is_spam_label(label: &str) -> bool {
    label == Reason::Lookalike.label() || label == Reason::Template.label()
}

/// The `spam_signals` switch (default ON).
pub fn spam_signals_on() -> bool {
    refresh_prefs(false);
    RULES.lock().spam_on
}

/// Is this receipt reason one of the user's own rules? The receipts
/// ledger's "once receipted, still receipted" rule (D48) must NOT hold
/// these: removing a rule has to bring its notes back.
pub fn is_user_rule_label(label: &str) -> bool {
    label.starts_with(USER_RULE_PREFIX) || label == MUTED_LABEL
}

static MUTED: LazyLock<Mutex<HashSet<PublicKey>>> = LazyLock::new(Default::default);

/// The mute set's mirror (pushed with the WoT graph's, at login and
/// after every mute toggle).
pub fn set_muted(muted: HashSet<PublicKey>) {
    *MUTED.lock() = muted;
}

/// Is this author muted? (The read paths' one spelling.)
pub fn is_muted(pk: &PublicKey) -> bool {
    MUTED.lock().contains(pk)
}

#[derive(Default)]
struct Rules {
    /// (as entered, lowercased; folded for matching).
    words: Vec<(String, String)>,
    /// Lowercased, no leading '#'.
    hashtags: Vec<String>,
    /// Valid pattern sources, in prefs order.
    patterns: Vec<String>,
    set: Option<RegexSet>,
    /// The prefs value the patterns were compiled from (change detection).
    patterns_raw: String,
    spam_on: bool,
    checked: Option<Instant>,
}

static RULES: LazyLock<Mutex<Rules>> = LazyLock::new(|| {
    Mutex::new(Rules {
        spam_on: true,
        ..Default::default()
    })
});
/// Bumped on every rule change: the verdict cache keys on it.
static GEN: AtomicU64 = AtomicU64::new(1);

fn bump() {
    GEN.fetch_add(1, Ordering::Relaxed);
}

/// The mute list's word and hashtag entries (login, and after every
/// local edit publishes). Replaces the previous set.
pub fn set_list_rules(
    words: impl IntoIterator<Item = String>,
    hashtags: impl IntoIterator<Item = String>,
) {
    let mut seen = HashSet::new();
    let words: Vec<(String, String)> = words
        .into_iter()
        .map(|w| normalize_word(&w))
        .filter(|w| !w.is_empty() && seen.insert(w.clone()))
        .map(|w| {
            let folded = fold(&w);
            (w, folded)
        })
        .collect();
    let mut seen = HashSet::new();
    let hashtags: Vec<String> = hashtags
        .into_iter()
        .map(|t| normalize_hashtag(&t))
        .filter(|t| !t.is_empty() && seen.insert(t.clone()))
        .collect();
    let mut r = RULES.lock();
    r.words = words;
    r.hashtags = hashtags;
    drop(r);
    bump();
}

/// The canonical stored form of a word entry (NIP-51: lowercase).
pub fn normalize_word(w: &str) -> String {
    w.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The canonical stored form of a hashtag entry: lowercase, no '#'.
pub fn normalize_hashtag(t: &str) -> String {
    t.trim().trim_start_matches('#').trim().to_lowercase()
}

/// The current word entries (as stored).
pub fn words() -> Vec<String> {
    RULES.lock().words.iter().map(|(w, _)| w.clone()).collect()
}

/// The current hashtag entries (as stored, no '#').
pub fn hashtags() -> Vec<String> {
    RULES.lock().hashtags.clone()
}

/// The regex patterns currently in force (valid ones, prefs order).
pub fn patterns() -> Vec<String> {
    refresh_prefs(true);
    RULES.lock().patterns.clone()
}

/// Sign-out: the list rules are session identity. (Prefs-backed rules
/// re-read from disk, which is per-machine.)
pub fn clear() {
    {
        let mut r = RULES.lock();
        r.words.clear();
        r.hashtags.clear();
    }
    TEMPLATES.lock().clear();
    MUTED.lock().clear();
    bump();
}

fn build_one(p: &str) -> Result<(), String> {
    if p.trim().is_empty() {
        return Err("Pattern is empty".to_string());
    }
    if p.len() > MAX_PATTERN_LEN {
        return Err(format!(
            "Pattern is longer than {MAX_PATTERN_LEN} characters"
        ));
    }
    RegexBuilder::new(p)
        .case_insensitive(true)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map(|_| ())
        .map_err(|e| {
            // The crate's error is a multi-line caret diagram; the last
            // line names the problem.
            let msg = e.to_string();
            let last = msg
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            format!(
                "Invalid pattern: {}",
                last.trim().trim_start_matches("error: ")
            )
        })
}

/// Validate one pattern before it is saved (the settings field).
pub fn validate_pattern(p: &str) -> Result<(), String> {
    build_one(p)
}

fn compile(sources: &[String]) -> (Vec<String>, Option<RegexSet>) {
    let valid: Vec<String> = sources
        .iter()
        .filter(|p| build_one(p).is_ok())
        .take(MAX_PATTERNS)
        .cloned()
        .collect();
    if valid.is_empty() {
        return (valid, None);
    }
    let set = RegexSetBuilder::new(&valid)
        .case_insensitive(true)
        .size_limit(REGEX_SIZE_LIMIT * 4)
        .build()
        .ok();
    (valid, set)
}

/// Re-read the prefs-backed rules when stale (`force` skips the cadence).
fn refresh_prefs(force: bool) {
    {
        let r = RULES.lock();
        if !force && r.checked.is_some_and(|t| t.elapsed() < PREFS_REFRESH) {
            return;
        }
    }
    let prefs = crate::config::prefs();
    let raw = prefs
        .as_ref()
        .and_then(|v| v.get("filter_regex").cloned())
        .map(|v| v.to_string())
        .unwrap_or_default();
    let spam_on = prefs
        .as_ref()
        .and_then(|v| v.get("spam_signals").and_then(|b| b.as_bool()))
        .unwrap_or(true);
    let mut r = RULES.lock();
    r.checked = Some(Instant::now());
    let mut changed = false;
    if raw != r.patterns_raw {
        let sources: Vec<String> = serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default();
        let (valid, set) = compile(&sources);
        r.patterns = valid;
        r.set = set;
        r.patterns_raw = raw;
        changed = true;
    }
    if spam_on != r.spam_on {
        r.spam_on = spam_on;
        changed = true;
    }
    drop(r);
    if changed {
        bump();
    }
}

// --- Lookalike folding ------------------------------------------------------

/// Map a lookalike letter to the Latin letter it imitates (a curated
/// subset of Unicode's confusables table, UTS #39: the Cyrillic, Greek,
/// Cherokee and Armenian letters spam actually uses, plus fullwidth
/// ASCII). Unmapped characters pass through.
fn fold_char(c: char) -> char {
    match c {
        // Fullwidth ASCII letters and digits.
        '\u{FF21}'..='\u{FF3A}' => char::from(b'A' + (c as u32 - 0xFF21) as u8),
        '\u{FF41}'..='\u{FF5A}' => char::from(b'a' + (c as u32 - 0xFF41) as u8),
        '\u{FF10}'..='\u{FF19}' => char::from(b'0' + (c as u32 - 0xFF10) as u8),
        // Cyrillic.
        'А' => 'A',
        'В' => 'B',
        'Е' | 'Ё' => 'E',
        'К' => 'K',
        'М' => 'M',
        'Н' => 'H',
        'О' => 'O',
        'Р' => 'P',
        'С' => 'C',
        'Т' => 'T',
        'Х' => 'X',
        'У' | 'Ү' => 'Y',
        'Ѕ' => 'S',
        'І' | 'Ӏ' => 'I',
        'Ј' => 'J',
        'Ԛ' => 'Q',
        'Ԝ' => 'W',
        'Һ' => 'h',
        'а' => 'a',
        'е' | 'ё' => 'e',
        'о' => 'o',
        'р' => 'p',
        'с' => 'c',
        'у' | 'ү' => 'y',
        'х' => 'x',
        'ѕ' => 's',
        'і' => 'i',
        'ј' => 'j',
        'һ' => 'h',
        'ԁ' => 'd',
        'ԛ' => 'q',
        'ԝ' => 'w',
        'ӏ' => 'l',
        'к' => 'k',
        // Greek.
        'Α' => 'A',
        'Β' => 'B',
        'Ε' => 'E',
        'Ζ' => 'Z',
        'Η' => 'H',
        'Ι' => 'I',
        'Κ' => 'K',
        'Μ' => 'M',
        'Ν' => 'N',
        'Ο' => 'O',
        'Ρ' => 'P',
        'Τ' => 'T',
        'Υ' => 'Y',
        'Χ' => 'X',
        'α' => 'a',
        'ε' => 'e',
        'ι' => 'i',
        'κ' => 'k',
        'ν' => 'v',
        'ο' => 'o',
        'ρ' => 'p',
        'υ' => 'u',
        'χ' => 'x',
        'γ' => 'y',
        // Cherokee (the capital block U+13A0..U+13F5).
        'Ꭰ' => 'D',
        'Ꭱ' => 'R',
        'Ꭲ' => 'T',
        'Ꭵ' => 'i',
        'Ꭹ' => 'Y',
        'Ꭺ' => 'A',
        'Ꭻ' => 'J',
        'Ꭼ' => 'E',
        'Ꮃ' => 'W',
        'Ꮇ' => 'M',
        'Ꮋ' => 'H',
        'Ꮍ' => 'Y',
        'Ꮐ' => 'G',
        'Ꮒ' => 'h',
        'Ꮓ' => 'Z',
        'Ꮟ' => 'b',
        'Ꮥ' | 'Ꮪ' => 'S',
        'Ꮩ' => 'V',
        'Ꮮ' => 'L',
        'Ꮯ' => 'C',
        'Ꮲ' => 'P',
        'Ꮶ' => 'K',
        'Ᏼ' => 'B',
        // Armenian.
        'Օ' => 'O',
        'օ' => 'o',
        'ս' => 'u',
        'ո' => 'n',
        'հ' => 'h',
        'ց' => 'g',
        'զ' => 'q',
        _ => c,
    }
}

/// Lookalikes folded to Latin, then lowercased: the matching form for
/// word rules and templates.
pub fn fold(s: &str) -> String {
    s.chars().map(fold_char).collect::<String>().to_lowercase()
}

/// Scripts whose letters impersonate Latin ones.
fn confusable_script(s: Script) -> bool {
    matches!(
        s,
        Script::Cyrillic | Script::Greek | Script::Cherokee | Script::Armenian | Script::Coptic
    )
}

/// Words mixing Latin with a lookalike script (or Cyrillic with Greek)
/// inside one word. Japanese-with-Latin ("Nostrクライアント") and
/// whole-word switches ("hello мир") never count.
pub fn mixed_script_words(content: &str) -> usize {
    content
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| {
            let (mut latin, mut cyr, mut greek, mut other_conf) = (false, false, false, false);
            for c in word.chars().filter(|c| c.is_alphabetic()) {
                match c.script() {
                    Script::Latin => latin = true,
                    Script::Cyrillic => cyr = true,
                    Script::Greek => greek = true,
                    s if confusable_script(s) => other_conf = true,
                    _ => {}
                }
            }
            (latin && (cyr || greek || other_conf)) || (cyr && greek)
        })
        .count()
}

/// The lookalike signal for one note's content.
pub fn is_lookalike(content: &str) -> bool {
    mixed_script_words(content) >= MIXED_WORDS_TO_FLAG
}

// --- Templates ---------------------------------------------------------------

/// The template key: folded text, URLs reduced to their host, everything
/// but letters dropped. None for short messages.
pub fn template_key(content: &str) -> Option<u64> {
    let folded = fold(content);
    let mut parts: Vec<String> = Vec::new();
    let mut letters = 0usize;
    for tok in folded.split_whitespace() {
        if let Some(rest) = tok
            .strip_prefix("https://")
            .or_else(|| tok.strip_prefix("http://"))
        {
            let host: String = rest.split(['/', '?', '#']).next().unwrap_or("").to_string();
            parts.push(format!("<{host}>"));
            continue;
        }
        let word: String = tok.chars().filter(|c| c.is_ascii_lowercase()).collect();
        if !word.is_empty() {
            letters += word.len();
            parts.push(word);
        }
    }
    if letters < TEMPLATE_MIN_LETTERS {
        return None;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.join(" ").hash(&mut h);
    Some(h.finish())
}

#[derive(Default)]
struct Templates {
    authors: HashMap<u64, Vec<u64>>,
    order: VecDeque<u64>,
}

impl Templates {
    fn clear(&mut self) {
        self.authors.clear();
        self.order.clear();
    }
}

static TEMPLATES: LazyLock<Mutex<Templates>> = LazyLock::new(Default::default);

fn author_hash(pk: &PublicKey) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    pk.to_bytes().hash(&mut h);
    h.finish()
}

/// Is this author exempt from the spam signals? (You, follows, Trusted.)
fn spam_exempt(v: Verdict) -> bool {
    matches!(v, Verdict::SelfUser | Verdict::Follow | Verdict::Trusted)
}

/// Feed one network event to the template ledger (the pool chokepoint
/// and the live bell, beside dedup.rs's observe). Speech kinds only;
/// exempt authors never count.
pub fn observe(ev: &Event) {
    if !matches!(u16::from(ev.kind), 1 | 1111) {
        return;
    }
    if spam_exempt(wot::verdict(&ev.pubkey)) {
        return;
    }
    let Some(key) = template_key(&ev.content) else {
        return;
    };
    record_template(key, author_hash(&ev.pubkey));
}

fn record_template(key: u64, author: u64) {
    let mut t = TEMPLATES.lock();
    if !t.authors.contains_key(&key) {
        while t.authors.len() >= TEMPLATE_CAP {
            match t.order.pop_front() {
                Some(old) => {
                    t.authors.remove(&old);
                }
                None => break,
            }
        }
        t.order.push_back(key);
    }
    let list = t.authors.entry(key).or_default();
    let was_flagged = list.len() >= TEMPLATE_AUTHORS_TO_FLAG;
    if !list.contains(&author) && list.len() < TEMPLATE_AUTHORS_KEPT {
        list.push(author);
    }
    let now_flagged = list.len() >= TEMPLATE_AUTHORS_TO_FLAG;
    drop(t);
    if now_flagged && !was_flagged {
        bump(); // cached verdicts for this template must re-judge
    }
}

fn template_flagged(key: u64) -> bool {
    TEMPLATES
        .lock()
        .authors
        .get(&key)
        .is_some_and(|a| a.len() >= TEMPLATE_AUTHORS_TO_FLAG)
}

// --- The verdict ---------------------------------------------------------------

#[derive(Clone)]
struct Judged {
    epoch: u64,
    /// A user rule's verdict (content-only).
    rule: Option<Reason>,
    lookalike: bool,
    template: Option<u64>,
}

static CACHE: LazyLock<Mutex<HashMap<EventId, Judged>>> = LazyLock::new(Default::default);

fn contains_word(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, m)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + m.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

fn rule_match(ev: &Event, folded: &str, r: &Rules) -> Option<Reason> {
    for (word, needle) in &r.words {
        let single = needle.chars().all(char::is_alphanumeric);
        let hit = if single {
            contains_word(folded, needle)
        } else {
            folded.contains(needle.as_str())
        };
        if hit {
            return Some(Reason::Word(word.clone()));
        }
    }
    if !r.hashtags.is_empty() {
        let mut tags: Vec<String> = ev
            .tags
            .iter()
            .filter(|t| t.as_slice().first().map(String::as_str) == Some("t"))
            .filter_map(|t| t.as_slice().get(1))
            .map(|t| normalize_hashtag(t))
            .collect();
        tags.extend(
            folded
                .split_whitespace()
                .filter_map(|w| w.strip_prefix('#'))
                .map(|w| {
                    w.trim_end_matches(|c: char| !c.is_alphanumeric())
                        .to_string()
                }),
        );
        if let Some(t) = r.hashtags.iter().find(|t| tags.contains(t)) {
            return Some(Reason::Hashtag(t.clone()));
        }
    }
    if let Some(set) = &r.set {
        let hit = set
            .matches(&ev.content)
            .iter()
            .next()
            .or_else(|| set.matches(folded).iter().next());
        if let Some(i) = hit {
            return Some(Reason::Pattern(r.patterns[i].clone()));
        }
    }
    None
}

fn judge_content(ev: &Event, epoch: u64) -> Judged {
    let folded = fold(&ev.content);
    let rule = {
        let r = RULES.lock();
        rule_match(ev, &folded, &r)
    };
    Judged {
        epoch,
        rule,
        lookalike: is_lookalike(&ev.content),
        template: template_key(&ev.content),
    }
}

/// The one question (D110): should this note be held, and why? None for
/// your own notes; user rules for everyone else; the spam signals only
/// for authors outside your follows and the qualified network. Cheap on
/// repeat: content work is cached per event id until a rule changes.
pub fn judge(ev: &Event) -> Option<Reason> {
    let verdict = wot::verdict(&ev.pubkey);
    if verdict == Verdict::SelfUser {
        return None;
    }
    if is_muted(&ev.pubkey) {
        return Some(Reason::Muted);
    }
    refresh_prefs(false);
    let epoch = GEN.load(Ordering::Relaxed);
    let cached = CACHE
        .lock()
        .get(&ev.id)
        .filter(|j| j.epoch == epoch)
        .cloned();
    let j = match cached {
        Some(j) => j,
        None => {
            let j = judge_content(ev, epoch);
            let mut c = CACHE.lock();
            if c.len() >= JUDGE_CACHE_CAP {
                c.clear();
            }
            c.insert(ev.id, j.clone());
            j
        }
    };
    if let Some(r) = j.rule {
        return Some(r);
    }
    let spam_on = RULES.lock().spam_on;
    if spam_on && !spam_exempt(verdict) {
        if j.lookalike {
            return Some(Reason::Lookalike);
        }
        if j.template.is_some_and(template_flagged) {
            return Some(Reason::Template);
        }
    }
    None
}

/// Is this reply held (veiled or collapsed) when its thread opens? The
/// one spelling of the thread's rules (D112/D114, deduplicated in the
/// review): the thread's ordering and the reply-count bubble both ask
/// it, so the count and the open thread always agree. Your own replies
/// never; muted and dedup-caught authors and any [`judge`] match always;
/// an author outside your network while `collapse_on` (the
/// `thread_collapse` pref, read once per batch by the caller).
pub fn reply_held(ev: &Event, collapse_on: bool) -> bool {
    let verdict = wot::verdict(&ev.pubkey);
    if verdict == Verdict::SelfUser {
        return false;
    }
    if crate::dedup::is_hidden(&ev.pubkey.to_string()) || judge(ev).is_some() {
        return true;
    }
    collapse_on && verdict == Verdict::NotInNetwork
}

/// The `thread_collapse` pref (default ON), for [`reply_held`] callers.
pub fn thread_collapse_on() -> bool {
    crate::config::prefs()
        .and_then(|v| v.get("thread_collapse").and_then(|b| b.as_bool()))
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Nov 27 2026 bot, verbatim (Greek/Cyrillic/Cherokee lookalikes).
    const SPAM: &str = "⚡️ ReΑl-time Solana Airdrop: CheСk Υour wallet! \
        https://ai.solhub.cc/?ref=9m8y19 \n🔗 Verify Ꭹour wallet & get Free ᏚOL now!";

    #[test]
    fn lookalike_flags_the_bot() {
        assert!(mixed_script_words(SPAM) >= 4);
        assert!(is_lookalike(SPAM));
    }

    #[test]
    fn lookalike_spares_real_text() {
        for ok in [
            "Plain English with a café and naïve résumé, nothing odd here.",
            "Nostrクライアントを作りました。 Satori is great",
            "Привет мир, hello world, это нормально",
            "Γειά σου κόσμε and hello again",
            "Math: α + β = γ, and the µs timing looks fine.",
        ] {
            assert!(!is_lookalike(ok), "{ok}");
        }
    }

    #[test]
    fn fold_maps_lookalikes() {
        assert_eq!(fold("ClaiᎷ yՕur frЕe SΟL"), "claim your free sol");
    }

    #[test]
    fn template_key_ignores_ref_codes_and_lookalikes() {
        let a = "🎁 Unbox your Solana Gift: Free SOL inside! https://ai.solhub.cc/?ref=q4boav \
                 💡 Limited slots available. Hurry up!";
        let b = "🎁 UnboХ Ꭹour SolᎪna Gift: Free SOL inside! https://ai.solhub.cc/?ref=rwc759 \
                 💡 Limited slots ΑvailΑВlᎬ. HurrΥ up!";
        assert_eq!(template_key(a), template_key(b));
        assert!(template_key(a).is_some());
        assert_eq!(template_key("gm nostr"), None);
    }

    #[test]
    fn template_flags_at_three_authors() {
        let key =
            template_key("this is a long enough message to be a template for sure ok").unwrap();
        record_template(key, 1);
        record_template(key, 1);
        record_template(key, 2);
        assert!(!template_flagged(key));
        record_template(key, 3);
        assert!(template_flagged(key));
    }

    #[test]
    fn word_boundaries() {
        assert!(contains_word("free sol now", "sol"));
        assert!(!contains_word("the solution", "sol"));
        assert!(contains_word("sol!", "sol"));
    }

    #[test]
    fn pattern_validation() {
        assert!(validate_pattern(r"air\s*drop").is_ok());
        assert!(validate_pattern("(unclosed").is_err());
        assert!(validate_pattern("").is_err());
        assert!(validate_pattern(&"a".repeat(MAX_PATTERN_LEN + 1)).is_err());
    }

    #[test]
    fn user_label_detection() {
        assert!(is_user_rule_label(&Reason::Word("x".into()).label()));
        assert!(!is_user_rule_label(&Reason::Lookalike.label()));
    }
}
