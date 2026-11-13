//! The recorded Node output of slice P6a (`fixtures/node-tools.v1.json`, written by
//! `record_tools.cjs`): every budget, estimate, write flag, resolution, Automatic addition, carry,
//! order, public flag, route note and turn must be what noevia-core's toolboxes.cjs,
//! tools-mode.cjs, tool-route-notes.cjs and chat.cjs's own lines produced.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::cell::RefCell;

use chat_prepare::offer::{self, OfferInput, Routing, ToolTurn};
use chat_prepare::route_notes::{self, NotesInput};
use chat_prepare::toolboxes::{self, NameNotText, Resolve, Resolved, Toolbox, Writes};
use chat_prepare::tools_mode::{self, Additions};
use chat_prepare::Entry;
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};

const FIXTURE: &str = include_str!("fixtures/node-tools.v1.json");
const SKILLS_FIXTURE: &str = include_str!("fixtures/node-skills.v1.json");
const PREPARE_FIXTURE: &str = include_str!("fixtures/node-prepare.v1.json");
/// The chat.cjs lines the recorder runs (`record_tools.cjs` cuts them between its anchors): a change
/// there must be reviewed against `src/offer.rs` and these constants edited with it.
const BLOCKS: [(&str, &str, [f64; 2]); 10] = [
    (
        "offBoxTurn",
        "ebcf82a1367ec580563b50d788b42530e8c9368d0026ca4e402e31b3f89a678d",
        [54.0, 56.0],
    ),
    (
        "skillLeads",
        "b42cd57a7b0248525309f104f55d418ec63f974729de9ec50dc69150b7a864a9",
        [214.0, 215.0],
    ),
    (
        "isWriteTool",
        "1ad2b7960d8488054c7ea945b8335a3ed94ccb5f2aeb5899002fb774fe5371b1",
        [311.0, 315.0],
    ),
    (
        "requestToolboxes",
        "fa8771d7fa69981b58152da97549509dc63a467009b2308374f0a6714d2a3e12",
        [516.0, 540.0],
    ),
    (
        "privateTools",
        "7e0e1165ea3aaf9464dae663e67308e347d3c552859f978423e7a3d27cb46a38",
        [1176.0, 1223.0],
    ),
    (
        "tools",
        "a06a9dd2df8e54425c6a347429f862e593ad5926c2d4c2b9bcc6808fc1b17c34",
        [1262.0, 1325.0],
    ),
    (
        "gate",
        "16afa2e03431d99a822151f4db6d2be8adafc726e6ce0d118d73a41dffcc8f4a",
        [1329.0, 1329.0],
    ),
    (
        "activeTools",
        "bfb281ab578bc44c987c84b91250f7019045c32aa219eba9bd472e1baec34251",
        [1641.0, 1679.0],
    ),
    (
        "names",
        "127722f1f10a0356f3aef18a9e51db2a86dcec688dae613228c174daa54ecce7",
        [1718.0, 1718.0],
    ),
    (
        "frame",
        "972bd05fb0e465d3610be6f964c0172c89f897a4ccff30e4e3c666ca3bc6956e",
        [1726.0, 1729.0],
    ),
];
/// The modules the port follows, pinned: a change must be reviewed against the port.
const PINNED: [(&str, &str); 7] = [
    (
        "server/tools-mode.cjs",
        "8374113b31413ed8f6aeab4d2ff388d24370858a9c5cff125f7728e2c33cdb50",
    ),
    (
        "server/toolboxes.cjs",
        "f2be13090ecb2efc3fb3e49c090108e94b7218e6dc3d6ff8b3224773f5d3b121",
    ),
    (
        "server/tool-route-notes.cjs",
        "b8b71712f4fa643f78f2255b119782c479a7458412f10f55c74c043e77967497",
    ),
    (
        "server/specialist-page-save.cjs",
        "e75cc3b07fab2e104bc6fb5628a603db73580794d8aad7cc918a1c58741bd79e",
    ),
    (
        "server/toolbox-flags.cjs",
        "1ed1f0cd4f58f29955323b713d6d8df718cfb71b18a82320d67405aaf9c68830",
    ),
    (
        "server/project-edit-target.cjs",
        "3f99c8fd3c2e6b948c3da827f07000bd543cb04508098446a6d10439fb721307",
    ),
    (
        "server/chat-tool-routing.cjs",
        "a1a0e37b1d08fdd38028be911a79d3f46ecf39abb9adeb8ce6456ccc1ec2c80d",
    ),
];
/// The lookahead `EXPLICIT_WEB_RE` holds and the port drops (see `tools_mode`).
const LOOKAHEAD: &str = r"(?!\s+(?:drive|docs?|sheets?|slides|calendar|photos|keep|mail))";

