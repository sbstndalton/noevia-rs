//! The recorded Node output of slice P5c (`fixtures/node-reroute.v1.json`, written by
//! `record_reroute.cjs`): whole turns through chat.cjs's `createChatHandler`, replayed here through
//! [`roles`] (P5b), [`offer`] (P6a), [`reroute`] (P5c) and [`route`] (P5a) in chat.cjs's order. The
//! seams' calls up to the meta event, the refusal or throw, the meta event's model, route,
//! routing decision, context profile and sampling, the status line, the `tools_scope` event, the
//! move's and the long-profile module's log lines and the reply request's model and tools must be
//! what Node produced.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::cell::RefCell;
use std::collections::HashMap;

use chat_prepare::effort::Engine;
use chat_prepare::offer::{self, Offer, OfferInput, Selection, ToolTurn};
use chat_prepare::reroute::{self, LongProfileLog, MoveTurn, ToolMove};
use chat_prepare::roles::{self, Classification, Halt, Role};
use chat_prepare::route::{self, RouteTurn};
use chat_prepare::toolboxes::{Toolbox, Writes};
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};

const FIXTURE: &str = include_str!("fixtures/node-reroute.v1.json");
const TOOLS_FIXTURE: &str = include_str!("fixtures/node-tools.v1.json");
const ROLES_FIXTURE: &str = include_str!("fixtures/node-roles.v1.json");
/// The chat.cjs lines this slice ports (`record_reroute.cjs` hashes them between its anchors): a
/// change there must be reviewed against `src/reroute.rs` and these constants edited with it.
const BLOCKS: [(&str, &str, [f64; 2]); 4] = [
    (
        "toolMove",
        "cc7f626517c6e586f78ec33663fa2b1aab84643671c1be83b7b0d3f4c9e4f466",
        [1337.0, 1382.0],
    ),
    (
        "longProfile",
        "aa2fd6d02861fe2e975f94eb8281b17435de8d4b2eafe3e45d60c9d947187173",
        [1387.0, 1400.0],
    ),
    (
        "metaProfile",
        "6306fdbba9e6dea42b59bbe40dd83065ead6d2eca5d5011caf5b61ccb81b31e7",
        [1453.0, 1452.0],
    ),
    (
        "status",
        "f55b72f240e36557294663646aba29c84f2030ca9e870424dcd56076a6f3de7f",
        [1459.0, 1460.0],
    ),
];
const DEFAULT: &str = "default";

fn fixture() -> JValue {
    wtf16::parse(FIXTURE).expect("the fixture is JSON")
}

fn items<'a>(v: &'a JValue, key: &str) -> &'a [JValue] {
    match v.get(key) {
        JValue::Arr(a) => a,
        other => panic!("{key} is not a list: {other:?}"),
    }
}

fn stringify(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "undefined".to_string())
}

fn text(v: &JValue) -> &str {
    v.as_str().unwrap_or_else(|| panic!("not text: {v:?}"))
}

fn texts(v: &JValue) -> Vec<String> {
    match v {
        JValue::Arr(a) => a.iter().map(|x| text(x).to_string()).collect(),
        other => panic!("not a list: {other:?}"),
    }
}

fn or_null(v: &JValue) -> JValue {
    match v {
        JValue::Undefined => JValue::Null,
        other => other.clone(),
    }
}

fn call(parts: Vec<JValue>) -> JValue {
    JValue::Arr(parts)
}

fn toolbox(v: &JValue) -> Toolbox {
    Toolbox {
        id: text(v.get("id")).to_string(),
        label: v.get("label").as_str().unwrap_or_default().to_string(),
        source: v.get("source").as_str().unwrap_or_default().to_string(),
        in_app: v.get("inApp") == &JValue::Bool(true),
        server: v
            .get("server")
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        tools: match v.get("tools") {
            JValue::Arr(t) => t.clone(),
            other => panic!("tools: {other:?}"),
        },
        reads: texts(v.get("reads")),
    }
}

