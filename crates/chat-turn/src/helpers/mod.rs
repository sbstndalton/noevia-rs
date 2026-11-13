//! Four small pure helpers of core's chat turn, ported from the Node files they are named after
//! and replayed against recordings of those files (`tests/fixtures/node-helpers.v1.json`, written
//! by `tests/record_helpers.cjs`):
//!
//! - [`search_query`] (`server/search-query.cjs`): a search tool's `query` without the request
//!   wrapper and citation instruction a model copied into it;
//! - [`announced`] (`server/announced-action.cjs`): whether a reply ends by announcing a tool call
//!   it did not make;
//! - [`final_answer`] (`server/final-answer.cjs`): when the no-tools final-answer round runs, the
//!   source links a tool result carries, and the reply text when that round writes nothing;
//! - [`route_notes`] (`server/tool-route-notes.cjs`): the system notes about this chat's tools.
//!
//! Text is JavaScript text: every length, slice and regex step counts UTF-16 code units, and a
//! `&str` may carry lone surrogates in `js_json::wtf16` form (a lone unit held as its mapped
//! character), which is how a Rust caller holds what Node would hold.
//!
//! Named differences from Node, each tested in `tests/node_helpers.rs`:
//! - **Arguments `js_json` cannot hold.** [`search_query::clean_search_args`] returns the
//!   arguments unchanged when `js_json::wtf16::parse` refuses them as unsupported though
//!   `JSON.parse` would accept them: a number outside the `f64` range (Node turns `1e400` into
//!   `Infinity` and writes it back as `null`), or nesting deeper than about 128 levels. Node
//!   rewrites such arguments when their query has a wrapper to remove.
//! - **Automatic web rule taken as an input.** [`route_notes::tool_route_notes`] does not run
//!   `tools-mode.cjs` `automaticBoxesFor` (Tools: Automatic); the caller passes whether it would
//!   add Web search for this message ([`route_notes::RouteInput::automatic_web`]). The recording
//!   takes that bit from the real `tools-mode.cjs` for every case.

pub mod announced;
pub mod final_answer;
pub mod js_regex;
pub mod route_notes;
pub mod search_query;

use js_json::wtf16;
use std::sync::LazyLock;

pub use announced::{announces_tool_action, ANNOUNCED_NOTE, TAIL_CHARS};
pub use final_answer::{
    fallback_text, needs_final_pass, source_links, FINAL_ANSWER_NOTE, LAST_TOOL_ROUND, MAX_SOURCES,
};
pub use route_notes::{tool_route_notes, RouteInput};
pub use search_query::{clean_search_args, clean_search_query};

/// The UTF-16 code units of a JS string held as `&str` (a mapped lone surrogate is its one unit).
pub fn units(text: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(text.len());
    let mut buf = [0u16; 2];
    for c in text.chars() {
        match wtf16::lone_unit(c) {
            Some(u) => out.push(u),
            None => out.extend_from_slice(c.encode_utf16(&mut buf)),
        }
    }
    out
}

/// The `&str` form of UTF-16 code units: pairs as characters, lone surrogates mapped.
pub fn text(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|r| {
            r.unwrap_or_else(|e| {
                wtf16::lone_char(e.unpaired_surrogate()).unwrap_or(char::REPLACEMENT_CHARACTER)
            })
        })
        .collect()
}

/// A constant pattern, compiled on first use. `None` only if the pattern were outside the
/// supported subset; `tests` compile every one, and a `None` pattern matches nothing.
type Pattern = LazyLock<Option<js_regex::Regex>>;

fn test(re: &Pattern, text: &[u16]) -> bool {
    re.as_ref().is_some_and(|re| re.test(text))
}

/// Whether every constant pattern compiled (a pattern that did not would match nothing).
pub fn patterns_compile() -> bool {
    search_query::patterns_compile()
        && announced::patterns_compile()
        && final_answer::patterns_compile()
        && route_notes::patterns_compile()
}
