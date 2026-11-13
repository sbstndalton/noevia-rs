//! The recorded Node output of slice P2c (`fixtures/node-vision.v1.json`, written by
//! `record_vision.cjs`): whole turns through chat.cjs's `createChatHandler`, replayed here through
//! [`vision::images`], [`chat_prepare::prepare`] (the wire the pass starts from) and
//! [`vision::Pass`] in chat.cjs's order. The asset reads, the seams' calls after the meta event
//! (the describe request's url, headers and body text among them), the status and warning events,
//! the `[assets]` and `[vision]` lines, the description cache's keys, and the prepared frame's
//! messages and image warning (or the model request's messages of a turn Node keeps) must be what
//! Node produced, byte for byte; the named differences are checked on their own.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;

use chat_prepare::vision::{
    self, DescribeAnswer, Descriptions, Halt, Images, LeaseRefusal, Pass, PassTurn, Reply, Seam,
    Step,
};
use chat_prepare::{account, js, memory, offer, reroute, time, ProjectTurn, Turn};
use js_json::{wtf16, JValue};

const FIXTURE: &str = include_str!("fixtures/node-vision.v1.json");
const REROUTE_FIXTURE: &str = include_str!("fixtures/node-reroute.v1.json");
const RETRIEVAL_FIXTURE: &str = include_str!("fixtures/node-retrieval.v1.json");
/// The chat.cjs lines this slice ports (`record_vision.cjs` hashes them between its anchors): a
/// change there must be reviewed against `src/vision.rs` and these constants edited with it.
const BLOCKS: [(&str, &str, [f64; 2]); 6] = [
    (
        "codeRouteNote",
        "7d72b1da3b0583cb7c3135d75e4db035ae821a41be2b4df86a3f54365ad9d2bc",
        [210.0, 210.0],
    ),
    (
        "describe",
        "32ae5d7cd1dbf23757144ac2365be277ca8eb4cc987049ba09f2c45dfe3dad4b",
        [783.0, 860.0],
    ),
    (
        "images",
        "258911579779c218fb88c0f51407b402333d5dc0c5d81eb131da52a6b73518ef",
        [866.0, 915.0],
    ),
    (
        "pass",
        "c7a779371e4265304f65267626e8b88382dee3496ed511b01e514a46d416d816",
        [1461.0, 1547.0],
    ),
    (
        "replyWarning",
        "6e59e1b69262eba8572173bb53a0a8a18fb67503cb3cf76ae6f0df2f47370ec5",
        [1616.0, 1619.0],
    ),
    (
        "frameImageWarning",
        "b99f06cb0cacff7c4f34d1c116444e5155878ab94f5b91e397924de0874420ae",
        [1738.0, 1740.0],
    ),
];
const DEFAULT: &str = "default";
const NOW: &str = "2026-11-13T12:00:00Z";

fn fixture() -> JValue {
    wtf16::parse(FIXTURE).expect("the fixture is JSON")
}

fn list<'a>(v: &'a JValue, key: &str) -> &'a [JValue] {
    match v.get(key) {
        JValue::Arr(a) => a,
        other => panic!("{key} is not a list: {other:?}"),
    }
}

fn stringify(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "undefined".to_string())
}

fn name(case: &JValue) -> &str {
    case.get("name").as_str().unwrap()
}

fn or_null(v: &JValue) -> JValue {
    match v {
        JValue::Undefined => JValue::Null,
        other => other.clone(),
    }
}

fn call<const N: usize>(parts: [JValue; N]) -> JValue {
    JValue::Arr(parts.into())
}

/// Standard base64 (the recording's image bytes).
fn decode(text: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("not base64: {c}"),
    };
    let digits: Vec<u8> = text.bytes().filter(|c| *c != b'=').map(value).collect();
    let mut out = Vec::new();
    for chunk in digits.chunks(4) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, d)| n | (u32::from(*d) << (18 - 6 * i)));
        for i in 0..chunk.len() - 1 {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}

