//! The recorded Node output of slice P3a (`fixtures/node-skills.v1.json`, written by
//! `record_skills.cjs`): every skill list, manifest, pin resolution, unmet requirement, skill
//! index, loaded and pinned block, refused turn and whole project turn with its skills must be the
//! bytes noevia-core produced.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chat_prepare::skills::{self, PinFailure, PinTurn, TypeError};
use chat_prepare::time::{self, FixedNow};
use chat_prepare::{account, memory, PrepareError, ProjectTurn, Turn};
use js_json::{wtf16, JValue};

const FIXTURE: &str = include_str!("fixtures/node-skills.v1.json");

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

fn strs(v: &JValue) -> Vec<String> {
    match v {
        JValue::Arr(items) => items
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect(),
        _ => panic!("not a list of strings: {v:?}"),
    }
}

/// `attempt(f)` as the recorder writes it: `{ out }` or `{ throws: { name, message, ... } }`.
fn attempted(r: Result<JValue, PinFailure>) -> String {
    let throws =
        |items: Vec<(&str, JValue)>| stringify(&JValue::obj([("throws", JValue::obj(items))]));
    match r {
        Ok(v) => stringify(&JValue::obj([("out", v)])),
        Err(PinFailure::Thrown(TypeError(m))) => {
            throws(vec![("name", "TypeError".into()), ("message", m.into())])
        }
        Err(PinFailure::Refused(e)) => throws(vec![
            ("name", "Error".into()),
            ("message", e.message.into()),
            ("status", JValue::from(u32::from(e.status))),
            ("code", e.code.into()),
        ]),
    }
}

fn thrown<T>(r: Result<T, TypeError>) -> Result<T, PinFailure> {
    r.map_err(PinFailure::Thrown)
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
            let cut = |s: &str| s.chars().take(1500).collect::<String>();
            self.failed.push(format!(
                "{what}\n  node: {:?}\n  rust: {:?}",
                cut(want),
                cut(got)
            ));
        }
    }
    fn done(self, section: &str, min: usize) {
        assert!(self.total >= min, "{section}: only {} cases", self.total);
        assert!(
            self.failed.is_empty(),
            "{section}: {} of {} differ:\n{}",
            self.failed.len(),
            self.total,
            self.failed[..self.failed.len().min(4)].join("\n")
        );
    }
}

#[test]
fn the_fixture_names_its_node_sources_and_the_crates_core_pin() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    let JValue::Obj(sources) = f.get("sources") else {
        panic!("no sources");
    };
    // Every server module the recording loaded, and the files it read.
    for file in [
        "server/chat.cjs",
        "server/index.cjs",
        "server/instruction-skills.cjs",
        "server/skill-index.cjs",
        "server/chat-skill-routing.cjs",
        "server/toolboxes-permitted.cjs",
        "server/project-docs-default.cjs",
        "server/source-text.cjs",
        "server/chat-context.cjs",
        "server/current-time-part.cjs",
        "server/account-instructions.cjs",
        "server/account-memory.cjs",
        "server/source-readability.cjs",
        "server/source-coverage.cjs",
        "server/shared-context.cjs",
        "server/quoted-name.cjs",
        "server/diary-extras.cjs",
        "server/reasoning-effort.cjs",
        "server/prompt-framing.cjs",
        "server/dav-parse-wasm.cjs",
        "server/dav-parse.lock",
        "server/wasm/dav-parse.wasm",
    ] {
        let sha = f.get("sources").get(file).as_str().unwrap_or_default();
        assert!(sha.len() == 64, "{file}");
    }
    // Every recording of the crate comes from the one core the CI checks out (oracle-core.ref):
    // a file the other recordings read too has the same sha.
    let mut shared = 0;
    for other in [
        include_str!("fixtures/node-prepare.v1.json"),
        include_str!("fixtures/node-project.v1.json"),
        include_str!("fixtures/node-frame.v1.json"),
    ] {
        let other = wtf16::parse(other).unwrap();
        for (file, sha) in sources {
            if let JValue::Str(theirs) = other.get("sources").get(file) {
                shared += 1;
                assert_eq!(sha.as_str(), Some(theirs.as_str()), "{file}");
            }
        }
    }
    assert!(shared >= 15, "only {shared} shared source files");
}

