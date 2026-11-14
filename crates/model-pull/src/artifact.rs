//! `mtp-artifact.cjs` `check(repo, files)`: whether the selected GGUF files of a public Hugging
//! Face repository hold MTP/NextN tensors, read from each file's tensor directory (a bounded byte
//! prefix), never from the weights.
//!
//! The steps, each a host fetch with core's URL, header and deadline:
//!
//! 1. `GET https://huggingface.co/api/models/<repo>` (10 s, no headers, redirects followed): the
//!    repository's immutable revision (`sha`, 40 to 64 lowercase hex digits), the reply read up to
//!    4 MiB.
//! 2. For each file in turn: `GET https://huggingface.co/<repo>/resolve/<revision>/<file>` (each
//!    path segment `encodeURIComponent`-encoded) with `Range: bytes=0-16777215` (20 s, redirects
//!    followed: Hugging Face answers a resolve with a redirect to its file CDN, which the host's
//!    egress policy must allow, the `Range` header kept), the body read up to 16 MiB, then
//!    [`crate::gguf::inspect`].
//!
//! The answer is `present`, `absent` or `unknown` with core's reasons; a definite one is cached
//! per `(repo, files)` for five minutes (100 entries, the oldest dropped first), and at most two
//! checks run at once ([`Artifacts`]). Every failure is an `unknown` answer, never an error.

use crate::gguf;
use crate::js;
use js_json::{wtf16, JValue};

/// How long a definite answer is reused.
pub const CACHE_MS: f64 = 300_000.0;
/// How many answers are kept.
pub const CACHE_ENTRIES: usize = 100;
/// How many checks may run at once.
pub const RUNNING: usize = 2;
/// The revision reply's cap (`readCappedJson(response, 4 MiB)`).
pub const META_CAP: usize = 4 * 1024 * 1024;
pub const META_TIMEOUT_MS: u32 = 10_000;
pub const PREFIX_TIMEOUT_MS: u32 = 20_000;
/// The `Range` header of every prefix request.
pub const RANGE: &str = "bytes=0-16777215";

pub const INVALID_REPO: &str = "Invalid public Hugging Face repository.";
pub const INVALID_FILES: &str = "Inspection supports up to four selected GGUF shards.";
pub const BUSY: &str = "Two artifact checks are already running. Try again shortly.";
pub const REVISION_UNAVAILABLE: &str = "Public repository revision unavailable.";
pub const IMMUTABLE_UNAVAILABLE: &str = "Immutable repository revision unavailable.";
pub const TIMED_OUT: &str = "Artifact check timed out.";
pub const BYTE_RANGE: &str = "Unexpected artifact byte range.";
pub const INCOMPLETE: &str = "The selected variant does not enumerate every GGUF shard.";
pub const PRESENT: &str = "MTP/NextN tensors are present in the selected files. Backend compatibility and a working head still require a load test.";
pub const INCONSISTENT: &str =
    "Metadata declares MTP layers, but no recognized MTP tensors were found.";
pub const ABSENT: &str = "No recognized MTP/NextN tensors in the complete selected tensor directories. Separate head files are not included in this check.";
/// `readCappedJson`'s error for a reply over the cap.
pub const META_TOO_LARGE: &str = "reply exceeded the 4096 KB limit";
/// `JSON.parse('')` (an empty or white-space reply).
pub const END_OF_INPUT: &str = "Unexpected end of JSON input";
/// `meta.sha` of a `null` reply.
pub const NULL_META: &str = "Cannot read properties of null (reading 'sha')";
/// Named difference `meta-not-json`: a revision reply that is not JSON. Node's reason is V8's
/// `SyntaxError` text (`Unexpected token 'x', "…" is not valid JSON` and its many variants),
/// which is not reproduced; the port says this.
pub const META_NOT_JSON: &str = "Public repository metadata is not valid JSON.";

/// A fetch failure, as `check` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The deadline passed (Node: a `TimeoutError` or `AbortError`).
    Timeout,
    /// Anything else, with the error's message (undici: `fetch failed` for a request that did
    /// not get an answer, `terminated` for a body cut off).
    Failed(String),
}

