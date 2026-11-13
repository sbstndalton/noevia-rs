//! The recorded Node output of slice P2a (`fixtures/node-project.v1.json`, written by
//! `record_project.cjs`): every quoted name, readability verdict, waiting notice, coverage note,
//! shared Code tasks part and whole project turn must be the bytes noevia-core produced.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chat_prepare::sources::{self, NotReady, Partial};
use chat_prepare::time::{self, FixedNow};
use chat_prepare::{
    account, memory, names, shared, PartKind, PrepareError, ProjectTurn, Thrown, Turn,
};
use js_json::{wtf16, JValue};
use std::collections::HashSet;

const FIXTURE: &str = include_str!("fixtures/node-project.v1.json");

fn fixture() -> JValue {
    wtf16::parse(FIXTURE).expect("the fixture is JSON")
}

fn items<'a>(v: &'a JValue, key: &str) -> &'a [JValue] {
    match v.get(key) {
        JValue::Arr(items) => items,
        other => panic!("{key} is not an array: {other:?}"),
    }
}

fn name(case: &JValue) -> &str {
    case.get("name").as_str().unwrap_or("?")
}

fn stringify(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_default()
}

fn bytes(v: &JValue) -> Option<Vec<u8>> {
    let hex = v.as_str()?;
    Some(
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect(),
    )
}

fn names_of(v: &JValue) -> HashSet<String> {
    match v {
        JValue::Arr(items) => items
            .iter()
            .filter_map(|n| n.as_str().map(str::to_string))
            .collect(),
        _ => HashSet::new(),
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

/// `attempt(f)` as the recorder writes it: `{ out }` or `{ throws: 'TypeError' }`.
fn attempted(r: Result<Option<String>, Thrown>, empty: &str) -> String {
    match r {
        Ok(v) => format!(
            "{{\"out\":{}}}",
            stringify(&JValue::Str(v.unwrap_or_else(|| empty.to_string())))
        ),
        Err(Thrown) => "{\"throws\":\"TypeError\"}".to_string(),
    }
}

#[test]
fn the_fixture_names_its_node_sources() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    for file in [
        "server/chat.cjs",
        "server/quoted-name.cjs",
        "server/shared-context.cjs",
        "server/source-readability.cjs",
        "server/source-coverage.cjs",
        "server/instruction-skills.cjs",
        "server/wasm/dav-parse.wasm",
    ] {
        let sha = f.get("sources").get(file).as_str().unwrap();
        assert!(sha.len() == 64, "{file}");
    }
}

#[test]
fn quoted_names_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in items(&f, "quote") {
        let n = case.get("name").as_str().unwrap();
        d.check(
            &format!("{n:?}"),
            case.get("quoted").as_str().unwrap(),
            &names::quote_name(n),
        );
    }
    for case in items(&f, "quoteLists") {
        let list: Vec<&str> = items(case, "names")
            .iter()
            .map(|n| n.as_str().unwrap())
            .collect();
        d.check(
            &format!("{list:?}"),
            case.get("quoted").as_str().unwrap(),
            &names::quoted_names(&list),
        );
    }
    d.done("quote", 250);
}

#[test]
fn readability_verdicts_and_waiting_notices_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in items(&f, "readability") {
        let files = case.get("files");
        let list: &[JValue] = match files {
            JValue::Arr(items) => items,
            _ => &[],
        };
        let verdicts = |g: fn(&JValue) -> Result<bool, Thrown>| {
            JValue::Arr(
                list.iter()
                    .map(|file| g(file).map_or(JValue::Null, JValue::Bool))
                    .collect(),
            )
        };
        let what = name(case);
        d.check(
            &format!("{what} unreadable"),
            &stringify(case.get("unreadable")),
            &stringify(&verdicts(sources::is_unreadable)),
        );
        d.check(
            &format!("{what} waiting"),
            &stringify(case.get("waiting")),
            &stringify(&verdicts(sources::is_waiting)),
        );
        let kept = sources::readable(files).map_or(JValue::Null, |kept| {
            JValue::Arr(
                kept.iter()
                    .map(|k| {
                        let at = list.iter().position(|f| std::ptr::eq(f, *k)).unwrap();
                        JValue::from(at as f64)
                    })
                    .collect(),
            )
        });
        d.check(
            &format!("{what} readable"),
            &stringify(case.get("readable")),
            &stringify(&kept),
        );
        d.check(
            &format!("{what} notice"),
            &stringify(case.get("notice")),
            &attempted(sources::waiting_notice(files), ""),
        );
    }
    d.done("readability", 4 * 170);
}

