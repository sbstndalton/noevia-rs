//! Replays `fixtures/node-model-pull.v1.json` (written by `record_model_pull.cjs` from routes/models.cjs's own
//! lines over the real managers, llamacpp-variants.cjs and mtp-artifact.cjs) through the crate: every status,
//! body, manager request, download tracker step and public fetch, in order.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::{wtf16, JValue};
use model_pull::artifact::{self, Artifacts, FetchError, Fetched, Fetcher, Start};
use model_pull::manager::{self, Config, Kind, Reply, Request, Target, Thrown};
use model_pull::{gguf, handle, mtp_selection, Call, Host, Outcome};
use std::collections::BTreeMap;
use std::sync::Mutex;

fn fixture() -> JValue {
    wtf16::parse(include_str!("fixtures/node-model-pull.v1.json")).expect("the fixture parses")
}

fn list<'a>(f: &'a JValue, key: &str) -> &'a [JValue] {
    match f.get(key) {
        JValue::Arr(items) if !items.is_empty() => items,
        _ => panic!("the fixture has no {key} cases"),
    }
}

fn items(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(items) => items,
        _ => &[],
    }
}

fn show(v: &JValue) -> String {
    let text = wtf16::stringify(v).unwrap_or_else(|| "undefined".into());
    text.chars().take(600).collect()
}

/// Standard base64 (the recorder's `toString('base64')`).
fn unbase64(text: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("base64"),
    };
    let digits: Vec<u8> = text.bytes().filter(|&c| c != b'=').map(value).collect();
    let mut out = Vec::new();
    for chunk in digits.chunks(4) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, d)| n | (u32::from(*d) << (18 - 6 * i)));
        out.extend(n.to_be_bytes().iter().skip(1).take(chunk.len() - 1));
    }
    out
}

/// The recorder's `fromParts`.
fn from_parts(parts: &JValue) -> Vec<u8> {
    let mut out = Vec::new();
    for p in items(parts) {
        match p.get("b64") {
            JValue::Str(b) => out.extend(unbase64(b)),
            _ => {
                let n = p.get("n").as_num().unwrap() as usize;
                let fill = p.get("fill").as_num().unwrap() as u8;
                out.resize(out.len() + n, fill);
            }
        }
    }
    out
}

/// The recorder's `padded(total)`.
fn padded(total: usize) -> Vec<u8> {
    let (head, tail) = (r#"{"checkpoint":"qa-org/qa-model:Q4_K_M","pad":""#, r#""}"#);
    let mut out = head.as_bytes().to_vec();
    out.resize(total - tail.len(), b'x');
    out.extend_from_slice(tail.as_bytes());
    out
}

fn request_bytes(c: &JValue) -> Option<Vec<u8>> {
    match (
        c.get("request"),
        c.get("requestBase64"),
        c.get("requestPadded"),
    ) {
        (JValue::Str(s), _, _) => Some(s.as_bytes().to_vec()),
        (_, JValue::Str(b), _) => Some(unbase64(b)),
        (_, _, JValue::Num(n)) => Some(padded(*n as usize)),
        _ => None,
    }
}

fn config(c: &JValue) -> Config {
    Config {
        kind: Kind::parse(c.get("kind").as_str()).unwrap(),
        base_url: c.get("base").as_str().unwrap_or("").to_string(),
        api_key: c.get("apiKey").as_str().map(str::to_string),
    }
}

/// The recorder's `respond`: a scripted fetch answer, read as the host would.
fn fetched(o: &JValue, max_bytes: usize) -> Result<Fetched, FetchError> {
    let failure = |kind: &str| match kind {
        "timeout" | "abort" => FetchError::Timeout,
        "network" => FetchError::Failed("fetch failed".into()),
        other => FetchError::Failed(other.into()),
    };
    if let JValue::Str(kind) = o.get("throw") {
        return Err(failure(kind));
    }
    let status = o.get("status").as_num().unwrap() as u16;
    let bytes = match (o.get("json"), o.get("text")) {
        (JValue::Undefined, JValue::Str(t)) => t.as_bytes().to_vec(),
        (JValue::Undefined, _) => from_parts(o.get("parts")),
        (json, _) => wtf16::stringify(json).unwrap().into_bytes(),
    };
    let body = match o.get("error") {
        JValue::Str(kind) if bytes.len() < max_bytes => Err(failure(kind)),
        _ => Ok(bytes.into_iter().take(max_bytes).collect()),
    };
    Ok(Fetched {
        status,
        content_range: o.get("contentRange").as_str().map(str::to_string),
        body,
    })
}

fn reply_of(next: &JValue) -> Result<Reply, Thrown> {
    if let JValue::Obj(_) = next.get("throw") {
        let t = next.get("throw");
        return Err(Thrown {
            status: t.get("status").as_num(),
            message: t.get("message").as_str().unwrap().to_string(),
        });
    }
    let r = next.get("reply");
    Ok(Reply {
        ok: r.get("ok").as_bool().unwrap(),
        status: r.get("status").as_num().unwrap() as u16,
        body: r.get("body").clone(),
    })
}

/// A host over a case's scripted answers, logging what Node's stubs log.
struct Stub<'a> {
    script: Vec<JValue>,
    hf: &'a JValue,
    now: i64,
    sse: String,
    barrier: Option<String>,
    calls: Vec<JValue>,
    fetches: Vec<JValue>,
    invalidated: f64,
    scan_cleared: f64,
    entered: i32,
    requested: Option<JValue>,
    rejected: bool,
}

