//! The recorded Node output of slice P2b (`fixtures/node-retrieval.v1.json`, written by
//! `record_retrieval.cjs`): every source item, partial read, coverage warning, frame `sources`,
//! readable set, Diary verdict and chat project id, and every whole turn's seam arguments, events,
//! system message and frame must be what noevia-core produced, byte for byte.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chat_prepare::retrieval::{self, items, Answer, Loaded, Retrieved, Settled, Source, Stopped};
use chat_prepare::sources::{self, NotReady, Partial};
use chat_prepare::time::{self, FixedNow};
use chat_prepare::{account, memory, PrepareError, ProjectTurn, Thrown, Turn};
use js_json::{wtf16, JValue};
use std::collections::HashSet;

const FIXTURE: &str = include_str!("fixtures/node-retrieval.v1.json");
const PROJECT_FIXTURE: &str = include_str!("fixtures/node-project.v1.json");
const SKILLS_FIXTURE: &str = include_str!("fixtures/node-skills.v1.json");
const TOOLS_FIXTURE: &str = include_str!("fixtures/node-tools.v1.json");
const PREPARE_FIXTURE: &str = include_str!("fixtures/node-prepare.v1.json");

/// The Node files this slice ports, as recorded at tests/oracle-core.ref.
const PINNED: [(&str, &str); 6] = [
    (
        "server/chat-sources.cjs",
        "56d929c4c3ea02152136e8c17fc6d6c10a9724b41016f2677c265e30945235f5",
    ),
    (
        "server/source-coverage.cjs",
        "dad3e604f74b30b017f25d4fb294adbcb00f1c2e244dc61bd88f9f8aab2cd79c",
    ),
    (
        "server/rust-chat-native.cjs",
        "559d3722a29b280695cea98806626438e7282f8c3a6e4e9679d199b3f1efab4e",
    ),
    (
        "server/multimodal-rag.cjs",
        "32176635fc344ad4031590bf3332b8d89a983a860d3a95aef5949bdb388df377",
    ),
    (
        "server/diary-extras.cjs",
        "1d8f6080333d46c01f0d31d6530bd1c74274bac034296265ea730856231f1d3f",
    ),
    (
        "server/rag.cjs",
        "fcc097b47a8ff766b373fe03987f24ce88b966e8d4363d34f9f351d70e3f5254",
    ),
];

/// chat.cjs's ported blocks: their sha256 and lines at the pinned core.
const BLOCKS: [(&str, &str, [f64; 2]); 6] = [
    (
        "projectWait",
        "83b9773c3d2186d3a1f808d464be0f2bfd246aa2cf571cc694143232af09c11d",
        [472.0, 488.0],
    ),
    (
        "retrieval",
        "f6ebb4d93849cda2e35f11864dec1cfd2e7b42f962dbcdf15b3a207afd4e08ed",
        [587.0, 616.0],
    ),
    (
        "sourcesEvent",
        "28b462f447d656fc898eacd858340795dec9176f8987f9dbe51614690957e35a",
        [1463.0, 1463.0],
    ),
    (
        "injectedRefusal",
        "58022f5d802536cccd70c1f3c3353b048a4a0cb95a4a6fb18454ada5d7c32c33",
        [1743.0, 1743.0],
    ),
    (
        "frameSources",
        "36ac814c9047a4111be35bf147c7ef4d1076f02510e40389e943d2b32a0e333a",
        [1776.0, 1776.0],
    ),
    (
        "coverageWarning",
        "e0fcdb20e9e39c3c932774b11717db668d9f9230c1c6095b378c5478cc4f6c76",
        [1787.0, 1787.0],
    ),
];

fn fixture() -> JValue {
    wtf16::parse(FIXTURE).expect("the fixture is JSON")
}

fn list<'a>(v: &'a JValue, key: &str) -> &'a [JValue] {
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

