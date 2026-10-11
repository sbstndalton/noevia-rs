//! Bounded GGUF metadata reader for noevia (strangler slice of
//! `services/model-manager/app/gguf_meta.py`).
//!
//! [`read_raw`] / [`read_raw_bytes`] parse the key/value header without trusting any declared
//! length; [`summarize`] builds the same summary shape the Python service returns.

#![forbid(unsafe_code)]

mod json;
pub mod node;
mod pyfmt;
mod raw;
mod summary;

pub use json::{Json, PyInt};
pub use raw::{
    read_raw, read_raw_bytes, read_raw_stream, read_raw_stream_with_limits, GgufError,
    LimitedError, Raw, ReadLimits, Value, MAX_ARRAY_DEPTH, MAX_ARRAY_ELEMENTS_KEPT, MAX_BYTES_READ,
    MAX_KEY_LEN, MAX_KV_COUNT, MAX_PER_LAYER_KEPT, MAX_PER_LAYER_VALUES, MAX_RETAINED_CHARS,
    MAX_RETAINED_VALUES, MAX_STRING_LEN,
};
pub use summary::{file_type_name, scan_chat_template_features, summarize, SummaryError};

/// Bounded reuse of the metadata reader's Python text formatting. Scalar strings retain
/// their exact Unicode text. Complex values admit ASCII strings only because the existing
/// formatter intentionally does not implement all Unicode Python repr categories.
/// Preflight limits are 64 KiB output, 1,024 values and eight nesting levels; refusal occurs
/// before formatting. Existing readers and formatting behavior are unchanged.
pub fn metadata_text_with_limit(value: &Value, limit: usize) -> Result<String, LimitedError> {
    fn bound(value: &Value, depth: usize, nodes: &mut usize) -> Option<usize> {
        *nodes = nodes.checked_add(1)?;
        if *nodes > 1024 || depth > 8 {
            return None;
        }
        match value {
            Value::Str(text) if depth == 0 => Some(text.len()),
            Value::Str(text) if text.is_ascii() => text.len().checked_mul(4)?.checked_add(2),
            Value::Str(_) => None,
            Value::Float(number) if !number.is_finite() => None,
            Value::List(values) | Value::ArraySummary { sample: values, .. } => {
                let mut bytes = if matches!(value, Value::List(_)) {
                    2usize
                } else {
                    80usize
                };
                for value in values {
                    bytes = bytes
                        .checked_add(bound(value, depth + 1, nodes)?)?
                        .checked_add(2)?;
                }
                Some(bytes)
            }
            Value::Int(_) => Some(40),
            Value::Float(_) => Some(32),
            Value::Bool(_) | Value::None => Some(5),
        }
    }
    if limit > 65536 || bound(value, 0, &mut 0).is_none_or(|size| size > limit) {
        return Err(LimitedError::Budget);
    }
    Ok(pyfmt::py_str(value))
}

/// Read a GGUF file and summarise it in one step.
pub fn summarize_file(path: &std::path::Path) -> Result<Json, Box<dyn std::error::Error>> {
    let raw = read_raw(path)?;
    Ok(summarize(&raw)?)
}