impl FetchError {
    fn reason(&self) -> String {
        match self {
            FetchError::Timeout => TIMED_OUT.to_string(),
            FetchError::Failed(m) => m.clone(),
        }
    }
}

/// One public fetch (the global `fetch`: redirects followed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetch {
    pub url: String,
    /// The `Range` header (the prefix requests only).
    pub range: Option<&'static str>,
    /// The deadline over the request and the body read.
    pub timeout_ms: u32,
    /// Read at most this many body bytes, then cancel the rest.
    pub max_bytes: usize,
}

/// A fetch's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub status: u16,
    /// The `Content-Range` header.
    pub content_range: Option<String>,
    /// The body, at most `max_bytes` of it; `Err` when reading failed before that.
    pub body: Result<Vec<u8>, FetchError>,
}

/// What the check needs of the deployment.
pub trait Fetcher {
    fn fetch(&mut self, request: &Fetch) -> Result<Fetched, FetchError>;
    /// `Date.now()`.
    fn now_ms(&self) -> i64;
}

/// The cache key: `JSON.stringify([repo, files])` of a valid selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    pub repo: String,
    pub files: Vec<String>,
}

fn unknown(reason: &str, files: &JValue, now: i64) -> JValue {
    JValue::obj([
        ("status", JValue::from("unknown")),
        ("reason", JValue::from(reason)),
        ("files", files.clone()),
        ("checkedAt", JValue::from(js::iso_string(now))),
    ])
}

/// `validate(repo, files)`: the selection as a key, or the reason it is refused.
pub fn validate(repo: &str, files: &JValue) -> Result<Key, &'static str> {
    if !js::is_repo(repo) || repo.split('/').any(|p| p == "." || p == "..") {
        return Err(INVALID_REPO);
    }
    let JValue::Arr(items) = files else {
        return Err(INVALID_FILES);
    };
    let mut names = Vec::with_capacity(items.len());
    for f in items {
        let Some(f) = f.as_str() else {
            return Err(INVALID_FILES);
        };
        let bad_segment = f.split('/').any(|p| p.is_empty() || p == "." || p == "..");
        let bad_char = f.chars().any(|c| matches!(c, '?' | '#' | '\\') || c < ' ');
        if !f.ends_with(".gguf")
            || f.starts_with('/')
            || bad_segment
            || bad_char
            || names.contains(&f)
        {
            return Err(INVALID_FILES);
        }
        names.push(f);
    }
    if names.is_empty() || names.len() > 4 {
        return Err(INVALID_FILES);
    }
    Ok(Key {
        repo: repo.to_string(),
        files: names.into_iter().map(str::to_string).collect(),
    })
}

/// What [`Artifacts::start`] decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Start {
    /// The answer, without a fetch (a refused selection, a cached answer, too many checks).
    Done(JValue),
    /// The same check is running: its answer is this one's too (core shares the promise).
    Join(Key),
    /// Run [`run`] for this key, then [`Artifacts::finish`].
    Run(Key),
}

/// A finished run.
#[derive(Debug, Clone, PartialEq)]
pub struct Ran {
    pub answer: JValue,
    /// A definite answer, which is cached.
    pub definite: bool,
}

/// `mtp-artifact.cjs`'s module state: the answers of the last five minutes and the running
/// checks. One per process, as core's.
#[derive(Debug, Clone, Default)]
pub struct Artifacts {
    /// In insertion order: `(key, time, answer)`.
    cache: Vec<(Key, f64, JValue)>,
    running: Vec<Key>,
}

impl Artifacts {
    pub fn new() -> Artifacts {
        Artifacts::default()
    }

    /// The checks running now.
    pub fn running(&self) -> &[Key] {
        &self.running
    }

