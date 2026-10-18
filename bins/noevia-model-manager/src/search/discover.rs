//! `model-manager/app/discover.py`: judging, filtering and ranking the repositories a search
//! found, against the GPU budget this server reports.
//!
//! The rules are `discover.py`'s, step by step. Its patterns are applied to printable ASCII only
//! (a path or id with any other character is [`Unsupported`], because Python's `\w`, `\d`, `\b`
//! and case folding differ from the Unicode classes of this side for such text); floats are
//! computed in the same order with Python's rounding and formatting (`py.rs`).
use super::py::{self, Instant, Unsupported};
use regex::Regex;
use std::sync::OnceLock;

/// `discover.SUB_Q4_PARAM_LIMIT`, billions.
const SUB_Q4_PARAM_LIMIT: f64 = 100.0;
/// `discover.MIN_MODEL_GB`.
const MIN_MODEL_GB: f64 = 0.3;

fn pattern(
    cell: &'static OnceLock<Option<Regex>>,
    source: &str,
) -> Result<&'static Regex, Unsupported> {
    cell.get_or_init(|| Regex::new(source).ok())
        .as_ref()
        .ok_or(Unsupported)
}

fn quant_pattern() -> Result<&'static Regex, Unsupported> {
    static CELL: OnceLock<Option<Regex>> = OnceLock::new();
    pattern(
        &CELL,
        r"(?i)(?:^|[-_.])((?:UD-)?(?:IQ\d\w*|Q\d(?:_[\w]+)*|MXFP4|F16|BF16|F32))(?:[-_.]|$)",
    )
}

fn sub_q4_pattern() -> Result<&'static Regex, Unsupported> {
    static CELL: OnceLock<Option<Regex>> = OnceLock::new();
    pattern(
        &CELL,
        r"(?i)(?:^|[-_.])(?:UD-)?(IQ[123]\w*|Q[123](?:_[\w]+)*)(?:[-_.]|$)",
    )
}

fn params_pattern() -> Result<&'static Regex, Unsupported> {
    static CELL: OnceLock<Option<Regex>> = OnceLock::new();
    pattern(
        &CELL,
        r"(?i)(?:^|[-_.])(\d+(?:\.\d+)?)\s*B(?:-A(\d+(?:\.\d+)?)B)?(?:[-_.]|$)",
    )
}

fn companion_pattern() -> Result<&'static Regex, Unsupported> {
    static CELL: OnceLock<Option<Regex>> = OnceLock::new();
    pattern(&CELL, r"(?i)mmproj|projector|\bmtp\b|draft|eagle\d?|medusa")
}

fn shard_pattern() -> Result<&'static Regex, Unsupported> {
    static CELL: OnceLock<Option<Regex>> = OnceLock::new();
    pattern(&CELL, r"-\d{5}-of-\d{5}")
}

/// `IMATRIX.search(name)` (`re.compile("imatrix", re.I)`) for any text: under `re.I` the letter
/// `i` also matches `\u{131}` and `\u{130}` (Python's equivalences and simple lower-casing), no
/// other letter of the word has a non-ASCII form.
pub fn has_imatrix(name: &str) -> bool {
    const WORD: [char; 7] = ['i', 'm', 'a', 't', 'r', 'i', 'x'];
    let chars: Vec<char> = name.chars().collect();
    chars.windows(WORD.len()).any(|window| {
        window.iter().zip(WORD).all(|(c, w)| {
            c.to_ascii_lowercase() == w || (w == 'i' && matches!(c, '\u{130}' | '\u{131}'))
        })
    })
}

/// Printable ASCII, the text the patterns above are applied to.
pub fn plain(text: &str) -> bool {
    text.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `discover.quant_of(path)`.
pub fn quant_of(path: &str) -> Result<Option<String>, Unsupported> {
    let name = basename(path);
    if !plain(name) {
        return Err(Unsupported);
    }
    Ok(quant_pattern()?
        .captures(name)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_ascii_uppercase()))
}

/// `discover.is_stray_gguf(path, size_bytes)`: imatrix calibration data, or too small to be
/// weights.
pub fn is_stray_gguf(path: &str, size_bytes: i128) -> bool {
    if has_imatrix(basename(path)) {
        return true;
    }
    (size_bytes as f64 / 1e9) < MIN_MODEL_GB
}

fn is_model_weight_file(path: &str, size_bytes: i128) -> Result<bool, Unsupported> {
    if has_imatrix(basename(path)) {
        return Ok(false);
    }
    Ok(quant_of(path)?.is_some() && (size_bytes as f64 / 1e9) >= MIN_MODEL_GB)
}

