//! Replays `fixtures/node-model-manager-proxy.v1.json` (written by `record_model_manager_proxy.cjs`
//! from core's own createModelRoutes(...) with stubs of the outside world) through the crate: the
//! status, the body text, Cache-Control, the throw, the reranker-settings lock, and every host call
//! in order with its arguments (the forwarded request among them).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::{wtf16, JValue};
use model_manager_proxy::evidence::throughput_records;
use model_manager_proxy::{
    handle, protected_action, Caps, Config, Fetch, HoldError, Host, Outcome, PresetHoldError,
    Reply, Request, Thrown, BODY_LIMIT,
};
use std::collections::VecDeque;

fn fixture() -> JValue {
    wtf16::parse(include_str!("fixtures/node-model-manager-proxy.v1.json"))
        .expect("the fixture parses")
}

fn list<'a>(f: &'a JValue, key: &str) -> &'a [JValue] {
    match f.get(key) {
        JValue::Arr(items) if !items.is_empty() => items,
        _ => panic!("the fixture has no {key}"),
    }
}

fn show(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "undefined".into())
}

fn text(v: &JValue) -> String {
    v.as_str().map(str::to_string).unwrap_or_default()
}

/// The recorder's `padded(total)`.
fn padded(total: usize) -> Vec<u8> {
    let (head, tail) = (r#"{"aliases":["qa-chat"],"pad":""#, r#""}"#);
    let mut out = head.as_bytes().to_vec();
    out.resize(total - tail.len(), b'x');
    out.extend_from_slice(tail.as_bytes());
    out
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

/// The body the case sent (`None`: more than the limit).
fn body_of(c: &JValue) -> Option<Vec<u8>> {
    let bytes = match (c.get("body"), c.get("base64"), c.get("padded")) {
        (JValue::Str(s), _, _) => s.as_bytes().to_vec(),
        (_, JValue::Str(b), _) => unbase64(b),
        (_, _, JValue::Num(n)) => padded(*n as usize),
        _ => Vec::new(),
    };
    (bytes.len() <= BODY_LIMIT).then_some(bytes)
}

fn thrown(t: &JValue) -> Thrown {
    Thrown {
        status: t.get("status").clone(),
        message: text(t.get("message")),
    }
}

struct Fake {
    name: String,
    calls: VecDeque<JValue>,
    now: f64,
    /// The route judges a parsed body and forwards it written again.
    normalise: bool,
    /// A named difference: no recorded calls to follow; it must not reach the model manager.
    permissive: bool,
}

impl Fake {
    fn next(&mut self, call: &str) -> JValue {
        if self.permissive {
            assert_ne!(call, "fetch", "{}: a named difference forwarded", self.name);
            return JValue::Undefined;
        }
        let c = self
            .calls
            .pop_front()
            .unwrap_or_else(|| panic!("{}: asked {call}, Node asked nothing more", self.name));
        assert_eq!(
            c.get("call").as_str(),
            Some(call),
            "{}: asked {call}, Node asked {}",
            self.name,
            show(&c)
        );
        c
    }

    fn same(&self, what: &str, ours: &JValue, node: &JValue) {
        if self.permissive {
            return;
        }
        assert_eq!(show(ours), show(node), "{}: {what}", self.name);
    }
}

impl Host for Fake {
    fn reranker_settings(&mut self) -> Result<(), Thrown> {
        let c = self.next("rerankerSettings");
        if !c.get("threw").is_nullish() {
            return Err(thrown(c.get("threw")));
        }
        Ok(())
    }
    fn invalidate_model_reads(&mut self) {
        self.next("invalidateModelReads");
    }
    fn embedding_protection_unknown(&mut self) -> bool {
        self.next("embeddingProtectionUnknown").get("answer") == &JValue::Bool(true)
    }
    fn is_system_model(&mut self, key: &str) -> bool {
        let c = self.next("isSystemModel");
        self.same("isSystemModel id", &JValue::from(key), c.get("id"));
        c.get("answer") == &JValue::Bool(true)
    }
    fn is_sidecar_model(&mut self, name: &str) -> Result<bool, Thrown> {
        let c = self.next("isSidecarModel");
        self.same("isSidecarModel name", &JValue::from(name), c.get("name"));
        if !c.get("threw").is_nullish() {
            return Err(thrown(c.get("threw")));
        }
        Ok(c.get("answer") == &JValue::Bool(true))
    }
    fn is_reranker_model(&mut self, name: &str) -> Result<bool, Thrown> {
        let c = self.next("isRerankerModel");
        self.same("isRerankerModel name", &JValue::from(name), c.get("name"));
        if !c.get("threw").is_nullish() {
            return Err(thrown(c.get("threw")));
        }
        Ok(c.get("answer") == &JValue::Bool(true))
    }
    fn served_catalogue(&mut self) -> Result<JValue, Thrown> {
        let c = self.next("servedCatalogue");
        if !c.get("threw").is_nullish() {
            return Err(thrown(c.get("threw")));
        }
        Ok(c.get("answer").clone())
    }
    fn load_refusal(&mut self, alias: &str) -> JValue {
        let c = self.next("loadRefusal");
        self.same("loadRefusal alias", &JValue::from(alias), c.get("alias"));
        c.get("answer").clone()
    }
    fn sweep_hold(&mut self, reason: &str) -> Result<(), HoldError> {
        let c = self.next("sweepHold");
        self.same("sweepHold reason", &JValue::from(reason), c.get("reason"));
        self.same(
            "sweepHold recovery",
            &JValue::Bool(false),
            c.get("recovery"),
        );
        if c.get("answer").as_str() == Some("granted") {
            return Ok(());
        }
        let r = c.get("refused");
        Err(HoldError {
            status: r.get("status").clone(),
            code: r.get("code").clone(),
        })
    }
    fn sweep_release(&mut self) {
        self.next("sweepRelease");
    }
    fn sweep_watch(&mut self, models: usize) {
        let c = self.next("sweepWatch");
        self.same(
            "sweepWatch models",
            &JValue::Num(models as f64),
            c.get("models"),
        );
    }
    fn scan_cache_get(&mut self) -> JValue {
        let c = self.next("scanCacheGet");
        self.same("scanCacheGet key", &JValue::from("models"), c.get("key"));
        c.get("answer").clone()
    }
    fn scan_cache_set(&mut self, body: &JValue) {
        let c = self.next("scanCacheSet");
        self.same("scanCacheSet key", &JValue::from("models"), c.get("key"));
        self.same("scanCacheSet body", body, c.get("body"));
    }
    fn scan_cache_clear(&mut self) {
        self.next("scanCacheClear");
    }
    fn preset_refusal(&mut self, name: &str, options: &JValue) -> Result<JValue, Thrown> {
        let c = self.next("presetRefusal");
        self.same("presetRefusal name", &JValue::from(name), c.get("name"));
        self.same("presetRefusal options", options, c.get("options"));
        if !c.get("threw").is_nullish() {
            return Err(thrown(c.get("threw")));
        }
        Ok(c.get("answer").clone())
    }
    fn sizing_budget_gib(&mut self) -> JValue {
        self.next("sizingBudgetGib").get("answer").clone()
    }
    fn fetch(&mut self, request: &Fetch) -> Option<Reply> {
        let c = self.next("fetch");
        self.same(
            "fetch url",
            &JValue::from(request.url.as_str()),
            c.get("url"),
        );
        self.same(
            "fetch method",
            &JValue::from(request.method.as_str()),
            c.get("method"),
        );
        let headers = JValue::Obj(
            request
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), JValue::from(v.as_str())))
                .collect(),
        );
        self.same("fetch headers", &headers, c.get("headers"));
        let body = request
            .body
            .as_deref()
            .map_or(JValue::Undefined, JValue::from);
        let node_body = match (self.normalise, c.get("body")) {
            (true, JValue::Str(raw)) if raw.is_empty() => JValue::from("{}"),
            (true, JValue::Str(raw)) => JValue::Str(show(&wtf16::parse(raw).unwrap())),
            (_, other) => other.clone(),
        };
        self.same("fetch body", &body, &node_body);
        self.same(
            "fetch timeout",
            &JValue::Num(request.timeout_ms as f64),
            c.get("timeoutMs"),
        );
        let a = c.get("answer");
        if a.is_nullish() {
            return None;
        }
        Some(Reply {
            ok: a.get("ok") == &JValue::Bool(true),
            status: a.get("status").as_num().unwrap() as u16,
            body: a.get("body").clone(),
        })
    }
    fn preset_hold(&mut self) -> Result<(), PresetHoldError> {
        let c = self.next("presetHold");
        if !c.get("threw").is_nullish() {
            return Err(PresetHoldError::Threw(thrown(c.get("threw"))));
        }
        match c.get("answer").as_str() {
            Some("granted") => Ok(()),
            Some("refused") => Err(PresetHoldError::Refused),
            other => panic!("{}: presetHold {other:?}", self.name),
        }
    }
    fn preset_reload(&mut self) -> Result<(), Thrown> {
        let c = self.next("presetReload");
        if !c.get("threw").is_nullish() {
            return Err(thrown(c.get("threw")));
        }
        Ok(())
    }
    fn preset_release(&mut self) {
        self.next("presetRelease");
    }
    fn reload_presets(&mut self) {
        self.next("reloadPresets");
    }
    fn now_ms(&mut self) -> f64 {
        self.now
    }
    fn record_evidence(&mut self, model: &JValue, record: &JValue) {
        let c = self.next("recordEvidence");
        self.same("recordEvidence model", model, c.get("model"));
        self.same("recordEvidence record", record, c.get("record"));
    }
    fn unload(&mut self, id: &JValue) -> bool {
        let c = self.next("unload");
        self.same("unload id", id, c.get("id"));
        c.get("answer") == &JValue::Bool(true)
    }
    fn clear_role_references(&mut self, id: &JValue) -> Result<JValue, Thrown> {
        let c = self.next("clearRoleReferences");
        self.same("clearRoleReferences id", id, c.get("id"));
        if !c.get("threw").is_nullish() {
            return Err(thrown(c.get("threw")));
        }
        Ok(c.get("answer").clone())
    }
    fn forget_model_state(&mut self, id: &JValue) -> Result<(), Thrown> {
        let c = self.next("forgetModelState");
        self.same("forgetModelState id", id, c.get("id"));
        if !c.get("threw").is_nullish() {
            return Err(thrown(c.get("threw")));
        }
        Ok(())
    }
}

