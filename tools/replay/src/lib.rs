//! Replays a noevia HTTP contract corpus (recorded by noevia-core with NOEVIA_CONTRACT_RECORD)
//! against a server and diffs what it answers: status, headers, JSON bodies, SSE event sequences,
//! and optionally the resulting data directory (file tree and cowork.db tables).
//!
//! This is the gate the full-Rust migration slices pass: a route moves from owner "node" to
//! "rust" in contracts/http/routes.toml when its exchanges replay clean against the Rust server.

pub mod bind;
pub mod corpus;
pub mod diff;
pub mod http;
pub mod routes;
pub mod state;

use bind::{has_placeholder, Bindings};
use corpus::{Corpus, Exchange, Input};
use diff::Diff;
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::time::Duration;

static NULL: Value = Value::Null;

/// `v[k]` without the panic lint: a missing key or index reads as null.
pub fn at<I: serde_json::value::Index>(v: &Value, k: I) -> &Value {
    v.get(k).unwrap_or(&NULL)
}

#[derive(Debug, Clone)]
pub struct Options {
    pub base: http::Base,
    pub timeout: Duration,
    /// Response headers to leave out of the comparison (lower-case).
    pub ignore_headers: Vec<String>,
    /// JSON body paths (`hours`, `peakHour/hour`) to leave out of the comparison: values that
    /// depend on the wall clock at replay time, not on the server's behaviour.
    pub ignore_body: Vec<String>,
    /// The server's data directory, for file inputs (the first-run setup code).
    pub data_dir: Option<PathBuf>,
    /// Text the `<ts>` placeholder becomes in a request.
    pub now: String,
}

#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub file: String,
    pub method: String,
    pub path: String,
    pub expected_status: u64,
    pub actual_status: Option<u16>,
    pub diffs: Vec<Diff>,
    /// The exchange could not be sent (an unbound placeholder, a connection error).
    pub error: Option<String>,
    /// Placeholders this exchange's request had to invent (no recorded or input value).
    pub generated: Vec<String>,
}

impl Outcome {
    pub fn clean(&self) -> bool {
        self.error.is_none() && self.diffs.is_empty()
    }
}

/// Fills placeholders for a request, falling back on manifest inputs and, for a value no response
/// ever revealed (a wrong password typed on purpose, a client-made id), a generated stand-in.
struct Filler<'a> {
    b: &'a mut Bindings,
    inputs: &'a [(String, Input)],
    data_dir: Option<&'a PathBuf>,
    now: &'a str,
    generated: Vec<String>,
}

impl Filler<'_> {
    fn fill(&mut self, template: &str) -> Result<String, String> {
        for _ in 0..64 {
            match self.b.fill(template, self.now) {
                Ok(s) => return Ok(s),
                Err(var) => self.resolve(&var)?,
            }
        }
        Err(format!("too many placeholders in {template:?}"))
    }

    fn resolve(&mut self, var: &str) -> Result<(), String> {
        let value = match self.inputs.iter().find(|(k, _)| k == var).map(|(_, i)| i) {
            Some(Input::Value(v)) => v.clone(),
            Some(Input::File(rel)) => {
                let dir = self.data_dir.ok_or_else(|| {
                    format!(
                        "{var} is read from {} in the data dir; pass --data-dir or --seed",
                        rel.display()
                    )
                })?;
                std::fs::read_to_string(dir.join(rel))
                    .map_err(|e| format!("{var}: read {}: {e}", rel.display()))?
                    .trim()
                    .to_string()
            }
            None => {
                let n: String = var.chars().filter(char::is_ascii_digit).collect();
                self.generated.push(var.to_string());
                if var.starts_with("<id:") {
                    format!("00000000-0000-4000-8000-{n:0>12}")
                } else {
                    format!("replay-generated-secret-{n}")
                }
            }
        };
        self.b.bind(var, &value);
        Ok(())
    }

    fn json(&mut self, v: &Value) -> Result<Value, String> {
        Ok(match v {
            Value::String(s) if has_placeholder(s) => Value::String(self.fill(s)?),
            Value::Array(a) => {
                Value::Array(a.iter().map(|x| self.json(x)).collect::<Result<_, _>>()?)
            }
            Value::Object(m) => {
                let mut out = Map::new();
                for (k, x) in m {
                    let key = if has_placeholder(k) {
                        self.fill(k)?
                    } else {
                        k.clone()
                    };
                    out.insert(key, self.json(x)?);
                }
                Value::Object(out)
            }
            other => other.clone(),
        })
    }
}