/// `discover.params_from(name)`: (total B, active B), the largest total in the name.
pub fn params_from(name: &str) -> Result<(Option<f64>, Option<f64>), Unsupported> {
    if !plain(name) {
        return Err(Unsupported);
    }
    let mut best: Option<(f64, Option<f64>)> = None;
    for m in params_pattern()?.captures_iter(name) {
        let total: f64 = m
            .get(1)
            .and_then(|g| g.as_str().parse().ok())
            .ok_or(Unsupported)?;
        let active = match m.get(2) {
            Some(g) => Some(g.as_str().parse::<f64>().map_err(|_| Unsupported)?),
            None => None,
        };
        if best.is_none_or(|(b, _)| total > b) {
            best = Some((total, active));
        }
    }
    Ok(match best {
        Some((total, active)) => (Some(total), active),
        None => (None, None),
    })
}

/// A tag as Python's `str(t)` renders it, or one this side cannot render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tag {
    Text(String),
    /// `str()` of a list or dict: its Python repr is not reproduced.
    Opaque,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub path: String,
    pub size: i128,
}

/// `discover.Candidate`.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub owner: String,
    pub downloads: i128,
    pub likes: i128,
    pub last_modified: String,
    pub tags: Vec<Tag>,
    pub files: std::sync::Arc<Vec<File>>,
    pub license: Option<String>,
}

/// One downloadable model file of a repository.
#[derive(Debug, Clone, PartialEq)]
pub struct Opt {
    pub path: String,
    pub bytes: i128,
    pub shards: i64,
    pub gb: f64,
    pub quant: Option<String>,
    pub params: Option<f64>,
    pub active: Option<f64>,
    pub fits: bool,
    pub reasons: Vec<String>,
}

/// `discover.judge(...)`'s dict.
#[derive(Debug, Clone)]
pub struct Judged {
    pub id: String,
    pub owner: String,
    pub downloads: i128,
    pub likes: i128,
    pub last_modified: String,
    pub age_days: i64,
    pub license: Option<String>,
    pub tags: Vec<String>,
    pub params: Option<f64>,
    pub active: Option<f64>,
    pub moe: bool,
    pub vision: bool,
    pub trusted: bool,
    pub options: Vec<Opt>,
    pub best: Option<Opt>,
    pub suitable: bool,
    pub reasons: Vec<String>,
    pub score: f64,
    pub popularity: f64,
}

/// `candidate.tags` as plain text, or `Unsupported` when one cannot be rendered.
fn tag_texts(tags: &[Tag]) -> Result<Vec<&str>, Unsupported> {
    tags.iter()
        .map(|t| match t {
            Tag::Text(s) => Ok(s.as_str()),
            Tag::Opaque => Err(Unsupported),
        })
        .collect()
}

/// `discover.is_moe`.
fn is_moe(candidate: &Candidate, active: Option<f64>) -> Result<bool, Unsupported> {
    if active.is_some() {
        return Ok(true);
    }
    let mut blob = candidate.id.clone();
    for tag in tag_texts(&candidate.tags)? {
        blob.push(' ');
        blob.push_str(tag);
    }
    let blob = blob.to_lowercase();
    Ok(["moe", "mixture-of-experts", "-a3b", "-a4b", "a22b"]
        .iter()
        .any(|k| blob.contains(k)))
}

/// `discover.popularity`.
fn popularity(candidate: &Candidate, age: f64) -> f64 {
    let raw = candidate.downloads + candidate.likes * 50;
    let decay = 0.5f64.powf(age / 90.0);
    // `(trending * 1000.0) + raw * pow`, with trending 0.0.
    0.0 + (raw as f64) * decay
}

