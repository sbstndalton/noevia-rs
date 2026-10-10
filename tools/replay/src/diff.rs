//! Comparing what a server answered with what the corpus recorded.

use crate::bind::{has_placeholder, tokens, Bindings, Tok};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

/// One difference, located by a JSON-pointer-like path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub at: String,
    pub expected: String,
    pub actual: String,
}

impl Diff {
    fn new(at: &str, expected: impl Into<String>, actual: impl Into<String>) -> Self {
        Self {
            at: at.to_string(),
            expected: expected.into(),
            actual: actual.into(),
        }
    }
}

fn show(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 200 {
        let cut: String = s.chars().take(200).collect();
        format!("{cut}...")
    } else {
        s
    }
}

/// A recorded string that is a single placeholder (`"<ts>"`, `"<secret:3>"`) also stands for a
/// number: the recorder writes epoch-ms time fields and numeric PINs that way.
fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Diffs a recorded (normalised) JSON value against a live one, binding placeholders.
pub fn json(expected: &Value, actual: &Value, at: &str, b: &mut Bindings, out: &mut Vec<Diff>) {
    match (expected, actual) {
        (Value::String(e), _) if has_placeholder(e) => {
            let single = matches!(tokens(e).as_slice(), [Tok::Var(_)] | [Tok::Any]);
            let text = match actual {
                Value::String(s) => Some(s.clone()),
                other if single => scalar_text(other),
                _ => None,
            };
            match text {
                Some(t) if b.matches(e, &t) => {}
                _ => out.push(Diff::new(at, e.clone(), b.unbind(&show(actual)))),
            }
        }
        (Value::Object(e), Value::Object(a)) => object(e, a, at, b, out),
        (Value::Array(e), Value::Array(a)) => {
            if e.len() != a.len() {
                out.push(Diff::new(
                    at,
                    format!("array of {}", e.len()),
                    format!("array of {}", a.len()),
                ));
            }
            for (i, (ev, av)) in e.iter().zip(a.iter()).enumerate() {
                json(ev, av, &format!("{at}/{i}"), b, out);
            }
        }
        (Value::Number(e), Value::Number(a)) => {
            if e.as_f64() != a.as_f64() {
                out.push(Diff::new(at, e.to_string(), a.to_string()));
            }
        }
        (e, a) => {
            if e != a {
                out.push(Diff::new(at, show(e), b.unbind(&show(a))));
            }
        }
    }
}

fn object(
    e: &Map<String, Value>,
    a: &Map<String, Value>,
    at: &str,
    b: &mut Bindings,
    out: &mut Vec<Diff>,
) {
    let mut claimed: BTreeSet<&str> = BTreeSet::new();
    // Plain keys first, so a placeholder key cannot claim a key that is literally expected.
    let (plain, templated): (Vec<_>, Vec<_>) = e.iter().partition(|(k, _)| !has_placeholder(k));
    for (k, ev) in plain {
        let path = format!("{at}/{k}");
        match a.get(k.as_str()) {
            Some(av) => {
                claimed.insert(k.as_str());
                json(ev, av, &path, b, out);
            }
            None => out.push(Diff::new(&path, show(ev), "(missing)")),
        }
    }
    for (k, ev) in templated {
        let path = format!("{at}/{k}");
        let found = a
            .iter()
            .find(|(ak, _)| !claimed.contains(ak.as_str()) && b.clone().matches(k, ak))
            .map(|(ak, av)| (ak.as_str(), av));
        match found {
            Some((ak, av)) => {
                b.matches(k, ak);
                claimed.insert(ak);
                json(ev, av, &path, b, out);
            }
            None => out.push(Diff::new(&path, show(ev), "(missing)")),
        }
    }
    for (k, av) in a {
        if !claimed.contains(k.as_str()) {
            out.push(Diff::new(
                &format!("{at}/{}", b.unbind(k)),
                "(absent)",
                b.unbind(&show(av)),
            ));
        }
    }
}