#[test]
fn skill_lists_manifests_and_project_toolboxes_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in items(&f, "projects") {
        let p = case.get("project");
        let known = strs(case.get("known"));
        let boxes = skills::project_toolbox_ids(p);
        d.check(
            &format!("{} boxes", name(case)),
            &stringify(case.get("boxes")),
            &stringify(&JValue::Arr(
                boxes.iter().map(|b| b.as_str().into()).collect(),
            )),
        );
        let list = thrown(skills::list(p))
            .map(|l| JValue::Arr(l.iter().map(skills::Skill::to_json).collect()));
        d.check(
            &format!("{} list", name(case)),
            &stringify(case.get("list")),
            &attempted(list),
        );
        let manifests = thrown(skills::manifests(p, &known))
            .map(|l| JValue::Arr(l.into_iter().map(|m| m.json).collect()));
        d.check(
            &format!("{} manifests", name(case)),
            &stringify(case.get("manifests")),
            &attempted(manifests),
        );
    }
    d.done("projects", 240);
}

#[test]
fn pinned_skills_resolve_and_refuse_as_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in items(&f, "pins") {
        let known = strs(case.get("known"));
        let got = skills::resolve_pinned(case.get("project"), case.get("raw"), &known).map(|x| {
            JValue::obj([
                ("record", x.record),
                ("content", JValue::Str(x.content)),
                ("manifest", JValue::Str(x.manifest.id)),
            ])
        });
        d.check(name(case), &stringify(case.get("result")), &attempted(got));
    }
    d.done("pins", 40);
}

#[test]
fn unmet_requirements_index_and_blocks_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in items(&f, "unmet") {
        let got = skills::unmet_requirements(case.get("requires"), case.get("selected"));
        d.check(
            &stringify(case),
            &stringify(case.get("out")),
            &stringify(&JValue::Arr(got)),
        );
    }
    let text = |r: Result<String, TypeError>| attempted(thrown(r).map(JValue::Str));
    for case in items(&f, "index") {
        let got = text(skills::format_skill_index(items(case, "skills")));
        d.check(name(case), &stringify(case.get("out")), &got);
    }
    for case in items(&f, "blocks") {
        let got = text(skills::skill_block(items(case, "skills")));
        d.check(name(case), &stringify(case.get("out")), &got);
    }
    for case in items(&f, "pinnedBlocks") {
        let got = text(skills::pinned_skill_block(
            case.get("record"),
            case.get("content"),
        ));
        d.check(name(case), &stringify(case.get("out")), &got);
    }
    d.done("unmet, index and blocks", 75);
}

#[test]
fn project_turns_with_skills_are_nodes() {
    let f = fixture();
    let (instructions, memory) = (account::read(None), memory::read(None));
    let now = time::fixed_now(Some("2026-11-13T12:00:00Z"));
    assert!(matches!(now, FixedNow::Pinned { .. }));
    let never = |_: &str| false;
    let mut d = Diffs::default();
    let (mut refused, mut threw, mut picked_some) = (0, 0, 0);
    for case in items(&f, "turns") {
        let project = Some(case.get("project")).filter(|p| !matches!(p, JValue::Null));
        let (known, carried) = (strs(case.get("known")), strs(case.get("carried")));
        let compact = case.get("compact") == &JValue::Bool(true);
        let turn = PinTurn {
            project,
            diary: false,
            compact,
            known: &known,
            carried: &carried,
        };
        let pinned = match skills::pin(&turn, case.get("skill")) {
            Ok(p) => p,
            Err(r) => {
                refused += 1;
                let got = JValue::obj([
                    ("status", JValue::from(u32::from(r.status))),
                    ("body", r.body),
                ]);
                d.check(
                    name(case),
                    &stringify(case.get("refusal")),
                    &stringify(&got),
                );
                continue;
            }
        };
        assert!(
            case.get("refusal").is_nullish(),
            "{}: Node refused",
            name(case)
        );
        let project = project.unwrap();
        let index = match skills::index(project, &known, &carried, pinned.is_some()) {
            Ok(i) => i,
            Err(TypeError(m)) => {
                threw += 1;
                assert_eq!(
                    case.get("throws").get("message").as_str(),
                    Some(m),
                    "{}",
                    name(case)
                );
                continue;
            }
        };
        let picked: Vec<skills::Skill> = match case.get("candidates") {
            JValue::Null => {
                assert!(index.candidates.is_empty(), "{}", name(case));
                Vec::new()
            }
            want => {
                let got = JValue::Arr(
                    index
                        .candidates
                        .iter()
                        .map(skills::Skill::to_json)
                        .collect(),
                );
                d.check(
                    &format!("{} candidates", name(case)),
                    &stringify(want),
                    &stringify(&got),
                );
                let pick = items(case, "pick")
                    .iter()
                    .map(|i| i.as_num().unwrap() as usize);
                pick.filter_map(|i| index.candidates.get(i).cloned())
                    .collect()
            }
        };
        picked_some += usize::from(!picked.is_empty());
        let parts = skills::parts(&index, &picked, pinned.as_ref()).unwrap();
        let prepared = chat_prepare::prepare(&Turn {
            instructions: &instructions,
            memory: &memory,
            now_ms: now.now(0),
            time_zone: "UTC",
            history: &JValue::Undefined,
            history_cap: 1000,
            message: if compact {
                None
            } else {
                case.get("message").as_str()
            },
            carry_orphan_tool: &never,
            known_tool: &never,
            project: Some(ProjectTurn {
                project,
                code_tasks: &[],
                files_block: None,
                not_ready: &[],
                partial_reads: &[],
                skills: &parts,
            }),
            trailing: &[],
        });
        match prepared {
            Ok(p) => d.check(
                name(case),
                &stringify(case.get("messages")),
                &stringify(&JValue::Arr(p.messages)),
            ),
            Err(e) => {
                assert_eq!(e, PrepareError::Thrown, "{}", name(case));
                threw += 1;
                assert!(
                    case.get("throws").get("name").as_str().is_some(),
                    "{}",
                    name(case)
                );
            }
        }
    }
    assert!(
        refused >= 12 && threw >= 2 && picked_some >= 10,
        "{refused} {threw} {picked_some}"
    );
    d.done("turns", 65);
}