fn names_of(v: &JValue) -> HashSet<String> {
    items(v)
        .iter()
        .filter_map(|n| n.as_str().map(str::to_string))
        .collect()
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
        assert!(self.total >= min, "{section}: only {} checks", self.total);
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
fn attempted(r: Result<JValue, Thrown>) -> String {
    match r {
        Ok(v) => stringify(&JValue::obj([("out", v)])),
        Err(Thrown) => "{\"throws\":\"TypeError\"}".to_string(),
    }
}

fn partial_json(partial: &[Partial]) -> JValue {
    JValue::Arr(
        partial
            .iter()
            .map(|p| {
                JValue::obj([
                    ("name", JValue::from(p.name.as_str())),
                    ("length", JValue::Num(p.length as f64)),
                    ("read", JValue::Num(p.read as f64)),
                ])
            })
            .collect(),
    )
}

fn partials(v: &JValue) -> Vec<Partial> {
    items(v)
        .iter()
        .map(|p| Partial {
            name: sources::string_of(p.get("name")).unwrap(),
            length: p.get("length").as_num().unwrap() as u64,
            read: p.get("read").as_num().unwrap() as u64,
        })
        .collect()
}

#[test]
fn the_fixture_names_its_node_sources() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    for (file, sha) in PINNED {
        assert_eq!(f.get("sources").get(file).as_str(), Some(sha), "{file}");
    }
    // Recorded at the same core as the crate's other fixtures (tests/oracle-core.ref).
    for (other, files) in [
        (
            PROJECT_FIXTURE,
            &[
                "server/chat.cjs",
                "server/source-coverage.cjs",
                "server/source-readability.cjs",
                "server/source-text.cjs",
                "server/instruction-skills.cjs",
                "server/quoted-name.cjs",
                "server/tools-mode.cjs",
                "server/wasm/dav-parse.wasm",
            ][..],
        ),
        (
            SKILLS_FIXTURE,
            &["server/diary-extras.cjs", "server/multimodal-rag.cjs"][..],
        ),
        (TOOLS_FIXTURE, &["server/rag.cjs"][..]),
        (PREPARE_FIXTURE, &["server/chat-sources.cjs"][..]),
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

#[test]
fn source_items_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in list(&f, "build") {
        // Plain files (no source-text.cjs accessors in Node either).
        let files = plain(case.get("files"));
        let got = retrieval::build_sources(items(case.get("entries")), &files).map(JValue::Arr);
        d.check(name(case), &stringify(case.get("sources")), &attempted(got));
    }
    d.done("build", 200);
}

#[test]
fn partial_reads_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in list(&f, "partial") {
        // partialSources(files, placed): [] unless both are lists. The files went through attachFile.
        let loaded = load(case.get("files"), case.get("texts"));
        let files: Vec<Source> = (0..items(case.get("files")).len())
            .filter_map(|at| loaded.source(at))
            .collect();
        let got = match (case.get("files"), case.get("placed")) {
            (JValue::Arr(_), JValue::Arr(placed)) => {
                retrieval::partial_sources(&files, placed).map(|p| partial_json(&p))
            }
            _ => Ok(JValue::Arr(Vec::new())),
        };
        d.check(name(case), &stringify(case.get("partial")), &attempted(got));
    }
    d.done("partial", 180);
}

#[test]
fn coverage_warnings_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in list(&f, "warning") {
        let got = retrieval::coverage_warning(&partials(case.get("partial")));
        d.check(
            name(case),
            &stringify(case.get("warning")),
            &stringify(&JValue::Str(got)),
        );
    }
    d.done("warning", 60);
}

#[test]
fn frame_sources_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in list(&f, "injected") {
        let i = case.get("input");
        let got = retrieval::injected_sources(
            items(i.get("injected")),
            items(i.get("placed")),
            &plain(i.get("readable")),
            items(i.get("reply")),
        );
        d.check(name(case), &stringify(case.get("sources")), &attempted(got));
    }
    d.done("injected", 200);
}

#[test]
fn readable_sets_diary_projects_and_chat_projects_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in list(&f, "readable") {
        let want = if case.get("throws").as_str().is_some() {
            "throws".to_string()
        } else {
            stringify(case.get("readable"))
        };
        let loaded = retrieval::load(case.get("project"), &|_| None).unwrap();
        let got = match retrieval::readable(&loaded) {
            Ok(at) => stringify(&JValue::Arr(
                at.iter().map(|i| JValue::Num(*i as f64)).collect(),
            )),
            Err(Thrown) => "throws".to_string(),
        };
        d.check(name(case), &want, &got);
    }
    for case in list(&f, "diaryProject") {
        let got = retrieval::diary_project(case.get("id")).map(JValue::Bool);
        d.check(
            &stringify(case.get("id")),
            &stringify(case.get("diary")),
            &attempted(got),
        );
    }
    for case in list(&f, "chatProject") {
        let got = retrieval::chat_project_id(case.get("chatId")).map_or(JValue::Null, JValue::Str);
        d.check(
            &stringify(case.get("chatId")),
            &stringify(case.get("id")),
            &stringify(&got),
        );
    }
    d.done("readable, diaryProject, chatProject", 160);
}

