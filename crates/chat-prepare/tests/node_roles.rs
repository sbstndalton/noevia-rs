//! The recorded Node output of slice P5b (`fixtures/node-roles.v1.json`, written by
//! `record_roles.cjs`): every heuristic, verdict, classification, roles check and turn route must be
//! what noevia-core's auto-router.cjs, system-one-router.cjs, auto-roles-check.cjs,
//! chat-model-kind.cjs and chat.cjs's own lines produced.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::cell::RefCell;
use std::collections::HashMap;

use chat_prepare::roles::{
    self, Classification, Halt, Preload, Reply, Role, Routing, Seam, Step, Turn,
};
use chat_prepare::router::{self, Answer, Rules};
use chat_prepare::system_one::{self, Backend, Router, Start};
use js_json::{wtf16, JValue};

const FIXTURE: &str = include_str!("fixtures/node-roles.v1.json");
const ROUTE_FIXTURE: &str = include_str!("fixtures/node-route.v1.json");
const SKILLS_FIXTURE: &str = include_str!("fixtures/node-skills.v1.json");
/// The chat.cjs lines the recorder runs (`record_roles.cjs` cuts them between its anchors): a change
/// there must be reviewed against `src/roles.rs` and these constants edited with it.
const BLOCKS: [(&str, &str, [f64; 2]); 4] = [
    (
        "wantsAuto",
        "5856ae86b0c5acc09063c17b80b4eaae92d487248193c4f90a3c24119dbda7f3",
        [917.0, 926.0],
    ),
    (
        "provider",
        "cbf084976224478229f237538242a2200e9a7e20d551458e6d7d7e4882c7abcc",
        [1009.0, 1009.0],
    ),
    (
        "recordResend",
        "ef0e3d8573b33a1cba00b0271b0478e288a7e6e1b9437ad60fc7a7ac1cbf1e3e",
        [379.0, 386.0],
    ),
    (
        "route",
        "07eedce0e05e065fb64cd68d7495f8cf697994eb5cb9849402b6f874db2f849c",
        [1056.0, 1165.0],
    ),
];

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

fn role(r: Role) -> JValue {
    JValue::from(r.as_str())
}

fn thrown() -> JValue {
    JValue::obj([("thrown", JValue::from("TypeError"))])
}

/// A recorded `{ out }` or `{ thrown }`, as the port's answer is stated.
fn outcome(c: &JValue) -> String {
    match c.get("thrown") {
        JValue::Undefined => stringify(c.get("out")),
        t => stringify(&JValue::obj([("thrown", t.clone())])),
    }
}

fn answered<T>(r: Result<T, impl std::fmt::Debug>, f: impl FnOnce(T) -> JValue) -> String {
    match r {
        Ok(v) => stringify(&f(v)),
        Err(_) => stringify(&thrown()),
    }
}

/// Collects failures and reports the first few with Node's and the port's output.
#[derive(Default)]
struct Diffs {
    failed: Vec<String>,
    total: usize,
}