fn sha_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn the_fixture_names_its_node_sources() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    for file in [
        "server/chat.cjs",
        "server/index.cjs",
        "server/tools-mode.cjs",
        "server/long-profile.cjs",
        "server/dav-parse-wasm.cjs",
        "server/chat-template-caps.cjs",
        "server/toolboxes.cjs",
        "server/auto-roles-check.cjs",
        "server/dav-parse.lock",
        "server/wasm/dav-parse.wasm",
    ] {
        let sha = f.get("sources").get(file).as_str().unwrap_or_default();
        assert_eq!(sha.len(), 64, "{file}");
    }
    // Recorded at the same core as the crate's other fixtures (tests/oracle-core.ref).
    for (other, files) in [
        (
            TOOLS_FIXTURE,
            &[
                "server/chat.cjs",
                "server/index.cjs",
                "server/tools-mode.cjs",
                "server/toolboxes.cjs",
                "server/dav-parse.lock",
                "server/wasm/dav-parse.wasm",
            ][..],
        ),
        (ROLES_FIXTURE, &["server/chat.cjs", "server/index.cjs"][..]),
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
    let roles: Vec<String> = texts(f.get("facts").get("FALLBACK_ROLES"));
    let ours: Vec<&str> = reroute::FALLBACK_ROLES.iter().map(|r| r.as_str()).collect();
    assert_eq!(roles, ours);
    // No named differences: every recorded turn replays byte for byte.
    assert!(items(&f, "differences").is_empty());
}

/// What the replay of one turn produced, in the recording's shape.
struct Out {
    calls: Vec<JValue>,
    throws: bool,
    refused: JValue,
    meta: JValue,
    status: JValue,
    scope: JValue,
    lines: Vec<String>,
    reply: JValue,
}

struct Ctx<'a> {
    spec: &'a JValue,
    calls: RefCell<Vec<JValue>>,
    catalogue_call: RefCell<usize>,
}

impl Ctx<'_> {
    fn seam(&self, parts: Vec<JValue>) {
        self.calls.borrow_mut().push(call(parts));
    }
    /// `servedCatalogue()`: the case's answer for this call (the last one again after the list).
    fn served_catalogue(&self) -> JValue {
        self.seam(vec![JValue::from("servedCatalogue")]);
        let list = items(self.spec, "catalogues");
        let mut k = self.catalogue_call.borrow_mut();
        let answer = list[(*k).min(list.len() - 1)].clone();
        *k += 1;
        answer
    }
}

fn refusal(r: &roles::Refusal) -> JValue {
    JValue::obj([
        ("status", JValue::Num(f64::from(r.status))),
        ("body", r.body()),
    ])
}

fn halt(out: &mut Out, h: Halt) {
    match h {
        Halt::Refused(r) => out.refused = refusal(&r),
        Halt::Thrown => out.throws = true,
    }
}

fn names(tools: &[JValue]) -> Vec<JValue> {
    tools
        .iter()
        .map(|t| t.get("function").get("name").clone())
        .collect()
}

fn replay(f: &JValue, c: &JValue, log: &mut LongProfileLog) -> Out {
    let spec = c.get("spec");
    let ctx = Ctx {
        spec,
        calls: RefCell::new(Vec::new()),
        catalogue_call: RefCell::new(0),
    };
    let mut out = Out {
        calls: Vec::new(),
        throws: false,
        refused: JValue::Null,
        meta: JValue::Null,
        status: JValue::Null,
        scope: JValue::Null,
        lines: Vec::new(),
        reply: JValue::Null,
    };
    run(f, c, &ctx, &mut out, log);
    out.calls = ctx.calls.take();
    out
}