fn plain(files: &JValue) -> Vec<Source> {
    items(files).iter().cloned().map(Source::plain).collect()
}

/// `files` (a list, or the record holding it) as getProject gives it: through [`retrieval::load`]
/// with the case's texts (keyed by `String(textRef)`, as the recorder wrote them).
fn load(v: &JValue, texts: &JValue) -> Loaded {
    let record = match v {
        JValue::Arr(_) => JValue::obj([("files", v.clone())]),
        _ => v.clone(),
    };
    let lookup = |r: &str| texts.get(r).as_str().map(str::to_string);
    retrieval::load(&record, &lookup).unwrap()
}

/// The body as chat.cjs holds it once a Diary extras turn is pointed at the extras project.
fn effective_body(body: &JValue) -> JValue {
    let JValue::Obj(fields) = body else {
        return body.clone();
    };
    if body.get("spaceId").as_str() != Some("diary-extras") {
        return body.clone();
    }
    let mut fields = fields.clone();
    wtf16::set(
        &mut fields,
        "projectId",
        JValue::from(retrieval::DIARY_EXTRAS_PROJECT),
    );
    JValue::Obj(fields)
}

/// What the stubs did with a case's answer (rag.cjs's contract: nothing readable answers null
/// without a report).
fn answer_of(case: &JValue, readable: usize) -> Answer {
    let a = case.get("answer");
    if a.get("throws").truthy() {
        return Answer::Threw;
    }
    if readable == 0 {
        return Answer::Answered {
            block: None,
            reported: None,
        };
    }
    let given = |key: &str| match a.get(key) {
        JValue::Undefined => JValue::Arr(Vec::new()),
        v => v.clone(),
    };
    Answer::Answered {
        block: a.get("block").as_str().map(str::to_string),
        reported: (a.get("report") != &JValue::Bool(false))
            .then(|| (given("placed"), given("injected"))),
    }
}

fn not_ready_of(v: &JValue) -> Vec<NotReady> {
    items(v)
        .iter()
        .map(|f| NotReady {
            name: sources::string_of(f.get("name")).unwrap(),
            failed: f.get("reason").as_str() == Some("failed"),
        })
        .collect()
}

