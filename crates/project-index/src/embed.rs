//! The batch rules of `embedFramed` in rag.cjs, over one `/v1/embeddings` call.
//!
//! The call itself (address, headers, the network guard, size caps, framing for the target's
//! profile) is the wiring's; this is what Node does with the answers: a single text needs exactly
//! one vector, a batch is taken in `index` order when every item has an integer index, and a batch
//! answered short (some providers return an empty `data` array) is retried one text per call.

use serde_json::Value;

/// One `/v1/embeddings` request: the framed inputs in, the answer's `data` items out.
pub trait EmbedOnce {
    fn once(&mut self, input: &[String]) -> Result<Vec<Value>, String>;
}

impl<F: FnMut(&[String]) -> Result<Vec<Value>, String>> EmbedOnce for F {
    fn once(&mut self, input: &[String]) -> Result<Vec<Value>, String> {
        self(input)
    }
}

/// JavaScript truthiness of `item.embedding`.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// A vector as Node passes it on: an array, each element read the way `new Float32Array(vector)`
/// reads it (JavaScript `Number()`: `null` is 0, `"2"` is 2, `true` is 1, anything unreadable is NaN).
/// Anything but an array is kept as None, which every later step treats as a vector of the wrong
/// width (named difference: Node would read a string's `.length` as a width).
pub type Vector = Option<Vec<f64>>;

fn vector(item: &Value) -> Vector {
    Some(
        item.get("embedding")?
            .as_array()?
            .iter()
            .map(js_number)
            .collect(),
    )
}

/// JavaScript `Number(v)` for a JSON value.
pub(crate) fn js_number(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::String(s) => string_number(s),
        // `Number([])` is 0 and `Number([x])` is `Number(String(x))`; more elements join with ",".
        Value::Array(a) => match a.as_slice() {
            [] => 0.0,
            [only] => js_string(only).map_or(f64::NAN, |s| string_number(&s)),
            _ => f64::NAN,
        },
        Value::Object(_) => f64::NAN,
    }
}

/// `String(n)` for a JSON number: integers without a fraction (`1`, not `1.0`), others as Rust
/// prints the shortest round-trip form (matches JavaScript for ordinary magnitudes).
pub(crate) fn js_number_string(n: &serde_json::Number) -> String {
    match n.as_f64() {
        Some(f) => {
            if f == 0.0 {
                "0".into()
            } else if f.fract() == 0.0 && f.abs() < 1e21 {
                format!("{f:.0}")
            } else {
                f.to_string()
            }
        }
        None => n.to_string(),
    }
}

/// `String(v)` as an array element joins it (`null` is empty); None for a plain object.
fn js_string(v: &Value) -> Option<String> {
    match v {
        Value::Null => Some(String::new()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(js_number_string(n)),
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => a
            .iter()
            .map(js_string)
            .collect::<Option<Vec<_>>>()
            .map(|parts| parts.join(",")),
        Value::Object(_) => None,
    }
}

/// StrUnsignedDecimalLiteral: `digits [. digits] [e [+-] digits]` or `. digits [e ...]`.
fn js_decimal(b: &[u8]) -> bool {
    let digits = |b: &[u8], mut i: usize| {
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        i
    };
    let int_end = digits(b, 0);
    let mut i = int_end;
    let mut any = int_end > 0;
    if b.get(i) == Some(&b'.') {
        let frac_end = digits(b, i + 1);
        any |= frac_end > i + 1;
        i = frac_end;
    }
    if !any {
        return false;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp_end = digits(b, i);
        if exp_end == i {
            return false;
        }
        i = exp_end;
    }
    i == b.len()
}

/// JavaScript's StringToNumber: trimmed (JS white space and line terminators), empty is 0, signed
/// decimal or `Infinity`, unsigned 0x/0o/0b integers; everything else NaN.
fn string_number(s: &str) -> f64 {
    let t = s.trim_matches(|c: char| crate::chunk::is_js_space(c));
    if t.is_empty() {
        return 0.0;
    }
    let (sign, body) = match (t.strip_prefix('-'), t.strip_prefix('+')) {
        (Some(rest), _) => (-1.0, rest),
        (None, Some(rest)) => (1.0, rest),
        (None, None) => (1.0, t),
    };
    if body == "Infinity" {
        return sign * f64::INFINITY;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = t.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return digits.chars().fold(0.0, |n, c| {
                n * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
            });
        }
    }
    if !js_decimal(body.as_bytes()) {
        return f64::NAN;
    }
    body.parse::<f64>().map_or(f64::NAN, |n| sign * n)
}