fn run(f: &JValue, c: &JValue, ctx: &Ctx<'_>, out: &mut Out, log: &mut LongProfileLog) {
    let spec = ctx.spec;
    let facts = f.get("facts");
    let project = or_null(spec.get("project"));
    let auto_roles = or_null(spec.get("roles"));
    let body = c.get("body");
    let message = text(body.get("message"));
    let compact_only = body.get("compactOnly").truthy();
    let wants = roles::wants_auto(&project, &auto_roles, DEFAULT);
    let pid = roles::provider_id(wants, &project, DEFAULT);
    let provider = facts.get("providers").get(text(&pid)).clone();
    let rt = roles::Turn {
        project: &project,
        auto_roles: &auto_roles,
        default_provider_id: DEFAULT,
        provider: &provider,
        message,
        manager_enabled: spec.get("managerEnabled") != &JValue::Bool(false),
        compact_only,
        task_mode: false,
        has_images: false,
        code_project: false,
    };

    // P5b: which model answers, as which role.
    let mut model;
    let mut role: Option<Role> = None;
    let mut decision = JValue::Null;
    if wants {
        let catalogue = ctx.served_catalogue();
        let check = match roles::auto_check(&rt, &catalogue) {
            Ok(check) => check,
            Err(h) => return halt(out, h),
        };
        // A pre-load (`startPreload`, P5b's) is no seam this recording logs.
        ctx.seam(vec![JValue::from("classify")]);
        let cls = match spec.get("classification") {
            JValue::Str(r) => Classification {
                role: Role::of(r).unwrap(),
                decision: JValue::Null,
            },
            o => Classification {
                role: Role::of(text(o.get("role"))).unwrap(),
                decision: or_null(o.get("routingDecision")),
            },
        };
        let picked = match roles::auto_pick(&rt, &catalogue, &check, &cls, body.get("resend")) {
            Ok(p) => p,
            Err(_) => return halt(out, Halt::Thrown),
        };
        model = picked.model;
        role = Some(picked.role);
        decision = picked.decision;
    } else {
        model = roles::manual_model(&project);
        if roles::wants_loaded_model(&rt, &model) {
            ctx.seam(vec![JValue::from("modelsInstalled")]);
            ctx.seam(vec![JValue::from("lastLoadedModel")]);
            model = or_null(spec.get("loaded"));
        }
    }
    if let Err(h) = roles::require_model(&rt, &model) {
        return halt(out, h);
    }
    if roles::chat_guard_applies(&rt, wants, &model) {
        let catalogue = ctx.served_catalogue();
        if let Err(h) = roles::chat_guard(&model, &catalogue, &HashMap::new()) {
            return halt(out, h);
        }
    }

    // P6a: the tools, resolved for the routed model.
    let cat = facts.get("catalogue");
    let boxes: Vec<Toolbox> = items(cat, "boxes").iter().map(toolbox).collect();
    let server_writes = texts(cat.get("serverWrites"));
    let sw = |n: &str| server_writes.iter().any(|w| w == n);
    let writes = Writes::new(&boxes, &sw);
    let account_ready = |_: &str| true;
    let ready = |_: &Toolbox| true;
    let default_provider = provider.get("id").as_str() == Some(DEFAULT);
    let external = provider.get("external") == &JValue::Bool(true);
    let turn = ToolTurn {
        project: &project,
        diary_extras_id: facts.get("DIARY_EXTRAS_ID").as_str(),
        message,
        history: &[],
        turn_toolboxes: body.get("turnToolboxes"),
        compact_only,
        boxes: &boxes,
        connected: &texts(cat.get("connected")),
        diary_enabled: true,
        oauth_ids: &[],
        account_ready: &account_ready,
        automatic_ready: &ready,
        writes: &writes,
        local: !offer::off_box(external, false, default_provider, false),
        strip_private: external,
        cloud_ungranted: false,
    };
    let sel = offer::select(&turn);
    let measured = spec.get("measured");
    let prefill = |m: &str, scope: Option<&str>| {
        assert_eq!(scope, None);
        ctx.seam(vec![
            JValue::from("budgetFor"),
            JValue::from(m),
            JValue::Num(14000.0),
        ]);
        measured.get(m).as_num()
    };
    let blocks = |_: &str, _: bool| false;
    let props = spec.get("props") != &JValue::Bool(false);
    let resolve = |m: &JValue, sel: &Selection| -> Offer {
        offer::offer(
            &turn,
            sel,
            OfferInput {
                ranked: None,
                model: text(m),
                prefill: &prefill,
                machine_scope: None,
                policy_blocks: &blocks,
                template_gate: default_provider && props,
            },
        )
        .expect("the recorded tools resolve")
    };
    let mut resolved = resolve(&model, &sel);
    let mut resolved_for = model.clone();
    let gate = if spec.get("gate").is_nullish() || compact_only {
        JValue::Null
    } else {
        ctx.seam(vec![
            JValue::from("gate"),
            JValue::Arr(names(&resolved.resolved.tools)),
            JValue::Num(0.0),
        ]);
        spec.get("gate").clone()
    };

    // P5c: the move to a tool-capable role (#1550).
    let mt = MoveTurn {
        project: &project,
        wants_auto: wants,
        compact_only,
        route_row: false,
        default_provider,
        local_tools_gate: props,
        gate_needs_tool: reroute::gate_says_tool_needed(&gate),
    };
    if reroute::tool_move_applies(&mt) {
        let routed = role.expect("an Automatic turn has a routed role");
        // The fresh autoRoles() read (chat.cjs:1355), after the gate answered.
        ctx.seam(vec![JValue::from("autoRoles")]);
        let fresh = match spec.get("rolesLater") {
            JValue::Undefined => auto_roles.clone(),
            later => later.clone(),
        };
        let tm = ToolMove::new(&model, routed, &fresh);
        let mut first = None;
        for (k, cand) in tm.candidates().iter().enumerate() {
            ctx.seam(vec![JValue::from("allowsTools"), cand.model.clone()]);
            let ok = match spec.get("toolsOk").get(text(&cand.model)) {
                JValue::Bool(b) => *b,
                _ => true,
            };
            if ok {
                first = Some(k);
                break;
            }
        }
        let Ok(moved) = tm.settle(first) else {
            return halt(out, Halt::Thrown);
        };
        if let Some(line) = moved.log_line().unwrap() {
            out.lines.push(line);
        }
        if moved.rerouted {
            decision = moved.decision(&decision);
            model = moved.model.clone();
            role = Some(moved.role);
            resolved = resolve(&model, &sel);
            resolved_for = model.clone();
        }
    }
    // P5c: the long-context profile (#1079).
    let mut context_profile = JValue::Null;
    let mut context_switch = false;
    if reroute::long_profile_applies(&project, default_provider, false) {
        let catalogue = ctx.served_catalogue();
        let picked = match reroute::long_pick(&model, &catalogue) {
            Ok(p) => p,
            Err(_) => return halt(out, Halt::Thrown),
        };
        if let Some(line) = picked.log_line().and_then(|l| log.admit(l)) {
            out.lines.push(line);
        }
        context_profile = JValue::from(picked.context_profile);
        context_switch = picked.context_switch;
        model = picked.model;
    }
    if reroute::model_changed(&model, &resolved_for) {
        resolved = resolve(&model, &sel);
    }

    // P5a with the final role, and what the turn sends.
    let r = route::route(&RouteTurn {
        project: &project,
        effort_default: &JValue::Undefined,
        sampling_presets: &JValue::Undefined,
        wants_auto: wants,
        routed_role: role.map(Role::as_str),
        provider: &provider,
        default_provider_id: DEFAULT,
        engine: Engine {
            kind: None,
            inference_base_url: None,
        },
        message,
    });
    out.meta = JValue::obj([
        ("model", model.clone()),
        (
            "route",
            role.map_or(JValue::Null, |r| JValue::from(r.as_str())),
        ),
        (
            "routingDecision",
            if decision.truthy() {
                decision.clone()
            } else {
                JValue::Null
            },
        ),
        ("contextProfile", context_profile),
        ("sampling", or_null(&r.meta_sampling())),
    ]);
    let status = reroute::status_event(context_switch, false);
    out.status = JValue::obj([
        ("id", status.get("id").clone()),
        ("text", status.get("text").clone()),
    ]);
    out.scope = resolved.scope.clone();
    if !compact_only {
        let tools = &resolved.active_tools;
        out.reply = JValue::obj([
            ("model", model.clone()),
            (
                "tools",
                if tools.is_empty() {
                    JValue::Null
                } else {
                    JValue::Arr(names(tools))
                },
            ),
            (
                "toolsSha256",
                if tools.is_empty() {
                    JValue::Null
                } else {
                    JValue::from(sha_hex(&stringify(&JValue::Arr(tools.clone()))))
                },
            ),
        ]);
    }
}