#[test]
fn whole_turns_are_nodes() {
    let f = fixture();
    let tools = f.get("turnTools");
    let (public, known) = (names_of(tools.get("public")), names_of(tools.get("known")));
    let carry = |n: &str| public.contains(n);
    let knows = |n: &str| known.contains(n);
    let (instructions, memory) = (account::read(None), memory::read(None));
    let now = time::fixed_now(Some("2026-11-13T12:00:00Z"));
    assert!(matches!(now, FixedNow::Pinned { .. }));
    let mut d = Diffs::default();
    let (mut frames, mut thrown, mut refused) = (0, 0, 0);
    let mut images_kept: HashSet<&str> = list(&f, "differences")
        .iter()
        .map(|d| d.get("name").as_str().unwrap())
        .collect();
    let kept = images_kept.len();
    for case in list(&f, "turns") {
        let n = name(case);
        let body = effective_body(case.get("body"));
        let stored = load(case.get("project"), case.get("texts"));
        let stored_id = stored
            .project
            .get("id")
            .as_str()
            .unwrap_or_default()
            .to_string();
        let compact = body.get("compactOnly").truthy();
        let message = body.get("message").as_str().unwrap();
        let mut calls = Vec::new();

        // 1-3: the project id, the wait, the record.
        let pid = retrieval::project_id(&body, &|id| id == stored_id);
        let mut not_ready = Vec::new();
        if let Some(w) = retrieval::wait(&pid, &body, true) {
            calls.push(JValue::obj([
                ("seam", JValue::from("settle")),
                ("user", JValue::from("qa-user")),
                ("projectId", w.project_id.clone()),
                ("timeoutMs", JValue::Num(w.timeout_ms as f64)),
                ("signal", JValue::Bool(true)),
            ]));
            let settled = if case.get("settle").get("aborted") == &JValue::Bool(true) {
                Settled::Aborted
            } else {
                Settled::Done(not_ready_of(case.get("settle").get("notReady")))
            };
            match settled {
                Settled::Aborted => {
                    assert_eq!(case.get("ended"), &JValue::Bool(true), "{n}");
                    d.check(
                        n,
                        &stringify(case.get("calls")),
                        &stringify(&JValue::Arr(calls)),
                    );
                    continue;
                }
                Settled::Done(list) => not_ready = list,
            }
        }
        let reads =
            retrieval::loads_project(&pid, &body) && pid.as_str() == Some(stored_id.as_str());
        let loaded = reads.then_some(&stored);
        let project = loaded.map(|l| &l.project);

        // 4: the request.
        let images = retrieval::multimodal(
            project,
            &body,
            case.get("multimodal") == &JValue::Bool(true),
        );
        let request = match (loaded, images) {
            (Some(l), Ok(images)) => retrieval::request(l, message, "qa-user", images),
            (_, Err(e)) => Err(e),
            (None, Ok(_)) => Ok(None),
        };
        if case.get("diarySpace") == &JValue::Bool(true) {
            // The Diary's own stream answers: neither seam is asked.
            assert!(
                project.is_none() && case.get("frame") == &JValue::Undefined,
                "{n}"
            );
            d.check(
                n,
                &stringify(case.get("calls")),
                &stringify(&JValue::Arr(calls)),
            );
            continue;
        }
        let request = match request {
            Ok(r) => r,
            Err(Thrown) => {
                thrown += 1;
                assert_eq!(case.get("throws").as_str(), Some("TypeError"), "{n}");
                d.check(
                    n,
                    &stringify(case.get("calls")),
                    &stringify(&JValue::Arr(calls)),
                );
                continue;
            }
        };
        if let Some(r) = &request {
            let mut options = vec![("attached", JValue::Bool(r.attached))];
            if r.images {
                options.push(("images", JValue::Bool(true)));
            }
            options.push((
                "onImages",
                JValue::from(if r.images { "function" } else { "undefined" }),
            ));
            calls.push(JValue::obj([
                ("seam", JValue::from("filesContext")),
                ("projectId", r.project_id.clone()),
                (
                    "readable",
                    JValue::Arr(r.readable.iter().map(|i| JValue::Num(*i as f64)).collect()),
                ),
                ("query", JValue::from(r.query)),
                ("user", JValue::from(r.user)),
                ("onSources", JValue::from("function")),
                ("options", JValue::obj(options)),
            ]));
        }
        d.check(
            n,
            &stringify(case.get("calls")),
            &stringify(&JValue::Arr(calls)),
        );

        // 5: the answer.
        let answer = answer_of(case, request.as_ref().map_or(0, |r| r.readable.len()));
        let got = match retrieval::retrieved(request.as_ref(), answer) {
            Ok(r) => r,
            Err(Stopped::Threw) => {
                thrown += 1;
                assert_eq!(case.get("throws").as_str(), Some("Error"), "{n}");
                continue;
            }
            Err(Stopped::Images) => {
                // The named difference: the turn stays in Node.
                assert!(
                    images_kept.remove(n),
                    "{n}: stopped for images, not a recorded difference"
                );
                continue;
            }
        };
        let r: Retrieved = got;

        // 6: what the turn reads of it.
        d.check(
            &format!("{n} notReady"),
            &stringify(case.get("notReady")),
            &stringify(&match retrieval::meta_not_ready(&not_ready) {
                JValue::Undefined => JValue::Null,
                v => v,
            }),
        );
        d.check(
            &format!("{n} sources event"),
            &stringify(case.get("sourcesEvent")),
            &stringify(
                &retrieval::sources_event(&r, compact)
                    .map_or(JValue::Null, |s| JValue::Arr(s.to_vec())),
            ),
        );
        let turn = Turn {
            instructions: &instructions,
            memory: &memory,
            now_ms: now.now(0),
            time_zone: "UTC",
            history: body.get("history"),
            history_cap: 1000,
            message: if compact { None } else { Some(message) },
            carry_orphan_tool: &carry,
            known_tool: &knows,
            project: project.map(|p| ProjectTurn {
                project: p,
                code_tasks: &[],
                files_block: r.files_block.as_deref(),
                not_ready: &not_ready,
                partial_reads: &r.partial_reads,
                skills: &[],
            }),
            trailing: &[],
        };
        let prepared = chat_prepare::prepare(&turn);
        let diary = body
            .get("spaceId")
            .as_str()
            .is_some_and(|s| s.starts_with("diary"))
            || (stored_id == retrieval::DIARY_EXTRAS_PROJECT && project.is_some());
        // rust-chat-native.cjs refusal's order: a Diary turn, then an injected name that is not text.
        let refusal = if diary {
            Some("diary")
        } else if retrieval::injected_not_text(&r.injected) {
            Some("injected-sources")
        } else {
            None
        };
        if let Some(why) = refusal {
            refused += 1;
            assert_eq!(case.get("refused").as_str(), Some(why), "{n}");
            let messages = prepared.unwrap_or_else(|e| panic!("{n}: {e:?}")).messages;
            d.check(
                n,
                &stringify(case.get("messages")),
                &stringify(&JValue::Arr(messages)),
            );
            continue;
        }
        let sources = match retrieval::frame_sources(&r) {
            Ok(s) => s,
            Err(Thrown) => {
                thrown += 1;
                assert_eq!(case.get("throws").as_str(), Some("TypeError"), "{n}");
                continue;
            }
        };
        frames += 1;
        let frame = case.get("frame");
        assert!(frame.is_object(), "{n}: {}", stringify(case));
        let messages = prepared
            .unwrap_or_else(|e: PrepareError| panic!("{n}: {e:?}"))
            .messages;
        d.check(
            n,
            &stringify(frame.get("messages")),
            &stringify(&JValue::Arr(messages)),
        );
        d.check(
            &format!("{n} sources"),
            &stringify(frame.get("sources")),
            &stringify(&sources),
        );
        let warning = project.map_or(JValue::Null, |_| {
            JValue::Str(retrieval::coverage_warning(&r.partial_reads))
        });
        d.check(
            &format!("{n} coverageWarning"),
            &stringify(frame.get("coverageWarning")),
            &stringify(&warning),
        );
    }
    assert!(
        images_kept.is_empty(),
        "differences not reproduced: {images_kept:?}"
    );
    assert!(kept >= 6, "only {kept} turns asked for images");
    assert!(frames >= 130, "only {frames} frames");
    assert!(thrown >= 3, "only {thrown} throwing turns");
    assert!(refused >= 4, "only {refused} refused turns");
    d.done("turns", 500);
}