fn partials(v: &JValue) -> Vec<Partial> {
    match v {
        JValue::Arr(items) => items
            .iter()
            .map(|p| Partial {
                name: p.get("name").as_str().unwrap().to_string(),
                length: p.get("length").as_num().unwrap() as u64,
                read: p.get("read").as_num().unwrap() as u64,
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[test]
fn coverage_notes_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in items(&f, "coverage") {
        let got = sources::coverage_note(&partials(case.get("partial"))).unwrap_or_default();
        d.check(name(case), case.get("note").as_str().unwrap(), &got);
    }
    d.done("coverage", 70);
}

#[test]
fn shared_code_task_parts_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in items(&f, "shared") {
        let project = case.get("project");
        let (chat, code) = shared::read(project);
        let read = JValue::obj([("chat", JValue::Bool(chat)), ("code", JValue::Bool(code))]);
        d.check(
            &format!("{} read", name(case)),
            &stringify(case.get("read")),
            &stringify(&read),
        );
        d.check(
            &format!("{} part", name(case)),
            &stringify(case.get("part")),
            &attempted(shared::for_chat(project, items(case, "tasks")), ""),
        );
    }
    d.done("shared", 2 * 200);
}

fn not_ready(v: &JValue) -> Vec<NotReady> {
    match v {
        JValue::Arr(items) => items
            .iter()
            .map(|f| NotReady {
                name: sources::string_of(f.get("name")).unwrap(),
                failed: f.get("reason").as_str() == Some("failed"),
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[test]
fn whole_project_turns_are_nodes() {
    let f = fixture();
    let tools = f.get("turnTools");
    let (public, known) = (names_of(tools.get("public")), names_of(tools.get("known")));
    let carry = |n: &str| public.contains(n);
    let knows = |n: &str| known.contains(n);
    let mut d = Diffs::default();
    let mut thrown = 0;
    for case in items(&f, "turns") {
        let instructions = account::read(bytes(case.get("instructionsHex")).as_deref());
        let memory = memory::read(bytes(case.get("memoryHex")).as_deref());
        let pin = case.get("pin").as_str().map(str::to_string);
        let now = time::fixed_now(pin.as_deref());
        assert!(matches!(now, FixedNow::Pinned { .. }), "{}", name(case));
        let ready = not_ready(case.get("notReady"));
        let partial = partials(case.get("partialReads"));
        let compact = case.get("compact") == &JValue::Bool(true);
        let turn = Turn {
            instructions: &instructions,
            memory: &memory,
            now_ms: now.now(0),
            time_zone: "UTC",
            history: case.get("history"),
            history_cap: 1000,
            message: if compact {
                None
            } else {
                case.get("message").as_str()
            },
            carry_orphan_tool: &carry,
            known_tool: &knows,
            project: Some(ProjectTurn {
                project: case.get("project"),
                code_tasks: items(case, "tasks"),
                files_block: case.get("filesBlock").as_str(),
                not_ready: &ready,
                partial_reads: &partial,
                skills: &[],
            }),
            trailing: &[],
        };
        let got = chat_prepare::prepare(&turn);
        if case.get("throws").as_str().is_some() {
            thrown += 1;
            assert_eq!(got, Err(PrepareError::Thrown), "{}", name(case));
            continue;
        }
        let prepared = got.unwrap_or_else(|e| panic!("{}: {e:?}", name(case)));
        d.check(
            name(case),
            &stringify(case.get("messages")),
            &stringify(&JValue::Arr(prepared.messages.clone())),
        );
        // The skill blocks (a later slice) go after the coverage note and before the stored-only
        // line, and the frame and brain blocks after everything, in order.
        let skills = [
            (
                PartKind::Other("qa skill index"),
                "QA skill index".to_string(),
            ),
            (PartKind::Other("qa empty"), String::new()),
            (PartKind::Other("qa pinned"), "QA pinned skill".to_string()),
        ];
        let trailing = [(PartKind::Other("qa frame"), "QA frame".to_string())];
        let more = chat_prepare::prepare(&Turn {
            project: Some(ProjectTurn {
                skills: &skills,
                ..turn.project.unwrap()
            }),
            trailing: &trailing,
            ..turn
        })
        .unwrap();
        let mut want: Vec<&str> = prepared
            .parts
            .parts()
            .iter()
            .map(|(_, t)| t.as_str())
            .collect();
        let at = prepared
            .parts
            .parts()
            .iter()
            .position(|(k, _)| *k == PartKind::StoredOnly)
            .unwrap_or(want.len());
        want.splice(at..at, ["QA skill index", "QA pinned skill"]);
        want.push("QA frame");
        assert_eq!(more.system, want.join("\n\n"), "{}", name(case));
    }
    assert!(thrown >= 2, "only {thrown} throwing turns recorded");
    d.done("turns", 150);
}

/// The named difference: a Code task's `updatedAt` that is not a number sorts as 0 here (a stable
/// sort, newest first), whatever Node made of it.
#[test]
fn the_named_differences_behave_as_documented() {
    let f = fixture();
    let differences = items(&f, "differences");
    assert!(differences.len() >= 3);
    let project = JValue::obj([("sharedContext", JValue::obj([("chat", JValue::Bool(true))]))]);
    for case in differences {
        assert_eq!(case.get("kind").as_str(), Some("code_task_order"));
        assert!(case.get("node").as_str().is_some(), "{}", name(case));
        let tasks = items(case, "tasks");
        let part = shared::for_chat(&project, tasks).unwrap().unwrap();
        let mut order: Vec<(usize, f64)> = tasks
            .iter()
            .enumerate()
            .map(|(i, t)| (i, t.get("updatedAt").as_num().unwrap_or(0.0)))
            .collect();
        order.sort_by(|a, b| b.1.total_cmp(&a.1));
        let at: Vec<usize> = order
            .iter()
            .map(|(i, _)| part.find(&format!("\"QA {i}\"")).unwrap())
            .collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]), "{}: {part}", name(case));
    }
}

/// The recording reaches the branches that matter, so a narrower recording cannot pass silently.
#[test]
fn the_recording_covers_every_branch() {
    let f = fixture();
    let turns: String = items(&f, "turns")
        .iter()
        .map(stringify)
        .collect::<Vec<_>>()
        .join("\n");
    for needle in [
        "You are working inside the user's project",
        "Project goal: ",
        "Project instructions (follow closely):",
        "Things you know about the user",
        "recent Code tasks",
        "Relevant knowledge-file excerpts for this message:",
        "but it was not finished processing in time",
        "but they were not processed in time",
        "but they were not finished processing in time",
        "is waiting to be read",
        "and 2 more are waiting to be read",
        "Source text was truncated for this reply",
        "none of it (it was not included)",
        "These sources are stored only",
        "QA attached excerpt\\n\\nQA question",
        "</untrusted>\\n\\nQA attached excerpt\\n\\nQA question",
    ] {
        assert!(turns.contains(needle), "turns lack {needle}");
    }
}