#[test]
fn turns_match_node() {
    let f = fixture();
    let turns = items(&f, "turns");
    let mut failed = Vec::new();
    let mut handlers: HashMap<String, LongProfileLog> = HashMap::new();
    let mut shared = 0;
    let (mut moved, mut switched, mut thrown, mut unavailable, mut refused) = (0, 0, 0, 0, 0);
    for c in turns {
        let name = text(c.get("name"));
        // One handler's log for a group of turns on one handler, a fresh one otherwise.
        let mut own = LongProfileLog::default();
        let log = match c.get("handler").as_str() {
            Some(id) => {
                shared += 1;
                handlers.entry(id.to_string()).or_default()
            }
            None => &mut own,
        };
        let got = replay(&f, c, log);
        let mut d = Vec::new();
        let mut same = |what: &str, want: &JValue, got: &JValue| {
            if stringify(want) != stringify(got) {
                d.push(format!(
                    "{what}\n  node: {}\n  rust: {}",
                    stringify(want),
                    stringify(got)
                ));
            }
        };
        same("calls", c.get("calls"), &JValue::Arr(got.calls.clone()));
        same(
            "throws",
            &JValue::Bool(c.get("throws").as_str().is_some()),
            &JValue::Bool(got.throws),
        );
        if let Some(t) = c.get("throws").as_str() {
            assert_eq!(t, "TypeError", "{name}");
            thrown += 1;
        }
        same("refused", c.get("refused"), &got.refused);
        let lines: Vec<JValue> = got.lines.iter().map(|l| JValue::from(l.as_str())).collect();
        same("lines", c.get("lines"), &JValue::Arr(lines));
        if !got.throws && got.refused.is_nullish() {
            same("meta", c.get("meta"), &got.meta);
            same("status", c.get("status"), &got.status);
            same("tools_scope", c.get("scope"), &got.scope);
            same("reply", c.get("reply"), &got.reply);
        } else {
            refused += usize::from(!got.refused.is_nullish());
        }
        moved += got
            .lines
            .iter()
            .filter(|l| l.contains("cannot take tools"))
            .count();
        unavailable += got
            .lines
            .iter()
            .filter(|l| l.starts_with("[long-profile]"))
            .count();
        switched += usize::from(got.status.get("id").as_str() == Some("loadingLongContext"));
        if !d.is_empty() {
            failed.push(format!("{name}:\n{}", d.join("\n")));
        }
    }
    assert!(turns.len() >= 200, "only {} turns recorded", turns.len());
    assert!(moved >= 20, "only {moved} moves recorded");
    assert!(
        switched >= 10,
        "only {switched} long-context switches recorded"
    );
    assert!(
        unavailable >= 8,
        "only {unavailable} refused picks recorded"
    );
    assert!(
        thrown >= 2,
        "only {thrown} turns on which Node throws recorded"
    );
    assert!(
        shared >= 3,
        "only {shared} turns on a shared handler recorded"
    );
    assert!(refused >= 1, "no refused turn was recorded");
    assert!(
        failed.is_empty(),
        "{} of {} turns differ:\n{}",
        failed.len(),
        turns.len(),
        failed[..failed.len().min(4)].join("\n")
    );
}