impl<'a> Stub<'a> {
    fn new(case: &JValue, hf: &'a JValue, now: i64) -> Stub<'a> {
        let config = config(case);
        Stub {
            script: items(case.get("script")).to_vec(),
            hf,
            now,
            sse: format!("{}/models/sse", config.base()),
            barrier: case.get("barrier").as_str().map(str::to_string),
            calls: Vec::new(),
            fetches: Vec::new(),
            invalidated: 0.0,
            scan_cleared: 0.0,
            entered: 0,
            requested: None,
            rejected: false,
        }
    }
}

impl Fetcher for Stub<'_> {
    fn fetch(&mut self, request: &artifact::Fetch) -> Result<Fetched, FetchError> {
        let mut entry = vec![("url".to_string(), JValue::from(request.url.as_str()))];
        if let Some(range) = request.range {
            entry.push(("range".to_string(), JValue::from(range)));
        }
        entry.push(("signal".to_string(), JValue::Bool(true)));
        self.fetches.push(JValue::Obj(entry));
        let o = self.hf.get(&request.url);
        assert!(o.is_object(), "an unscripted fetch {}", request.url);
        fetched(o, request.max_bytes)
    }

    fn now_ms(&self) -> i64 {
        self.now
    }
}

impl Host for Stub<'_> {
    fn invalidate_model_reads(&mut self) {
        self.invalidated += 1.0;
    }

    fn clear_model_scan(&mut self) {
        self.scan_cleared += 1.0;
    }

    fn send(&mut self, request: &Request) -> Result<Reply, Thrown> {
        let hub = request.url.starts_with("https://huggingface.co/");
        assert_eq!(request.target == Target::PublicHub, hub, "{request:?}");
        if hub {
            assert!(request.headers.is_empty());
        }
        let mut send = vec![
            ("url".to_string(), JValue::from(request.url.as_str())),
            ("method".to_string(), JValue::from(request.method)),
            (
                "headers".to_string(),
                JValue::obj(
                    request
                        .headers
                        .iter()
                        .map(|(k, v)| (k.clone(), JValue::from(v.as_str()))),
                ),
            ),
        ];
        if let Some(body) = &request.body {
            send.push(("body".to_string(), JValue::from(body.as_str())));
        }
        send.push((
            "timeout".to_string(),
            JValue::Num(f64::from(request.timeout_ms)),
        ));
        self.calls.push(JValue::obj([("send", JValue::Obj(send))]));
        assert!(
            !self.script.is_empty(),
            "an unscripted manager call {}",
            request.url
        );
        reply_of(&self.script.remove(0))
    }

    fn enter(&mut self) -> Result<(), Thrown> {
        if let Some(reason) = &self.barrier {
            return Err(Thrown {
                status: Some(503.0),
                message: reason.clone(),
            });
        }
        self.entered += 1;
        Ok(())
    }

    fn leave(&mut self) {
        self.entered -= 1;
        assert!(self.entered >= 0, "left more often than entered");
    }

    fn downloads_requested(&mut self, checkpoint: &JValue) {
        assert!(self.requested.is_none());
        self.requested = Some(checkpoint.clone());
        self.calls
            .push(JValue::obj([("sse", JValue::from(self.sse.as_str()))]));
    }

    fn downloads_rejected(&mut self, checkpoint: &JValue) {
        assert_eq!(self.requested.as_ref(), Some(checkpoint));
        self.rejected = true;
    }
}