#[derive(Default)]
struct Diffs {
    checked: usize,
    bad: Vec<String>,
}

impl Diffs {
    fn check(&mut self, what: &str, want: &str, got: &str) {
        self.checked += 1;
        if want != got {
            let clip = |s: &str| s.chars().take(1500).collect::<String>();
            self.bad.push(format!(
                "{what}\n  node: {}\n  port: {}",
                clip(want),
                clip(got)
            ));
        }
    }

    fn done(self, min: usize) {
        assert!(
            self.bad.is_empty(),
            "{} of {} checks differ:\n{}",
            self.bad.len(),
            self.checked,
            self.bad[..self.bad.len().min(12)].join("\n")
        );
        assert!(self.checked >= min, "only {} checks", self.checked);
    }
}

#[test]
fn the_fixture_names_its_node_sources() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    let JValue::Obj(sources) = f.get("sources") else {
        panic!("no sources")
    };
    // Recorded at the same core as the crate's other whole-turn fixtures (tests/oracle-core.ref).
    let mut shared = 0;
    for other in [REROUTE_FIXTURE, RETRIEVAL_FIXTURE] {
        let other = wtf16::parse(other).unwrap();
        for (file, sha) in sources {
            if let JValue::Str(theirs) = other.get("sources").get(file) {
                assert_eq!(sha.as_str(), Some(theirs.as_str()), "{file}");
                shared += 1;
            }
        }
    }
    for file in [
        "server/chat.cjs",
        "server/quoted-name.cjs",
        "server/prompt-framing.cjs",
        "server/http.cjs",
        "server/wasm/dav-parse.wasm",
    ] {
        assert!(f.get("sources").get(file).as_str().is_some(), "{file}");
    }
    assert!(shared >= 40, "only {shared} files cross-checked");
    for (block, sha, lines) in BLOCKS {
        let b = f.get("blocks").get(block);
        assert_eq!(b.get("sha256").as_str(), Some(sha), "{block}");
        let got: Vec<f64> = list(b, "lines")
            .iter()
            .map(|n| n.as_num().unwrap())
            .collect();
        assert_eq!(got, lines, "{block}");
    }
}

/// `providerHeaders(row)` of the recording: the row's id, and the case's extra headers for it.
fn headers_for(spec: &JValue, id: &JValue) -> JValue {
    let id = js_json::to_js_string(id).unwrap();
    let mut items = vec![("X-QA-Provider".to_string(), JValue::Str(id.clone()))];
    if let JValue::Obj(extra) = spec.get("headers").get(&id) {
        for (k, v) in extra {
            wtf16::set(&mut items, k, v.clone());
        }
    }
    JValue::Obj(items)
}

/// The recorder's describe reply of a case.
fn describe_answer(spec: &JValue, cap: usize) -> DescribeAnswer {
    let d = spec.get("describe");
    let body = |v: &JValue| DescribeAnswer::Body(stringify(v).into_bytes());
    if d.get("hang").truthy() {
        return DescribeAnswer::Threw {
            message: "The operation was aborted due to timeout".into(),
            stage_timeout: true,
        };
    }
    if d.get("leave").truthy() {
        return DescribeAnswer::Threw {
            message: "QA client left".into(),
            stage_timeout: false,
        };
    }
    if let Some(m) = d.get("throws").as_str() {
        return DescribeAnswer::Threw {
            message: m.into(),
            stage_timeout: false,
        };
    }
    if let Some(s) = d.get("status").as_num() {
        return DescribeAnswer::Status(s as u16);
    }
    if let Some(raw) = d.get("raw").as_str() {
        return DescribeAnswer::Body(raw.as_bytes().to_vec());
    }
    if let Some(kind) = d.get("big").as_str() {
        let json = r#"{"choices":[{"finish_reason":"stop","message":{"content":"QA description of a big reply."}}]}"#;
        let pad = cap - json.len() + usize::from(kind == "over");
        return DescribeAnswer::Body(format!("{json}{}", " ".repeat(pad)).into_bytes());
    }
    if !matches!(d.get("json"), JValue::Undefined) {
        return body(d.get("json"));
    }
    let text = match d.get("text") {
        JValue::Undefined => JValue::from("QA description: a white square with one dot."),
        t => t.clone(),
    };
    let finish = match d.get("finish") {
        JValue::Str(f) => f.clone(),
        _ => "stop".into(),
    };
    body(&JValue::obj([(
        "choices",
        JValue::Arr(vec![JValue::obj([
            ("index", JValue::Num(0.0)),
            ("finish_reason", JValue::Str(finish)),
            (
                "message",
                JValue::obj([("role", JValue::from("assistant")), ("content", text)]),
            ),
        ])]),
    )]))
}

