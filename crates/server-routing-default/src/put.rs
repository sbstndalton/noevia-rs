//! Native `PUT /api/routing-default` (track R12): core `server/routes/models.cjs` PUT, the routing new
//! projects start in, and on request (`applyToExisting: true`) the same routing for every project of
//! the account.
//!
//! In Node's order: the body read up to 1 MiB and `JSON.parse`d (an oversize body is the same
//! `invalid JSON` 400 there, since the read sits inside the parse's `try`); `routing` must be exactly
//! `"auto"` or `"manual"`; `preferences.json` becomes `{ ...preferences, defaultRouting }`
//! (`atomicJson`: `JSON.stringify(value, null, 2)`, mode 0600, a rename); then, with
//! `applyToExisting === true`, each project whose `routing` differs gets the routing and
//! `updatedAt = now`, and the answer is `{ routing, updated }`.
//!
//! This module is the pure part: [`parse`], [`merged_preferences`], [`apply`] and [`answer`], plus
//! [`crate::Root::write_preferences`]. The front holds the projects lock (server-projects
//! `store::update`) around [`apply`].
//!
//! Differences from Node, each fail-closed or stricter, none on a value Node could have written:
//! - `preferences.json` is read at the write, not taken from the copy Node cached when it first
//!   loaded the account. Nothing else writes the file, so the two agree.
//! - A file Node reads but this port cannot hold (a number outside the `f64` range, a character of
//!   the private range lone surrogates are held in, nesting past 128) refuses the write (503)
//!   instead of rewriting it; a symlinked or hard-linked or non-regular file too.
//! - Projects are written as stored, with only `routing` and `updatedAt` changed: Node's load
//!   migrations of legacy shapes (the `lemonade` provider id, `modes` without Chat first, inline
//!   source text) ride along in Node's save. Every native projects writer leaves them the same way,
//!   and every reader applies them on read.
//! - A projects save that fails answers 503. Node answers 200 before its unawaited save fails.
//! - The body must arrive within 10 s (the front's deadline); a slower one is the same 400
//!   `invalid JSON` a body that fails to arrive is in Node.
//! - A projects list with an entry that is not an object (hand-edited): Node's workspace load throws
//!   on a `null` entry for every request; with a number, string or boolean entry Node writes the
//!   preferences and then throws (a 500); an array entry is counted and saved. Here `applyToExisting`
//!   answers 500 with nothing written for all of them, and a PUT without it writes the preferences (it
//!   never reads the list).
//! - With `applyToExisting`, the preferences are written under the projects.json lock, so two PUTs of
//!   one account never leave the preferences of one and the projects of the other.

use crate::Routing;
use js_json::{wtf16, JValue};

/// http.cjs `readBody`'s default limit.
pub const BODY_LIMIT: usize = 1024 * 1024;

pub const INVALID_JSON: &str = "invalid JSON";
pub const BAD_ROUTING: &str = "routing must be 'auto' or 'manual'";

/// A valid request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Put {
    pub routing: Routing,
    /// `body.applyToExisting === true`.
    pub apply_to_existing: bool,
}

/// Why the request is refused (both are 400s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The body was over [`BODY_LIMIT`], or `JSON.parse` throws on it.
    InvalidJson,
    /// `body?.routing` is not exactly `"auto"` or `"manual"`.
    BadRouting,
}

impl Refusal {
    pub fn message(self) -> &'static str {
        match self {
            Refusal::InvalidJson => INVALID_JSON,
            Refusal::BadRouting => BAD_ROUTING,
        }
    }
}

/// `JSON.parse(await readBody(req))` and the routing check. `over` is true when the body went past
/// [`BODY_LIMIT`]. The bytes are decoded as `Buffer.toString('utf8')` decodes them.
pub fn parse(body: &[u8], over: bool) -> Result<Put, Refusal> {
    if over {
        return Err(Refusal::InvalidJson);
    }
    let text = String::from_utf8_lossy(body);
    // A body JSON.parse accepts but this port cannot hold (a number past the f64 range, a deep
    // nesting) is refused the same way: the routing in it is never guessed.
    let value = wtf16::parse(&text).map_err(|_| Refusal::InvalidJson)?;
    let routing = match value.get("routing").as_str() {
        Some("auto") => Routing::Auto,
        Some("manual") => Routing::Manual,
        _ => return Err(Refusal::BadRouting),
    };
    Ok(Put {
        routing,
        apply_to_existing: value.get("applyToExisting") == &JValue::Bool(true),
    })
}

/// Why the stored preferences cannot be rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unholdable;

