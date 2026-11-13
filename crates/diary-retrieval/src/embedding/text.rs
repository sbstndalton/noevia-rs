//! Profile framing and the "document too long for the server" fallback (#1759).
//!
//! A text the server accepts is sent exactly as before; only a document it refuses as too long
//! is cut at paragraph, line, sentence or word boundaries, each piece is embedded on its own and
//! the chunk's vector is the length-weighted mean of the piece vectors, normalised. Queries are
//! never split. All lengths are in Unicode code points, as Python's `len` and slicing are.

use super::pystr::is_space;
use super::{known_profiles, EmbeddingConfigError, KINDS, PROFILES};

/// Pieces aim at this share of the limit the server reported.
pub const SPLIT_MARGIN: f64 = 0.85;
/// A piece refused again is cut again, at most this many times.
pub const SPLIT_MAX_DEPTH: usize = 6;
/// A piece this short that is still refused is a real refusal.
pub const SPLIT_MIN_CHARS: usize = 64;
/// One cut never makes more pieces than this, whatever token count a server reports (#1766).
pub const SPLIT_MAX_PARTS: i128 = 32;

/// `kind` of `frame` / `embed_fitting`: one kind for every text, or one per text.
#[derive(Debug, Clone, Copy)]
pub enum Kinds<'a> {
    One(&'a str),
    Each(&'a [&'a str]),
}

impl Kinds<'_> {
    fn at(&self, i: usize) -> Option<&str> {
        match self {
            Self::One(k) => Some(k),
            Self::Each(list) => list.get(i).copied(),
        }
    }
}

fn short_argument(what: &'static str) -> EmbeddingConfigError {
    // Python raises IndexError here (known difference 6).
    EmbeddingConfigError::new(what, "badArguments", 500)
}