struct Answers<'a> {
    spec: &'a JValue,
    case: &'a JValue,
    facts: &'a JValue,
    clock: f64,
    calls: Vec<JValue>,
    events: Vec<JValue>,
}

impl Answers<'_> {
    /// The recorder's stubs: what each seam answers, and the call it logs.
    fn answer(&mut self, seam: Seam) -> Reply {
        let (spec, facts) = (self.spec, self.facts);
        match seam {
            Seam::AutoRoles => {
                self.calls.push(call([JValue::from("autoRoles")]));
                Reply::AutoRoles(or_null(spec.get("roles")))
            }
            Seam::GetProvider(id) => {
                self.calls.push(call([
                    JValue::from("getProvider"),
                    JValue::from(id.as_str()),
                ]));
                let (row, external) = match spec.get("describeRow") {
                    JValue::Undefined => (
                        or_null(facts.get("providers").get(&id)),
                        facts.get("external").get(&id).as_bool().unwrap(),
                    ),
                    row => (
                        row.clone(),
                        self.case.get("describeExternal").as_bool().unwrap_or(false),
                    ),
                };
                Reply::Provider { row, external }
            }
            Seam::ProviderHeaders(row) => {
                let id = or_null(row.get("id"));
                self.calls
                    .push(call([JValue::from("providerHeaders"), id.clone()]));
                Reply::Headers(headers_for(spec, &id))
            }
            Seam::AcquireLease(model) => {
                self.calls.push(call([JValue::from("acquireLease"), model]));
                let refused = |code: &str, stage_timeout, client_left| {
                    Reply::Lease(Err(LeaseRefusal {
                        code: code.into(),
                        stage_timeout,
                        client_left,
                    }))
                };
                match spec.get("leaseAnswer").as_str() {
                    Some("code") => refused("qa_busy", false, false),
                    Some("status") => refused("503", false, false),
                    Some("bare") => refused("error", false, false),
                    Some("hang") => refused("23", true, false),
                    Some("leave") => refused("error", false, true),
                    _ => Reply::Lease(Ok(())),
                }
            }
            Seam::Describe {
                url,
                method,
                redirect,
                headers,
                body,
                stage_timeout_ms,
                timeout_ms,
            } => {
                // Never follow a redirect; the two deadlines are the stage's and the request's.
                assert_eq!((method, redirect), ("POST", "error"));
                let stage = spec.get("stageMs").as_num().unwrap_or(120_000.0);
                assert_eq!((stage_timeout_ms, timeout_ms), (stage, 180_000));
                self.calls.push(call([
                    JValue::from("fetch"),
                    JValue::Str(url),
                    JValue::from(method),
                    JValue::from(redirect),
                    headers,
                    JValue::Str(body),
                ]));
                let cap = facts.get("cap").as_num().unwrap() as usize;
                Reply::Described {
                    answer: describe_answer(spec, cap),
                    client_left: spec.get("describe").get("leave").truthy(),
                    now_ms: self.clock,
                }
            }
            Seam::ReleaseLease => {
                self.calls.push(call([JValue::from("release")]));
                Reply::Released {
                    client_left: spec.get("describe").get("leave").truthy(),
                }
            }
            Seam::Send(event) => {
                self.events.push(event);
                Reply::Sent
            }
            Seam::Probe {
                base_url,
                headers,
                model,
            } => {
                self.calls.push(call([
                    JValue::from("visionProbe"),
                    base_url,
                    headers,
                    model,
                ]));
                Reply::Probe(match spec.get("vision") {
                    JValue::Undefined => facts.get("blind").clone(),
                    v => v.clone(),
                })
            }
            Seam::RecordEvidence { model, evidence } => {
                self.calls
                    .push(call([JValue::from("recordEvidence"), model, evidence]));
                Reply::Recorded
            }
        }
    }
}

