//! Routing modes (slice P5d, first part) replayed on recorded Node output
//! (`fixtures/node-modes.v1.json`, written by `record_modes.cjs`): every turn's seam calls in
//! order from `routingModes.enabled()` on, the log entries, the route events, the refusals and
//! throws, the `[routing]` line, the meta event's `routing`, and a cloud route's provider and
//! model.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]


use chat_prepare::modes::{
    attachment_names, cloud_model, has_diary_content, off_box_turn, sent_text, Modes, ModesTurn,
    PreText, Reply, Seam, Step,
};
use chat_prepare::roles::Halt;
use chat_prepare::Thrown;
use js_json::{wtf16, JValue};

const FIXTURE: &str = include_str!("fixtures/node-modes.v1.json");
const REROUTE_FIXTURE: &str = include_str!("fixtures/node-reroute.v1.json");

fn fixture() -> JValue {
    wtf16::parse(FIXTURE).expect("the fixture is JSON")
}

fn items<'a>(v: &'a JValue, key: &str) -> &'a [JValue] {
    match v.get(key) {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

fn s(v: &str) -> JValue {
    JValue::from(v)
}

/// A stub's answer: `{ throws: true }` rejects.
fn answer(v: &JValue) -> Result<JValue, Thrown> {
    if v.get("throws") == &JValue::Bool(true) {
        Err(Thrown)
    } else {
        Ok(v.clone())
    }
}

const MODES_SEAMS: [&str; 9] = [
    "enabled",
    "settings",
    "preRule",
    "sensitivity",
    "openChoice",
    "awaitChoice",
    "log",
    "setChatFlags",
    "randomUUID",
];

#[test]
fn the_fixture_names_its_node_sources() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    let other = wtf16::parse(REROUTE_FIXTURE).unwrap();
    for file in ["server/chat.cjs", "server/index.cjs", "server/routing-modes.cjs"] {
        let sha = f.get("sources").get(file).as_str().unwrap_or_default();
        assert_eq!(sha.len(), 64, "{file}");
    }
    // Recorded at the same core as the crate's other fixtures (tests/oracle-core.ref).
    for file in ["server/chat.cjs", "server/index.cjs"] {
        assert_eq!(f.get("sources").get(file), other.get("sources").get(file), "{file}");
    }
    assert_eq!(items(&f, "differences"), &[] as &[JValue]);
}

