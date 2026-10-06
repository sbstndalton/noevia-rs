//! Bounded GGUF metadata reader for noevia (strangler slice of
//! `services/model-manager/app/gguf_meta.py`).
//!
//! [`read_raw`] / [`read_raw_bytes`] parse the key/value header without trusting any declared
//! length; [`summarize`] builds the same summary shape the Python service returns.

#![forbid(unsafe_code)]

mod json;
mod pyfmt;
mod raw;
mod summary;

pub use json::{Json, PyInt};
pub use raw::{
    read_raw, read_raw_bytes, read_raw_stream, GgufError, Raw, Value, MAX_ARRAY_DEPTH,
    MAX_ARRAY_ELEMENTS_KEPT, MAX_BYTES_READ, MAX_KEY_LEN, MAX_KV_COUNT, MAX_RETAINED_CHARS,
    MAX_RETAINED_VALUES, MAX_STRING_LEN,
};
pub use summary::{file_type_name, scan_chat_template_features, summarize, SummaryError};

/// Read a GGUF file and summarise it in one step.
pub fn summarize_file(path: &std::path::Path) -> Result<Json, Box<dyn std::error::Error>> {
    let raw = read_raw(path)?;
    Ok(summarize(&raw)?)
}