/// What the turn's route is, as the recorder's stubs make P5b pick it: Automatic on the default
/// provider when the project asks for it and the roles are set, the classification's role (else
/// Smart, else Fast); otherwise the project's provider and model (the loaded model without one).
struct Route {
    provider_id: String,
    model: JValue,
    code: bool,
}

fn route(project: &JValue, spec: &JValue) -> Route {
    let roles = spec.get("roles");
    let provider = match project.get("provider") {
        JValue::Str(p) if p == "lemonade" => DEFAULT.to_string(),
        JValue::Str(p) if !p.is_empty() => p.clone(),
        _ => DEFAULT.to_string(),
    };
    let auto = (!project.truthy() || project.get("routing").as_str() == Some("auto"))
        && provider == DEFAULT
        && roles.truthy();
    if auto {
        let mut role = spec.get("classification").as_str().unwrap_or("fast");
        if !roles.get(role).truthy() {
            role = if roles.get("smart").truthy() {
                "smart"
            } else {
                "fast"
            };
        }
        return Route {
            provider_id: DEFAULT.into(),
            model: roles.get(role).clone(),
            code: role == "code",
        };
    }
    let model = match project.get("model") {
        m if m.truthy() => m.clone(),
        _ => JValue::from("qa-model"),
    };
    Route {
        provider_id: provider,
        model,
        code: false,
    }
}

/// The asset reads Node made (under the turn's asset directory, and the file beside it).
fn node_reads(case: &JValue) -> Vec<String> {
    list(case, "reads")
        .iter()
        .map(|r| {
            let JValue::Arr(r) = r else { panic!() };
            format!("{} {}", r[0].as_str().unwrap(), r[1].as_bool().unwrap())
        })
        .filter(|r| r.starts_with("assets") || r.starts_with("qa-outside.png"))
        .collect()
}