fn config(c: &JValue, defaults: &JValue) -> Config {
    let env = match c.get("env") {
        JValue::Undefined => defaults.get("env"),
        e => e,
    };
    let caps_list = match c.get("caps") {
        JValue::Undefined => defaults.get("caps"),
        e => e,
    };
    let has = |name: &str| match caps_list {
        JValue::Arr(items) => items.iter().any(|x| x.as_str() == Some(name)),
        _ => false,
    };
    Config {
        loader_url: env.get("url").as_str().map(str::to_string),
        loader_token: env.get("token").as_str().map(str::to_string),
        caps: Caps {
            load_refusal: has("loadRefusal"),
            preset_refusal: has("presetRefusal"),
            sizing_budget: has("sizingBudgetGib"),
            preset_writer: has("writePresets"),
            reload_presets: has("reloadPresets"),
            record_evidence: has("recordEvidence"),
            unload: has("unload"),
            sweep_guard: has("holdMaintenance"),
        },
    }
}

#[test]
fn every_recorded_request_answers_as_core() {
    let f = fixture();
    let defaults = f.get("defaults");
    let mut answered = 0;
    let mut differences = 0;
    for c in list(&f, "cases") {
        let name = text(c.get("name"));
        let e = c.get("expect");
        let method = text(c.get("method"));
        let path = text(c.get("p"));
        let search = text(c.get("search"));
        let role = c.get("role").as_str().unwrap_or("admin").to_string();
        let body = body_of(c);
        let calls: VecDeque<JValue> = match e.get("calls") {
            JValue::Arr(items) => items.iter().cloned().collect(),
            _ => VecDeque::new(),
        };
        let difference = c.get("difference");
        let guarded = path
            .strip_prefix(model_manager_proxy::PREFIX)
            .and_then(model_manager_proxy::manager_path)
            .is_some_and(|rest| {
                (method == "POST"
                    && matches!(
                        rest.as_str(),
                        "benchmark/start" | "benchmark/sweep" | "models/delete"
                    ))
                    || (method == "PUT"
                        && model_manager_proxy::section_name(&rest).is_some()
                        && !model_manager_proxy::section_rename(&rest))
            });
        let mut host = Fake {
            normalise: guarded,
            permissive: !matches!(difference, JValue::Undefined),
            name: name.clone(),
            calls,
            now: c
                .get("now")
                .as_num()
                .or_else(|| defaults.get("now").as_num())
                .unwrap(),
        };
        let req = Request {
            method: &method,
            path: &path,
            search: &search,
            role: &role,
            body: body.as_deref(),
        };
        assert_eq!(
            protected_action(&method, &path, &role),
            e.get("locked") == &JValue::Bool(true),
            "{name}: the reranker-settings lock"
        );
        match handle(&req, &config(c, defaults), &mut host) {
            Outcome::Pass => {
                assert_eq!(e.get("passed"), &JValue::Bool(true), "{name}: passed");
                // The bare `/api/model-manager` is outside the block: the reads fence of models.cjs's
                // prelude and wrapper is all core did with it here.
                assert!(
                    host.calls
                        .iter()
                        .all(|c| c.get("call").as_str() == Some("invalidateModelReads")),
                    "{name}: calls left"
                );
            }
            Outcome::Core => panic!("{name}: sent to core"),
            Outcome::Answer(a) if !matches!(difference, JValue::Undefined) => {
                assert_eq!(
                    Some(f64::from(a.status)),
                    difference.get("status").as_num(),
                    "{name}: the named difference's status"
                );
                assert_eq!(
                    wtf16::stringify(&a.body).as_deref(),
                    difference.get("bodyJson").as_str(),
                    "{name}: the named difference's body"
                );
                differences += 1;
            }
            Outcome::Answer(a) => {
                assert_eq!(
                    Some(f64::from(a.status)),
                    e.get("status").as_num(),
                    "{name}: status"
                );
                assert_eq!(
                    wtf16::stringify(&a.body).as_deref(),
                    e.get("bodyJson").as_str(),
                    "{name}: body"
                );
                assert_eq!(
                    a.no_store,
                    e.get("noStore") == &JValue::Bool(true),
                    "{name}: Cache-Control"
                );
                assert_eq!(
                    a.thrown,
                    e.get("threw") == &JValue::Bool(true),
                    "{name}: thrown"
                );
                assert!(
                    host.calls.is_empty(),
                    "{name}: Node asked more: {}",
                    host.calls.iter().map(show).collect::<Vec<_>>().join(" ")
                );
                answered += 1;
            }
        }
    }
    assert!(answered > 900, "{answered} answered");
    assert!(differences > 40, "{differences} named differences");
}