    /// `check`'s synchronous part, at `now`.
    pub fn start(&mut self, repo: &str, files: &JValue, now: i64) -> Start {
        let key = match validate(repo, files) {
            Ok(key) => key,
            Err(reason) => return Start::Done(unknown(reason, files, now)),
        };
        if let Some((_, time, answer)) = self.cache.iter().find(|(k, _, _)| *k == key) {
            if now as f64 - time < CACHE_MS {
                return Start::Done(answer.clone());
            }
        }
        if self.running.contains(&key) {
            return Start::Join(key);
        }
        if self.running.len() >= RUNNING {
            return Start::Done(unknown(BUSY, files, now));
        }
        self.running.push(key.clone());
        Start::Run(key)
    }

    /// The end of a run started here, at `now`: a definite answer is cached (when 100 are kept,
    /// the oldest is dropped first, even when this key's own expired entry is the one refreshed,
    /// which keeps its place).
    pub fn finish(&mut self, key: &Key, ran: Ran, now: i64) -> JValue {
        self.running.retain(|k| k != key);
        if ran.definite {
            if self.cache.len() >= CACHE_ENTRIES && !self.cache.is_empty() {
                self.cache.remove(0);
            }
            match self.cache.iter_mut().find(|(k, _, _)| k == key) {
                Some(slot) => {
                    slot.1 = now as f64;
                    slot.2 = ran.answer.clone();
                }
                None => self
                    .cache
                    .push((key.clone(), now as f64, ran.answer.clone())),
            }
        }
        ran.answer
    }

    /// `check(repo, files)` start to end over `host`. `None`: the same check is running elsewhere
    /// ([`Start::Join`]), whose answer is this one's.
    pub fn check(&mut self, repo: &str, files: &JValue, host: &mut impl Fetcher) -> Option<JValue> {
        match self.start(repo, files, host.now_ms()) {
            Start::Done(answer) => Some(answer),
            Start::Join(_) => None,
            Start::Run(key) => {
                let ran = run(&key, host);
                Some(self.finish(&key, ran, host.now_ms()))
            }
        }
    }
}

/// `readCappedJson` of the revision reply: the value, or the error's message.
fn read_meta(fetched: Fetched) -> Result<JValue, String> {
    // A 204 or 205 reply has no body: its text is ''.
    let bytes = if matches!(fetched.status, 204 | 205) {
        Vec::new()
    } else {
        fetched.body.map_err(|e| e.reason())?
    };
    if bytes.len() > META_CAP {
        return Err(META_TOO_LARGE.to_string());
    }
    let text = String::from_utf8_lossy(&bytes);
    match wtf16::parse(&text) {
        Ok(v) => Ok(v),
        Err(_) if text.trim_matches([' ', '\t', '\n', '\r']).is_empty() => {
            Err(END_OF_INPUT.to_string())
        }
        // Named difference `meta-unsupported`: JSON Node reads but the port cannot hold (a number
        // outside the f64 range, nesting deeper than 128, text in U+10F800..U+10FFFF) is refused
        // as text that is not JSON.
        Err(_) => Err(META_NOT_JSON.to_string()),
    }
}

/// `prefix(url)`: at most [`gguf::LIMIT`] bytes of the file.
fn prefix(url: String, host: &mut impl Fetcher) -> Result<Vec<u8>, String> {
    let fetched = host
        .fetch(&Fetch {
            url,
            range: Some(RANGE),
            timeout_ms: PREFIX_TIMEOUT_MS,
            max_bytes: gguf::LIMIT,
        })
        .map_err(|e| e.reason())?;
    let ok = (200..300).contains(&fetched.status);
    if !ok || matches!(fetched.status, 204 | 205) {
        return Err(format!(
            "Artifact metadata unavailable ({}).",
            fetched.status
        ));
    }
    if fetched.status == 206
        && !js::starts_with_ci(
            fetched.content_range.as_deref().unwrap_or("").as_bytes(),
            "bytes 0-",
        )
    {
        return Err(BYTE_RANGE.to_string());
    }
    let mut bytes = fetched.body.map_err(|e| e.reason())?;
    bytes.truncate(gguf::LIMIT);
    Ok(bytes)
}