#[test]
fn whole_turns_are_nodes() {
    let f = fixture();
    let facts = f.get("facts");
    let differences: HashMap<&str, &JValue> = list(&f, "differences")
        .iter()
        .map(|d| (name(d), d))
        .collect();
    let (instructions, memory) = (account::read(None), memory::read(None));
    let now = time::fixed_now(Some(NOW)).now(0);
    let zoned = time::part(now, "UTC", true).unwrap();
    let zoneless = time::part(now, "UTC", false).unwrap();
    let no_tool = |_: &str| false;
    let mut caches: HashMap<String, (Descriptions, f64)> = HashMap::new();
    let mut d = Diffs::default();
    let (mut frames, mut kept, mut thrown, mut left) = (0, 0, 0, 0);
    for case in list(&f, "turns") {
        let n = name(case);
        let (spec, body) = (case.get("spec"), case.get("body"));
        let project = match spec.get("project") {
            p if p.is_object() && strict(body.get("projectId"), p.get("id")) => p.clone(),
            _ => JValue::Null,
        };
        let difference = differences.get(n).map(|d| d.get("kind").as_str().unwrap());

        // 1: the images, before routing.
        let mut asked = Vec::new();
        let got = vision::images(&project, body, &mut |id| {
            let bytes = spec.get("images").get(id).as_str().map(decode);
            asked.push(format!("assets/{id} {}", bytes.is_some()));
            bytes
        });
        if difference != Some("asset_path") {
            d.check(
                &format!("{n} reads"),
                &node_reads(case).join("\n"),
                &asked.join("\n"),
            );
        }
        let Ok(images) = got else {
            thrown += 1;
            assert_eq!(case.get("throws").as_str(), Some("TypeError"), "{n}");
            assert!(
                list(case, "events").is_empty() && list(case, "calls").is_empty(),
                "{n}"
            );
            continue;
        };
        if difference == Some("asset_path")
            && list(&f, "differences")
                .iter()
                .any(|x| name(x) == n && !list(x, "nodeRead").is_empty())
        {
            // Node read the image through another path; the port has it missing (tested below).
            assert!(!images.missing_names.is_empty(), "{n}");
            continue;
        }

        // The wire the pass starts from (prepare(); a turn off the box states the zoneless day).
        let r = route(&project, spec);
        let provider = facts.get("providers").get(&r.provider_id);
        let external = facts.get("external").get(&r.provider_id).as_bool().unwrap();
        let off_box = offer::off_box(external, false, r.provider_id == DEFAULT, false);
        let compact = body.get("compactOnly").truthy();
        let message = body.get("message").as_str().unwrap();
        let turn = Turn {
            instructions: &instructions,
            memory: &memory,
            now_ms: now,
            time_zone: "UTC",
            history: body.get("history"),
            history_cap: 1000,
            message: (!compact).then_some(message),
            carry_orphan_tool: &no_tool,
            known_tool: &no_tool,
            project: project.truthy().then_some(ProjectTurn {
                project: &project,
                code_tasks: &[],
                files_block: None,
                not_ready: &[],
                partial_reads: &[],
                skills: &[],
            }),
            trailing: &[],
        };
        let mut wire = chat_prepare::prepare(&turn).unwrap().messages;
        if off_box {
            let system = wire[0]
                .get("content")
                .as_str()
                .unwrap()
                .replacen(&zoned, &zoneless, 1);
            wire[0] = js::message("system", system);
        }

        // 2: the pass, after the meta event and the status line.
        let (cache, clock) = caches
            .entry(spec.get("group").as_str().unwrap_or(n).to_string())
            .or_default();
        *clock += 1.0;
        if spec.get("expire").truthy() {
            *clock += vision::DESCRIPTION_TTL_MS + 1.0;
        }
        let mut answers = Answers {
            spec,
            case,
            facts,
            clock: *clock,
            calls: Vec::new(),
            events: vec![reroute::status_event(false, images.reading())],
        };
        let upstream = headers_for(spec, &JValue::from(r.provider_id.as_str()));
        let user = spec.get("user").as_str().unwrap();
        let stage = spec.get("stageMs").as_num().unwrap_or(120_000.0);
        let mut lines = images.lines.clone();
        let mut pass = Pass::new(
            PassTurn {
                images,
                wire,
                sys: true,
                compact_only: compact,
                external,
                off_box,
                code_route: r.code,
                provider,
                default_provider_id: DEFAULT,
                model: &r.model,
                upstream_headers: &upstream,
                message,
                user_id: user,
                project_id: project.get("id"),
                stage_timeout_ms: stage,
                lease_hook: spec.get("lease") != &JValue::Bool(false),
                evidence_hook: spec.get("evidence") != &JValue::Bool(false),
                now_ms: *clock,
            },
            cache,
        );
        let mut reply = Reply::Start;
        let seen = loop {
            match pass.step(reply) {
                Step::Ask(seam) => reply = answers.answer(seam),
                Step::Done { seen, lines: more } => {
                    lines.extend(more);
                    break seen;
                }
            }
        };
        if difference == Some("parse_message") {
            assert!(lines.contains(&format!("[vision] describe failed: {}", vision::NOT_JSON)));
        } else if difference == Some("unreadable_json") {
            // Node took the description; the port says it failed (tested below).
            assert!(lines.contains(&format!("[vision] describe failed: {}", vision::UNREADABLE)));
            continue;
        } else {
            d.check(
                &format!("{n} lines"),
                &stringify(case.get("lines")),
                &stringify(&JValue::Arr(lines.into_iter().map(JValue::Str).collect())),
            );
        }
        let client_left = spec.get("describe").get("leave").truthy()
            || spec.get("leaseAnswer").as_str() == Some("leave");
        let seen = match seen {
            Ok(seen) => {
                if !client_left {
                    answers.events.push(vision::generating_event());
                    let warning = vision::reply_warning(&seen.image_warning, "", "");
                    answers.events.extend(vision::warning_event(&warning));
                }
                Some(seen)
            }
            Err(Halt::Thrown) => {
                thrown += 1;
                assert_eq!(case.get("throws").as_str(), Some("TypeError"), "{n}");
                None
            }
            Err(other) => panic!("{n}: {other:?}"),
        };
        d.check(
            &format!("{n} calls"),
            &stringify(case.get("calls")),
            &stringify(&JValue::Arr(answers.calls)),
        );
        d.check(
            &format!("{n} events"),
            &stringify(case.get("events")),
            &stringify(&JValue::Arr(answers.events)),
        );
        if let JValue::Arr(keys) = case.get("cache") {
            let mine: Vec<JValue> = cache.keys().map(JValue::from).collect();
            d.check(
                &format!("{n} cache"),
                &stringify(&JValue::Arr(keys.clone())),
                &stringify(&JValue::Arr(mine)),
            );
        }
        let Some(seen) = seen else { continue };

        // 3: the wire and the image warning.
        let wire = stringify(&JValue::Arr(seen.wire));
        if let JValue::Obj(_) = case.get("frame") {
            frames += 1;
            let frame = case.get("frame");
            d.check(n, &stringify(frame.get("messages")), &wire);
            let warning = if project.truthy() {
                JValue::Str(seen.image_warning)
            } else {
                JValue::Null
            };
            d.check(
                &format!("{n} imageWarning"),
                &stringify(frame.get("imageWarning")),
                &stringify(&warning),
            );
        } else if let Some(why) = case.get("refused").as_str() {
            kept += 1;
            let diary = project.get("id").as_str() == facts.get("diaryExtrasId").as_str();
            assert_eq!(why, if diary { "diary" } else { "provider" }, "{n}");
            if compact && case.get("messages") == &JValue::Null {
                // A compaction Node keeps sends its summary request, not this wire.
                continue;
            }
            d.check(n, &stringify(case.get("messages")), &wire);
        } else {
            // The client left during the description: Node ends the turn after the probe.
            left += 1;
            assert!(client_left && case.get("messages") == &JValue::Null, "{n}");
        }
    }
    assert!(frames >= 250, "only {frames} frames");
    assert!(kept >= 30, "only {kept} turns Node keeps");
    assert!(thrown >= 8, "only {thrown} throwing turns");
    assert_eq!(left, 2);
    d.done(1500);
}