/// Replaces every maximal run of characters matching `pred` with one space.
fn collapse_runs(s: &str, pred: impl Fn(char) -> bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.chars() {
        if pred(c) {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

/// `clean_title`: line breaks and pipes become spaces, whitespace runs collapse, the result is
/// stripped and cut to 160 code points (after stripping, so it may end in a space); empty is
/// `none`.
pub fn clean_title(title: Option<&str>) -> String {
    let step1 = collapse_runs(title.unwrap_or(""), |c| matches!(c, '\r' | '\n' | '|'));
    let step2 = collapse_runs(&step1, is_space);
    let cut: String = step2.trim_matches(is_space).chars().take(160).collect();
    if cut.is_empty() {
        "none".to_owned()
    } else {
        cut
    }
}

/// The texts to send for `profile`. Fails closed on an unknown profile or kind.
///
/// `titles` is read only for a document under a framing profile, like Python's
/// `titles[i] if titles else None` (an empty list counts as none).
pub fn frame<S: AsRef<str>>(
    profile: &str,
    texts: &[S],
    kind: Kinds<'_>,
    titles: Option<&[Option<&str>]>,
) -> Result<Vec<String>, EmbeddingConfigError> {
    if !PROFILES.contains(&profile) {
        return Err(EmbeddingConfigError::new(
            format!(
                "Unknown embedding profile \"{profile}\" (known: {})",
                known_profiles()
            ),
            "unknownProfile",
            400,
        ));
    }
    let mut out = Vec::with_capacity(texts.len());
    for (i, text) in texts.iter().enumerate() {
        let k = kind
            .at(i)
            .ok_or_else(|| short_argument("kind is shorter than texts"))?;
        if !KINDS.contains(&k) {
            return Err(EmbeddingConfigError::new(
                format!("Unknown embedding kind \"{k}\""),
                "unknownKind",
                400,
            ));
        }
        let text = text.as_ref();
        if profile == "nomic-v1" {
            out.push(text.to_owned());
        } else if k == "query" {
            out.push(format!("task: search result | query: {text}"));
        } else if k == "similarity" {
            out.push(format!("task: sentence similarity | query: {text}"));
        } else {
            let title = match titles {
                Some(list) if !list.is_empty() => *list
                    .get(i)
                    .ok_or_else(|| short_argument("titles is shorter than texts"))?,
                _ => None,
            };
            out.push(format!("title: {} | text: {text}", clean_title(title)));
        }
    }
    Ok(out)
}

/// `text` cut into about `parts` pieces of similar length, at the best boundary near each cut.
pub fn split_text(text: &str, parts: i64) -> Vec<String> {
    let parts = parts.max(2);
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let size = n as f64 / parts as f64;
    let size_margin = (size * 0.4) as i64;
    let mut pieces: Vec<String> = Vec::new();
    let mut start = 0usize;
    let slice = |a: usize, b: usize| -> String {
        chars
            .get(a..b.min(n))
            .map_or_else(String::new, |s| s.iter().collect())
    };
    for i in 1..parts {
        let ideal = (i as f64 * size) as i64;
        let start_i = start as i64;
        if ideal <= start_i {
            continue;
        }
        let lo = (start_i + 1).max(ideal - size_margin);
        let mut cut = ideal as usize;
        // text.rfind(sep, lo, ideal + 1): the last match lying inside [lo, ideal + 1).
        let end = ((ideal + 1) as usize).min(n);
        for sep in ["\n\n", "\n", ". ", " "] {
            let sep: Vec<char> = sep.chars().collect();
            let found = rfind(&chars, &sep, usize::try_from(lo).unwrap_or(0), end);
            if let Some(at) = found.filter(|&at| at > start) {
                cut = at + sep.len();
                break;
            }
        }
        let piece = slice(start, cut);
        if !piece.trim_matches(is_space).is_empty() {
            pieces.push(piece);
        }
        start = cut;
    }
    let rest = slice(start, n);
    if !rest.trim_matches(is_space).is_empty() {
        pieces.push(rest);
    }
    if pieces.len() > 1 {
        pieces
    } else {
        vec![slice(0, n / 2), slice(n / 2, n)]
    }
}

/// `str.rfind(sep, lo, end)` over code points: the highest index `at` with `lo <= at` and
/// `at + sep.len() <= end`.
fn rfind(chars: &[char], sep: &[char], lo: usize, end: usize) -> Option<usize> {
    let last = end.checked_sub(sep.len())?;
    (lo..=last)
        .rev()
        .find(|&at| chars.get(at..at + sep.len()) == Some(sep))
}

/// CPython 3.12's `sum()` over floats: starts from the integer 0 and then adds with Neumaier
/// compensation (`Python/bltinmodule.c`), so the order and the compensation match exactly.
fn py_sum(items: impl IntoIterator<Item = f64>) -> f64 {
    let mut iter = items.into_iter();
    let Some(first) = iter.next() else {
        return 0.0;
    };
    let mut total = 0.0 + first;
    let mut compensation = 0.0_f64;
    for x in iter {
        let t = total + x;
        if total.abs() >= x.abs() {
            compensation += (total - t) + x;
        } else {
            compensation += (x - t) + total;
        }
        total = t;
    }
    if compensation != 0.0 && compensation.is_finite() {
        total += compensation;
    }
    total
}

/// `x ** 0.5` the way Python computes it: the C library `pow`, not `sqrt` (they differ in the
/// last bit for about one input in a thousand).
fn pow_half(x: f64) -> f64 {
    x.powf(std::hint::black_box(0.5))
}

/// `_mean`: the weighted mean of `vectors`, then L2-normalised. `None` is where Python raises
/// `IndexError` (no vectors, or a vector shorter than the first).
pub fn mean(vectors: &[Vec<f64>], weights: &[usize]) -> Option<Vec<f64>> {
    let sum: u128 = weights.iter().map(|&w| w as u128).sum();
    let total = if sum == 0 { 1.0 } else { sum as f64 };
    let first = vectors.first()?;
    let mut out = Vec::with_capacity(first.len());
    for j in 0..first.len() {
        let mut terms = Vec::with_capacity(vectors.len());
        for (v, &w) in vectors.iter().zip(weights) {
            terms.push(*v.get(j)? * w as f64);
        }
        out.push(py_sum(terms) / total);
    }
    let norm = pow_half(py_sum(out.iter().map(|x| x * x)));
    if norm > 0.0 {
        Some(out.into_iter().map(|x| x / norm).collect())
    } else {
        Some(out)
    }
}

/// What a `send` call can fail with (Python: `InputTooLarge`, or any other exception).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError<E> {
    /// The server refused the input as longer than one batch or context; `tokens` and `limit`
    /// are what it reported, when it did.
    TooLarge {
        tokens: Option<i64>,
        limit: Option<i64>,
    },
    /// Any other error: returned unchanged.
    Other(E),
}

/// What `embed_fitting` fails with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FitError<E> {
    /// The send's error, unchanged (a refused query, a text too short to cut, depth exhausted).
    Send(SendError<E>),
    /// Python raises `IndexError` here (known difference 6).
    Malformed(&'static str),
}

impl<E> From<SendError<E>> for FitError<E> {
    fn from(e: SendError<E>) -> Self {
        Self::Send(e)
    }
}

type Vectors = Vec<Vec<f64>>;

/// The `send(texts, kinds, titles)` callback of [`embed_fitting`].
pub type SendFn<'a, E> =
    dyn FnMut(&[String], &[String], &[Option<String>]) -> Result<Vectors, SendError<E>> + 'a;

/// How many pieces a refused document is cut into.
fn parts_for(tokens: Option<i64>, limit: Option<i64>) -> i128 {
    let mut parts: i128 = 2;
    if let (Some(tokens), Some(limit)) = (tokens, limit) {
        if tokens != 0 && limit > 0 {
            let per_piece = i128::from(((limit as f64 * SPLIT_MARGIN) as i64).max(1));
            // -(-tokens // per_piece): a ceiling division.
            let ceil = -(-i128::from(tokens)).div_euclid(per_piece);
            parts = parts.max(ceil);
        }
    }
    parts.min(SPLIT_MAX_PARTS)
}

fn fit<E>(
    send: &mut SendFn<'_, E>,
    texts: &[String],
    kinds: &[String],
    titles: &[Option<String>],
    depth: usize,
) -> Result<Vectors, FitError<E>> {
    let (tokens, limit) = match send(texts, kinds, titles) {
        Ok(v) => return Ok(v),
        Err(SendError::TooLarge { tokens, limit }) => (tokens, limit),
        Err(other) => return Err(other.into()),
    };
    let refused = || FitError::Send(SendError::TooLarge { tokens, limit });
    if texts.len() > 1 {
        // The server refused the whole batch without saying which text. Bisect: a half it
        // accepts goes out once as a batch, only a half that is refused again is cut further.
        let mid = texts.len() / 2;
        let (t1, t2) = texts.split_at(mid);
        let (k1, k2) = kinds.split_at(mid.min(kinds.len()));
        let (i1, i2) = titles.split_at(mid.min(titles.len()));
        let mut out = fit(send, t1, k1, i1, depth)?;
        out.extend(fit(send, t2, k2, i2, depth)?);
        return Ok(out);
    }
    let (Some(text), Some(kind)) = (texts.first(), kinds.first()) else {
        return Err(FitError::Malformed("the server refused an empty batch"));
    };
    if kind != "document" || depth >= SPLIT_MAX_DEPTH || text.chars().count() < SPLIT_MIN_CHARS {
        return Err(refused());
    }
    let parts = parts_for(tokens, limit);
    let pieces = split_text(text, i64::try_from(parts).unwrap_or(2));
    let title = titles.first().cloned().unwrap_or(None);
    let mut vectors = Vec::with_capacity(pieces.len());
    for piece in &pieces {
        let one = fit(
            send,
            std::slice::from_ref(piece),
            std::slice::from_ref(kind),
            std::slice::from_ref(&title),
            depth + 1,
        )?;
        vectors.push(
            one.into_iter()
                .next()
                .ok_or(FitError::Malformed("the server returned no vector"))?,
        );
    }
    let weights: Vec<usize> = pieces.iter().map(|p| p.chars().count()).collect();
    let merged =
        mean(&vectors, &weights).ok_or(FitError::Malformed("the piece vectors differ in width"))?;
    Ok(vec![merged])
}

/// `send(texts, kinds, titles)` for `texts`, re-splitting any document the server refuses as
/// too long. Any other error, and a refused query, is returned unchanged.
///
/// `send` receives the (unframed) texts, one kind and one title per text; it frames them itself
/// with [`frame`], exactly like Python's `lambda ts, ks, tis: client.embed(frame(...))`.
pub fn embed_fitting<E, S: AsRef<str>>(
    send: &mut SendFn<'_, E>,
    texts: &[S],
    kind: Kinds<'_>,
    titles: Option<&[Option<&str>]>,
) -> Result<Vectors, FitError<E>> {
    let texts: Vec<String> = texts.iter().map(|t| t.as_ref().to_owned()).collect();
    let mut kinds = Vec::with_capacity(texts.len());
    let mut tlist = Vec::with_capacity(texts.len());
    for i in 0..texts.len() {
        kinds.push(
            kind.at(i)
                .ok_or(FitError::Malformed("kind is shorter than texts"))?
                .to_owned(),
        );
        tlist.push(match titles {
            Some(list) if !list.is_empty() => list
                .get(i)
                .ok_or(FitError::Malformed("titles is shorter than texts"))?
                .map(str::to_owned),
            _ => None,
        });
    }
    fit(send, &texts, &kinds, &tlist, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_title_matches_the_python_steps() {
        assert_eq!(clean_title(None), "none");
        assert_eq!(clean_title(Some(" | \r\n ")), "none");
        assert_eq!(clean_title(Some("a|b\nc  d")), "a b c d");
        let long = "x ".repeat(200);
        // cut after stripping: 160 characters, ending in a space.
        assert_eq!(clean_title(Some(&long)).chars().count(), 160);
    }

    #[test]
    fn small_texts_split_in_half() {
        assert_eq!(split_text("abcd", 2), vec!["ab", "cd"]);
        assert_eq!(split_text("", 5), vec!["", ""]);
    }

    #[test]
    fn parts_follow_the_reported_tokens() {
        assert_eq!(parts_for(None, None), 2);
        assert_eq!(parts_for(Some(1000), Some(100)), 12);
        assert_eq!(parts_for(Some(100_000), Some(100)), 32);
        assert_eq!(parts_for(Some(0), Some(100)), 2);
        assert_eq!(parts_for(Some(500), Some(0)), 2);
    }

    #[test]
    fn mean_is_normalised() -> Result<(), &'static str> {
        let m = mean(&[vec![3.0, 0.0], vec![0.0, 4.0]], &[1, 1]).ok_or("mean")?;
        assert!((m.iter().map(|x| x * x).sum::<f64>() - 1.0).abs() < 1e-12);
        assert!(mean(&[], &[]).is_none());
        Ok(())
    }
}
