//! The recorded Node output of slice P5a (`fixtures/node-route.v1.json`, written by
//! `record_route.cjs`): every effort, thinking switch, local-engine answer and sampling choice
//! must be what noevia-core's reasoning-effort.cjs, sampling-presets.cjs and chat.cjs's own lines
//! produced.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chat_prepare::effort::{self, Effort, Engine};
use chat_prepare::route::{self, RouteTurn};
use chat_prepare::sampling::{self, Preset, PRESETS_VERSION, SAMPLING_KEYS};
use js_json::{wtf16, JValue};

const FIXTURE: &str = include_str!("fixtures/node-route.v1.json");
const PROJECT_FIXTURE: &str = include_str!("fixtures/node-project.v1.json");
const SKILLS_FIXTURE: &str = include_str!("fixtures/node-skills.v1.json");

fn fixture() -> JValue {
    wtf16::parse(FIXTURE).expect("the fixture is JSON")
}

fn items<'a>(v: &'a JValue, key: &str) -> &'a [JValue] {
    match v.get(key) {
        JValue::Arr(items) => items,
        other => panic!("{key} is not an array: {other:?}"),
    }
}

/// `JSON.stringify(v)`, with `undefined` for a missing value.
fn stringify(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "undefined".to_string())
}

/// A recorded routedRole: a string, or `null`/`undefined` (the recorder records no other).
fn role(v: &JValue) -> Option<&str> {
    match v {
        JValue::Str(s) => Some(s),
        JValue::Null | JValue::Undefined => None,
        other => panic!("a routedRole that is not a string: {other:?}"),
    }
}

/// A recorded message: a string, or left out (`selectSamplingParams`' default `''`).
fn message(v: &JValue) -> &str {
    match v {
        JValue::Str(s) => s,
        JValue::Undefined => "",
        other => panic!("a message that is not a string: {other:?}"),
    }
}

fn engine(case: &JValue) -> Engine<'_> {
    Engine {
        kind: case.get("kind").as_str(),
        inference_base_url: case.get("inference").as_str(),
    }
}

/// Collects failures and reports the first few with Node's and the port's output.
#[derive(Default)]
struct Diffs {
    failed: Vec<String>,
    total: usize,
}

impl Diffs {
    fn check(&mut self, what: &str, want: &str, got: &str) {
        self.total += 1;
        if want != got {
            self.failed
                .push(format!("{what}\n  node: {want:?}\n  rust: {got:?}"));
        }
    }
    fn done(self, section: &str, min: usize) {
        assert!(
            self.total >= min,
            "{section}: only {} cases recorded",
            self.total
        );
        assert!(
            self.failed.is_empty(),
            "{section}: {} of {} differ:\n{}",
            self.failed.len(),
            self.total,
            self.failed[..self.failed.len().min(5)].join("\n")
        );
    }
}

fn input(case: &JValue) -> String {
    let mut c = case.clone();
    if let JValue::Obj(items) = &mut c {
        items.retain(|(k, _)| k != "out");
    }
    let s = stringify(&c);
    if s.len() > 300 {
        format!(
            "{}…",
            &s[..s.char_indices().nth(300).map_or(s.len(), |(i, _)| i)]
        )
    } else {
        s
    }
}

#[test]
fn the_fixture_names_its_node_sources() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    for file in [
        "server/chat.cjs",
        "server/reasoning-effort.cjs",
        "server/sampling-presets.cjs",
        "server/http.cjs",
        "server/thinking-budget.cjs",
    ] {
        let sha = f.get("sources").get(file).as_str().unwrap();
        assert!(sha.len() == 64, "{file}");
    }
    // Recorded at the same core as the crate's other fixtures (tests/oracle-core.ref): chat.cjs,
    // whose block is run, is the same bytes the project and skills recordings pinned.
    for other in [PROJECT_FIXTURE, SKILLS_FIXTURE] {
        let other = wtf16::parse(other).unwrap();
        assert_eq!(
            f.get("sources").get("server/chat.cjs"),
            other.get("sources").get("server/chat.cjs")
        );
    }
    assert_eq!(f.get("defaultProviderId").as_str(), Some("default"));
    assert!(f.get("block").get("sha256").as_str().is_some());
    // The presets, their version and keys are the module's constants.
    assert_eq!(
        f.get("presetsVersion").as_num(),
        Some(f64::from(PRESETS_VERSION))
    );
    let keys: Vec<&str> = items(&f, "samplingKeys")
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert_eq!(keys, SAMPLING_KEYS);
    let presets = f.get("presets");
    let JValue::Obj(ids) = presets else {
        panic!("presets")
    };
    assert_eq!(ids.len(), 4);
    for p in [
        Preset::General,
        Preset::Coding,
        Preset::Creative,
        Preset::Reasoning,
    ] {
        let want = stringify(presets.get(p.as_str()));
        let got = stringify(&JValue::obj(
            p.params().iter().map(|&(k, v)| (k, JValue::Num(v))),
        ));
        assert_eq!(want, got, "{}", p.as_str());
    }
}