#[test]
fn every_recorded_turn_routes_as_node() {
    let f = fixture();
    let diary = f.get("facts").get("DIARY_EXTRAS_ID").as_str().unwrap().to_string();
    let providers = f.get("facts").get("providers").clone();
    let turns = items(&f, "turns");
    assert!(turns.len() >= 130);
    let mut cloud_routes = 0;
    for turn in turns {
        let name = turn.get("name").as_str().unwrap();
        let spec = turn.get("spec");
        let body = turn.get("body");
        let modes = spec.get("modes");
        let project = if spec.get("project").is_nullish() { JValue::Null } else { spec.get("project").clone() };
        let manual = project.truthy() && project.get("routing").as_str() != Some("auto");
        // A project chat without an id gets chat.cjs's minted one (the recorder pins its clock and random).
        let chat_id = if body.get("chatId").truthy() {
            body.get("chatId").clone()
        } else if project.truthy() {
            s("p-1763035200000-i")
        } else {
            JValue::Null
        };
        if !turn.get("meta").is_nullish() {
            assert_eq!(turn.get("meta").get("chatId"), &chat_id, "{name}: chat id");
        }
        let names = attachment_names(&project).unwrap();
        let history = match body.get("history") {
            JValue::Arr(h) => JValue::Arr(h.clone()),
            _ => JValue::Arr(vec![]),
        };
        let t = ModesTurn {
            wants_auto: !manual,
            project: &project,
            list_project: body.get("projectId").truthy() && project.truthy(),
            chat_id: &chat_id,
            space_id: body.get("spaceId"),
            message: spec.get("message").as_str().unwrap(),
            attachments: Some(&names),
            history: &history,
            compact_only: body.get("compactOnly").truthy(),
            default_provider_id: "default",
            diary_project_id: &diary,
            user: spec.get("noUser") != &JValue::Bool(true),
            open_choice: !modes.get("openChoice").is_undefined_js(),
        };
        let calls = items(turn, "calls");
        let mut at = calls
            .iter()
            .position(|c| items_of(c).first().and_then(|x| x.as_str()) == Some("enabled"))
            .unwrap_or(calls.len());
        let events = items(turn, "events");
        let mut ev = 0;
        let mut m = Modes::new(t);
        let mut reply = Reply::Start;
        let done = loop {
            let seam = match m.step(reply) {
                Step::Done(d) => break d,
                Step::Ask(seam) => seam,
            };
            let mut next = || {
                let c = calls.get(at).unwrap_or_else(|| panic!("{name}: {seam:?} asked past the calls"));
                at += 1;
                items_of(c).to_vec()
            };
            reply = match &seam {
                Seam::OpenStream => Reply::Done,
                Seam::Send(e) => {
                    assert_eq!(Some(e), events.get(ev), "{name}: event");
                    ev += 1;
                    Reply::Done
                }
                _ => {
                    let c = next();
                    let kind = c[0].as_str().unwrap().to_string();
                    match (&seam, kind.as_str()) {
                        (Seam::Enabled, "enabled") => Reply::Value(Ok(modes.get("enabled").clone())),
                        (Seam::Settings, "settings") => Reply::Value(answer(modes.get("settings"))),
                        (Seam::ChatList(Some(id)), "getProject") => {
                            assert_eq!(&c[1], id, "{name}");
                            Reply::Value(Ok(project.get("chats").clone()))
                        }
                        (Seam::ChatList(None), "freeChats") => Reply::Value(Ok(match spec.get("freeChats") {
                            JValue::Undefined => JValue::Arr(vec![]),
                            v => v.clone(),
                        })),
                        (Seam::Provider(id), "getProvider") => {
                            assert_eq!(&c[1], id, "{name}");
                            let row = providers.get(id.as_str().unwrap());
                            Reply::Value(Ok(if row.is_nullish() { JValue::Null } else { row.clone() }))
                        }
                        (Seam::AutoRoles, "autoRoles") => Reply::Value(Ok(spec.get("roles").clone())),
                        (Seam::PreRule(text), "preRule") => {
                            match text {
                                PreText::Message(x) | PreText::Attachments(x) => assert_eq!(c[1].as_str(), Some(x.as_str()), "{name}"),
                                PreText::Sent => {}
                            }
                            Reply::Value(Ok(c[2].clone()))
                        }
                        (Seam::Sensitivity, "sensitivity") => Reply::Value(answer(modes.get("verdict"))),
                        (Seam::ChoiceId, "randomUUID") => Reply::Id(c[1].as_str().unwrap().to_string()),
                        (Seam::OpenChoice(id), "openChoice") => {
                            assert_eq!(c[1].as_str(), Some(id.as_str()), "{name}");
                            assert_eq!(c[3], chat_id, "{name}");
                            Reply::Value(answer(modes.get("openChoice")))
                        }
                        (Seam::AwaitChoice(id), "awaitChoice") => {
                            assert_eq!(c[1].as_str(), Some(id.as_str()), "{name}");
                            assert_eq!(c[2], s("qa-user"), "{name}");
                            Reply::Value(answer(modes.get("choice")))
                        }
                        (Seam::Log(entry), "log") => {
                            assert_eq!(&c[1], entry, "{name}: log entry");
                            Reply::Done
                        }
                        (Seam::SetChatFlags { project_id, chat_id, patch }, "setChatFlags") => {
                            assert_eq!((&c[1], &c[2], &c[3]), (project_id, chat_id, patch), "{name}");
                            Reply::Done
                        }
                        _ => panic!("{name}: Rust asked {seam:?}, Node called {kind}"),
                    }
                }
            };
        };
        assert_eq!(ev, events.iter().filter(|e| e.get("type").as_str() != Some("error")).count(), "{name}: events");
        // Nothing of routing modes is called after the part.
        for c in &calls[at..] {
            let kind = items_of(c)[0].as_str().unwrap();
            assert!(!MODES_SEAMS.contains(&kind), "{name}: Node called {kind} after Rust was done");
        }
        let lines = items(turn, "lines");
        match done {
            Err(Halt::Thrown) => assert!(turn.get("throws").truthy(), "{name}: Node did not throw"),
            Err(Halt::Refused(r)) => {
                let error = events.iter().find(|e| e.get("type").as_str() == Some("error"));
                match error {
                    Some(e) => assert_eq!(e.get("text").as_str(), Some(r.error.as_str()), "{name}"),
                    None => assert_eq!(turn.get("refused"), &r.body_with_status(), "{name}"),
                }
            }
            Err(h) => panic!("{name}: {h:?}"),
            Ok(d) => {
                assert!(!turn.get("throws").truthy(), "{name}: Node threw");
                let meta = turn.get("meta");
                let want_line: Vec<String> = lines.iter().map(|l| l.as_str().unwrap().to_string()).collect();
                assert_eq!(d.log_line.into_iter().collect::<Vec<_>>(), want_line, "{name}: line");
                match d.target {
                    None => assert!(meta.get("routing").is_nullish(), "{name}"),
                    Some(target) => {
                        assert_eq!(meta.get("routing"), &target.meta_routing(), "{name}: routing");
                        let provider = target.provider();
                        assert_eq!(off_box_turn(false, Some(&target), provider.as_ref().unwrap_or(&JValue::obj([("id", s("default"))])), "default"), target.route == "cloud");
                        if let Some(row) = provider {
                            cloud_routes += 1;
                            assert_eq!(row.get("external"), &JValue::Bool(true));
                            let base = row.get("baseUrl").as_str().unwrap();
                            assert!(turn.get("reply").get("url").as_str().unwrap().starts_with(base), "{name}");
                            let role = meta.get("route").as_str().unwrap();
                            assert_eq!(meta.get("model"), &cloud_model(&target.cloud, role).unwrap(), "{name}: model");
                        }
                    }
                }
            }
        }
    }
    assert!(cloud_routes >= 20, "{cloud_routes}");
}