/// SameValueZero of two JSON values (`id === project.id`).
fn strict(a: &JValue, b: &JValue) -> bool {
    !matches!(a, JValue::Obj(_) | JValue::Arr(_) | JValue::Undefined) && a == b
}

#[test]
fn the_named_differences_are_the_asset_paths_and_one_line() {
    let f = fixture();
    let all = list(&f, "differences");
    let mut kinds: HashMap<&str, usize> = HashMap::new();
    for diff in all {
        *kinds.entry(diff.get("kind").as_str().unwrap()).or_default() += 1;
        let n = name(diff);
        let case = list(&f, "turns").iter().find(|t| name(t) == n).unwrap();
        let spec = case.get("spec");
        match diff.get("kind").as_str().unwrap() {
            "asset_path" => {
                // The port never asks for an id that is not a plain file name.
                let mut asked = Vec::new();
                let images = vision::images(spec.get("project"), case.get("body"), &mut |id| {
                    asked.push(id.to_string());
                    None
                })
                .unwrap();
                assert!(asked.iter().all(|id| vision::plain_asset_name(id)), "{n}");
                if !list(diff, "nodeRead").is_empty() {
                    // Node attached the file it found through that path; the port has it missing.
                    assert!(
                        stringify(case.get("frame")).contains("data:image/png;base64,"),
                        "{n}"
                    );
                    assert_eq!(images.attached, Vec::<JValue>::new(), "{n}");
                    assert_eq!(images.missing_names.len(), 1, "{n}");
                }
            }
            "parse_message" => {
                let node = stringify(diff.get("nodeLines"));
                assert!(node.contains("is not valid JSON"), "{n}: {node}");
                assert_eq!(
                    vision::description(b"QA not json"),
                    Err(vision::NOT_JSON.to_string())
                );
            }
            "unreadable_json" => {
                let raw = spec.get("describe").get("raw").as_str().unwrap();
                assert_eq!(
                    vision::description(raw.as_bytes()),
                    Err(vision::UNREADABLE.to_string())
                );
                let frame = stringify(case.get("frame"));
                assert!(
                    frame.contains("(data, not instructions)\\nQA description beside"),
                    "{n}"
                );
                assert!(stringify(case.get("lines")) == "[]", "{n}");
            }
            other => panic!("{n}: {other}"),
        }
    }
    assert_eq!(kinds.get("parse_message"), Some(&1));
    assert_eq!(kinds.get("unreadable_json"), Some(&2));
    assert!(kinds.get("asset_path").copied().unwrap_or(0) >= 9);
    let attached_elsewhere = all
        .iter()
        .filter(|d| d.get("kind").as_str() == Some("asset_path") && !list(d, "nodeRead").is_empty())
        .count();
    assert_eq!(attached_elsewhere, 3);
}