#[test]
fn throughput_records_match_benchmark_evidence() {
    let f = fixture();
    for (i, c) in list(&f, "evidence").iter().enumerate() {
        let now = c.get("now").as_num().unwrap();
        let got = throughput_records(c.get("detail"), now);
        match c.get("threw").as_num() {
            Some(status) => {
                let t = got.expect_err("Node threw");
                assert_eq!(f64::from(t.answer().0), status, "evidence {i}");
            }
            None => {
                let out = JValue::Arr(
                    got.unwrap_or_else(|_| panic!("evidence {i} threw"))
                        .into_iter()
                        .map(|(model, record)| JValue::obj([("model", model), ("record", record)]))
                        .collect(),
                );
                assert_eq!(show(&out), show(c.get("out")), "evidence {i}");
            }
        }
    }
}

#[test]
fn the_token_stays_out_of_debug() {
    let cfg = Config {
        loader_url: Some("http://qa-loader.invalid".into()),
        loader_token: Some("qa-secret-token".into()),
        caps: Caps::default(),
    };
    assert!(!format!("{cfg:?}").contains("qa-secret"));
    let fetch = Fetch {
        url: "http://qa-loader.invalid/api/v1/models".into(),
        method: "GET".into(),
        headers: vec![("X-Model-Loader-Token".into(), "qa-secret-token".into())],
        body: None,
        timeout_ms: 1,
    };
    assert!(!format!("{fetch:?}").contains("qa-secret"));
}