struct Built {
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn build(ex: &Exchange, f: &mut Filler<'_>) -> Result<Built, String> {
    let req = ex.request();
    let mut target = String::new();
    for (i, seg) in ex.path().split('/').enumerate() {
        if i > 0 {
            target.push('/');
        }
        if has_placeholder(seg) {
            target.push_str(&http::encode(&f.fill(seg)?));
        } else {
            target.push_str(seg);
        }
    }
    if let Some(q) = at(req, "query").as_array().filter(|q| !q.is_empty()) {
        let mut parts = Vec::new();
        for pair in q {
            let (Some(k), Some(v)) = (at(pair, 0).as_str(), at(pair, 1).as_str()) else {
                return Err(format!("{}: malformed query pair", ex.file));
            };
            parts.push(format!("{}={}", http::encode(k), http::encode(&f.fill(v)?)));
        }
        target.push('?');
        target.push_str(&parts.join("&"));
    }
    let mut headers = Vec::new();
    if let Some(h) = at(req, "headers").as_object() {
        for (k, v) in h {
            let values: Vec<&str> = match v {
                Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
                Value::String(s) => vec![s.as_str()],
                _ => Vec::new(),
            };
            for val in values {
                headers.push((k.clone(), f.fill(val)?));
            }
        }
    }
    let body = &at(req, "body");
    let bytes = match at(body, "kind").as_str().unwrap_or("empty") {
        "empty" => Vec::new(),
        "json" => serde_json::to_vec(&f.json(at(body, "json"))?).map_err(|e| e.to_string())?,
        "text" => f
            .fill(at(body, "text").as_str().unwrap_or(""))?
            .into_bytes(),
        "form" => {
            let mut parts = Vec::new();
            for pair in at(body, "form").as_array().into_iter().flatten() {
                let (Some(k), Some(v)) = (at(pair, 0).as_str(), at(pair, 1).as_str()) else {
                    continue;
                };
                parts.push(format!("{}={}", http::encode(k), http::encode(&f.fill(v)?)));
            }
            parts.join("&").into_bytes()
        }
        other => {
            return Err(format!(
                "request body of kind {other} was not recorded, so it cannot be replayed"
            ))
        }
    };
    Ok(Built {
        target,
        headers,
        body: bytes,
    })
}

/// True when a diff location (`body/hours/3`) is at or under an ignored body path.
pub fn body_ignored(at: &str, ignore: &[String]) -> bool {
    ignore.iter().any(|p| {
        let p = format!("body/{}", p.trim_matches('/'));
        at == p || at.starts_with(&format!("{p}/"))
    })
}

fn compare(
    ex: &Exchange,
    res: &http::Response,
    opts: &Options,
    b: &mut Bindings,
    out: &mut Vec<Diff>,
) {
    let want = ex.response();
    let status = at(want, "status").as_u64().unwrap_or(0);
    if u64::from(res.status) != status {
        out.push(Diff {
            at: "status".into(),
            expected: status.to_string(),
            actual: res.status.to_string(),
        });
    }
    if let Some(h) = at(want, "headers").as_object() {
        diff::headers(h, &res.headers, &opts.ignore_headers, b, out);
    }
    let body = &at(want, "body");
    let text = String::from_utf8_lossy(&res.body);
    match at(body, "kind").as_str().unwrap_or("empty") {
        "empty" => {
            if !res.body.is_empty() {
                out.push(Diff {
                    at: "body".into(),
                    expected: "(empty)".into(),
                    actual: format!("{} bytes", res.body.len()),
                });
            }
        }
        "json" => match serde_json::from_slice::<Value>(&res.body) {
            Ok(actual) => diff::json(at(body, "json"), &actual, "body", b, out),
            Err(e) => out.push(Diff {
                at: "body".into(),
                expected: "JSON".into(),
                actual: format!("not JSON ({e})"),
            }),
        },
        "sse" => diff::sse(
            at(body, "events")
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            &text,
            b,
            out,
        ),
        "text" => {
            let t = at(body, "text").as_str().unwrap_or("");
            if !b.matches(t, &text) {
                out.push(Diff {
                    at: "body".into(),
                    expected: t.chars().take(200).collect(),
                    actual: b.unbind(&text.chars().take(200).collect::<String>()),
                });
            }
        }
        "bytes" | "truncated" => {
            if res.body.is_empty() {
                out.push(Diff {
                    at: "body".into(),
                    expected: "a body".into(),
                    actual: "(empty)".into(),
                });
            }
        }
        other => out.push(Diff {
            at: "body".into(),
            expected: format!("kind {other}"),
            actual: "(unsupported)".into(),
        }),
    }
}

/// Replays one exchange: build the request from the recording and the bindings so far, send it,
/// diff the answer (binding new placeholders as it goes).
pub fn replay_one(ex: &Exchange, corpus: &Corpus, opts: &Options, b: &mut Bindings) -> Outcome {
    let mut outcome = Outcome {
        file: ex.file.clone(),
        method: ex.method().to_string(),
        path: ex.path().to_string(),
        expected_status: at(ex.response(), "status").as_u64().unwrap_or(0),
        ..Outcome::default()
    };
    let mut filler = Filler {
        b,
        inputs: &corpus.inputs,
        data_dir: opts.data_dir.as_ref(),
        now: &opts.now,
        generated: Vec::new(),
    };
    let built = build(ex, &mut filler);
    outcome.generated = std::mem::take(&mut filler.generated);
    let built = match built {
        Ok(x) => x,
        Err(e) => {
            outcome.error = Some(e);
            return outcome;
        }
    };
    match http::send(
        &opts.base,
        ex.method(),
        &built.target,
        &built.headers,
        &built.body,
        opts.timeout,
    ) {
        Ok(res) => {
            outcome.actual_status = Some(res.status);
            compare(ex, &res, opts, b, &mut outcome.diffs);
            outcome
                .diffs
                .retain(|d| !body_ignored(&d.at, &opts.ignore_body));
        }
        Err(e) => outcome.error = Some(e),
    }
    outcome
}

/// Replays the whole corpus in recorded order.
pub fn replay(
    corpus: &Corpus,
    opts: &Options,
    b: &mut Bindings,
    stop_on_first: bool,
) -> Vec<Outcome> {
    let mut out = Vec::new();
    for ex in &corpus.exchanges {
        let o = replay_one(ex, corpus, opts, b);
        let stop = stop_on_first && !o.clean();
        out.push(o);
        if stop {
            break;
        }
    }
    out
}
