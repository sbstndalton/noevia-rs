//! The optional reranker of project search (`rag.cjs` `initRerank`, `rerankPool`; `decision/index.cjs`
//! `decide` for the one-backend chain `rag.rerank`; `decision/backends.cjs` `llamaRerankBackend`,
//! `teiRerankBackend`, `order`), checked against outputs recorded from Node
//! (`tests/generate_rerank_fixtures.cjs`). The caller sends the request and races its deadline.
//!
//! The decision layer's request and result checks (`invalidRequest`, `invalidResult`, the `decision`
//! crate) cannot fail here: the request [`request`] builds is always a valid `rank` request (a
//! purpose, a fallback, a deadline of at least 100 ms, at least two items), and both backends answer
//! a score for every offered id and a ranking of exactly those ids, or fail (see [`parse`]).
//!
//! Named differences: a reranker answer whose JSON holds a lone surrogate escape (`"\ud800"`), which
//! `JSON.parse` takes, is unreadable here (a fallback to the cosine order); the request is cut at the
//! deadline (Node's llama.cpp backend lets it run on, ignoring its answer); and the bench (three
//! deadline misses in a row: 60 s without the reranker) is this process's, beside Node's own.

use std::collections::HashMap;

use prompt_framing::js::units;
use serde_json::Value;

use crate::hits::Hit;
use crate::policy::{rerank_target, Backend};
use crate::search::TOP_K;

/// `RERANK_POOL_MAX` (`rag.cjs:114`).
pub const POOL_MAX: usize = 24;
/// `maxDocChars` of both backends, in UTF-16 code units.
pub const MAX_DOC_UNITS: usize = 4000;
/// The answer cap of both backends (`readCappedJson(r, 4 MiB)`).
pub const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;
/// Deadline misses in a row that bench the reranker, and for how long (`decision/index.cjs`).
pub const BENCH_AFTER: u32 = 3;
pub const BENCH_MS: f64 = 60_000.0;

/// The reranker as `initRerank` configures it, or `None` when it is off.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub backend: Backend,
    pub base_url: String,
    pub model: Option<String>,
    pub api_key: Option<String>,
    /// How many cosine hits are reranked (`RAG_RERANK_POOL`, 12, clamped to TOP_K..=24).
    pub pool: usize,
    /// How many reranked hits are kept (`RAG_RERANK_KEEP`, 6, clamped to 1..=24).
    pub keep: usize,
    /// `RAG_RERANK_DEADLINE_MS`, 3000, clamped to 100..=30000.
    pub deadline_ms: u64,
}

/// `parseInt(v, 10)` with `clampInt`'s default and bounds.
fn clamp_int(v: Option<&str>, default: i64, lo: i64, hi: i64) -> i64 {
    let s = String::from_utf16_lossy(prompt_framing::js::trim(&units(v.unwrap_or("undefined"))));
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(&s)),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return default;
    }
    // Beyond i64 is beyond every bound: saturate.
    let n = digits.parse::<i64>().unwrap_or(i64::MAX);
    (if neg { -n } else { n }).clamp(lo, hi)
}

impl Config {
    /// `initRerank()`: `env` is the reranker's effective environment (`rerank-settings.cjs`), the
    /// three `RAG_RERANK_*` knobs come from the process's own (`process.env`).
    pub fn from_env(
        env: &HashMap<String, String>,
        process: impl Fn(&str) -> Option<String>,
    ) -> Option<Config> {
        let target = rerank_target(env);
        if !target.enabled {
            return None;
        }
        let as_usize = |n: i64| usize::try_from(n).unwrap_or(0);
        Some(Config {
            backend: target.backend?,
            base_url: target.base_url?,
            model: target.model,
            // An own `RERANK_API_KEY` of the effective environment (always present there), empty = none.
            api_key: env
                .get("RERANK_API_KEY")
                .cloned()
                .or_else(|| process("RERANK_API_KEY"))
                .filter(|k| !k.is_empty()),
            pool: as_usize(clamp_int(
                process("RAG_RERANK_POOL").as_deref(),
                12,
                TOP_K as i64,
                POOL_MAX as i64,
            )),
            keep: as_usize(clamp_int(
                process("RAG_RERANK_KEEP").as_deref(),
                6,
                1,
                POOL_MAX as i64,
            )),
            deadline_ms: u64::try_from(clamp_int(
                process("RAG_RERANK_DEADLINE_MS").as_deref(),
                3000,
                100,
                30_000,
            ))
            .unwrap_or(3000),
        })
    }
}

/// One request to the reranker.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub url: String,
    /// `Bearer <key>` when a key is configured.
    pub authorization: Option<String>,
    /// The JSON body, as the backend writes it.
    pub body: String,
}

