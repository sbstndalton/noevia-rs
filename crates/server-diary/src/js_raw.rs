//! The lossless `JSON.parse`/`JSON.stringify` reader the local exchange and the proxy share. It lives
//! in js-json (`js_json::raw`, with its V8 parity tests) so the chat port uses the same one.
pub(crate) use js_json::raw::*;