fn items_of(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

trait Undef {
    fn is_undefined_js(&self) -> bool;
}
impl Undef for JValue {
    fn is_undefined_js(&self) -> bool {
        matches!(self, JValue::Undefined)
    }
}

trait Body {
    fn body_with_status(&self) -> JValue;
}
impl Body for chat_prepare::roles::Refusal {
    fn body_with_status(&self) -> JValue {
        JValue::obj([("status", JValue::Num(f64::from(self.status))), ("body", self.body())])
    }
}

#[test]
fn the_pure_helpers() {
    let h = |name: &str| JValue::Arr(vec![JValue::obj([("role", s("tool")), ("name", s(name))])]);
    assert!(has_diary_content(&h("DIARY_x")).unwrap());
    assert!(!has_diary_content(&h("diaryx")).unwrap());
    assert!(!has_diary_content(&h("diar\u{fc}_x")).unwrap());
    let bad = JValue::Arr(vec![JValue::obj([("role", s("tool")), ("name", JValue::obj([("toString", s("x"))]))])]);
    assert_eq!(has_diary_content(&bad), Err(Thrown));
    let p = JValue::obj([("assets", JValue::Arr(vec![JValue::obj([("name", s("qa.png"))])])), ("files", JValue::Arr(vec![JValue::obj([("name", JValue::Num(1.0))]), JValue::obj([("name", s("qa.txt"))])]))]);
    assert_eq!(attachment_names(&p).unwrap(), "qa.png\nqa.txt");
    assert_eq!(attachment_names(&JValue::obj([("files", s("qa"))])), Err(Thrown));
    assert_eq!(attachment_names(&JValue::obj([("files", JValue::Arr(vec![JValue::Null]))])), Err(Thrown));
    let msgs = [JValue::obj([("content", s("QA a"))]), JValue::obj([("content", JValue::Arr(vec![]))])];
    assert_eq!(sent_text("QA sys", &msgs), "QA sys\nQA a\n");
    let cloud = JValue::obj([("smart", s("")), ("fast", s("")), ("code", s("qa-c"))]);
    assert_eq!(cloud_model(&cloud, "fast").unwrap(), s("qa-c"));
    assert!(cloud_model(&JValue::Null, "fast").is_err());
    assert!(off_box_turn(false, None, &JValue::obj([("id", s("qa-cloud"))]), "default"));
    assert!(!off_box_turn(false, None, &JValue::obj([("id", s("qa-m")), ("machine", JValue::Bool(true))]), "default"));
    assert!(!off_box_turn(false, None, &JValue::obj([("id", s("default"))]), "default"));
    // A reply out of order ends the part.
    let (project, chat, history) = (JValue::Null, JValue::Null, JValue::Arr(vec![]));
    let mut m = Modes::new(ModesTurn {
        wants_auto: true, project: &project, list_project: false, chat_id: &chat, space_id: &JValue::Undefined, message: "QA",
        attachments: Some(""), history: &history, compact_only: false, default_provider_id: "default",
        diary_project_id: "qa-diary", user: true, open_choice: false,
    });
    assert_eq!(m.step(Reply::Start), Step::Ask(Seam::Enabled));
    assert_eq!(m.step(Reply::Done), Step::Done(Err(Halt::OutOfOrder)));
}