#[test]
fn the_named_difference_is_the_images_request() {
    let f = fixture();
    let differences = list(&f, "differences");
    assert!(!differences.is_empty());
    for d in differences {
        assert_eq!(
            d.get("kind").as_str(),
            Some("retrieved_images"),
            "{}",
            name(d)
        );
        // Node handed such turns to the executor when retrieval found no images (the stub finds none).
        let turn = list(&f, "turns")
            .iter()
            .find(|t| t.get("name") == d.get("name"))
            .unwrap();
        assert_eq!(
            d.get("nodeFrame").as_bool(),
            Some(turn.get("frame").is_object())
        );
    }
}

/// The recording reaches the branches that matter, so a narrower recording cannot pass silently.
#[test]
fn the_recording_covers_every_branch() {
    let f = fixture();
    let all = |key: &str| {
        list(&f, key)
            .iter()
            .map(stringify)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let (turns, build) = (all("turns"), all("build"));
    for needle in [
        "\"seam\":\"settle\"",
        "\"attached\":true",
        "\"images\":true,\"onImages\":\"function\"",
        "\"ended\":true",
        "\"refused\":\"diary\"",
        "\"refused\":\"injected-sources\"",
        "\"throws\":\"Error\"",
        "\"throws\":\"TypeError\"",
        "\"sourcesEvent\":[",
        "Source text was truncated for this reply",
        "none of it (it was not included)",
        "Only part of \\\"QA ext big.txt\\\" was read",
        "is waiting to be read",
        "but they were not finished processing in time",
        "Relevant knowledge-file excerpts for this message:",
        "QA attached excerpt\\n\\nQA question",
        "\"item\":null",
        "\"page\":3",
    ] {
        assert!(turns.contains(needle), "turns lack {needle}");
    }
    let partial = all("partial");
    for needle in [
        "{\"name\":\"QA gone.txt\",\"length\":7000,\"read\":10}",
        "{\"name\":\"QA list.txt\",\"length\":6000,\"read\":20}",
        "{\"name\":\"QA stale big.txt\",\"length\":90000,\"read\":0}",
        "{\"name\":\"QA c.txt\",\"length\":5014,\"read\":50}",
    ] {
        assert!(partial.contains(needle), "partial lacks {needle}");
    }
    assert!(turns.contains("\\\"QA list ref.txt\\\" is 5,018 characters long"));
    for needle in [
        "\"page\":100000",
        "\"score\":0.123}",
        "\"score\":1.001}",
        "\"score\":0}",
        "…\",\"kind\"",
        "{\"throws\":\"TypeError\"}",
    ] {
        assert!(build.contains(needle), "build lacks {needle}");
    }
}
