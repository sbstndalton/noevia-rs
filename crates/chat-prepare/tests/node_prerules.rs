//! Routing modes' pre-rules, the cloud hold of tool results and the router's input (slice P5d,
//! part 3) replayed on recorded Node output (`fixtures/node-prerules.v1.json`, written by
//! `record_prerules.cjs`): `preRule` with and without the health rule, chat.cjs's own
//! `holdForCloud` lines over whole turns of tool results (held or not, the log entries and console
//! lines), `routerInput`'s chunks and coverage, and `chunkCharsFor`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chat_prepare::pre_rules::{pre_rule, CloudHold, SCAN_LIMIT};
use chat_prepare::router_input::{chunk_chars_for, router_input, RouterInput};
use chat_prepare::toolboxes::Toolbox;
use chat_prepare::tools_mode::is_public_tool_name;
use js_json::{wtf16, JValue};

const FIXTURE: &str = include_str!("fixtures/node-prerules.v1.json");
const MODES_FIXTURE: &str = include_str!("fixtures/node-modes.v1.json");

fn fixture() -> JValue {
    wtf16::parse(FIXTURE).expect("the fixture is JSON")
}

fn items<'a>(v: &'a JValue, key: &str) -> &'a [JValue] {
    match v.get(key) {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

fn flag(v: &JValue) -> Option<&str> {
    match v {
        JValue::Null => None,
        JValue::Str(s) => Some(s.as_str()),
        other => panic!("not a flag: {other:?}"),
    }
}

fn json(v: &JValue) -> String {
    wtf16::stringify(v).unwrap()
}

/// Failures collected so one run names them all (the first few).
#[derive(Default)]
struct Failures {
    failed: Vec<String>,
    total: usize,
}

impl Failures {
    fn check(&mut self, ok: bool, what: impl FnOnce() -> String) {
        self.total += 1;
        if !ok {
            self.failed.push(what());
        }
    }

    fn done(self, kind: &str) {
        assert!(
            self.failed.is_empty(),
            "{kind}: {} of {} differ from Node:\n{}",
            self.failed.len(),
            self.total,
            self.failed[..self.failed.len().min(5)].join("\n")
        );
    }
}

#[test]
fn the_fixture_names_its_node_sources() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    for file in [
        "server/chat.cjs",
        "server/routing-modes.cjs",
        "server/tools-mode.cjs",
    ] {
        let sha = f.get("sources").get(file).as_str().unwrap_or_default();
        assert_eq!(sha.len(), 64, "{file}");
    }
    // Recorded at the same core as the crate's other fixtures (tests/oracle-core.ref).
    let other = wtf16::parse(MODES_FIXTURE).unwrap();
    for file in ["server/chat.cjs", "server/routing-modes.cjs"] {
        assert_eq!(
            f.get("sources").get(file),
            other.get("sources").get(file),
            "{file}"
        );
    }
    assert_eq!(f.get("scanLimit").as_num(), Some(SCAN_LIMIT as f64));
    assert_eq!(f.get("block").get("sha256").as_str().unwrap().len(), 64);
}

#[test]
fn pre_rules_are_node_s() {
    let f = fixture();
    let mut out = Failures::default();
    let cases = items(&f, "preRule");
    assert!(cases.len() > 2500);
    for c in cases {
        let text = match c.get("build") {
            JValue::Arr(parts) => parts
                .iter()
                .map(|p| match p {
                    JValue::Arr(pair) => {
                        let piece = pair[0].as_str().unwrap();
                        piece.repeat(pair[1].as_num().unwrap() as usize)
                    }
                    other => panic!("not a piece: {other:?}"),
                })
                .collect::<String>(),
            _ => c.get("text").as_str().unwrap().to_string(),
        };
        let shown = || {
            let short: String = text.chars().take(80).collect();
            format!("{short:?} ({} chars)", text.chars().count())
        };
        let (want, want_public) = (flag(c.get("flag")), flag(c.get("public")));
        let (got, got_public) = (pre_rule(&text, true), pre_rule(&text, false));
        out.check(got == want, || {
            format!("{}: {got:?}, Node {want:?}", shown())
        });
        out.check(got_public == want_public, || {
            format!(
                "{} health off: {got_public:?}, Node {want_public:?}",
                shown()
            )
        });
    }
    // Every rule is recorded both tripped and not.
    for code in ["secret", "iban", "card", "health"] {
        assert!(
            cases.iter().any(|c| flag(c.get("flag")) == Some(code)),
            "{code}"
        );
    }
    assert!(cases
        .iter()
        .any(|c| flag(c.get("flag")) == Some("health") && flag(c.get("public")).is_none()));
    out.done("preRule");
}