/// `JSON.stringify` of a string of UTF-16 code units: `"`, `\\` and the C0 controls escaped (the
/// short forms where JS has them), a lone surrogate as `\uXXXX`, everything else as itself.
fn quote(units: &[u16], out: &mut String) {
    out.push('"');
    let mut i = 0;
    while let Some(&u) = units.get(i) {
        match u {
            0x22 => out.push_str("\\\""),
            0x5c => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            0x0a => out.push_str("\\n"),
            0x0d => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            0xd800..=0xdbff => match units.get(i + 1) {
                Some(&low) if (0xdc00..=0xdfff).contains(&low) => {
                    out.push_str(&String::from_utf16_lossy(&[u, low]));
                    i += 1;
                }
                _ => out.push_str(&format!("\\u{u:04x}")),
            },
            0xdc00..=0xdfff => out.push_str(&format!("\\u{u:04x}")),
            u if u < 0x20 => out.push_str(&format!("\\u{u:04x}")),
            u => out.push(char::from_u32(u32::from(u)).unwrap_or('\u{fffd}')),
        }
        i += 1;
    }
    out.push('"');
}

/// The label of a hit as the backend sends it: `String(item.label ?? '').slice(0, 4000)`.
pub fn document(body: &str) -> Vec<u16> {
    let mut u = units(body);
    u.truncate(MAX_DOC_UNITS);
    u
}

/// The request the configured backend sends for `question` over `pool` (`llamaRerankBackend`:
/// `POST <base>/v1/rerank {model?, query, documents, top_n}`; `teiRerankBackend`: `POST <base>/rerank
/// {query, texts, truncate: true, raw_scores: false}`).
pub fn request(config: &Config, question: &str, pool: &[Hit]) -> Request {
    let base = config.base_url.trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    let mut body = String::from("{");
    let docs = |body: &mut String| {
        body.push('[');
        for (i, h) in pool.iter().enumerate() {
            if i > 0 {
                body.push(',');
            }
            quote(&document(&h.body), body);
        }
        body.push(']');
    };
    let url = match config.backend {
        Backend::Llamacpp => {
            if let Some(model) = &config.model {
                body.push_str("\"model\":");
                quote(&units(model), &mut body);
                body.push(',');
            }
            body.push_str("\"query\":");
            quote(&units(question), &mut body);
            body.push_str(",\"documents\":");
            docs(&mut body);
            body.push_str(&format!(",\"top_n\":{}}}", pool.len()));
            format!("{base}/v1/rerank")
        }
        Backend::Tei => {
            body.push_str("\"query\":");
            quote(&units(question), &mut body);
            body.push_str(",\"texts\":");
            docs(&mut body);
            body.push_str(",\"truncate\":true,\"raw_scores\":false}");
            format!("{base}/rerank")
        }
    };
    Request {
        url,
        authorization: config.api_key.as_ref().map(|k| format!("Bearer {k}")),
        body,
    }
}

/// `request.items[row.index]` for llama.cpp's rows: a property lookup, so `3`, `-0`, `"3"` and
/// `[3]` (whose string is `"3"`) find item 3 and `1.5`, `"03"`, `-1` or `[1, 2]` find nothing.
fn llama_index(v: Option<&Value>, n: usize) -> Option<usize> {
    let i = match v? {
        Value::Array(a) if a.len() == 1 => return llama_index(a.first(), n),
        Value::Number(x) => {
            let f = x.as_f64()?;
            (f.fract() == 0.0 && f >= 0.0 && f < n as f64).then_some(f as usize)?
        }
        Value::String(s) => {
            let i: usize = s.parse().ok()?;
            (i.to_string() == *s).then_some(i)?
        }
        _ => return None,
    };
    (i < n).then_some(i)
}

fn finite(v: Option<&Value>) -> Option<f64> {
    v?.as_f64().filter(|f| f.is_finite())
}

/// The backend's scores by item index, or `None` where it throws (a non-2xx, an answer that is not
/// JSON, a partial, repeated or malformed answer): then the cosine order answers.
pub fn parse(backend: Backend, items: usize, status: u16, answer: &[u8]) -> Option<Vec<f64>> {
    if answer.len() > MAX_ANSWER_BYTES {
        return None;
    }
    parse_value(
        backend,
        items,
        status,
        &serde_json::from_slice(answer).ok()?,
    )
}

/// [`parse`] of an answer already read as JSON.
pub fn parse_value(backend: Backend, items: usize, status: u16, body: &Value) -> Option<Vec<f64>> {
    if !(200..300).contains(&status) {
        return None;
    }
    let mut scores: Vec<Option<f64>> = vec![None; items];
    match backend {
        Backend::Llamacpp => {
            let rows = body
                .get("results")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for row in &rows {
                // `row.index` of null throws in JS: the backend fails and the cosine order answers.
                if row.is_null() {
                    return None;
                }
                if let (Some(i), Some(s)) = (
                    llama_index(row.get("index"), items),
                    finite(row.get("relevance_score")),
                ) {
                    if let Some(slot) = scores.get_mut(i) {
                        *slot = Some(s);
                    }
                }
            }
        }
        Backend::Tei => {
            let rows = body.as_array()?;
            if rows.len() != items {
                return None;
            }
            for row in rows {
                let f = row.get("index")?.as_f64()?;
                if !(f.fract() == 0.0 && f >= 0.0 && f < items as f64) {
                    return None;
                }
                let slot = scores.get_mut(f as usize)?;
                if slot.is_some() {
                    return None;
                }
                *slot = Some(finite(row.get("score"))?);
            }
        }
    }
    scores.into_iter().collect()
}