#[test]
fn reasoning_effort_matches_node() {
    let f = fixture();
    let mut d = Diffs::default();
    for c in items(&f, "efforts") {
        let got = effort::valid_effort(c.get("value"));
        d.check(&input(c), &stringify(c.get("out")), &got.to_string());
    }
    d.done("efforts", 30);
    let mut d = Diffs::default();
    for c in items(&f, "resolve") {
        let got = effort::resolve_effort(c.get("project"), c.get("globalDefault"));
        d.check(
            &input(c),
            &stringify(c.get("out")),
            &stringify(&got.as_str().into()),
        );
    }
    d.done("resolve", 400);
    let mut d = Diffs::default();
    for c in items(&f, "forRoute") {
        let e = Effort::of(c.get("effort")).unwrap();
        let got = effort::effort_for_route(e, role(c.get("routedRole")));
        d.check(
            &input(c),
            &stringify(c.get("out")),
            &stringify(&got.as_str().into()),
        );
    }
    d.done("forRoute", 40);
    let mut d = Diffs::default();
    for c in items(&f, "thinkingOff") {
        let auto = c.get("auto").as_bool().unwrap();
        let got = effort::fast_route_thinking_off(
            auto,
            role(c.get("routedRole")),
            c.get("projectEffort"),
        );
        d.check(&input(c), &stringify(c.get("out")), &got.to_string());
    }
    d.done("thinkingOff", 900);
}

#[test]
fn local_engine_matches_node() {
    let f = fixture();
    let mut d = Diffs::default();
    let mut local = 0;
    for c in items(&f, "engines") {
        let got = effort::local_engine(c.get("provider"), &engine(c));
        local += usize::from(got);
        d.check(&input(c), &stringify(c.get("out")), &got.to_string());
    }
    // Enough pairs on each side of the comparison to mean something.
    assert!(local >= 200, "only {local} local");
    d.done("engines", 5000);
}

#[test]
fn sampling_rules_match_node() {
    let f = fixture();
    let mut d = Diffs::default();
    for c in items(&f, "values") {
        let got = sampling::valid_value(c.get("key").as_str().unwrap(), c.get("value"));
        d.check(&input(c), &stringify(c.get("out")), &got.to_string());
    }
    d.done("values", 280);
    let mut d = Diffs::default();
    for c in items(&f, "sanitize") {
        let got = sampling::sanitize_explicit(c.get("raw")).map_or(JValue::Undefined, |p| {
            JValue::obj(p.into_iter().map(|(k, v)| (k, JValue::Num(v))))
        });
        d.check(&input(c), &stringify(c.get("out")), &stringify(&got));
    }
    d.done("sanitize", 30);
    let roles: Vec<Option<&str>> = items(&f, "roles").iter().map(role).collect();
    let mut d = Diffs::default();
    let mut creative = 0;
    for c in items(&f, "classify") {
        let m = message(c.get("message"));
        let got: Vec<JValue> = roles
            .iter()
            .map(|r| {
                let p = sampling::classify(*r, m);
                creative += usize::from(p == Preset::Creative && r.is_none());
                JValue::from(p.as_str())
            })
            .collect();
        d.check(
            &input(c),
            &stringify(c.get("presets")),
            &stringify(&JValue::Arr(got)),
        );
    }
    assert!(creative >= 100, "only {creative} creative by text");
    d.done("classify", 500);
    let mut d = Diffs::default();
    for c in items(&f, "creative") {
        let got = sampling::creative_route(
            role(c.get("routedRole")),
            message(c.get("message")),
            c.get("roles"),
        );
        let got = if got {
            JValue::from("creative")
        } else {
            JValue::Null
        };
        d.check(&input(c), &stringify(c.get("out")), &stringify(&got));
    }
    d.done("creative", 800);
    let mut d = Diffs::default();
    for c in items(&f, "select") {
        // `autoEnabled = true` when left out.
        let auto = c.get("autoEnabled").as_bool().unwrap_or(true);
        let got = sampling::select(
            role(c.get("routedRole")),
            message(c.get("message")),
            c.get("explicit"),
            auto,
        );
        d.check(
            &input(c),
            &stringify(c.get("out")),
            &stringify(&got.to_value()),
        );
    }
    d.done("select", 2000);
}