/// `{ ...(readJson(file, {}) || {}), defaultRouting: routing }` as `atomicJson` writes it.
/// `stored` is the file's bytes (`None`: no file). Node reads it with `fs.readFileSync(file, 'utf8')`
/// (a BOM stays and fails the parse, invalid UTF-8 becomes U+FFFD); a parse failure is `{}`.
pub fn merged_preferences(stored: Option<&[u8]>, routing: Routing) -> Result<String, Unholdable> {
    let base = match stored {
        None => JValue::Undefined,
        Some(bytes) => match wtf16::parse(&String::from_utf8_lossy(bytes)) {
            Ok(v) => v,
            Err(wtf16::Refusal::Invalid) => JValue::Undefined,
            Err(wtf16::Refusal::Unsupported) => return Err(Unholdable),
        },
    };
    let mut items = spread(&base);
    wtf16::set(
        &mut items,
        "defaultRouting",
        JValue::Str(routing.as_str().to_string()),
    );
    wtf16::stringify_pretty(&JValue::Obj(items)).ok_or(Unholdable)
}

/// The own enumerable properties `{ ...value }` copies: an object's entries (already in JS order),
/// an array's indexes, a string's UTF-16 units (a surrogate pair splits into two lone halves);
/// nothing for a number, a boolean, `null` or `undefined` (and `|| {}` turns every falsy value into
/// `{}`, which spreads to nothing as well).
fn spread(value: &JValue) -> Vec<(String, JValue)> {
    match value {
        JValue::Obj(items) => items.clone(),
        JValue::Arr(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect(),
        JValue::Str(s) => units(s)
            .into_iter()
            .enumerate()
            .map(|(i, u)| (i.to_string(), JValue::Str(u)))
            .collect(),
        _ => Vec::new(),
    }
}

/// Each UTF-16 unit of `s` as a one-unit JS string (a lone surrogate held as its mapped character).
fn units(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for c in s.chars() {
        if wtf16::lone_unit(c).is_some() || c.len_utf16() == 1 {
            out.push(c.to_string());
            continue;
        }
        let mut pair = [0u16; 2];
        for unit in c.encode_utf16(&mut pair).iter() {
            // A surrogate pair's halves are always surrogates, so this always maps.
            if let Some(half) = wtf16::lone_char(*unit) {
                out.push(half.to_string());
            }
        }
    }
    out
}

/// A project that is not an object: Node cannot load such a list (its load reads `project.provider`
/// of every entry), so the route never ran there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotAProject;

/// `for (const project of ws.projects) if (project.routing !== routing) { project.routing = routing;
/// project.updatedAt = now; updated++; }`: the number of projects changed.
pub fn apply(projects: &mut [JValue], routing: Routing, now_ms: i64) -> Result<usize, NotAProject> {
    if projects.iter().any(|p| !matches!(p, JValue::Obj(_))) {
        return Err(NotAProject);
    }
    let mut updated = 0usize;
    for project in projects.iter_mut() {
        if project.get("routing").as_str() == Some(routing.as_str()) {
            continue;
        }
        if let JValue::Obj(items) = project {
            wtf16::set(items, "routing", JValue::Str(routing.as_str().to_string()));
            wtf16::set(items, "updatedAt", JValue::Num(now_ms as f64));
            updated += 1;
        }
    }
    Ok(updated)
}

/// `json(res, 200, { routing, updated })`.
pub fn answer(routing: Routing, updated: usize) -> String {
    format!(
        "{{\"routing\":\"{}\",\"updated\":{updated}}}",
        routing.as_str()
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn body_rules() {
        assert_eq!(
            parse(br#"{"routing":"manual","applyToExisting":true}"#, false),
            Ok(Put {
                routing: Routing::Manual,
                apply_to_existing: true
            })
        );
        assert_eq!(
            parse(br#"{"routing":"auto","applyToExisting":1}"#, false),
            Ok(Put {
                routing: Routing::Auto,
                apply_to_existing: false
            })
        );
        assert_eq!(parse(b"", false), Err(Refusal::InvalidJson));
        assert_eq!(parse(b"null", false), Err(Refusal::BadRouting));
        assert_eq!(
            parse(br#"{"routing":"Manual"}"#, false),
            Err(Refusal::BadRouting)
        );
        assert_eq!(
            parse(br#"{"routing":"auto"}"#, true),
            Err(Refusal::InvalidJson)
        );
    }

    #[test]
    fn spread_of_a_string_splits_pairs() {
        let out = merged_preferences(Some("\"a😀\"".as_bytes()), Routing::Auto).unwrap();
        assert_eq!(
            out,
            "{\n  \"0\": \"a\",\n  \"1\": \"\\ud83d\",\n  \"2\": \"\\ude00\",\n  \"defaultRouting\": \"auto\"\n}"
        );
    }

    #[test]
    fn apply_counts_and_refuses_non_objects() {
        let mut list = vec![
            js_json::wtf16::parse(r#"{"id":"a","routing":"auto"}"#).unwrap(),
            js_json::wtf16::parse(r#"{"id":"b"}"#).unwrap(),
        ];
        assert_eq!(apply(&mut list, Routing::Auto, 5), Ok(1));
        assert_eq!(list[1].get("updatedAt"), &JValue::Num(5.0));
        let mut bad = vec![JValue::Null];
        assert_eq!(apply(&mut bad, Routing::Auto, 5), Err(NotAProject));
    }
}