/// Response headers the recorder drops and the replay therefore ignores.
pub const VOLATILE_HEADERS: &[&str] = &[
    "date",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "content-length",
    "etag",
    "last-modified",
    "age",
    "server-timing",
    "x-response-time",
];

/// Diffs recorded response headers (lower-case name -> string or array of strings) against live
/// ones. Live headers the corpus does not have are differences too, unless ignored.
pub fn headers(
    expected: &Map<String, Value>,
    actual: &[(String, String)],
    ignore: &[String],
    b: &mut Bindings,
    out: &mut Vec<Diff>,
) {
    let skip = |n: &str| VOLATILE_HEADERS.contains(&n) || ignore.iter().any(|i| i == n);
    for (name, ev) in expected {
        if skip(name) {
            continue;
        }
        let at = format!("header/{name}");
        let live: Vec<&str> = actual
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect();
        let want: Vec<String> = match ev {
            Value::Array(items) => items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect(),
            Value::String(s) => vec![s.clone()],
            other => vec![other.to_string()],
        };
        // Node folds a repeated header other than set-cookie into one comma-joined value.
        let live: Vec<String> = if live.len() > 1 && name != "set-cookie" && want.len() == 1 {
            vec![live.join(", ")]
        } else {
            live.iter().map(|s| s.to_string()).collect()
        };
        if live.is_empty() {
            out.push(Diff::new(&at, want.join(" | "), "(missing)"));
            continue;
        }
        if want.len() != live.len() {
            out.push(Diff::new(
                &at,
                want.join(" | "),
                b.unbind(&live.join(" | ")),
            ));
            continue;
        }
        for (w, l) in want.iter().zip(live.iter()) {
            if !b.matches(w, l) {
                out.push(Diff::new(&at, w.clone(), b.unbind(l)));
            }
        }
    }
    let mut extra: BTreeSet<&str> = BTreeSet::new();
    for (n, _) in actual {
        if !skip(n) && !expected.contains_key(n.as_str()) {
            extra.insert(n.as_str());
        }
    }
    for n in extra {
        out.push(Diff::new(&format!("header/{n}"), "(absent)", "present"));
    }
}

/// One server-sent event as the recorder writes it.
pub fn parse_sse(text: &str) -> Vec<Value> {
    let mut events = Vec::new();
    let normalised = text.replace("\r\n", "\n");
    for block in normalised.split("\n\n") {
        if block.trim().is_empty() {
            continue;
        }
        let mut ev = Map::new();
        let mut data: Vec<&str> = Vec::new();
        let mut comment = false;
        for line in block.split('\n') {
            if line.starts_with(':') {
                comment = true;
                continue;
            }
            let (field, val) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line, ""),
            };
            match field {
                "data" => data.push(val),
                "event" => {
                    ev.insert("event".into(), Value::String(val.to_string()));
                }
                "id" => {
                    ev.insert("id".into(), Value::String(val.to_string()));
                }
                "retry" => {
                    let n = val
                        .trim()
                        .parse::<u64>()
                        .map(Value::from)
                        .unwrap_or(Value::Null);
                    ev.insert("retry".into(), n);
                }
                _ => {}
            }
        }
        if !data.is_empty() {
            let joined = data.join("\n");
            match serde_json::from_str::<Value>(&joined) {
                Ok(v) => {
                    ev.insert("data".into(), v);
                    ev.insert("json".into(), Value::Bool(true));
                }
                Err(_) => {
                    ev.insert("data".into(), Value::String(joined));
                }
            }
        }
        if comment && ev.is_empty() {
            continue;
        }
        events.push(Value::Object(ev));
    }
    events
}

