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

/// A vector as Node passes it on: an array of numbers. Anything else is kept as None, which every
/// later step treats as a vector of the wrong width (named difference: Node would read a string's
/// `.length` as a width).
pub type Vector = Option<Vec<f64>>;

fn vector(item: &Value) -> Vector {
    item.get("embedding")?
        .as_array()?
        .iter()
        .map(Value::as_f64)
        .collect()
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
}