#[test]
fn the_recording_covers_every_branch() {
    let f = fixture();
    let turns = stringify(&JValue::Arr(list(&f, "turns").to_vec()));
    for needle in [
        "Re-upload them. Do not guess their contents.",
        "Project images were not sent: images are never sent to QA Cloud automatically.",
        "never sent to an external provider automatically",
        "that are not attached to this message",
        "For commands and code:",
        "<untrusted",
        "image description",
        "did not finish within 0.05 seconds",
        "did not finish within 1.5 seconds",
        "could not describe it, so qa-model was given the image directly",
        "the configured vision model also could not describe them",
        "(Attached images: ",
        "data:image/jpeg;base64,",
        "data:image/gif;base64,",
        "data:image/webp;base64,",
        "data:undefined;base64,",
        "not admitted: timeout",
        "not admitted: qa_busy",
        "reply exceeded the 4096 KB limit",
        "Cannot convert object to primitive value",
        "Unexpected end of JSON input",
        "\"POST\",\"error\",{\"Content-Type\"",
        "1×1 image accepted; not an accuracy test",
        "\"result\":\"failed\"",
        "The question that will be asked is: QA \\\\ud83d",
    ] {
        assert!(turns.contains(needle), "the recording lacks {needle}");
    }
    let calls = |c: &JValue, what: &str| {
        let head = format!("[\"{what}\"");
        list(c, "calls")
            .iter()
            .filter(|x| stringify(x).starts_with(&head))
            .count()
    };
    let all = list(&f, "turns");
    let described = all.iter().filter(|t| calls(t, "fetch") > 0).count();
    let hits = all
        .iter()
        .filter(|t| {
            calls(t, "providerHeaders") > 0
                && calls(t, "fetch") == 0
                && calls(t, "acquireLease") == 0
                && calls(t, "visionProbe") == 0
        })
        .count();
    let probed = all.iter().filter(|t| calls(t, "visionProbe") > 0).count();
    assert!(described >= 120, "only {described} described");
    assert!(hits >= 5, "only {hits} cache hits");
    assert!(probed >= 150, "only {probed} probed");
    let evicted = all
        .iter()
        .find(|t| name(t) == "cache_evicted_asked_again")
        .unwrap();
    assert_eq!(list(evicted, "cache").len(), vision::DESCRIPTIONS_MAX);
}