fn integer_index(item: &Value) -> Option<f64> {
    let n = item.get("index")?.as_f64()?;
    (n.fract() == 0.0).then_some(n)
}

/// `embedFramed(target, input)`.
pub fn embed_framed(call: &mut dyn EmbedOnce, input: &[String]) -> Result<Vec<Vector>, String> {
    match input.len() {
        0 => Ok(Vec::new()),
        1 => {
            let items = call.once(input)?;
            match items.as_slice() {
                [only] if truthy(only.get("embedding")) => Ok(vec![vector(only)]),
                _ => Err("embeddings returned no vector".into()),
            }
        }
        n => {
            let items = call.once(input)?;
            if items.len() == n && items.iter().all(|it| truthy(it.get("embedding"))) {
                let mut ordered: Vec<&Value> = items.iter().collect();
                if items.iter().all(|it| integer_index(it).is_some()) {
                    // Array.prototype.sort is stable; equal indexes keep their order.
                    ordered.sort_by(|a, b| {
                        integer_index(a)
                            .unwrap_or(0.0)
                            .total_cmp(&integer_index(b).unwrap_or(0.0))
                    });
                }
                return Ok(ordered.into_iter().map(vector).collect());
            }
            let mut out = Vec::with_capacity(n);
            for text in input {
                out.extend(embed_framed(call, std::slice::from_ref(text))?);
            }
            Ok(out)
        }
    }
}

/// `embed(texts, { kind: 'document', target })` for index chunks: refuses while the admin has the
/// embedding service off (`embeddingOff`), frames every text for the target's profile with no title
/// (as the Diary's index does: a file name is not part of a chunk's text) and applies [`embed_framed`].
/// `call` is one `/v1/embeddings` request to the target's address.
pub struct DocumentEmbedder<C, O>
where
    C: FnMut(&crate::slots::Target, &[String]) -> Result<Vec<Value>, String>,
    O: Fn(&crate::slots::Target) -> bool,
{
    pub call: C,
    pub off: O,
}

/// Node's `embed` error when the embedding service is off (code `embeddingOff`).
pub const EMBEDDING_OFF: &str = "Embedding is turned off.";

/// Frames one document text for a profile (`frame(profile, [text], { kind: 'document' })`).
pub fn frame_document(profile: &str, text: &str) -> Result<String, String> {
    use prompt_framing::embedding_input::{frame, Kind, Profile};
    // Named difference: Node's message also lists the known profiles.
    let p = Profile::try_from(profile)
        .map_err(|_| format!("Unknown embedding profile \"{profile}\""))?;
    let units: Vec<u16> = text.encode_utf16().collect();
    Ok(String::from_utf16_lossy(&frame(
        p,
        Kind::Document,
        &units,
        None,
    )))
}

