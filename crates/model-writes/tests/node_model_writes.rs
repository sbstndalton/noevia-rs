//! Replays `fixtures/node-model-writes.v1.json` (written by `record_model_writes.cjs` from
//! routes/models.cjs's own lines over inference-budget.cjs and idle-unload-setting.cjs) through the
//! crate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::{wtf16, JValue};
use model_writes::idle::{self, HoldRefusal, Host, Reply};
use model_writes::{budget, round1};

fn fixture() -> JValue {
    wtf16::parse(include_str!("fixtures/node-model-writes.v1.json")).expect("the fixture parses")
}

fn list<'a>(f: &'a JValue, key: &str) -> &'a [JValue] {
    match f.get(key) {
        JValue::Arr(items) if !items.is_empty() => items,
        _ => panic!("the fixture has no {key} cases"),
    }
}

fn show(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "undefined".into())
}

/// The request body a case sent (`None`: the large one, over the limit).
fn request(c: &JValue) -> Option<Vec<u8>> {
    match c.get("request") {
        JValue::Str(s) => Some(s.as_bytes().to_vec()),
        JValue::Undefined
            if c.get("requestBytes")
                .as_num()
                .is_some_and(|n| n > 1_048_576.0) =>
        {
            None
        }
        JValue::Undefined => Some(Vec::new()),
        other => panic!("a request body: {}", show(other)),
    }
}

fn no_store(c: &JValue) -> bool {
    c.get("headers").get("Cache-Control").as_str() == Some("no-store")
}

#[test]
fn the_fixture_names_its_sources_and_the_cut_lines() {
    let f = fixture();
    for file in [
        "server/routes/models.cjs",
        "server/inference-budget.cjs",
        "server/idle-unload-setting.cjs",
        "server/http.cjs",
    ] {
        assert_eq!(
            f.get("sources").get(file).as_str().map(str::len),
            Some(64),
            "{file}"
        );
    }
    for block in ["prelude", "budget", "idle"] {
        assert!(f.get("blocks").get(block).is_object(), "{block}");
    }
    assert_eq!(f.get("differences"), &JValue::Arr(Vec::new()));
}

#[test]
fn host_ram_matches_node() {
    for c in list(&fixture(), "meminfo") {
        let text = if c.get("throws").as_bool() == Some(true) {
            Err(())
        } else {
            Ok(c.get("text").as_str().unwrap())
        };
        let got = budget::host_ram_gib(text).map_or(JValue::Null, JValue::Num);
        assert_eq!(&got, c.get("out"), "{}", show(c));
    }
}

#[test]
fn round1_is_javascript_s() {
    for (n, want) in [
        (0.25, 0.3),
        (0.35, 0.4),
        (-0.25, -0.2),
        (8.3, 8.3),
        (8.25, 8.3),
        (2.05, 2.1),
    ] {
        assert_eq!(round1(n), want, "{n}");
    }
}

#[test]
fn the_budget_put_matches_node() {
    let (mut saved, mut forwarded) = (0, 0);
    for c in list(&fixture(), "budget") {
        if c.get("method").as_str() != Some("PUT") || c.get("missing").as_bool() == Some(true) {
            continue;
        }
        let mem = match c.get("mem") {
            JValue::Str(s) => Ok(s.as_str()),
            _ => Err(()),
        };
        let host = budget::host_ram_gib(mem);
        let default = budget::default_gib(c.get("env").get("INFERENCE_MEMORY_BUDGET_GIB").as_str());
        let body = request(c);
        let Some(got) = budget::put(
            c.get("role").as_str() == Some("admin"),
            body.as_deref(),
            default,
            host,
        ) else {
            // Core answers what the port cannot hold.
            assert!(
                c.get("request")
                    .as_str()
                    .is_some_and(|r| r.contains("1e400")),
                "{}",
                show(c)
            );
            forwarded += 1;
            continue;
        };
        assert_eq!(
            c.get("status").as_num(),
            Some(f64::from(got.status)),
            "{}",
            show(c)
        );
        assert_eq!(show(&got.body), show(c.get("body")), "{}", show(c));
        assert_eq!(got.no_store, no_store(c), "{}", show(c));
        // The row and the audit record core wrote, and only on a save.
        match got.save {
            Some(n) => {
                saved += 1;
                assert_eq!(
                    Some(budget::row(n).as_str()),
                    c.get("rowAfter").as_str(),
                    "{}",
                    show(c)
                );
                let audit = JValue::Arr(vec![JValue::Arr(vec![
                    JValue::from(budget::AUDIT),
                    JValue::from("qa-admin"),
                    budget::audit_detail(n),
                ])]);
                assert_eq!(&audit, c.get("audit"), "{}", show(c));
            }
            None => {
                assert_eq!(c.get("audit"), &JValue::Arr(Vec::new()), "{}", show(c));
                assert_eq!(
                    c.get("rowAfter"),
                    c.get("row")
                        .as_str()
                        .map_or(&JValue::Null, |_| c.get("row")),
                    "{}",
                    show(c)
                );
            }
        }
    }
    assert!(saved > 50);
    assert!(forwarded > 0);
}

/// A host scripted as the recorder's stubs: the manager's GET and PUT answers, the held writer
/// (`refused`, `failed`, `coverage`, `held`) or the unheld reload, and the router's presets.
struct Scripted<'a> {
    case: &'a JValue,
    calls: Vec<JValue>,
    audit: Vec<JValue>,
    held: bool,
}