#[test]
fn base64_is_buffers() {
    assert_eq!(vision::base64(b""), "");
    assert_eq!(vision::base64(b"f"), "Zg==");
    assert_eq!(vision::base64(b"fo"), "Zm8=");
    assert_eq!(vision::base64(b"foo"), "Zm9v");
    assert_eq!(vision::base64(&[0xff, 0xfe, 0xfd, 0x00]), "//79AA==");
    let large = fixture()
        .get("facts")
        .get("large")
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(vision::base64(&decode(&large)), large);
}

#[test]
fn only_plain_names_are_read() {
    assert!(vision::plain_asset_name("img-0123abcd"));
    for id in ["", ".", "..", "../x", "a/b", "a\\b", "a\0"] {
        assert!(!vision::plain_asset_name(id), "{id:?}");
    }
    // A lone surrogate (held as wtf16 holds it) is not plain; an astral character is.
    let lone = wtf16::lone_char(0xd800).unwrap();
    assert!(!vision::plain_asset_name(&format!("img-{lone}")));
    assert!(vision::plain_asset_name("img-😀"));
}

#[test]
fn descriptions_keep_their_order_and_bound() {
    let mut d = Descriptions::default();
    for k in 0..=vision::DESCRIPTIONS_MAX {
        d.set(&format!("k{k}"), "t", 0.0);
    }
    assert_eq!(d.keys().count(), vision::DESCRIPTIONS_MAX);
    assert_eq!(d.keys().next(), Some("k1"));
    d.set("k1", "again", 10.0);
    assert_eq!(d.keys().next(), Some("k1"));
    let ttl = vision::DESCRIPTION_TTL_MS;
    assert_eq!(d.get("k1", 10.0 + ttl - 1.0), Some("again"));
    assert_eq!(d.get("k1", 10.0 + ttl), None);
}

#[test]
fn an_answer_out_of_order_halts() {
    let (provider, model, headers, id) = (
        JValue::obj([("id", JValue::from(DEFAULT))]),
        JValue::from("qa-model"),
        JValue::obj::<&str>([]),
        JValue::from("qa-project"),
    );
    let mut cache = Descriptions::default();
    let mut images = Images::default();
    images.attached.push(JValue::from("part"));
    let turn = PassTurn {
        images,
        wire: vec![
            js::message("system", "QA".into()),
            js::message("user", "QA?".into()),
        ],
        sys: true,
        compact_only: false,
        external: false,
        off_box: false,
        code_route: false,
        provider: &provider,
        default_provider_id: DEFAULT,
        model: &model,
        upstream_headers: &headers,
        message: "QA?",
        user_id: "qa-user",
        project_id: &id,
        stage_timeout_ms: 120_000.0,
        lease_hook: true,
        evidence_hook: true,
        now_ms: 0.0,
    };
    let mut pass = Pass::new(turn, &mut cache);
    assert_eq!(pass.step(Reply::Start), Step::Ask(Seam::AutoRoles));
    let halted = |s: Step| {
        matches!(
            s,
            Step::Done {
                seen: Err(Halt::OutOfOrder),
                ..
            }
        )
    };
    assert!(halted(pass.step(Reply::Sent)));
    assert!(halted(pass.step(Reply::Start)));
}