/// `order(scores)`: the ids best first, equal scores by id text (`localeCompare` of the decimal
/// ids, which for ASCII digits is their byte order: "10" before "2").
pub fn order(scores: &[f64]) -> Vec<usize> {
    let mut ids: Vec<(usize, String)> = (0..scores.len()).map(|i| (i, i.to_string())).collect();
    ids.sort_by(|(a, a_id), (b, b_id)| {
        let (sa, sb) = (
            scores.get(*a).copied().unwrap_or(0.0),
            scores.get(*b).copied().unwrap_or(0.0),
        );
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a_id.cmp(b_id))
    });
    ids.into_iter().map(|(i, _)| i).collect()
}

/// `rerankPool`'s answer: with a ranking, its first `keep` pool entries; without (`None`: a fallback),
/// the cosine top [`TOP_K`].
pub fn merge(pool: Vec<Hit>, keep: usize, ranking: Option<Vec<usize>>) -> Vec<Hit> {
    match ranking {
        Some(ids) => {
            let mut slots: Vec<Option<Hit>> = pool.into_iter().map(Some).collect();
            ids.into_iter()
                .take(keep)
                .filter_map(|i| slots.get_mut(i).and_then(Option::take))
                .collect()
        }
        None => pool.into_iter().take(TOP_K).collect(),
    }
}

/// Whether `rerankPool` asks the reranker at all: two or more hits (`pool.length <= 1` is the cosine
/// pool as it is).
pub fn needed(pool: &[Hit]) -> bool {
    pool.len() > 1
}

/// The reranker as last configured, with its bench: `initRerank` builds a new decision chain (and
/// with it a fresh bench) whenever the configuration changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Chain {
    config: Option<Config>,
    bench: Bench,
}

impl Chain {
    /// The bench of `config`, a fresh one when the configuration changed.
    pub fn bench_for(&mut self, config: &Config) -> &mut Bench {
        if self.config.as_ref() != Some(config) {
            self.config = Some(config.clone());
            self.bench = Bench::default();
        }
        &mut self.bench
    }
}

/// The chain's deadline bench (`decision/index.cjs` `benched`, `misses`), one per process.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bench {
    misses: u32,
    until_ms: f64,
}

impl Bench {
    /// Whether the reranker is benched at `now_ms` (skipped: the fallback answers).
    pub fn benched(&self, now_ms: f64) -> bool {
        self.until_ms > now_ms
    }

    /// A deadline miss: the third in a row benches it for [`BENCH_MS`].
    pub fn missed(&mut self, now_ms: f64) {
        self.misses += 1;
        if self.misses >= BENCH_AFTER {
            self.until_ms = now_ms + BENCH_MS;
            self.misses = 0;
        }
    }

    /// An answer in time (valid or not) resets the count; another failure leaves it.
    pub fn answered(&mut self) {
        self.misses = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::{clamp_int, order, quote, Bench};

    #[test]
    fn misses_in_a_row_bench_and_an_answer_resets_them() {
        let mut b = Bench::default();
        b.missed(0.0);
        b.missed(1.0);
        b.answered();
        b.missed(2.0);
        b.missed(3.0);
        assert!(!b.benched(3.0), "an answer in between: two misses in a row");
        b.missed(4.0);
        assert!(b.benched(4.0) && b.benched(60_003.0) && !b.benched(60_004.0));
        // After the bench the count starts again.
        b.missed(70_000.0);
        b.missed(70_001.0);
        assert!(!b.benched(70_001.0));
    }

    #[test]
    fn parse_int_clamps() {
        assert_eq!(clamp_int(None, 12, 6, 24), 12);
        assert_eq!(clamp_int(Some(" 30x"), 12, 6, 24), 24);
        assert_eq!(clamp_int(Some("-5"), 6, 1, 24), 1);
        assert_eq!(clamp_int(Some("2.9"), 12, 6, 24), 6);
        assert_eq!(
            clamp_int(Some("99999999999999999999999"), 3000, 100, 30000),
            30000
        );
        assert_eq!(clamp_int(Some("many"), 3000, 100, 30000), 3000);
    }

    #[test]
    fn order_is_score_then_id_text() {
        let s: Vec<f64> = (0..12)
            .map(|i| if i == 2 || i == 10 { 5.0 } else { 0.0 })
            .collect();
        assert_eq!(order(&s), vec![10, 2, 0, 1, 11, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn quoting_matches_json_stringify() {
        let mut out = String::new();
        quote(
            &[0x61, 0x22, 0x5c, 0x0a, 0x01, 0xd83d, 0xde00, 0xd83d],
            &mut out,
        );
        assert_eq!(out, "\"a\\\"\\\\\\n\\u0001😀\\ud83d\"");
        let mut out = String::new();
        quote(&[0xde00, 0x7f, 0xe9], &mut out);
        assert_eq!(out, "\"\\ude00\u{7f}é\"");
    }
}