/// projectToolboxIds goes through the port's own entry: a toolbox list whose JSON passes the
/// 4 MiB request cap carries no box at all, as core's dav-parse.wasm call refuses it.
#[test]
fn a_toolbox_list_over_the_ports_cap_carries_no_box() {
    let list = |n: usize| {
        let ids = (0..n).map(|i| JValue::from(format!("qa-box-{i:07}").as_str()));
        JValue::obj([("toolboxes", JValue::Arr(ids.collect()))])
    };
    // Each id is 17 bytes of JSON with its quotes and comma: 240,000 fit under 4 MiB, 250,000 do not.
    assert_eq!(skills::project_toolbox_ids(&list(240_000)).len(), 240_000);
    assert!(skills::project_toolbox_ids(&list(250_000)).is_empty());
}

#[test]
fn the_recording_covers_every_branch_and_names_no_difference() {
    let f = fixture();
    assert!(items(&f, "differences").is_empty());
    for needle in [
        "\"status\":\"enabled\"",
        "\"status\":\"disabled\"",
        "\"status\":\"updated\"",
        "\"status\":\"review\"",
        "\"status\":\"invalid\"",
        "Instruction skills must be Markdown files.",
        "Skill exceeds the 32 KiB limit.",
        "Use a closed frontmatter block of at most 2 KiB.",
        "Use one scalar name: value per frontmatter line.",
        "Unsupported metadata field: tags",
        "Duplicate metadata field: name",
        "Lists, mappings, multiline values",
        "Unclosed metadata quote.",
        "Metadata exceeds its length limit",
        "requires must be a comma-separated list",
        "allowed-tools lists at most 32 entries.",
        "\"kind\":\"attached-folder\"",
        "\"kind\":\"published\"",
        "\"executable\":true",
        "\"unselectedToolboxes\":[\"web-search\"",
        "\"unsupportedToolboxes\":[\"qa-missing\"]",
        "skill_requirements_unmet",
        "\"missing\":[\"web-search\",\"qa-box\"]",
        "skill_pin_requires_project",
        "Compaction does not accept a pinned Skill.",
        "skill_version_changed",
        "skill_hash_mismatch",
        "additional skill metadata entries omitted",
        "(Truncated; read qa-big/SKILL.md with read_project_file for the rest.)",
        "matches this message; follow it:",
        "The user invoked skill",
        "Cannot read properties of null (reading 'content')",
        "Cannot read properties of null (reading 'file')",
        "Cannot read properties of null (reading 'name')",
        "Cannot read properties of undefined (reading 'name')",
        "{\"name\":\"helper.md\",\"textRef\":",
        "This skill is invalid: Use a closed frontmatter block of at most 2 KiB.",
        "(project.files || []).map is not a function",
    ] {
        assert!(FIXTURE.contains(needle), "the recording lacks {needle}");
    }
}