fn outcome_parts(out: &Outcome) -> (Option<u16>, JValue) {
    match out {
        Outcome::Answer { status, body } => (Some(*status), body.clone()),
        Outcome::Pass => (None, JValue::Undefined),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn the_fixture_names_its_sources_and_the_cut_lines() {
    let f = fixture();
    for file in [
        "server/routes/models.cjs",
        "server/models.cjs",
        "server/mtp-artifact.cjs",
        "server/http.cjs",
        "server/model-manager.cjs",
        "server/llamacpp-manager.cjs",
        "server/llamacpp-variants.cjs",
    ] {
        assert_eq!(
            f.get("sources").get(file).as_str().map(str::len),
            Some(64),
            "{file}"
        );
    }
    for block in ["prelude", "scan", "mtp", "pull", "run", "derive"] {
        assert!(f.get("blocks").get(block).is_object(), "{block}");
    }
    assert_eq!(f.get("limit"), &JValue::Num(gguf::LIMIT as f64));
    let names: Vec<_> = items(f.get("differences"))
        .iter()
        .filter_map(JValue::as_str)
        .collect();
    assert_eq!(
        names,
        [
            "body-unsupported",
            "meta-not-json",
            "meta-unsupported",
            "meta-error-at-cap"
        ]
    );
}

#[test]
fn inspect_matches_node() {
    let (mut read, mut refused) = (0, 0);
    for c in list(&fixture(), "inspect") {
        let bytes = from_parts(c.get("parts"));
        assert_eq!(JValue::Num(bytes.len() as f64), *c.get("bytes"));
        match gguf::inspect(&bytes) {
            Ok(i) => {
                let JValue::Obj(mut got) = i.evidence("") else {
                    panic!()
                };
                got.remove(0);
                assert_eq!(&JValue::Obj(got), c.get("out"), "{}", show(c));
                read += 1;
            }
            Err(e) => {
                assert_eq!(c.get("error").as_str(), Some(e), "{}", show(c));
                refused += 1;
            }
        }
    }
    assert!(read > 50 && refused > 50, "{read} read, {refused} refused");
}

#[test]
fn derived_model_names_match_node() {
    for c in list(&fixture(), "names") {
        match manager::derive_user_model_name(c.get("input")) {
            Ok(name) => assert_eq!(c.get("out").as_str(), Some(name.as_str()), "{}", show(c)),
            Err(_) => assert!(c.get("error").as_str().is_some(), "{}", show(c)),
        }
    }
}

#[test]
fn llamacpp_variants_match_node() {
    let config = Config {
        kind: Kind::Llamacpp,
        base_url: "http://qa-router.invalid".into(),
        api_key: None,
    };
    for c in list(&fixture(), "quants") {
        let body = c.get("body").clone();
        let got = manager::variants(&config, "qa-org/qa-model", &mut |r| {
            assert_eq!(
                r.url,
                "https://huggingface.co/api/models/qa-org/qa-model?blobs=true"
            );
            assert!(r.headers.is_empty());
            Ok(Reply {
                ok: true,
                status: 200,
                body: body.clone(),
            })
        });
        let out = c.get("out");
        let got = match got {
            Err(_) => {
                assert_eq!(c.get("error").as_str(), Some("TypeError"), "{}", show(c));
                continue;
            }
            Ok(r) => r,
        };
        assert_eq!(JValue::Bool(got.ok), *out.get("ok"), "{}", show(c));
        assert_eq!(JValue::Num(f64::from(got.status)), *out.get("status"));
        if !got.ok {
            assert_eq!(&got.body, out.get("body"));
            continue;
        }
        // By name: core sorts the rows with localeCompare, which the route never looks at.
        let rows = |v: &JValue| -> BTreeMap<String, (JValue, JValue)> {
            items(v.get("variants"))
                .iter()
                .map(|r| {
                    (
                        r.get("name").as_str().unwrap().to_string(),
                        (r.get("primary_file").clone(), r.get("files").clone()),
                    )
                })
                .collect()
        };
        assert_eq!(rows(&got.body), rows(out.get("body")), "{}", show(c));
    }
}

#[test]
fn the_pull_matches_node() {
    let (mut accepted, mut forwarded) = (0, 0);
    for c in list(&fixture(), "pull") {
        let config = config(c);
        let mut host = Stub::new(c, &JValue::Undefined, 0);
        let body = request_bytes(c);
        let call = Call {
            method: c.get("method").as_str().unwrap(),
            path: "/api/models/pull",
            query: "",
            admin: c.get("role").as_str() == Some("admin"),
            body: body
                .as_deref()
                .filter(|b| b.len() <= model_pull::BODY_LIMIT),
        };
        let out = handle(&config, &Mutex::new(Artifacts::new()), call, &mut host);
        if out == Outcome::Forward {
            // Named difference body-unsupported.
            assert!(
                c.get("request").as_str().unwrap().contains("1e400"),
                "{}",
                show(c)
            );
            forwarded += 1;
            continue;
        }
        let (status, answer) = outcome_parts(&out);
        assert_eq!(
            status.map_or(JValue::Undefined, |s| JValue::Num(f64::from(s))),
            *c.get("status"),
            "{}",
            show(c)
        );
        assert_eq!(c.get("unhandled").as_bool() == Some(true), status.is_none());
        assert_eq!(&answer, c.get("body"), "{}", show(c));
        assert_eq!(
            host.invalidated,
            c.get("invalidated").as_num().unwrap_or(0.0)
        );
        assert_eq!(
            host.scan_cleared,
            c.get("scanCleared").as_num().unwrap_or(0.0),
            "{}",
            show(c)
        );
        assert_eq!(
            JValue::Arr(host.calls.clone()),
            *c.get("calls"),
            "{}",
            show(c)
        );
        assert_eq!(host.entered, 0);
        if let JValue::Arr(downloads) = c.get("downloads") {
            let mine: Vec<JValue> = host
                .requested
                .iter()
                .map(|m| {
                    JValue::obj([
                        ("model", m.clone()),
                        (
                            "status",
                            JValue::from(if host.rejected {
                                "rejected"
                            } else {
                                "requested"
                            }),
                        ),
                    ])
                })
                .collect();
            assert_eq!(&mine, downloads, "{}", show(c));
        }
        if status == Some(200) {
            accepted += 1;
        }
    }
    assert!(accepted > 100, "{accepted} accepted");
    assert!(forwarded >= 14, "{forwarded} forwarded");
}

/// One request's expected answer, with the named differences of the artifact check applied.
fn compare_mtp(
    node: &JValue,
    status: Option<u16>,
    body: &JValue,
    hf: &JValue,
    seen: &mut BTreeMap<&'static str, usize>,
) -> bool {
    let node_body = node.get("body");
    assert_eq!(
        status.map_or(JValue::Undefined, |s| JValue::Num(f64::from(s))),
        *node.get("status"),
        "{}",
        show(node)
    );
    // Compared as sent: `[undefined]` is written `[null]`, as JSON.stringify writes it.
    if wtf16::stringify(node_body) == wtf16::stringify(body) {
        return false;
    }
    let reason = body.get("reason").as_str().unwrap_or("");
    let meta = hf
        .get(&format!(
            "https://huggingface.co/api/models/{}",
            model_pull::js::search_param(node.get("query").as_str().unwrap_or(""), "repo")
                .unwrap_or_default()
        ))
        .clone();
    let same_but = |keys: &[&str]| {
        for key in ["status", "files", "checkedAt"] {
            if !keys.contains(&key) {
                assert_eq!(body.get(key), node_body.get(key), "{key}: {}", show(node));
            }
        }
    };
    let text = match (meta.get("text"), meta.get("json")) {
        (JValue::Str(t), _) => t.clone(),
        (_, JValue::Undefined) => String::new(),
        (_, json) => wtf16::stringify(json).unwrap(),
    };
    if reason == artifact::META_NOT_JSON && node_body.get("status").as_str() == Some("unknown") {
        assert!(wtf16::parse(&text).is_err(), "{}", show(node));
        same_but(&[]);
        *seen.entry("meta-not-json").or_default() += 1;
        true
    } else if reason == artifact::META_NOT_JSON {
        assert!(text.contains("1e400"), "{}", show(node));
        same_but(&["status"]);
        *seen.entry("meta-unsupported").or_default() += 1;
        true
    } else if reason == "terminated" && node_body.get("status").as_str() != Some("unknown") {
        assert_eq!(
            from_parts(meta.get("parts")).len(),
            artifact::META_CAP,
            "{}",
            show(node)
        );
        assert!(meta.get("error").as_str().is_some());
        same_but(&["status"]);
        *seen.entry("meta-error-at-cap").or_default() += 1;
        true
    } else {
        panic!("{}\n!= {}", show(body), show(node));
    }
}

#[test]
fn the_mtp_artifact_check_matches_node() {
    let f = fixture();
    let mut seen = BTreeMap::new();
    let (mut requests, mut definite) = (0, 0);
    for scenario in list(&f, "mtp") {
        let artifacts = Mutex::new(Artifacts::new());
        for step in items(scenario.get("steps")) {
            let now = step.get("at").as_num().unwrap() as i64;
            let hf = step.get("hf");
            let mut fetches = Vec::new();
            let mut differs = false;
            if step.get("wave").as_bool() == Some(true) {
                // At once: every request reaches the check before any fetch answers.
                enum Pending {
                    Done(u16, JValue),
                    Run(artifact::Key),
                    Join(artifact::Key),
                }
                let mut pending = Vec::new();
                let mut hosts = Vec::new();
                for r in items(step.get("requests")) {
                    assert_eq!(r.get("method").as_str(), Some("GET"));
                    let config = config(r);
                    let mut host = Stub::new(r, hf, now);
                    let query = r.get("query").as_str().unwrap_or("");
                    pending.push(match mtp_selection(&config, query, &mut host) {
                        Err(out) => {
                            let (s, b) = outcome_parts(&out);
                            Pending::Done(s.unwrap(), b)
                        }
                        Ok((repo, files)) => {
                            match artifacts.lock().unwrap().start(&repo, &files, now) {
                                Start::Done(b) => Pending::Done(200, b),
                                Start::Run(key) => Pending::Run(key),
                                Start::Join(join) => Pending::Join(join.key),
                            }
                        }
                    });
                    hosts.push(host);
                }
                let mut answers: Vec<(artifact::Key, JValue)> = Vec::new();
                for (p, host) in pending.iter().zip(hosts.iter_mut()) {
                    if let Pending::Run(key) = p {
                        let ran = artifact::run(key, host);
                        answers
                            .push((key.clone(), artifacts.lock().unwrap().finish(key, ran, now)));
                    }
                }
                for ((p, host), r) in pending
                    .into_iter()
                    .zip(hosts)
                    .zip(items(step.get("requests")))
                {
                    let (status, body) = match p {
                        Pending::Done(s, b) => (s, b),
                        Pending::Run(key) | Pending::Join(key) => (
                            200,
                            answers.iter().find(|(k, _)| *k == key).unwrap().1.clone(),
                        ),
                    };
                    differs |= compare_mtp(r, Some(status), &body, hf, &mut seen);
                    assert_eq!(
                        JValue::Arr(host.calls.clone()),
                        *r.get("calls"),
                        "{}",
                        show(r)
                    );
                    fetches.extend(host.fetches);
                    requests += 1;
                }
                fetches.sort_by_key(|f| wtf16::stringify(f).unwrap());
            } else {
                for r in items(step.get("requests")) {
                    let config = config(r);
                    let mut host = Stub::new(r, hf, now);
                    let call = Call {
                        method: r.get("method").as_str().unwrap(),
                        path: "/api/models/mtp-artifact",
                        query: r.get("query").as_str().unwrap_or(""),
                        admin: r.get("role").as_str() == Some("admin"),
                        body: None,
                    };
                    let out = handle(&config, &artifacts, call, &mut host);
                    let (status, body) = outcome_parts(&out);
                    differs |= compare_mtp(r, status, &body, hf, &mut seen);
                    assert_eq!(
                        host.invalidated,
                        r.get("invalidated").as_num().unwrap_or(0.0)
                    );
                    assert_eq!(
                        host.scan_cleared,
                        r.get("scanCleared").as_num().unwrap_or(0.0),
                        "{}",
                        show(r)
                    );
                    assert_eq!(
                        JValue::Arr(host.calls.clone()),
                        *r.get("calls"),
                        "{}",
                        show(r)
                    );
                    fetches.extend(host.fetches);
                    if matches!(body.get("status").as_str(), Some("present" | "absent")) {
                        definite += 1;
                    }
                    requests += 1;
                }
            }
            if differs {
                // A named difference stops the port earlier: its fetches are the first of Node's.
                let node = items(step.get("fetches"));
                assert!(fetches.len() <= node.len() && fetches[..] == node[..fetches.len()]);
                continue;
            }
            assert_eq!(
                JValue::Arr(fetches),
                *step.get("fetches"),
                "{}: {}",
                scenario.get("name").as_str().unwrap(),
                show(step.get("fetches"))
            );
        }
    }
    assert!(
        requests > 300 && definite > 100,
        "{requests} requests, {definite} definite"
    );
    for name in ["meta-not-json", "meta-unsupported", "meta-error-at-cap"] {
        assert!(
            seen.get(name).is_some_and(|n| *n > 0),
            "{name} is exercised: {seen:?}"
        );
    }
}