fn toolbox(v: &JValue) -> Toolbox {
    Toolbox {
        id: v.get("id").as_str().unwrap().to_string(),
        label: String::new(),
        source: String::new(),
        in_app: false,
        server: None,
        tools: items(v, "tools").to_vec(),
        reads: Vec::new(),
    }
}

#[test]
fn tool_results_are_held_from_the_cloud_as_in_node() {
    let f = fixture();
    let mut out = Failures::default();
    let turns = items(&f, "hold");
    assert!(turns.len() > 60);
    for (i, t) in turns.iter().enumerate() {
        let boxes: Vec<Toolbox> = items(t, "boxes").iter().map(toolbox).collect();
        let cloud = t.get("route").as_str() == Some("cloud");
        let mut hold = CloudHold::default();
        let (mut logs, mut lines) = (Vec::new(), Vec::new());
        for call in items(t, "calls") {
            let name = call.get("name").as_str().unwrap();
            let text = call.get("text").as_str().unwrap();
            let public = is_public_tool_name(Some(name), &boxes);
            out.check(Some(public) == call.get("public").as_bool(), || {
                format!("turn {i}: {name} public {public}")
            });
            let held = hold.hold(cloud, name, text, public);
            out.check(Some(held.is_some()) == call.get("held").as_bool(), || {
                format!("turn {i}: {name} {text:?} held {held:?}")
            });
            if let Some(h) = held {
                logs.push(h.log);
                lines.push(JValue::from(h.line));
            }
        }
        out.check(hold.held == flag(t.get("held")), || {
            format!("turn {i}: held {:?}", hold.held)
        });
        out.check(
            json(&JValue::Arr(logs.clone())) == json(t.get("logs")),
            || format!("turn {i}: logs {logs:?}"),
        );
        out.check(
            json(&JValue::Arr(lines.clone())) == json(t.get("lines")),
            || format!("turn {i}: lines {lines:?}"),
        );
    }
    assert!(turns.iter().any(|t| flag(t.get("held")) == Some("diary")));
    out.done("hold");
}

fn option(o: &JValue, key: &str) -> Option<f64> {
    match o.get(key) {
        JValue::Num(n) => Some(*n),
        JValue::Undefined | JValue::Null => None,
        other => panic!("{key}: {other:?}"),
    }
}

#[test]
fn the_router_reads_what_node_s_router_input_gives_it() {
    let f = fixture();
    let mut out = Failures::default();
    let cases = items(&f, "routerInput");
    assert!(cases.len() > 150);
    for (i, c) in cases.iter().enumerate() {
        let input = c.get("input");
        let attachments: &[JValue] = match input.get("attachments") {
            JValue::Arr(a) => a,
            _ => &[],
        };
        let o = c.get("options");
        let got = router_input(
            input.get("message").as_str().unwrap_or_default(),
            input.get("system").as_str().unwrap_or_default(),
            input.get("history"),
            attachments,
            option(o, "chunkChars"),
            option(o, "maxChunks"),
        );
        let want = c.get("out");
        match got {
            Err(_) => out.check(want.get("threw").as_str() == Some("TypeError"), || {
                format!("case {i}: threw, Node {}", json(want))
            }),
            Ok(RouterInput { chunks, complete }) => {
                let got = JValue::Obj(vec![
                    (
                        "chunks".into(),
                        JValue::Arr(chunks.into_iter().map(JValue::from).collect()),
                    ),
                    ("complete".into(), JValue::Bool(complete)),
                ]);
                out.check(json(&got) == json(want), || {
                    let g = json(&got);
                    let w = json(want);
                    format!(
                        "case {i}: {}..., Node {}...",
                        g.chars().take(200).collect::<String>(),
                        w.chars().take(200).collect::<String>()
                    )
                });
            }
        }
    }
    assert!(cases
        .iter()
        .any(|c| c.get("out").get("complete") == &JValue::Bool(false)));
    assert!(cases
        .iter()
        .any(|c| c.get("out").get("threw").as_str().is_some()));
    out.done("routerInput");
}

#[test]
fn chunk_chars_for_is_node_s() {
    let f = fixture();
    assert!(items(&f, "chunkCharsFor").len() >= 10);
    for c in items(&f, "chunkCharsFor") {
        let limit = match c.get("limit") {
            JValue::Num(n) => Some(*n),
            JValue::Null => Some(0.0),
            _ => None,
        };
        assert_eq!(
            Some(chunk_chars_for(limit) as f64),
            c.get("out").as_num(),
            "{}",
            json(c)
        );
    }
}