fn fixture() -> JValue {
    wtf16::parse(FIXTURE).expect("the fixture is JSON")
}

fn items<'a>(v: &'a JValue, key: &str) -> &'a [JValue] {
    match v.get(key) {
        JValue::Arr(items) => items,
        other => panic!("{key} is not an array: {other:?}"),
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
        JValue::Arr(items) => items.iter().map(|i| text(i).to_string()).collect(),
        JValue::Undefined => Vec::new(),
        other => panic!("not a list: {other:?}"),
    }
}

fn strs(v: &[String]) -> JValue {
    JValue::Arr(v.iter().map(|s| JValue::from(s.as_str())).collect())
}

fn num(n: f64) -> JValue {
    JValue::Num(n)
}

/// A recorded number or `None` (`null`, missing).
fn measured(v: &JValue) -> Option<f64> {
    v.as_num()
}

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

struct Catalogue {
    boxes: Vec<Toolbox>,
    server_writes: Vec<String>,
}

fn catalogue(f: &JValue, name: &str) -> Catalogue {
    let c = f.get("catalogues").get(name);
    Catalogue {
        boxes: items(c, "boxes").iter().map(toolbox).collect(),
        server_writes: texts(c.get("serverWrites")),
    }
}

fn resolved_json(r: &Resolved, sha: bool) -> JValue {
    let names: Vec<JValue> = r
        .tools
        .iter()
        .map(|t| t.get("function").get("name").clone())
        .collect();
    let mut out = vec![
        ("tools".to_string(), JValue::Arr(names)),
        ("dropped".to_string(), strs(&r.dropped)),
        ("boxes".to_string(), strs(&r.boxes)),
        ("cap".to_string(), num(r.cap as f64)),
        ("budget".to_string(), num(r.budget)),
        ("estTokens".to_string(), num(r.est_tokens)),
    ];
    let routes = r
        .routes
        .iter()
        .map(|(n, s)| JValue::Arr(vec![JValue::from(n.as_str()), JValue::from(s.as_str())]));
    out.push(("routes".to_string(), JValue::Arr(routes.collect())));
    if sha {
        let json = wtf16::stringify(&JValue::Arr(r.tools.clone())).unwrap();
        let hex: String = Sha256::digest(json.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        out.push(("toolsSha256".to_string(), JValue::from(hex)));
    }
    JValue::Obj(out)
}

#[test]
fn the_fixture_names_its_node_sources() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    for file in [
        "server/chat.cjs",
        "server/index.cjs",
        "server/toolboxes-permitted.cjs",
        "server/tool-gate.cjs",
        "server/provider-egress.cjs",
        "server/gdrive-tools.cjs",
        "server/kiwix.cjs",
        "server/dav-parse.lock",
        "server/wasm/dav-parse.wasm",
    ] {
        let sha = f.get("sources").get(file).as_str().unwrap_or_default();
        assert_eq!(sha.len(), 64, "{file}");
    }
    for (file, sha) in PINNED {
        assert_eq!(f.get("sources").get(file).as_str(), Some(sha), "{file}");
    }
    // Recorded at the same core as the crate's other fixtures (tests/oracle-core.ref).
    for (other, files) in [
        (
            SKILLS_FIXTURE,
            &[
                "server/chat.cjs",
                "server/index.cjs",
                "server/dav-parse.lock",
                "server/wasm/dav-parse.wasm",
            ][..],
        ),
        (PREPARE_FIXTURE, &["server/tools-mode.cjs"][..]),
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
}

#[test]
fn the_constants_and_patterns_are_core_s() {
    let f = fixture();
    let p = f.get("patterns");
    for (name, src) in tools_mode::sources() {
        let want = text(p.get(name)).replace(LOOKAHEAD, "");
        assert_eq!(src, want, "{name}");
        if name.ends_with("_RE") {
            assert!(tools_mode::compile(&src).is_some(), "{name} compiles");
        }
    }
    assert!(text(p.get("EXPLICIT_WEB_RE")).contains(LOOKAHEAD));
    let mut notes: Vec<String> = texts(p.get("CONNECTORS"));
    notes.push(text(p.get("ABOUT_FILES")).to_string());
    notes.push(text(p.get("TIME_SENSITIVE")).to_string());
    assert_eq!(route_notes::sources().map(str::to_string).to_vec(), notes);
    assert_eq!(
        p.get("MAX_ADDED").as_num(),
        Some(tools_mode::MAX_ADDED as f64)
    );
    assert_eq!(
        p.get("CARRY_USER_TURNS").as_num(),
        Some(tools_mode::CARRY_USER_TURNS as f64)
    );
    let list = |k: &str| texts(p.get(k));
    let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(list("PUBLIC_BOXES"), own(&tools_mode::PUBLIC_BOXES));
    assert_eq!(list("CLOUD_SAFE_BOXES"), own(&tools_mode::CLOUD_SAFE_BOXES));
    assert_eq!(
        list("PRIVATE_CORE_TOOLS"),
        own(&tools_mode::PRIVATE_CORE_TOOLS)
    );
    assert_eq!(
        list("HEALTH_PUBLIC_BOXES"),
        own(&tools_mode::HEALTH_PUBLIC_BOXES)
    );
    assert_eq!(list("EDIT_TOOLS"), own(&offer::EDIT_TOOLS));
    assert_eq!(
        list("DEFAULT_TOOLBOXES"),
        own(&toolboxes::DEFAULT_TOOLBOXES)
    );
    assert_eq!(list("CONNECTOR_BOXES"), own(&toolboxes::CONNECTOR_BOXES));
    assert_eq!(text(p.get("PROJECT_DOCS_BOX")), toolboxes::PROJECT_DOCS_BOX);
    assert_eq!(
        text(p.get("PAGE_SAVE_PREFIX")),
        toolboxes::SPECIALIST_PREFIX
    );
    // The core box and the coworker page tool, byte for byte.
    assert_eq!(toolboxes::core_box(), Some(toolbox(f.get("coreBox"))));
    let r = toolboxes::resolve_tools(
        &[],
        &Resolve {
            boxes: &[],
            model: "qa",
            skip: &|_| false,
            specialist: true,
            measured: None,
        },
    )
    .unwrap();
    assert_eq!(stringify(&r.tools[0]), stringify(f.get("pageSaveTool")));
}

#[test]
fn budgets_and_estimates_match_node() {
    let f = fixture();
    let mut d = Diffs::default();
    for c in items(&f, "budgets") {
        let model = text(c.get("model"));
        let m = measured(c.get("measured"));
        let budget = toolboxes::tool_token_budget_for(model, m);
        let got = JValue::obj([
            ("model", JValue::from(model)),
            ("measured", c.get("measured").clone()),
            ("measuredMachine", c.get("measuredMachine").clone()),
            ("cap", num(toolboxes::tool_cap_for(model) as f64)),
            ("budget", num(budget)),
            (
                "scoped",
                num(toolboxes::tool_token_budget_for(
                    model,
                    measured(c.get("measuredMachine")),
                )),
            ),
        ]);
        d.check(c.get("model"), &stringify(c), &stringify(&got));
    }
    d.done("budgets", 300);
    let mut d = Diffs::default();
    for c in items(&f, "estimates") {
        let tools = items(c, "tools");
        let got = toolboxes::estimate_tool_tokens(tools);
        d.check(c, &stringify(c.get("tokens")), &stringify(&num(got)));
    }
    d.done("estimates", 100);
}

#[test]
fn write_flags_match_node() {
    let f = fixture();
    let mut d = Diffs::default();
    for name in ["plain", "full", "offered", "dupid", "numeric"] {
        let cat = catalogue(&f, name);
        let sw = |n: &str| cat.server_writes.iter().any(|w| w == n);
        let writes = Writes::new(&cat.boxes, &sw);
        let read_only = texts(f.get("catalogues").get(name).get("readOnly"));
        for n in &read_only {
            assert!(writes.read_only(n), "{name}: {n} is read-only");
        }
        for c in items(&f, "writes")
            .iter()
            .filter(|c| c.get("catalogue").as_str() == Some(name))
        {
            let n = text(c.get("name"));
            // chat.cjs's isWriteTool (the page save always, else toolboxes.cjs's).
            let got = writes.is_write(n);
            assert_eq!(
                read_only.iter().any(|r| r == n),
                writes.read_only(n),
                "{name}: {n}"
            );
            d.check(
                c,
                &stringify(c.get("write")),
                &stringify(&JValue::Bool(got)),
            );
        }
    }
    d.done("writes", 150);
}

#[test]
fn resolutions_match_node() {
    let f = fixture();
    let mut d = Diffs::default();
    for c in items(&f, "resolve") {
        let cat = catalogue(&f, text(c.get("catalogue")));
        let skip = texts(c.get("skip"));
        let skip_fn = |n: &str| skip.iter().any(|s| s == n);
        let got = toolboxes::resolve_tools(
            &texts(c.get("ids")),
            &Resolve {
                boxes: &cat.boxes,
                model: text(c.get("model")),
                skip: &skip_fn,
                specialist: c
                    .get("projectId")
                    .as_str()
                    .is_some_and(|id| id.starts_with(toolboxes::SPECIALIST_PREFIX)),
                measured: measured(c.get("measured")),
            },
        );
        let got = match got {
            Ok(r) => stringify(&resolved_json(&r, true)),
            Err(NameNotText) => "NameNotText".to_string(),
        };
        d.check(c, &stringify(c.get("out")), &got);
    }
    d.done("resolve", 250);
}

#[test]
fn messages_match_node() {
    let f = fixture();
    let mut d = Diffs::default();
    for c in items(&f, "messages") {
        let m = text(c.get("message"));
        let got = JValue::obj([
            ("message", JValue::from(m)),
            (
                "boxes",
                JValue::Arr(
                    tools_mode::automatic_boxes_for(m)
                        .into_iter()
                        .map(JValue::from)
                        .collect(),
                ),
            ),
            ("web", JValue::Bool(tools_mode::wants_web(m))),
        ]);
        d.check(c.get("message"), &stringify(c), &stringify(&got));
    }
    d.done("messages", 1000);
}

fn history(v: &JValue) -> Vec<Entry> {
    match v {
        JValue::Arr(items) => items
            .iter()
            .map(|h| Entry {
                role: text(h.get("role")).to_string(),
                content: h.get("content").as_str().unwrap_or_default().to_string(),
                name: h.get("name").as_str().map(str::to_string),
                applied: false,
                declined: false,
                target: None,
                args: None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `ready(box) === true`, a recorded `'throw'` being `false`.
fn ready(map: &JValue, b: &Toolbox) -> bool {
    match map.get(&b.id) {
        JValue::Undefined => true,
        v => v == &JValue::Bool(true),
    }
}

#[test]
fn additions_carries_order_and_public_names_match_node() {
    let f = fixture();
    let mut d = Diffs::default();
    for c in items(&f, "additions") {
        let cat = catalogue(&f, text(c.get("catalogue")));
        let sw = |n: &str| cat.server_writes.iter().any(|w| w == n);
        let writes = Writes::new(&cat.boxes, &sw);
        let is_write = |n: &str| writes.is_write(n);
        let ready_fn = |b: &Toolbox| ready(c.get("ready"), b);
        let base = texts(c.get("base"));
        let carried = texts(c.get("carried"));
        let got = tools_mode::automatic_additions(
            text(c.get("message")),
            &Additions {
                base: &base,
                boxes: &cat.boxes,
                is_write: &is_write,
                ready: &ready_fn,
                private_data: c.get("privateData") == &JValue::Bool(true),
                carried: &carried,
            },
        );
        d.check(c, &stringify(c.get("out")), &stringify(&strs(&got)));
    }
    d.done("additions", 400);
    let mut d = Diffs::default();
    for c in items(&f, "carried") {
        let cat = catalogue(&f, text(c.get("catalogue")));
        let got = tools_mode::carried_public_boxes(&history(c.get("history")), &cat.boxes);
        d.check(c, &stringify(c.get("out")), &stringify(&strs(&got)));
    }
    d.done("carried", 300);
    let mut d = Diffs::default();
    for c in items(&f, "order") {
        let head = texts(c.get("head"));
        let head: Vec<&str> = head.iter().map(String::as_str).collect();
        let got =
            tools_mode::order_with_additions(&texts(c.get("ids")), &texts(c.get("added")), &head);
        d.check(c, &stringify(c.get("out")), &stringify(&strs(&got)));
    }
    d.done("order", 200);
    let mut d = Diffs::default();
    for c in items(&f, "public") {
        let cat = catalogue(&f, text(c.get("catalogue")));
        let got = tools_mode::is_public_tool_name(c.get("name").as_str(), &cat.boxes);
        d.check(c, &stringify(c.get("out")), &stringify(&JValue::Bool(got)));
    }
    d.done("public", 100);
}

#[test]
fn route_notes_match_node() {
    let f = fixture();
    let mut d = Diffs::default();
    for c in items(&f, "notes") {
        let got = route_notes::tool_route_notes(&NotesInput {
            message: text(c.get("message")),
            offered: &texts(c.get("offered")),
            on_server: &texts(c.get("onServer")),
            automatic: c.get("automatic") == &JValue::Bool(true),
            local_model: c.get("localModel") == &JValue::Bool(true),
            gate_none: c.get("gateNone") == &JValue::Bool(true),
        });
        d.check(c, &stringify(c.get("out")), &stringify(&JValue::from(got)));
    }
    d.done("notes", 500);
}

/// One recorded turn through the port: `Err` when the port refuses it.
fn replay(f: &JValue, c: &JValue) -> Result<(), String> {
    let cat = catalogue(f, text(c.get("catalogue")));
    let out = c.get("out");
    let seams = c.get("seams");
    let sw = |n: &str| cat.server_writes.iter().any(|w| w == n);
    let writes = Writes::new(&cat.boxes, &sw);
    let hist = history(c.get("history"));
    let signed_in = texts(c.get("signedIn"));
    let account_ready = |id: &str| signed_in.iter().any(|s| s == id);
    let ready_fn = |b: &Toolbox| ready(c.get("ready"), b);
    let provider = text(c.get("provider"));
    let route = text(c.get("route"));
    let local = !offer::off_box(
        seams.get("externalProvider") == &JValue::Bool(true),
        route == "cloud" || route == "ungranted",
        provider == "local",
        provider == "machine",
    );
    let null = JValue::Null;
    let project = match c.get("project") {
        JValue::Undefined => &null,
        p => p,
    };
    let extras = text(f.get("patterns").get("DIARY_EXTRAS_ID"));
    let turn = ToolTurn {
        project,
        diary_extras_id: (c.get("noExtras") != &JValue::Bool(true)).then_some(extras),
        message: text(c.get("message")),
        history: &hist,
        turn_toolboxes: c.get("body").get("turnToolboxes"),
        compact_only: c.get("body").get("compactOnly").truthy(),
        boxes: &cat.boxes,
        connected: &texts(seams.get("connected")),
        diary_enabled: seams.get("diaryEnabled") == &JValue::Bool(true),
        oauth_ids: &texts(c.get("oauth")),
        account_ready: &account_ready,
        automatic_ready: &ready_fn,
        writes: &writes,
        local,
        strip_private: seams.get("stripPrivate") == &JValue::Bool(true),
        cloud_ungranted: route == "ungranted",
    };
    let mut d: Vec<String> = Vec::new();
    let mut same = |what: &str, want: &JValue, got: JValue| {
        if stringify(want) != stringify(&got) {
            d.push(format!(
                "{what}\n  node: {}\n  rust: {}",
                stringify(want),
                stringify(&got)
            ));
        }
    };
    same(
        "automaticLocal",
        out.get("automaticLocal"),
        JValue::Bool(local),
    );
    same(
        "cloudUngranted",
        out.get("cloudUngranted"),
        JValue::Bool(turn.cloud_ungranted),
    );
    let sel = offer::select(&turn);
    same(
        "automaticTools",
        out.get("automaticTools"),
        JValue::Bool(sel.automatic_tools),
    );
    same(
        "selectedBoxes",
        out.get("selectedBoxes"),
        strs(&sel.selected),
    );
    same(
        "routerShown",
        &items(out, "routerShown")[0],
        strs(&sel.selected),
    );
    same(
        "automaticAdded",
        out.get("automaticAdded"),
        strs(&sel.added),
    );
    same(
        "privateStripped",
        out.get("privateStripped"),
        JValue::Bool(sel.private_stripped),
    );
    // The ranking's answer (`'down'`: the embeddings failed; `null`: the router is off).
    let ranked = match c.get("ranked") {
        r @ JValue::Arr(_) => Some(texts(r)),
        _ => None,
    };
    let routing_json = |r: &JValue| {
        JValue::obj([
            ("ids", r.get("ids").clone()),
            ("routed", r.get("routed").clone()),
            ("narrowed", JValue::Bool(r.get("narrowed").truthy())),
        ])
    };
    let port_routing = |r: &Routing| {
        JValue::obj([
            ("ids", strs(&r.ids)),
            ("routed", JValue::Bool(r.routed)),
            ("narrowed", JValue::Bool(r.narrowed)),
        ])
    };
    let answer = offer::routing(&turn, &sel, ranked.as_deref());
    same(
        "routerAnswer",
        &routing_json(&items(out, "routerAnswers")[0]),
        port_routing(&answer),
    );
    // Never a box the turn does not carry, whatever the ranking names.
    if let Some(id) = answer.ids.iter().find(|id| !sel.selected.contains(id)) {
        same(
            "router kept a box the turn does not carry",
            &JValue::Null,
            JValue::from(id.as_str()),
        );
    }
    let asked: RefCell<Vec<JValue>> = RefCell::new(Vec::new());
    let prefill = |model: &str, scope: Option<&str>| {
        let mut call = vec![JValue::from(model), num(14000.0)];
        call.extend(scope.map(JValue::from));
        asked.borrow_mut().push(JValue::Arr(call));
        measured(c.get(if scope.is_some() {
            "measuredMachine"
        } else {
            "measured"
        }))
    };
    let policy = c.get("policy");
    let write_mode = c.get("writeMode").as_str().unwrap_or("ask");
    let blocks = |name: &str, w: bool| {
        policy
            .get(name)
            .as_str()
            .filter(|m| !m.is_empty())
            .unwrap_or(if w { write_mode } else { "allow" })
            == "block"
    };
    let o = offer::offer(
        &turn,
        &sel,
        OfferInput {
            ranked,
            model: text(c.get("model")),
            prefill: &prefill,
            machine_scope: (provider == "machine").then_some("machine:qa"),
            policy_blocks: &blocks,
            template_gate: provider == "local" && c.get("props") == &JValue::Bool(true),
        },
    )
    .map_err(|r| format!("{r:?}"))?;
    same(
        "routing",
        &routing_json(out.get("routing")),
        port_routing(&o.routing),
    );
    same(
        "budgetCalls",
        out.get("budgetCalls"),
        JValue::Arr(asked.borrow().clone()),
    );
    same(
        "automaticOnlyWrites",
        out.get("automaticOnlyWrites"),
        strs(&o.automatic_only_writes),
    );
    same(
        "resolved",
        out.get("resolved"),
        resolved_json(&o.resolved, false),
    );
    let names: Vec<JValue> = o
        .resolved
        .tools
        .iter()
        .map(|t| t.get("function").get("name").clone())
        .collect();
    if let Some(shown) = items(out, "gateShown").last() {
        same("gateShown", shown, JValue::Arr(names));
    }
    same("sent", out.get("sent"), JValue::Arr(vec![o.scope.clone()]));
    let gate_none = c.get("gate").as_str() == Some("none") && !turn.compact_only;
    let fields = o.frame_fields(o.route_note(&turn, &sel, gate_none));
    same("frameFields", out.get("frameFields"), JValue::obj(fields));
    if d.is_empty() {
        Ok(())
    } else {
        Err(d.join("\n"))
    }
}

#[test]
fn turns_match_node() {
    let f = fixture();
    let differences = items(&f, "differences");
    let named: Vec<usize> = differences
        .iter()
        .map(|d| {
            assert_eq!(d.get("kind").as_str(), Some("name-not-text"));
            d.get("turn").as_num().unwrap() as usize
        })
        .collect();
    assert!(!named.is_empty(), "the refused tool name is recorded");
    let mut failed = Vec::new();
    let mut thrown = 0;
    let turns = items(&f, "turns");
    for (k, c) in turns.iter().enumerate() {
        let got = replay(&f, c);
        if let Some(t) = c.get("out").get("thrown").as_str() {
            // Node throws (a null tool read by routeNotes): the port refuses the turn as well.
            assert_eq!(t, "TypeError");
            thrown += 1;
            if got != Err("Thrown".to_string()) {
                failed.push(format!("turn {k}: Node threw, the port gave {got:?}"));
            }
        } else if named.contains(&k) {
            // Named difference: Node offers a tool named by a number; the port refuses the turn.
            if got != Err("NameNotText".to_string()) {
                failed.push(format!("turn {k}: expected NameNotText, got {got:?}"));
            }
        } else if let Err(e) = got {
            failed.push(format!("turn {k} ({}):\n{e}", stringify(c.get("message"))));
        }
    }
    assert!(turns.len() >= 450, "only {} turns recorded", turns.len());
    assert!(thrown >= 1, "no turn on which Node throws was recorded");
    // The recording holds rankings that name boxes the turn does not carry (stripped, not
    // connected, unknown); every replay above checks the router kept none of them.
    let impossible = turns
        .iter()
        .filter(|c| {
            let shown = texts(&items(c.get("out"), "routerShown")[0]);
            matches!(c.get("ranked"), JValue::Arr(r) if r.iter().any(|id| !shown.iter().any(|s| Some(s.as_str()) == id.as_str())))
        })
        .count();
    assert!(
        impossible >= 10,
        "only {impossible} impossible rankings recorded"
    );
    assert!(
        failed.is_empty(),
        "{} of {} turns differ:\n{}",
        failed.len(),
        turns.len(),
        failed[..failed.len().min(4)].join("\n")
    );
}