impl<C, O> crate::index::Embed for DocumentEmbedder<C, O>
where
    C: FnMut(&crate::slots::Target, &[String]) -> Result<Vec<Value>, String>,
    O: Fn(&crate::slots::Target) -> bool,
{
    fn embed(
        &mut self,
        target: &crate::slots::Target,
        texts: &[String],
    ) -> Result<Vec<Vector>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        if (self.off)(target) {
            return Err(EMBEDDING_OFF.into());
        }
        let framed = texts
            .iter()
            .map(|t| frame_document(&target.profile, t))
            .collect::<Result<Vec<_>, _>>()?;
        let call = &mut self.call;
        let mut once = |input: &[String]| call(target, input);
        embed_framed(&mut once, &framed)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::index::Embed;
    use crate::slots::{Target, LEGACY_PROFILE};
    use serde_json::json;

    #[test]
    fn documents_are_framed_for_the_target_and_refused_when_off() {
        let seen = std::cell::RefCell::new(Vec::new());
        let mut e = DocumentEmbedder {
            call: |_: &Target, input: &[String]| {
                seen.borrow_mut().extend(input.iter().cloned());
                Ok(input
                    .iter()
                    .enumerate()
                    .map(|(i, _)| json!({"index": i, "embedding": [1.0]}))
                    .collect())
            },
            off: |t: &Target| t.model == "qa-off",
        };
        let legacy = Target {
            profile: LEGACY_PROFILE.into(),
            model: "qa-a".into(),
        };
        assert_eq!(
            e.embed(&legacy, &["raw text".into()]).unwrap(),
            vec![Some(vec![1.0])]
        );
        assert_eq!(
            *seen.borrow(),
            vec!["raw text"],
            "the legacy profile sends the text unchanged"
        );
        let off = Target {
            model: "qa-off".into(),
            ..legacy.clone()
        };
        assert_eq!(e.embed(&off, &["x".into()]).unwrap_err(), EMBEDDING_OFF);
        assert!(
            e.embed(&off, &[]).unwrap().is_empty(),
            "nothing to embed is never refused"
        );
        let unknown = Target {
            profile: "qa-unknown".into(),
            model: "qa-c".into(),
        };
        assert!(e
            .embed(&unknown, &["x".into()])
            .unwrap_err()
            .starts_with("Unknown embedding profile"));
    }

    #[test]
    fn a_batch_is_ordered_by_index() {
        let mut call = |_: &[String]| {
            Ok(vec![
                json!({"index": 1, "embedding": [2.0]}),
                json!({"index": 0, "embedding": [1.0]}),
            ])
        };
        let out = embed_framed(&mut call, &["a".into(), "b".into()]).unwrap();
        assert_eq!(out, vec![Some(vec![1.0]), Some(vec![2.0])]);
    }

    #[test]
    fn a_short_batch_falls_back_to_one_text_per_call() {
        let mut calls = Vec::new();
        let mut call = |input: &[String]| {
            calls.push(input.len());
            if input.len() > 1 {
                Ok(vec![])
            } else {
                Ok(vec![json!({"embedding": [input[0].len() as f64]})])
            }
        };
        let out = embed_framed(&mut call, &["a".into(), "bb".into()]).unwrap();
        assert_eq!(out, vec![Some(vec![1.0]), Some(vec![2.0])]);
        assert_eq!(calls, vec![2, 1, 1]);
    }

    #[test]
    fn one_text_needs_exactly_one_vector() {
        let mut call = |_: &[String]| Ok(vec![json!({"embedding": []}), json!({"embedding": []})]);
        assert_eq!(
            embed_framed(&mut call, &["a".into()]).unwrap_err(),
            "embeddings returned no vector"
        );
        let mut none = |_: &[String]| Ok(vec![json!({"embedding": null})]);
        assert!(embed_framed(&mut none, &["a".into()]).is_err());
    }

    #[test]
    fn mixed_indexes_keep_the_answer_order() {
        let mut call = |_: &[String]| {
            Ok(vec![
                json!({"index": 1, "embedding": [2.0]}),
                json!({"embedding": [1.0]}),
            ])
        };
        let out = embed_framed(&mut call, &["a".into(), "b".into()]).unwrap();
        assert_eq!(out, vec![Some(vec![2.0]), Some(vec![1.0])]);
    }

    /// Recorded with Node: `Array.from(new Float32Array(v)).map(String)` (the input list below, run on
    /// the builder with the CI node, 2026-11-14).
    #[test]
    fn vector_elements_read_as_float32array_reads_them() {
        let input = json!([1, null, "2", true, false, "x", [], ["3"], [[4]], [null], {"a": 1}, [1, 2],
            " 0x10 ", "-0x10", "0b101", "0o17", "-Infinity", "+Infinity", "1e3", "", "inf", "1.",
            "  .5\n", "\u{a0}7\u{2028}", "1_0", "0x", "--1", "1e", "Infinity", 1.5e40, "NaN"]);
        let node = [
            "1",
            "0",
            "2",
            "1",
            "0",
            "NaN",
            "0",
            "3",
            "4",
            "0",
            "NaN",
            "NaN",
            "16",
            "NaN",
            "5",
            "15",
            "-Infinity",
            "Infinity",
            "1000",
            "0",
            "NaN",
            "1",
            "0.5",
            "7",
            "NaN",
            "NaN",
            "NaN",
            "NaN",
            "Infinity",
            "Infinity",
            "NaN",
        ];
        let got = vector(&json!({ "embedding": input })).unwrap();
        let shown: Vec<String> = got
            .iter()
            .map(|&x| {
                let f = x as f32;
                if f.is_nan() {
                    "NaN".into()
                } else if f.is_infinite() {
                    if f > 0.0 {
                        "Infinity".into()
                    } else {
                        "-Infinity".into()
                    }
                } else {
                    f.to_string()
                }
            })
            .collect();
        assert_eq!(shown, node);
        assert_eq!(vector(&json!({ "embedding": "abc" })), None);
    }
}