impl Diffs {
    fn check(&mut self, what: &JValue, want: &str, got: &str) {
        self.total += 1;
        if want != got {
            let what = stringify(what);
            let cut = what.char_indices().nth(400).map_or(what.len(), |(i, _)| i);
            self.failed.push(format!(
                "{}\n  node: {want:?}\n  rust: {got:?}",
                &what[..cut]
            ));
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

#[test]
fn the_fixture_names_its_node_sources() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    for file in [
        "server/chat.cjs",
        "server/index.cjs",
        "server/auto-router.cjs",
        "server/system-one-router.cjs",
        "server/auto-roles-check.cjs",
        "server/chat-model-kind.cjs",
        "server/model-system.cjs",
        "server/route-preload.cjs",
        "server/decision-wording.cjs",
        "server/decision/index.cjs",
        "server/sampling-presets.cjs",
    ] {
        let sha = f.get("sources").get(file).as_str().unwrap_or_default();
        assert_eq!(sha.len(), 64, "{file}");
    }
    // Recorded at the same core as the crate's other fixtures (tests/oracle-core.ref).
    for (other, files) in [
        (
            ROUTE_FIXTURE,
            &["server/chat.cjs", "server/sampling-presets.cjs"][..],
        ),
        (SKILLS_FIXTURE, &["server/chat.cjs", "server/index.cjs"][..]),
    ] {
        let other = wtf16::parse(other).unwrap();
        for file in files {
            assert_eq!(
                f.get("sources").get(file),
                other.get("sources").get(file),
                "{file}"
            );
        }
    }
    for (name, sha, lines) in BLOCKS {
        let block = f.get("blocks").get(name);
        assert_eq!(block.get("sha256").as_str(), Some(sha), "{name}");
        let got: Vec<f64> = items(block, "lines")
            .iter()
            .map(|n| n.as_num().unwrap())
            .collect();
        assert_eq!(got, lines, "{name}");
    }
    assert_eq!(f.get("defaultProviderId").as_str(), Some("default"));
    // No named differences: everything recorded replays byte for byte.
    assert!(items(&f, "differences").is_empty());
}

#[test]
fn the_heuristics_match_node() {
    let f = fixture();
    let mut d = Diffs::default();
    for c in items(&f, "heuristics") {
        let m = c.get("message").as_str().unwrap();
        let got = JValue::obj([
            ("message", JValue::from(m)),
            ("smart", JValue::Bool(router::wants_smart(m))),
            ("code", JValue::Bool(router::wants_code(m))),
            ("simple", JValue::Bool(system_one::clearly_simple(m))),
        ]);
        d.check(c.get("message"), &stringify(c), &stringify(&got));
    }
    d.done("heuristics", 800);
}

#[test]
fn verdicts_and_bodies_match_node() {
    let f = fixture();
    let mut d = Diffs::default();
    for c in items(&f, "verdicts") {
        let with_code = c.get("withCode").as_bool().unwrap();
        let got = answered(router::verdict(c.get("msg"), with_code), |r| {
            r.map_or(JValue::Null, role)
        });
        d.check(c, &outcome(c), &got);
    }
    d.done("verdicts", 300);
    let mut d = Diffs::default();
    for c in items(&f, "bodies") {
        let got = router::classifier_body(
            c.get("model"),
            c.get("message").as_str().unwrap(),
            c.get("suppress").as_bool().unwrap(),
            c.get("withCode").as_bool().unwrap(),
        );
        d.check(c, c.get("out").as_str().unwrap(), &got);
    }
    d.done("bodies", 100);
}

fn answer(reply: &JValue) -> Option<Answer<'_>> {
    (reply.as_str() != Some("throw") && !reply.is_nullish()).then(|| Answer {
        ok: reply.get("ok").truthy(),
        status: reply.get("status").as_num().unwrap() as u16,
        body: reply.get("body"),
    })
}

/// The auto router's classify, its seams answered by the case: admission, then up to two calls.
fn classify(c: &JValue) -> (Role, Vec<JValue>) {
    let roles = c.get("roles");
    let message = c.get("message").as_str().unwrap();
    let mut sent = Vec::new();
    let with_code = match router::rules(roles, message) {
        Rules::Role(r) => return (r, sent),
        Rules::Ask { with_code } => with_code,
    };
    let admit = c.get("admit");
    if admit.as_str() == Some("throw") || admit.is_object() {
        return (Role::Fast, sent);
    }
    let replies = items(c, "replies");
    let mut reply = answer(&replies[0]);
    sent.push(JValue::Str(router::classifier_body(
        roles.get("fast"),
        message,
        true,
        with_code,
    )));
    if reply.as_ref().is_some_and(router::retry_plainly) {
        sent.push(JValue::Str(router::classifier_body(
            roles.get("fast"),
            message,
            false,
            with_code,
        )));
        reply = replies.get(1).and_then(answer);
    }
    (router::role_of(reply.as_ref(), with_code), sent)
}

#[test]
fn the_auto_router_matches_node() {
    let f = fixture();
    let mut d = Diffs::default();
    for c in items(&f, "classify") {
        let (r, sent) = classify(c);
        let want = JValue::obj([
            ("out", c.get("out").clone()),
            ("sent", c.get("sent").clone()),
        ]);
        let got = JValue::obj([("out", role(r)), ("sent", JValue::Arr(sent))]);
        d.check(c, &stringify(&want), &stringify(&got));
    }
    d.done("classify", 450);
}

#[test]
fn the_system_one_router_matches_node() {
    let f = fixture();
    let service_model = f.get("serviceModel").as_str().unwrap();
    let mut d = Diffs::default();
    for c in items(&f, "systemOne") {
        let backend = c.get("backend");
        let router = Router {
            roles: c.get("roles"),
            enabled: c.get("enabled").as_bool().unwrap(),
            backend: backend.truthy().then(|| Backend {
                id: backend.get("id"),
                wording: backend.get("wording"),
            }),
            precheck: c.get("precheck").as_bool() != Some(false),
            deadline_ms: c.get("deadlineMs"),
            service_model,
        };
        let message = c.get("message").as_str().unwrap();
        let fallback_role = Role::of(c.get("fallbackRole").as_str().unwrap()).unwrap();
        let (mut request, mut fallbacks) = (JValue::Undefined, 0);
        let got: Classification = match router.start(message) {
            Start::Fallback(reason) => {
                fallbacks = 1;
                router.fallback(reason, fallback_role)
            }
            Start::Simple(simple) => simple,
            Start::Decide(req) => {
                request = req;
                let result = c.get("result");
                let r = router.accepted(result).unwrap_or_else(|| {
                    fallbacks = 1;
                    fallback_role
                });
                router.decided(result, r)
            }
        };
        let out = |role: JValue, decision: JValue, request: JValue, fallbacks: JValue| {
            stringify(&JValue::obj([
                (
                    "out",
                    JValue::obj([("role", role), ("routingDecision", decision)]),
                ),
                ("request", request),
                ("fallbacks", fallbacks),
            ]))
        };
        let want = c.get("out");
        d.check(
            c,
            &out(
                want.get("role").clone(),
                want.get("routingDecision").clone(),
                c.get("request").clone(),
                c.get("fallbacks").clone(),
            ),
            &out(
                role(got.role),
                got.decision,
                request,
                JValue::Num(f64::from(fallbacks)),
            ),
        );
    }
    d.done("systemOne", 500);
}

#[test]
fn the_roles_checks_match_node() {
    let f = fixture();
    let mut d = Diffs::default();
    for c in items(&f, "missing") {
        let missing = roles::missing_roles(c.get("roles"), c.get("installed"));
        let list = |m: &[(Role, JValue)]| {
            JValue::Arr(
                m.iter()
                    .map(|(r, model)| JValue::obj([("role", role(*r)), ("model", model.clone())]))
                    .collect(),
            )
        };
        let mut want = outcome(c);
        let mut got = answered(missing.clone(), |m| list(&m));
        if let Ok(m) = &missing {
            want += &outcome(c.get("stale"));
            got += &answered(roles::stale_roles_error(m), |e| {
                e.map_or(JValue::Null, JValue::Str)
            });
        }
        d.check(c, &want, &got);
    }
    d.done("missing", 200);
    let mut d = Diffs::default();
    for c in items(&f, "creative") {
        let routed = Role::of(c.get("routed").as_str().unwrap()).unwrap();
        let got = roles::creative_move_is_free(c.get("roles"), routed, c.get("installed"));
        d.check(c, &outcome(c), &answered(got, JValue::Bool));
    }
    d.done("creative", 800);
}

fn system_roles(c: &JValue) -> HashMap<String, String> {
    items(c, "systemRoles")
        .iter()
        .map(|pair| match pair {
            JValue::Arr(kv) => (
                kv[0].as_str().unwrap().to_string(),
                kv[1].as_str().unwrap().to_string(),
            ),
            other => panic!("a section role that is not a pair: {other:?}"),
        })
        .collect()
}

#[test]
fn the_chat_model_kind_matches_node() {
    let f = fixture();
    let mut d = Diffs::default();
    let mut system = 0;
    for c in items(&f, "chatModels") {
        let map = system_roles(c);
        let got = roles::is_chat_generation_model(c.get("name"), c.get("labels"), &map);
        system += usize::from(
            c.get("name")
                .as_str()
                .is_some_and(|n| roles::is_system_model(n, &map)),
        );
        d.check(c, &outcome(c), &answered(got, JValue::Bool));
    }
    assert!(system >= 100, "only {system} system models");
    d.done("chatModels", 2000);
}

/// One recorded turn through [`Routing`]: each seam answered from the case, as a caller would, and
/// the pre-load actions done as [`Preload`] says. The outcome and the calls, in the recorder's terms.
fn turn(c: &JValue, default: &str) -> (JValue, Vec<String>) {
    let calls = RefCell::new(Vec::<String>::new());
    let push = |s: String| calls.borrow_mut().push(s);
    let code_project = || {
        push("codeProject".into());
        c.get("codeProject").truthy()
    };
    let system_roles = system_roles(c);
    let mut routing = Routing::new(Turn {
        project: c.get("project"),
        default_provider_id: default,
        message: c.get("message").as_str().unwrap(),
        manager_enabled: c.get("managerEnabled").truthy(),
        compact_only: c.get("compactOnly").truthy(),
        task_mode: c.get("mode").as_str() == Some("cowork"),
        has_images: c.get("images").truthy(),
        code_project: &code_project,
        resend: c.get("resend"),
        system_roles: &system_roles,
    });
    let (mut reply, mut reads) = (Reply::Start, 0);
    let (route, preload, resend) = loop {
        let seam = match routing.step(reply) {
            Step::Ask(seam) => seam,
            Step::Done {
                route,
                preload,
                resend,
            } => break (route, preload, resend),
        };
        reply = match seam {
            Seam::AutoRoles => {
                push("autoRoles".into());
                reads += 1;
                let later = c.get("savedRoles");
                let saved = reads > 1
                    && matches!(c, JValue::Obj(o) if o.iter().any(|(k, _)| k == "savedRoles"));
                Reply::AutoRoles(if saved { later } else { c.get("autoRoles") }.clone())
            }
            Seam::Provider(id) => {
                push(format!("getProvider:{}", stringify(&id)));
                Reply::Provider(c.get("provider").clone())
            }
            Seam::ServedCatalogue => {
                push("servedCatalogue".into());
                Reply::Catalogue(c.get("catalogue").clone())
            }
            Seam::StartPreload(model) => {
                push(format!("startPreload:{}", stringify(&model)));
                Reply::Preload {
                    started: c.get("preloadStarts").as_bool() != Some(false),
                }
            }
            Seam::Classify => {
                push("classify".into());
                Reply::Classified(match c.get("classification") {
                    JValue::Str(r) => Classification {
                        role: Role::of(r).unwrap(),
                        decision: JValue::Null,
                    },
                    o => Classification {
                        role: Role::of(o.get("role").as_str().unwrap()).unwrap(),
                        decision: o.get("routingDecision").clone(),
                    },
                })
            }
            Seam::ModelsInstalled => {
                push("modelsInstalled".into());
                Reply::Installed
            }
            Seam::LastLoadedModel => {
                push("lastLoadedModel".into());
                Reply::LastLoaded(c.get("lastLoaded").clone())
            }
        };
    };
    if let Some(entry) = &resend {
        push(format!("recordOutcome:{}", stringify(entry)));
    }
    if preload == Preload::Abandoned {
        if !c.get("aborted").truthy() {
            push("preload.use".into());
        }
        push("preload.cancel".into());
    }
    if preload != Preload::NotStarted {
        push("preload.releaseIfUnused".into());
    }
    let out = match route {
        Ok(r) => JValue::obj([
            ("projectProvider", r.project_provider),
            ("wantsAuto", JValue::Bool(r.wants_auto)),
            ("model", r.model),
            ("routedRole", r.role.map_or(JValue::Null, role)),
            ("routingDecision", r.decision),
        ]),
        Err(Halt::Refused(r)) => JValue::obj([(
            "refusal",
            JValue::obj([
                ("status", JValue::Num(f64::from(r.status))),
                ("body", r.body()),
            ]),
        )]),
        Err(Halt::Thrown) => thrown(),
        Err(Halt::OutOfOrder) => panic!("the replay answered a seam out of order"),
    };
    (out, calls.into_inner())
}

#[test]
fn the_turns_route_as_node_does() {
    let f = fixture();
    let default = f.get("defaultProviderId").as_str().unwrap();
    let mut d = Diffs::default();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for c in items(&f, "turns") {
        let (out, calls) = turn(c, default);
        let calls = JValue::Arr(calls.into_iter().map(JValue::Str).collect());
        let both = |out: &JValue, calls: &JValue| {
            stringify(&JValue::obj([
                ("out", out.clone()),
                ("calls", calls.clone()),
            ]))
        };
        let text = stringify(&calls);
        for (what, hit) in [
            ("auto", out.get("wantsAuto").truthy()),
            ("refused", !out.get("refusal").is_nullish()),
            (
                "notConfigured",
                stringify(&out).contains("Auto routing is not configured yet"),
            ),
            (
                "creative",
                out.get("routedRole").as_str() == Some("creative"),
            ),
            ("preload", text.contains("startPreload")),
            ("cancel", text.contains("preload.cancel")),
            ("use", text.contains("preload.use")),
            ("codeProject", text.contains("codeProject")),
        ] {
            *seen.entry(what).or_default() += usize::from(hit);
        }
        d.check(c, &both(c.get("out"), c.get("calls")), &both(&out, &calls));
    }
    // Enough of each outcome to mean something.
    for (what, min) in [
        ("auto", 500),
        ("refused", 500),
        ("notConfigured", 20),
        ("creative", 20),
        ("preload", 200),
        ("cancel", 20),
        ("use", 15),
        ("codeProject", 30),
    ] {
        assert!(
            seen.get(what).copied().unwrap_or(0) >= min,
            "{what}: {seen:?}"
        );
    }
    d.done("turns", 3000);
}

#[test]
fn a_seam_answered_out_of_order_ends_the_routing() {
    let no = || false;
    let roles = HashMap::new();
    let t = Turn {
        project: &JValue::Null,
        default_provider_id: "default",
        message: "Say hello",
        manager_enabled: true,
        compact_only: false,
        task_mode: false,
        has_images: false,
        code_project: &no,
        resend: &JValue::Undefined,
        system_roles: &roles,
    };
    let mut r = Routing::new(t);
    assert_eq!(r.step(Reply::Start), Step::Ask(Seam::AutoRoles));
    let out_of_order = Step::Done {
        route: Err(Halt::OutOfOrder),
        preload: Preload::NotStarted,
        resend: None,
    };
    assert_eq!(r.step(Reply::Installed), out_of_order);
    assert_eq!(r.step(Reply::Start), out_of_order);
}
