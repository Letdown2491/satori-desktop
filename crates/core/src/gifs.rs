//! GIF search — gifs.nostr.build (nostr.build's GIF service), 0.12.0's
//! composer GIF button (D104). Search / browse / suggest over the JSON
//! API; the picker (app side) renders tiles from `previews` and a pick
//! publishes the ORIGINAL `url` with a NIP-92 imeta — never re-hosted,
//! never a preview URL (the integration guide's §9).
//!
//! TODO(port): no TS origin — post-retirement feature, ported from the
//! service's own integration guide (gifs.nostr.build/developers/llms-full.txt).
//!
//! Auth (the guide's §1): native code identifies with
//! `Authorization: Bearer gnb_…`. Keys ship in native apps "not secret
//! in any strong sense" — the service expects and rate-limits this
//! (1000 req/min per client; a debounced picker stays far below it).
//!
//! Attribution (§2) is a condition of use: the picker header shows
//! "GIFs from nostr.build" — the app side renders it (ATTRIBUTION here).
//!
//! `safe` stays at its default (1): adult content is excluded, and
//! `safe=0` is refused outright (403). There is no adult mode.

use serde::Deserialize;

/// The registered client's API key, injected at build time (build.rs:
/// `gifs_key.txt` locally, the `GIFS_API_KEY` repo secret in CI — the
/// public source never shows it). Native identification per the
/// integration guide §1 (no Origin exists for a desktop app). Empty
/// when unbuilt: `configured()` gates the button.
const API_KEY: &str = env!("GIFS_API_KEY");

/// Is the GIF feature usable? (build.rs found a key.)
pub fn configured() -> bool {
    !API_KEY.is_empty()
}

/// The attribution line the picker header must show, and its target
/// (the guide's §2: visible, legible, a real link).
pub const ATTRIBUTION: &str = "GIFs from nostr.build";
pub const ATTRIBUTION_URL: &str = "https://nostr.build";

const API_BASE: &str = "https://gifs.nostr.build/api/v1";

/// The starter chips (the guide's §5: no trending endpoint exists; a
/// picker opens on everyday searches, the first one runs on open).
pub const STARTERS: [&str; 8] = [
    "gm", "lol", "yes", "wow", "applause", "facepalm", "love", "fire",
];

/// One GIF (the guide's §4 Gif object — the fields Satori uses; the
/// service may add more, serde ignores them).
#[derive(Debug, Clone, Deserialize)]
pub struct Gif {
    pub id: String,
    /// The original — the ONLY url a note may carry (§9).
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub format: String,
    pub title: String,
    /// null in the wild (§4); the imeta `size` entry is omitted then.
    pub bytes: Option<u64>,
    /// H.264 of the animation at source size. Its URL KEEPS the .gif
    /// extension on purpose; `.mp4` appended is served identically —
    /// which is what Satori's extension-judging video gate needs.
    pub mp4: String,
    /// The first frame as PNG — the grid tile's face.
    pub previews: Previews,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Previews {
    pub small: Preview,
    pub medium: Preview,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Preview {
    pub width: u32,
    pub height: u32,
    /// Animated WebP (despite any .gif-ish URL) — unused in Satori's
    /// native grid; the still is the tile, the mp4 is the motion.
    pub animated: String,
    pub still: String,
}

/// One search page. `count` is the RANKED LIST's length (≤ 200), not
/// the match total — paging walks `offset` until a short page.
#[derive(Debug, Clone, Deserialize)]
pub struct GifPage {
    pub q: Option<String>,
    pub count: usize,
    pub offset: usize,
    pub limit: usize,
    pub items: Vec<Gif>,
}

/// Suggest terms (the combobox under the search field).
#[derive(Debug, Clone, Deserialize)]
pub struct SuggestTerm {
    pub term: String,
    /// tag | character | title | text (unused for now).
    #[allow(dead_code)]
    pub kind: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SuggestAnswer {
    terms: Vec<SuggestTerm>,
}

/// What went wrong, in the service's own codes (§6) — the picker maps
/// each to its line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GifError {
    /// 400: the query normalised to nothing (punctuation-only input).
    Validation,
    /// 403: the key is gone or wrong. Don't retry.
    NotRegistered,
    /// 429: over the limit; Retry-After is 60 (native code may read
    /// the header, but the minute is the whole rule).
    RateLimited,
    /// 503 / network: "search is unavailable right now", retry with
    /// backoff.
    Unavailable,
}

impl std::fmt::Display for GifError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GifError::Validation => write!(f, "nothing to search for"),
            GifError::NotRegistered => write!(f, "the GIF service refused this client's key"),
            GifError::RateLimited => write!(f, "too many searches, wait a minute"),
            GifError::Unavailable => write!(f, "search is unavailable right now"),
        }
    }
}