impl Scripted<'_> {
    fn reply(v: &JValue) -> Option<Reply> {
        if v.as_str() == Some("throw") || !v.is_object() {
            return None;
        }
        Some(Reply {
            ok: v.get("ok").as_bool() == Some(true),
            status: v.get("status").as_num().map_or(0, |n| n as u16),
            body: v.get("body").clone(),
        })
    }
    fn base(&self) -> String {
        self.case
            .get("env")
            .get("MODEL_LOADER_URL")
            .as_str()
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string()
    }
}

impl Host for Scripted<'_> {
    fn manager_missing(&self) -> bool {
        self.base().is_empty()
    }
    fn manager(&mut self, method: &str, body: Option<&str>) -> Option<Reply> {
        let mut call = vec![
            (
                "url".to_string(),
                JValue::Str(format!("{}{}", self.base(), idle::MANAGER_PATH)),
            ),
            ("method".to_string(), JValue::from(method)),
        ];
        if let Some(token) = self
            .case
            .get("env")
            .get("MODEL_LOADER_TOKEN")
            .as_str()
            .filter(|t| !t.is_empty())
        {
            call.push(("token".to_string(), JValue::from(token)));
        }
        call.push(("contentType".to_string(), JValue::from("application/json")));
        if let Some(b) = body {
            call.push(("body".to_string(), JValue::from(b)));
        }
        call.push((
            "timeout".to_string(),
            JValue::Num(idle::MANAGER_TIMEOUT_MS as f64),
        ));
        self.calls.push(JValue::Obj(call));
        Self::reply(self.case.get(if method == "GET" { "get" } else { "put" }))
    }
    fn has_writer(&self) -> bool {
        self.case.get("writer").as_str().is_some()
    }
    fn hold(&mut self) -> Result<(), HoldRefusal> {
        match self.case.get("writer").as_str() {
            Some("refused") => Err(HoldRefusal::Busy),
            Some("failed") => Err(HoldRefusal::Failed {
                status: 503,
                message: "The model service could not confirm that chat is idle, so nothing was changed. Try again shortly.".into(),
            }),
            _ => {
                self.held = true;
                Ok(())
            }
        }
    }
    fn reload_held(&mut self) -> Result<Reply, HoldRefusal> {
        assert!(self.held);
        if self.case.get("writer").as_str() == Some("coverage") {
            return Err(HoldRefusal::Failed {
                status: 503,
                message: "The model service stopped confirming that chat is held back, so the reload stopped. Try again shortly.".into(),
            });
        }
        Ok(Self::reply(self.case.get("reload")).unwrap_or(Reply {
            ok: false,
            status: 0,
            body: JValue::Undefined,
        }))
    }
    fn release(&mut self) {
        assert!(self.held);
        self.held = false;
    }
    fn reload(&mut self) -> Option<Option<Reply>> {
        if self.case.get("reloader").as_bool() != Some(true) {
            return None;
        }
        self.calls
            .push(JValue::obj([("reload", JValue::Bool(true))]));
        Some(Self::reply(self.case.get("reload")))
    }
    fn router_text(&mut self) -> Option<String> {
        self.calls
            .push(JValue::obj([("routerText", JValue::Bool(true))]));
        self.case
            .get("routerText")
            .as_str()
            .filter(|t| *t != "throw")
            .map(str::to_string)
    }
    fn audit(&mut self, action: &str, detail: JValue) {
        self.audit.push(JValue::Arr(vec![
            JValue::from(action),
            JValue::from("qa-admin"),
            detail,
        ]));
    }
}

#[test]
fn the_idle_unload_put_matches_node() {
    let mut saved = 0;
    for c in list(&fixture(), "idle") {
        if c.get("method").as_str() != Some("PUT") || c.get("missing").as_bool() == Some(true) {
            continue;
        }
        let mut host = Scripted {
            case: c,
            calls: Vec::new(),
            audit: Vec::new(),
            held: false,
        };
        let body = request(c);
        let got = idle::put(
            c.get("role").as_str() == Some("admin"),
            body.as_deref(),
            &mut host,
        )
        .expect("every recorded idle body is held");
        assert!(!host.held, "the hold is released: {}", show(c));
        assert_eq!(
            c.get("status").as_num(),
            Some(f64::from(got.status)),
            "{}",
            show(c)
        );
        assert_eq!(show(&got.body), show(c.get("body")), "{}", show(c));
        assert_eq!(got.no_store, no_store(c), "{}", show(c));
        assert_eq!(&JValue::Arr(host.calls), c.get("calls"), "{}", show(c));
        assert_eq!(&JValue::Arr(host.audit), c.get("audit"), "{}", show(c));
        saved += usize::from(got.status == 200);
    }
    assert!(saved > 40);
}

#[test]
fn the_unconfigured_answer_is_node_s() {
    let f = fixture();
    let c = list(&f, "idle")
        .iter()
        .find(|c| c.get("missing").as_bool() == Some(true))
        .unwrap();
    assert_eq!(c.get("status").as_num(), Some(404.0));
    assert_eq!(show(&idle::unavailable()), show(c.get("body")));
    assert!(!no_store(c));
}

#[test]
fn global_idle_seconds_matches_node() {
    for c in list(&fixture(), "global") {
        let got = idle::global_idle_seconds(c.get("text").as_str().unwrap())
            .map_or(JValue::Null, JValue::Num);
        assert_eq!(&got, c.get("out"), "{}", show(c));
    }
}