fn checked(key: &Key, host: &mut impl Fetcher) -> Result<JValue, String> {
    let meta = host
        .fetch(&Fetch {
            url: format!("https://huggingface.co/api/models/{}", key.repo),
            range: None,
            timeout_ms: META_TIMEOUT_MS,
            max_bytes: META_CAP + 1,
        })
        .map_err(|e| e.reason())?;
    if !(200..300).contains(&meta.status) {
        return Err(REVISION_UNAVAILABLE.to_string());
    }
    let meta = read_meta(meta)?;
    if meta == JValue::Null {
        return Err(NULL_META.to_string());
    }
    let revision = meta.get("sha").clone();
    // `/^[a-f0-9]{40,64}$/.test(revision || '')`: the test reads `String(revision)`.
    let text = if revision.truthy() {
        js::to_string(&revision).ok_or_else(|| js::NO_PRIMITIVE.to_string())?
    } else {
        String::new()
    };
    let hex = (40..=64).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !hex {
        return Err(IMMUTABLE_UNAVAILABLE.to_string());
    }
    let mut evidence = Vec::with_capacity(key.files.len());
    let mut found = false;
    let mut declared = false;
    let mut incomplete = false;
    let shards = key.files.len() as f64;
    for file in &key.files {
        let mut path = Vec::new();
        for segment in file.split('/') {
            path.push(
                js::encode_uri_component(segment).ok_or_else(|| js::URI_MALFORMED.to_string())?,
            );
        }
        let url = format!(
            "https://huggingface.co/{}/resolve/{text}/{}",
            key.repo,
            path.join("/")
        );
        let inspection = gguf::inspect(&prefix(url, host)?).map_err(str::to_string)?;
        incomplete |= js::greater_than(&inspection.split_count, shards);
        found |= !inspection.tensors.is_empty();
        declared |= js::greater_than(&inspection.declared_layers, 0.0);
        evidence.push(inspection.evidence(file));
    }
    if incomplete {
        return Err(INCOMPLETE.to_string());
    }
    let inconsistent = !found && declared;
    let (status, reason) = if found {
        ("present", PRESENT)
    } else if inconsistent {
        ("unknown", INCONSISTENT)
    } else {
        ("absent", ABSENT)
    };
    Ok(JValue::obj([
        ("status", JValue::from(status)),
        ("reason", JValue::from(reason)),
        (
            "files",
            JValue::Arr(key.files.iter().map(|f| JValue::from(f.as_str())).collect()),
        ),
        ("revision", revision),
        ("evidence", JValue::Arr(evidence)),
        ("checkedAt", JValue::from(js::iso_string(host.now_ms()))),
    ]))
}

/// One check's fetches and reading: a definite answer, or `unknown` with the failure's reason.
pub fn run(key: &Key, host: &mut impl Fetcher) -> Ran {
    match checked(key, host) {
        Ok(answer) => Ran {
            answer,
            definite: true,
        },
        Err(reason) => {
            let files = JValue::Arr(key.files.iter().map(|f| JValue::from(f.as_str())).collect());
            Ran {
                answer: unknown(&reason, &files, host.now_ms()),
                definite: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selections_are_validated_as_core_does() {
        let files = |v: &[&str]| JValue::Arr(v.iter().map(|f| JValue::from(*f)).collect());
        assert!(validate("qa/x", &files(&["a.gguf"])).is_ok());
        assert_eq!(validate("qa/..", &files(&["a.gguf"])), Err(INVALID_REPO));
        for bad in [
            files(&[]),
            files(&["a.gguf", "a.gguf"]),
            files(&["a.bin"]),
            files(&["d//a.gguf"]),
            files(&["a?.gguf"]),
            files(&["1.gguf", "2.gguf", "3.gguf", "4.gguf", "5.gguf"]),
            JValue::from("a.gguf"),
        ] {
            assert_eq!(validate("qa/x", &bad), Err(INVALID_FILES));
        }
    }
}