fn http() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            // The guide's §6: ~15s request timeout.
            .timeout(std::time::Duration::from_secs(15))
            // Same authority as upload.rs's client: every dial re-walks
            // SafeResolver, and redirect hops stay on safe public hosts.
            .dns_resolver(std::sync::Arc::new(crate::net_guard::SafeResolver))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() > 4
                    || !crate::media::is_safe_media_url(attempt.url().as_str())
                {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .expect("gifs client")
    })
}

/// The error-body shape: `{ "error": { "code", "message" } }`.
#[derive(Debug, Deserialize)]
struct ErrorBody {
    error: ApiError,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    code: String,
}

fn classify(status: reqwest::StatusCode) -> GifError {
    match status.as_u16() {
        400 => GifError::Validation,
        403 => GifError::NotRegistered,
        429 => GifError::RateLimited,
        _ => GifError::Unavailable,
    }
}

/// A GET that returns a JSON body, with the service's error codes
/// classified (§6). Bodies are read as text and parsed with serde_json
/// (the workspace reqwest runs feature-light; upload.rs's pattern).
async fn get_json<T: serde::de::DeserializeOwned>(
    path: &str,
    query: &[(&str, &str)],
) -> Result<T, GifError> {
    let res = get(path, query).await?;
    let body = res.text().await.map_err(|_| GifError::Unavailable)?;
    serde_json::from_str(&body).map_err(|_| GifError::Unavailable)
}

async fn get(path: &str, query: &[(&str, &str)]) -> Result<reqwest::Response, GifError> {
    let res = http()
        .get(format!("{API_BASE}{path}"))
        .bearer_auth(API_KEY)
        .query(query)
        .send()
        .await
        .map_err(|_| GifError::Unavailable)?;
    // A 5xx or a registry hiccup answers here; the body's code only
    // refines 4xx cases (the codes the picker words differently).
    if !res.status().is_success() {
        return Err(classify(res.status()));
    }
    Ok(res)
}

/// Search (the guide's §3): words, emoji, or both. `limit` ≤ 200.
pub async fn search(q: &str, limit: usize, offset: usize) -> Result<GifPage, GifError> {
    get_json(
        "/search",
        &[
            ("q", q),
            ("limit", &limit.to_string()),
            ("offset", &offset.to_string()),
        ],
    )
    .await
}

/// Browse one kind without a query (today's order, reshuffled daily).
pub async fn browse(kind: &str, limit: usize, offset: usize) -> Result<GifPage, GifError> {
    get_json(
        "/gifs",
        &[
            ("kind", kind),
            ("limit", &limit.to_string()),
            ("offset", &offset.to_string()),
        ],
    )
    .await
}

/// Autocomplete terms for the combobox.
pub async fn suggest(q: &str) -> Result<Vec<SuggestTerm>, GifError> {
    get_json("/suggest", &[("q", q)]).await
}

/// The mp4 url a video gate can admit: the service serves the mp4 at
/// BOTH `…/mp4/orig/<id>.gif` and `…/mp4/orig/<id>.gif.mp4` (§4) — the
/// .mp4 form is the same bytes with an extension Satori's
/// extension-judging video path accepts.
pub fn mp4_for_grid(gif: &Gif) -> String {
    if gif.mp4.ends_with(".mp4") {
        gif.mp4.clone()
    } else {
        format!("{}.mp4", gif.mp4)
    }
}

/// The deterministic original → mp4 transform (§4), for URLs seen in
/// the WILD (a note's imeta or a standalone link) where no Gif object
/// rides along. `None` for anything that isn't a gifs.nostr.build
/// original — other hosts' GIFs have no mp4 twin and stay stills.
pub fn mp4_from_original(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://gifs.nostr.build/")?;
    // An original is ONE path segment (the id), extension .gif or .webp
    // (§4). Anything with more path, a query, or a fragment is some
    // other page of the service — not a postable original.
    if rest.is_empty()
        || rest.contains('/')
        || rest.contains('?')
        || rest.contains('#')
        || !(rest.ends_with(".gif") || rest.ends_with(".webp"))
    {
        return None;
    }
    Some(format!("https://gifs.nostr.build/mp4/orig/{rest}.mp4"))
}