/// `discover.build_options`.
fn build_options(candidate: &Candidate, budget_gb: f64) -> Result<Vec<Opt>, Unsupported> {
    let (total_b, active_b) = params_from(&candidate.id)?;
    struct Row {
        path: String,
        bytes: i128,
        shards: i64,
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for file in candidate.files.iter() {
        if !plain(&file.path) {
            return Err(Unsupported);
        }
        if !file.path.to_lowercase().ends_with(".gguf")
            || companion_pattern()?.is_match(basename(&file.path))
        {
            continue;
        }
        let base = shard_pattern()?.replace_all(&file.path, "").into_owned();
        let at = match index.get(&base) {
            Some(i) => *i,
            None => {
                rows.push(Row {
                    path: base.clone(),
                    bytes: 0,
                    shards: 0,
                });
                index.insert(base, rows.len() - 1);
                rows.len() - 1
            }
        };
        if let Some(row) = rows.get_mut(at) {
            row.bytes += file.size;
            row.shards += 1;
        }
    }
    let mut out: Vec<Opt> = Vec::new();
    for row in rows {
        if !is_model_weight_file(&row.path, row.bytes)? {
            continue;
        }
        let gb = row.bytes as f64 / 1e9;
        let quant = quant_of(&row.path)?;
        // `total_b or params_from(path)[0]`: a zero total falls through.
        let params = match total_b {
            Some(t) if t != 0.0 => Some(t),
            _ => params_from(&row.path)?.0,
        };
        let sub_q4 = sub_q4_pattern()?.is_match(basename(&row.path))
            && params.is_none_or(|p| p < SUB_Q4_PARAM_LIMIT);
        let mut reasons = Vec::new();
        if gb > budget_gb {
            reasons.push(format!(
                "{} GB does not fit the {} GB the GPU can hold",
                py::fixed(gb, 1),
                py::fixed(budget_gb, 1)
            ));
        }
        if sub_q4 {
            reasons.push(format!(
                "{} is below Q4",
                quant.as_deref().unwrap_or("this quantisation")
            ));
        }
        out.push(Opt {
            path: row.path,
            bytes: row.bytes,
            shards: row.shards,
            gb: py::round_to(gb, 2),
            quant,
            params,
            active: active_b,
            fits: reasons.is_empty(),
            reasons,
        });
    }
    // `out.sort(key=lambda r: (-r["fits"], -r["gb"]))`, stable.
    out.sort_by(|a, b| {
        b.fits
            .cmp(&a.fits)
            .then(b.gb.partial_cmp(&a.gb).unwrap_or(std::cmp::Ordering::Equal))
    });
    Ok(out)
}

/// `discover.judge(candidate, budget_gb=..., trusted=...)`. `quantisers` and `labs` are
/// lower-case.
pub fn judge(
    candidate: &Candidate,
    budget_gb: f64,
    quantisers: &std::collections::HashSet<String>,
    labs: &std::collections::HashSet<String>,
    now: Instant,
) -> Result<Judged, Unsupported> {
    let options = build_options(candidate, budget_gb)?;
    let usable: Vec<&Opt> = options.iter().filter(|o| o.fits).collect();
    let (total_b, active_b) = params_from(&candidate.id)?;
    let owner = candidate.owner.to_lowercase();
    let trusted = quantisers.contains(&owner) || labs.contains(&owner);
    // `max(usable, key=gb, default=None)`: the first of equal figures.
    let mut best: Option<&Opt> = None;
    for o in &usable {
        if best.is_none_or(|b| o.gb > b.gb) {
            best = Some(o);
        }
    }
    let mut reasons = Vec::new();
    if options.is_empty() {
        reasons.push("no GGUF model files in this repository".to_owned());
    } else if usable.is_empty() {
        // `min((o["reasons"][0] ...), key=len)`: the first of the shortest.
        let mut shortest: Option<&str> = None;
        for first in options.iter().filter_map(|o| o.reasons.first()) {
            if shortest.is_none_or(|s| first.chars().count() < s.chars().count()) {
                shortest = Some(first);
            }
        }
        reasons.extend(shortest.map(str::to_owned));
    }
    let age = py::age_days(&candidate.last_modified, now)?;
    let tags: Vec<String> = candidate
        .tags
        .iter()
        .take(12)
        .map(|t| match t {
            Tag::Text(s) => Ok(s.clone()),
            Tag::Opaque => Err(Unsupported),
        })
        .collect::<Result<_, _>>()?;
    let vision = candidate
        .files
        .iter()
        .any(|f| f.path.to_lowercase().contains("mmproj"));
    let suitable = !usable.is_empty();
    let score = (if suitable { 2.0 } else { 0.0 }) + (if trusted { 0.5 } else { 0.0 });
    let best = best.cloned();
    Ok(Judged {
        id: candidate.id.clone(),
        owner: candidate.owner.clone(),
        downloads: candidate.downloads,
        likes: candidate.likes,
        last_modified: candidate.last_modified.clone(),
        age_days: age.round_ties_even() as i64,
        license: candidate.license.clone(),
        tags,
        params: total_b,
        active: active_b,
        moe: is_moe(candidate, active_b)?,
        vision,
        trusted,
        options: options.into_iter().take(12).collect(),
        best,
        suitable,
        reasons,
        score,
        popularity: py::round_to(popularity(candidate, age), 2),
    })
}

/// `discover.rank`: fit first, then trusted publishers, then age-weighted popularity.
pub fn rank(results: &mut [Judged]) {
    results.sort_by(|a, b| {
        let by = |x: f64, y: f64| y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal);
        by(a.score, b.score)
            .then(by(a.popularity, b.popularity))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// `discover.apply_filters`' arguments.
pub struct Filters<'a> {
    pub trusted_only: bool,
    pub show_unsuitable: bool,
    pub min_gb: Option<f64>,
    pub max_gb: Option<f64>,
    pub quants: Vec<&'a str>,
    pub min_params: Option<f64>,
    pub max_params: Option<f64>,
    pub moe: &'a str,
    pub vision: bool,
    pub license_contains: &'a str,
    pub owner: &'a str,
}

/// `discover.apply_filters(results, ...)`.
pub fn apply_filters(results: Vec<Judged>, f: &Filters<'_>) -> Vec<Judged> {
    let mut out = Vec::new();
    let wanted: Vec<String> = f.quants.iter().map(|q| q.to_uppercase()).collect();
    for mut r in results {
        if f.trusted_only && !r.trusted {
            continue;
        }
        if !f.show_unsuitable && !r.suitable {
            continue;
        }
        if f.moe == "moe" && !r.moe {
            continue;
        }
        if f.moe == "dense" && r.moe {
            continue;
        }
        if f.vision && !r.vision {
            continue;
        }
        if !f.owner.is_empty() && !r.owner.to_lowercase().contains(&f.owner.to_lowercase()) {
            continue;
        }
        if !f.license_contains.is_empty()
            && !r
                .license
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains(&f.license_contains.to_lowercase())
        {
            continue;
        }
        if let Some(min) = f.min_params {
            if r.params.is_none_or(|p| p < min) {
                continue;
            }
        }
        if let Some(max) = f.max_params {
            if r.params.is_none_or(|p| p > max) {
                continue;
            }
        }
        if !wanted.is_empty() {
            r.options
                .retain(|o| wanted.contains(&o.quant.as_deref().unwrap_or("").to_uppercase()));
        }
        if let Some(min) = f.min_gb {
            r.options.retain(|o| o.gb >= min);
        }
        if let Some(max) = f.max_gb {
            r.options.retain(|o| o.gb <= max);
        }
        if (!wanted.is_empty() || f.min_gb.is_some() || f.max_gb.is_some()) && r.options.is_empty()
        {
            continue;
        }
        out.push(r);
    }
    out
}

/// `discover.hub_url(query, owner=owner)`.
pub fn hub_url(query: &str, owner: &str) -> String {
    let mut params = String::from("filter=gguf");
    if !query.is_empty() {
        params.push_str("&search=");
        params.push_str(&py::quote_plus(query));
    }
    if !owner.is_empty() {
        params.push_str("&author=");
        params.push_str(&py::quote_plus(owner));
    }
    format!("https://huggingface.co/models?{params}&sort=trending")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn imatrix_follows_case_folding() {
        assert!(has_imatrix("model-IMATRIX.gguf"));
        assert!(has_imatrix("\u{130}matr\u{131}x"));
        assert!(has_imatrix("xx\u{131}matrix"));
        assert!(!has_imatrix("imatri"));
        assert!(!has_imatrix("imat\u{ff52}ix"));
    }

    #[test]
    fn quant_and_params_rules() {
        assert_eq!(
            quant_of("a/b-Q4_K_M.gguf").unwrap().as_deref(),
            Some("Q4_K_M")
        );
        assert_eq!(
            quant_of("b-UD-IQ2_XXS.gguf").unwrap().as_deref(),
            Some("UD-IQ2_XXS")
        );
        assert_eq!(quant_of("b.bf16.gguf").unwrap().as_deref(), Some("BF16"));
        assert_eq!(quant_of("plain.gguf").unwrap(), None);
        assert!(quant_of("\u{fc}-Q4.gguf").is_err());
        assert_eq!(
            params_from("org-35B-A3B-it").unwrap(),
            (Some(35.0), Some(3.0))
        );
        assert_eq!(params_from("m-7B-Instruct").unwrap(), (Some(7.0), None));
        // The delimiter after the first match is consumed, so the second figure is not found.
        assert_eq!(params_from("m-7B-70B").unwrap(), (Some(7.0), None));
        assert_eq!(params_from("m-70B_7B").unwrap(), (Some(70.0), None));
        assert_eq!(params_from("m").unwrap(), (None, None));
    }

    #[test]
    fn hub_url_encodes_like_urlencode() {
        assert_eq!(
            hub_url("", ""),
            "https://huggingface.co/models?filter=gguf&sort=trending"
        );
        assert_eq!(
            hub_url("a b/c", "o*"),
            "https://huggingface.co/models?filter=gguf&search=a+b%2Fc&author=o%2A&sort=trending"
        );
    }
}
