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
    item.get("embedding")?.as_array()?.iter().map(Value::as_f64).collect()
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
                        integer_index(a).unwrap_or(0.0).total_cmp(&integer_index(b).unwrap_or(0.0))
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_batch_is_ordered_by_index() {
        let mut call = |_: &[String]| Ok(vec![json!({"index": 1, "embedding": [2.0]}), json!({"index": 0, "embedding": [1.0]})]);
        let out = embed_framed(&mut call, &["a".into(), "b".into()]).unwrap();
        assert_eq!(out, vec![Some(vec![1.0]), Some(vec![2.0])]);
    }

    #[test]
    fn a_short_batch_falls_back_to_one_text_per_call() {
        let mut calls = Vec::new();
        let mut call = |input: &[String]| {
            calls.push(input.len());
            if input.len() > 1 { Ok(vec![]) } else { Ok(vec![json!({"embedding": [input[0].len() as f64]})]) }
        };
        let out = embed_framed(&mut call, &["a".into(), "bb".into()]).unwrap();
        assert_eq!(out, vec![Some(vec![1.0]), Some(vec![2.0])]);
        assert_eq!(calls, vec![2, 1, 1]);
    }

    #[test]
    fn one_text_needs_exactly_one_vector() {
        let mut call = |_: &[String]| Ok(vec![json!({"embedding": []}), json!({"embedding": []})]);
        assert_eq!(embed_framed(&mut call, &["a".into()]).unwrap_err(), "embeddings returned no vector");
        let mut none = |_: &[String]| Ok(vec![json!({"embedding": null})]);
        assert!(embed_framed(&mut none, &["a".into()]).is_err());
    }

    #[test]
    fn mixed_indexes_keep_the_answer_order() {
        let mut call = |_: &[String]| Ok(vec![json!({"index": 1, "embedding": [2.0]}), json!({"embedding": [1.0]})]);
        let out = embed_framed(&mut call, &["a".into(), "b".into()]).unwrap();
        assert_eq!(out, vec![Some(vec![2.0]), Some(vec![1.0])]);
    }
}