/// The NIP-92 imeta a pick publishes (§9): the original's url, the mime
/// from `format`, the dim, the size when known, the alt when titled.
/// The composer's Attachment carries this verbatim — publish, synced
/// draft and the render side all read the one tag.
pub fn imeta_for(gif: &Gif) -> Vec<String> {
    let mut tag = vec![
        format!("url {}", gif.url),
        format!(
            "m image/{}",
            if gif.format == "webp" { "webp" } else { "gif" }
        ),
        format!("dim {}x{}", gif.width, gif.height),
    ];
    if let Some(bytes) = gif.bytes {
        tag.push(format!("size {bytes}"));
    }
    if !gif.title.is_empty() {
        tag.push(format!("alt {}", gif.title));
    }
    tag
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "id": "20099e4e59051.gif",
        "url": "https://gifs.nostr.build/20099e4e59051.gif",
        "width": 224, "height": 126,
        "frames": 37, "duration": 3.7, "bytes": 806309,
        "format": "gif",
        "title": "Giant wave curling with an eye",
        "tags": ["wave", "ocean", "sea"],
        "lqip": null,
        "mp4": "https://gifs.nostr.build/mp4/orig/20099e4e59051.gif",
        "transparency": 0,
        "webm": null, "hevc": null,
        "previews": {
            "small":  { "width": 171, "height": 96,
                        "animated": "…/anim/96/…", "still": "…/still/96/…" },
            "medium": { "width": 224, "height": 126,
                        "animated": "…/anim/192/…", "still": "…/still/192/…" },
            "w240":   { "width": 224, "height": 126,
                        "animated": "…/anim/w240/…", "still": "…/still/w240/…" },
            "w480":   { "width": 224, "height": 126,
                        "animated": "…/anim/w480/…", "still": "…/still/w480/…" }
        }
    }"#;

    #[test]
    fn gif_parses_and_the_pick_imeta_carries_the_original() {
        let gif: Gif = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(gif.width, 224);
        assert_eq!(gif.previews.small.height, 96);

        let imeta = imeta_for(&gif);
        assert_eq!(imeta[0], "url https://gifs.nostr.build/20099e4e59051.gif");
        assert_eq!(imeta[1], "m image/gif");
        assert_eq!(imeta[2], "dim 224x126");
        assert_eq!(imeta[3], "size 806309");
        assert_eq!(imeta[4], "alt Giant wave curling with an eye");

        // The grid/preview mp4 gets the .mp4 suffix the video gate
        // needs; the POSTED url is the original, untouched.
        assert_eq!(
            mp4_for_grid(&gif),
            "https://gifs.nostr.build/mp4/orig/20099e4e59051.gif.mp4"
        );
        assert!(imeta[0].ends_with(".gif"));
    }

    #[test]
    fn the_wild_transform_recognizes_only_originals() {
        assert_eq!(
            mp4_from_original("https://gifs.nostr.build/20099e4e59051.gif").as_deref(),
            Some("https://gifs.nostr.build/mp4/orig/20099e4e59051.gif.mp4")
        );
        // The mp4 namespace itself is NOT an original (no double wrap).
        assert_eq!(
            mp4_from_original("https://gifs.nostr.build/mp4/orig/20099e4e59051.gif"),
            None
        );
        // Other hosts, deep paths, queries: not ours to transform.
        assert_eq!(mp4_from_original("https://blossom.example/x.gif"), None);
        assert_eq!(
            mp4_from_original("https://gifs.nostr.build/anim/96/x.gif"),
            None
        );
        assert_eq!(
            mp4_from_original("https://gifs.nostr.build/x.gif?size=big"),
            None
        );
    }

    #[test]
    fn an_untitled_webp_omits_size_and_alt() {
        let mut gif: Gif = serde_json::from_str(SAMPLE).unwrap();
        gif.format = "webp".into();
        gif.bytes = None;
        gif.title = String::new();
        let imeta = imeta_for(&gif);
        assert_eq!(imeta[1], "m image/webp");
        assert_eq!(imeta.len(), 3, "no size (null bytes), no alt (empty title)");
    }
}