/// Diffs a recorded event sequence against a live stream body.
pub fn sse(expected: &[Value], actual_body: &str, b: &mut Bindings, out: &mut Vec<Diff>) {
    let live = parse_sse(actual_body);
    if expected.len() != live.len() {
        out.push(Diff::new(
            "sse",
            format!("{} events", expected.len()),
            format!("{} events", live.len()),
        ));
    }
    for (i, (e, a)) in expected.iter().zip(live.iter()).enumerate() {
        json(e, a, &format!("sse/{i}"), b, out);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(e: Value, a: Value, b: &mut Bindings) -> Vec<Diff> {
        let mut out = Vec::new();
        json(&e, &a, "", b, &mut out);
        out
    }

    #[test]
    fn equal_bodies_with_placeholders_bind_and_pass() {
        let mut b = Bindings::new("");
        let e = json!({"project": {"id": "<id:1>", "createdAt": "<ts>", "name": "Synthetic"}, "csrfToken": "<secret:2>", "pin": "<secret:3>"});
        let a = json!({"project": {"id": "p-123", "createdAt": 1760000000000_u64, "name": "Synthetic"}, "csrfToken": "tok", "pin": 4242});
        assert!(d(e, a, &mut b).is_empty());
        assert_eq!(b.get("<id:1>"), Some("p-123"));
        assert_eq!(b.get("<secret:3>"), Some("4242"));
    }

    #[test]
    fn reports_changed_missing_and_extra_fields() {
        let mut b = Bindings::new("");
        let out = d(
            json!({"a": 1, "b": "x", "c": [1, 2]}),
            json!({"a": 2, "c": [1], "z": true}),
            &mut b,
        );
        let at: Vec<&str> = out.iter().map(|x| x.at.as_str()).collect();
        assert_eq!(at, vec!["/a", "/b", "/c", "/z"]);
    }

    #[test]
    fn a_bound_id_must_repeat() {
        let mut b = Bindings::new("");
        assert!(d(json!({"id": "<id:1>"}), json!({"id": "p1"}), &mut b).is_empty());
        let out = d(
            json!({"projectId": "<id:1>"}),
            json!({"projectId": "p2"}),
            &mut b,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out.first().unwrap().expected, "<id:1>");
    }

    #[test]
    fn placeholder_keys_match_unclaimed_live_keys() {
        let mut b = Bindings::new("");
        b.bind("<id:1>", "aaaaaaaa1");
        let out = d(
            json!({"<id:1>": 1, "<id:2>": 2, "plain": 3}),
            json!({"aaaaaaaa1": 1, "bbbbbbbb2": 2, "plain": 3}),
            &mut b,
        );
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(b.get("<id:2>"), Some("bbbbbbbb2"));
    }

    #[test]
    fn headers_compare_set_cookie_lists_and_flag_extras() {
        let mut b = Bindings::new("");
        let e = json!({"content-type": "application/json", "set-cookie": ["s=<secret:1>; Path=/", "c=<secret:2>; Path=/"], "date": "<ts>"});
        let live = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("set-cookie".to_string(), "s=one; Path=/".to_string()),
            ("set-cookie".to_string(), "c=two; Path=/".to_string()),
            ("date".to_string(), "whenever".to_string()),
            ("x-new".to_string(), "1".to_string()),
        ];
        let mut out = Vec::new();
        headers(e.as_object().unwrap(), &live, &[], &mut b, &mut out);
        assert_eq!(out, vec![Diff::new("header/x-new", "(absent)", "present")]);
        assert_eq!(b.get("<secret:2>"), Some("two"));
        let mut out = Vec::new();
        headers(
            e.as_object().unwrap(),
            &live,
            &["x-new".to_string()],
            &mut b,
            &mut out,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn sse_matches_the_recorder_parse() {
        let body = ": ping\n\ndata: {\"type\":\"start\",\"chatId\":\"c1\"}\n\nevent: delta\ndata: {\"text\":\"Hel\"}\n\ndata: [DONE]\n\n";
        let expected = vec![
            json!({"data": {"type": "start", "chatId": "<id:4>"}, "json": true}),
            json!({"event": "delta", "data": {"text": "Hel"}, "json": true}),
            json!({"data": "[DONE]"}),
        ];
        let mut b = Bindings::new("");
        let mut out = Vec::new();
        sse(&expected, body, &mut b, &mut out);
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(b.get("<id:4>"), Some("c1"));
        let mut out = Vec::new();
        sse(&expected, "data: [DONE]\n\n", &mut b, &mut out);
        assert_eq!(out.first().unwrap().at, "sse");
    }
}