#[test]
fn turns_match_chat_cjs() {
    let f = fixture();
    let default_id = f.get("defaultProviderId").as_str().unwrap();
    let mut d = Diffs::default();
    let mut off = 0;
    for c in items(&f, "turns") {
        // No settings table (`undefined`) or no `db` (`null`): neither row is there.
        let settings = c.get("settings");
        let turn = RouteTurn {
            project: c.get("project"),
            effort_default: settings.get("reasoning_effort_default"),
            sampling_presets: settings.get("auto_sampling_presets_enabled"),
            wants_auto: c.get("wantsAuto").as_bool().unwrap(),
            routed_role: role(c.get("routedRole")),
            provider: c.get("provider"),
            default_provider_id: default_id,
            engine: engine(c),
            message: message(c.get("message")),
        };
        let r = route::route(&turn);
        off += usize::from(r.thinking_off);
        let got = JValue::obj([
            ("resolvedEffort", JValue::from(r.resolved_effort.as_str())),
            ("effort", JValue::from(r.effort.as_str())),
            ("fastThinkingOff", JValue::Bool(r.thinking_off)),
            ("thinkingOffFields", r.thinking_off_fields()),
            ("samplingAutoEnabled", JValue::Bool(r.sampling_auto)),
            ("sampling", r.sampling.to_value()),
            ("request", r.request_fields()),
            ("meta", r.meta_sampling()),
            (
                "prepared",
                JValue::obj([
                    ("effort", JValue::from(r.effort.as_str())),
                    ("thinkingOff", JValue::Bool(r.thinking_off)),
                    ("sampling", r.sampling.params_value()),
                ]),
            ),
        ]);
        // JSON.stringify leaves an undefined `meta` out.
        let got = match got {
            JValue::Obj(items) => JValue::Obj(
                items
                    .into_iter()
                    .filter(|(_, v)| *v != JValue::Undefined)
                    .collect(),
            ),
            other => other,
        };
        d.check(&input(c), &stringify(c.get("out")), &stringify(&got));
    }
    assert!(off >= 100, "only {off} turns with thinking off");
    d.done("turns", 1500);
}

#[test]
fn no_named_differences() {
    // Every case above replays byte for byte; the recorder states none.
    assert!(items(&fixture(), "differences").is_empty());
}

#[test]
fn settings_values_read_as_chat_cjs_reads_them() {
    // `raw === undefined || raw === null ? true : raw !== 'false'`.
    for (raw, on) in [
        (JValue::Undefined, true),
        (JValue::Null, true),
        (JValue::from("false"), false),
        (JValue::from("FALSE"), true),
        (JValue::from(" false"), true),
        (JValue::from(""), true),
        (JValue::Num(0.0), true),
        (JValue::Bool(false), true),
    ] {
        assert_eq!(route::sampling_auto_enabled(&raw), on, "{raw:?}");
    }
}

#[test]
fn the_js_edges_hold_without_the_fixture() {
    // Non-Unicode /i folds ASCII only; JS \s holds U+FEFF but not U+0085; \b is ASCII.
    for (m, want) in [
        ("WRITE A POEM", Preset::Creative),
        ("write a\u{feff}poem", Preset::Creative),
        ("write a\u{85}poem", Preset::General),
        ("write a hai\u{212a}u", Preset::General),
        ("write a \u{17f}tory", Preset::General),
        ("write a poem\u{e9}", Preset::Creative),
        ("write a poem_", Preset::General),
        ("rewrite a poem", Preset::General),
    ] {
        assert_eq!(sampling::classify(None, m), want, "{m:?}");
    }
    assert_eq!(
        sampling::classify(Some("code"), "write a poem"),
        Preset::Coding
    );
    let row = JValue::obj([
        ("id", JValue::from("default")),
        ("baseUrl", JValue::from("HTTP://QA-Engine.INVALID:80/v1//")),
    ]);
    let on = Engine {
        kind: Some("llamacpp"),
        inference_base_url: Some("http://qa-engine.invalid/v1"),
    };
    assert!(effort::local_engine(&row, &on));
    let other_kind = Engine {
        kind: Some("LlamaCpp"),
        ..on
    };
    assert!(!effort::local_engine(&row, &other_kind));
    // A project's own effort keeps a Fast turn's thinking; the account default does not.
    let turn = |project: &JValue| {
        route::route(&RouteTurn {
            project,
            effort_default: &JValue::from("high"),
            sampling_presets: &JValue::Undefined,
            wants_auto: true,
            routed_role: Some("fast"),
            provider: &row,
            default_provider_id: "default",
            engine: on,
            message: "Say hello",
        })
    };
    let fast = turn(&JValue::Null);
    assert_eq!((fast.effort, fast.thinking_off), (Effort::Low, true));
    let own = turn(&JValue::obj([("reasoningEffort", JValue::from("high"))]));
    assert_eq!((own.effort, own.thinking_off), (Effort::High, false));
}