/// The inputs no recorded turn reaches (routing modes, images): chat.cjs's conditions as written.
#[test]
fn unrecorded_inputs_follow_chat_cjs() {
    let project = wtf16::parse(r#"{"routing":"auto","contextProfile":"high"}"#).unwrap();
    let base = MoveTurn {
        project: &project,
        wants_auto: true,
        compact_only: false,
        route_row: false,
        default_provider: true,
        local_tools_gate: true,
        gate_needs_tool: true,
    };
    assert!(reroute::tool_move_applies(&base));
    assert!(!reroute::tool_move_applies(&MoveTurn {
        route_row: true,
        ..base
    }));
    assert!(reroute::long_profile_applies(&project, true, false));
    assert!(!reroute::long_profile_applies(&project, true, true));
    assert_eq!(
        reroute::status_event(true, true).get("id").as_str(),
        Some("readingImages")
    );
    assert_eq!(
        reroute::status_event(false, true).get("id").as_str(),
        Some("readingImages")
    );
    // A routed model that takes tools, or no candidate that does, keeps the turn.
    let roles = wtf16::parse(r#"{"fast":"qa-a","smart":"qa-b","code":"qa-a"}"#).unwrap();
    let tm = ToolMove::new(&JValue::from("qa-a"), Role::Fast, &roles);
    let models: Vec<&str> = tm
        .candidates()
        .iter()
        .map(|c| c.model.as_str().unwrap())
        .collect();
    assert_eq!(models, ["qa-a", "qa-b"]);
    assert!(!tm.settle(Some(0)).unwrap().rerouted);
    assert!(!tm.settle(None).unwrap().rerouted);
    assert!(!tm.settle(Some(7)).unwrap().rerouted);
    assert_eq!(tm.settle(Some(1)).unwrap().role, Role::Smart);
    // Roles cleared before the move's fresh read: only the routed model is asked, and its refusal
    // throws (`roles[role]` of null); a routed model that takes tools never reads them.
    let gone = ToolMove::new(&JValue::from("qa-a"), Role::Fast, &JValue::Null);
    assert_eq!(gone.candidates().len(), 1);
    assert!(gone.settle(None).is_err());
    assert!(!gone.settle(Some(0)).unwrap().rerouted);
}
