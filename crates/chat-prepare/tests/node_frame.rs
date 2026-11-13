//! The recorded Node output of slice P4a (`fixtures/node-frame.v1.json`, written by
//! `record_frame.cjs`): every normalized frame, stored frame, "Chat frame" block and tool-gate bias
//! must be what noevia-core produced, and every code point must reach the block's tags as it does
//! in Node.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chat_prepare::frame::{self, Frame, FrameTurn, Kind, Lookup, FRAME_PART};
use chat_prepare::{account, memory, PartKind, Turn};
use js_json::{wtf16, JValue};

const FIXTURE: &str = include_str!("fixtures/node-frame.v1.json");
const PROJECT_FIXTURE: &str = include_str!("fixtures/node-project.v1.json");

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

/// `JSON.stringify(v)`, with `null` for a missing value as the recorder's `JSON.stringify` of
/// `null` gives.
fn stringify(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "null".to_string())
}

fn opt(v: Option<JValue>) -> String {
    v.map_or_else(|| "null".to_string(), |v| stringify(&v))
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

fn lookup(input: &JValue) -> Lookup<'_> {
    Lookup {
        enabled: input.get("enabled").truthy(),
        chat_id: input.get("chatId").as_str(),
        project_id: input.get("projectId").as_str(),
        project: input.get("project"),
        free_chats: input.get("freeChats"),
    }
}

#[test]
fn the_fixture_names_its_node_sources() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    for file in [
        "server/chat-frame-steering.cjs",
        "server/chat-framing.cjs",
        "server/tool-router.cjs",
        "server/project-modes.cjs",
        "server/prompt-framing.cjs",
        "server/dav-parse-wasm.cjs",
        "server/dav-parse.lock",
        "server/wasm/dav-parse.wasm",
    ] {
        let sha = f.get("sources").get(file).as_str().unwrap();
        assert!(sha.len() == 64, "{file}");
    }
    // Recorded at the same core as the crate's other fixtures (tests/oracle-core.ref): the files
    // both read are the same bytes.
    let project = wtf16::parse(PROJECT_FIXTURE).unwrap();
    for file in ["server/prompt-framing.cjs", "server/wasm/dav-parse.wasm"] {
        assert_eq!(
            f.get("sources").get(file),
            project.get("sources").get(file),
            "{file}"
        );
    }
    // safeTag's NFKC and `\p{L}`/`\p{N}` are ICU4X's data (icu_normalizer and icu_properties 2.3,
    // Unicode 17.0); the sweep was recorded on the same Unicode release in Node's ICU.
    assert_eq!(f.get("unicode").as_str(), Some("17.0"));
    assert_eq!(f.get("maxBlockChars").as_num(), Some(400.0));
    // No difference from Node was found; a later recording that finds one must name it here and
    // in the module doc.
    assert!(items(&f, "differences").is_empty());
}

/// server-projects' `normalizeFrame` port is the one `storedFrame` uses here: it must be Node's
/// on every recorded frame.
#[test]
fn normalized_frames_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in items(&f, "normalize") {
        d.check(
            name(case),
            &stringify(case.get("out")),
            &opt(server_projects::frame::normalize(case.get("frame"))),
        );
    }
    d.done("normalize", 45);
}

#[test]
fn stored_frames_blocks_and_biases_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in items(&f, "stored") {
        let input = case.get("input");
        let found = frame::stored_frame(&lookup(input));
        d.check(
            &format!("{} frame", name(case)),
            &stringify(case.get("frame")),
            &opt(found.as_ref().map(Frame::to_value)),
        );
        let block = frame::frame_block(found.as_ref());
        d.check(
            &format!("{} block", name(case)),
            case.get("block").as_str().unwrap(),
            &block,
        );
        d.check(
            &format!("{} bias", name(case)),
            &stringify(case.get("bias")),
            &opt(frame::gate_bias(found.as_ref()).map(|b| b.to_value())),
        );
        // chat.cjs's call site gives the same frame for an ordinary turn, and the block as the part.
        let turn = FrameTurn {
            compact_only: false,
            diary_space: false,
            lookup: Some(lookup(input)),
        };
        let at_call = frame::chat_frame(&turn);
        assert_eq!(at_call, found, "{}", name(case));
        let part = frame::frame_part(at_call.as_ref());
        assert_eq!(
            part,
            (!block.is_empty()).then(|| (FRAME_PART, block.clone())),
            "{}",
            name(case)
        );
    }
    d.done("stored", 550);
}

#[test]
fn blocks_and_biases_of_given_frames_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in items(&f, "blocks") {
        let given = Frame::from_value(case.get("frame"))
            .unwrap_or_else(|| panic!("{}: not a frame", name(case)));
        assert_eq!(&given.to_value(), case.get("frame"), "{}", name(case));
        d.check(
            name(case),
            case.get("block").as_str().unwrap(),
            &frame::frame_block(Some(&given)),
        );
        d.check(
            name(case),
            &stringify(case.get("bias")),
            &opt(frame::gate_bias(Some(&given)).map(|b| b.to_value())),
        );
    }
    d.done("blocks", 1500);
}

fn char_of(cp: u32) -> char {
    wtf16::lone_char(u16::try_from(cp).unwrap_or(0))
        .or_else(|| char::from_u32(cp))
        .unwrap()
}

/// Every code point (lone surrogates included), alone and after `a`, reaches the block's tags as
/// it does in Node: unchanged, removed, or as the recorded text.
#[test]
fn every_code_point_reaches_the_tags_as_in_node() {
    const UNSET: u8 = 0;
    const KEPT: u8 = 1;
    const REMOVED: u8 = 2;
    const CHANGED: u8 = 3;
    let f = fixture();
    let sweeps = items(&f, "sweep");
    assert_eq!(sweeps.len(), 2);
    for sweep in sweeps {
        let prefix = sweep.get("prefix").as_str().unwrap();
        let mut class = vec![UNSET; 0x11_0000];
        let mut changed: Vec<Option<&str>> = vec![None; 0x11_0000];
        for (key, cls) in [("kept", KEPT), ("removed", REMOVED)] {
            for run in items(sweep, key) {
                let JValue::Arr(pair) = run else {
                    panic!("a run is a pair")
                };
                let lo = pair[0].as_num().unwrap() as usize;
                let hi = pair[1].as_num().unwrap() as usize;
                for c in &mut class[lo..=hi] {
                    assert_eq!(*c, UNSET, "{prefix:?}: a code point in two runs");
                    *c = cls;
                }
            }
        }
        for entry in items(sweep, "changed") {
            let JValue::Arr(pair) = entry else {
                panic!("a changed entry is a pair")
            };
            let cp = pair[0].as_num().unwrap() as usize;
            assert_eq!(class[cp], UNSET, "{prefix:?}: U+{cp:04X} twice");
            class[cp] = CHANGED;
            changed[cp] = pair[1].as_str();
        }
        let mut failed = Vec::new();
        for cp in 0..0x11_0000u32 {
            let s = format!("{prefix}{}", char_of(cp));
            let got = frame::safe_tag(&s);
            let at = cp as usize;
            let ok = match class[at] {
                KEPT => got == s,
                REMOVED => got == prefix,
                CHANGED => Some(got.as_str()) == changed[at],
                _ => panic!("{prefix:?}: U+{cp:04X} not recorded"),
            };
            if !ok {
                failed.push(format!(
                    "U+{cp:04X}: node class {} {:?}, rust {got:?}",
                    class[at], changed[at]
                ));
            }
        }
        assert!(
            failed.is_empty(),
            "{prefix:?}: {} code points differ:\n{}",
            failed.len(),
            failed[..failed.len().min(10)].join("\n")
        );
        // Why the categories are General_Category L/N: Node removes marks that Rust's
        // `char::is_alphanumeric` (Alphabetic, which includes many marks) would keep.
        if prefix.is_empty() {
            let marks_kept_by_alphanumeric = (0..0x11_0000u32)
                .filter(|&cp| class[cp as usize] == REMOVED)
                .filter_map(char::from_u32)
                .filter(|c| c.is_alphanumeric())
                .count();
            assert!(marks_kept_by_alphanumeric > 100);
        }
    }
}

/// The recording reaches the branches that matter, so a narrower recording cannot pass silently.
#[test]
fn the_recording_covers_every_branch() {
    let f = fixture();
    let stored = items(&f, "stored");
    let framed = |pred: &dyn Fn(&JValue) -> bool| stored.iter().filter(|c| pred(c)).count();
    assert!(framed(&|c| c.get("frame").get("inProject") == &JValue::Bool(true)) >= 20);
    assert!(framed(&|c| c.get("frame").get("inProject") == &JValue::Bool(false)) >= 20);
    assert!(framed(&|c| c.get("frame").is_nullish()) >= 100);
    for kind in Kind::ALL {
        let k = kind.as_str();
        assert!(
            framed(&|c| c.get("frame").get("kind").as_str() == Some(k)) >= 5,
            "{k}"
        );
    }
    for bias in [
        r#"{"prefer":["search","drive"],"hint":"The user framed this chat as a lookup."}"#,
        r#"{"noForce":true}"#,
        "a tool is expected",
    ] {
        assert!(
            stored
                .iter()
                .any(|c| stringify(c.get("bias")).contains(bias)),
            "{bias}"
        );
    }
    let all: String = [stored, items(&f, "blocks")]
        .concat()
        .iter()
        .map(|c| stringify(c.get("block")))
        .collect();
    for needle in [
        " Stay within this project and its sources.",
        " Stay within the topics tagged below.\\n<untrusted kind=\\\"chat tags\\\">",
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\ud801",
        "qa-notes",
        "fi, ffile, st, ff, IJ",
        "\u{e9}",
    ] {
        assert!(all.contains(needle), "blocks lack {needle}");
    }
    // The 400-unit trimming: a given frame whose six tags do not all fit.
    let trimmed = items(&f, "blocks").iter().any(|c| {
        let block = c.get("block").as_str().unwrap();
        // The line before `</untrusted>` holds the tags shown.
        let shown = block
            .lines()
            .rev()
            .nth(1)
            .map_or(0, |l| l.split(", ").count());
        name(c).starts_with("edge_")
            && block.contains("<untrusted")
            && shown < items(c.get("frame"), "tags").len().min(6)
    });
    assert!(trimmed);
    assert!(items(&f, "blocks")
        .iter()
        .any(|c| wtf16::len(c.get("block").as_str().unwrap()) > 390));
}

#[test]
fn the_call_site_rules_give_no_frame() {
    let f = fixture();
    let case = items(&f, "stored")
        .iter()
        .find(|c| name(c) == "project_list_only")
        .unwrap();
    let input = case.get("input");
    let base = FrameTurn {
        compact_only: false,
        diary_space: false,
        lookup: Some(lookup(input)),
    };
    assert!(frame::chat_frame(&base).is_some());
    for turn in [
        FrameTurn {
            compact_only: true,
            ..base
        },
        FrameTurn {
            diary_space: true,
            ..base
        },
        FrameTurn {
            lookup: None,
            ..base
        },
    ] {
        assert_eq!(frame::chat_frame(&turn), None);
    }
    assert_eq!(frame::frame_part(None), None);
}

/// The frame part is the last system part of a prepared turn, as chat.cjs pushes it.
#[test]
fn the_frame_part_ends_the_system_message() {
    let f = fixture();
    let case = items(&f, "stored")
        .iter()
        .find(|c| name(c) == "tags_duplicates_search")
        .unwrap();
    let found = frame::stored_frame(&lookup(case.get("input")));
    let part = frame::frame_part(found.as_ref()).unwrap();
    assert_eq!(part.0, PartKind::Other("frameBlock"));
    let instructions = account::read(None);
    let memory = memory::read(None);
    let never = |_: &str| false;
    let history = JValue::Arr(Vec::new());
    let trailing = [part.clone()];
    let turn = Turn {
        instructions: &instructions,
        memory: &memory,
        now_ms: 1_767_225_600_000,
        time_zone: "UTC",
        history: &history,
        history_cap: 1000,
        message: Some("QA question"),
        carry_orphan_tool: &never,
        known_tool: &never,
        project: None,
        trailing: &trailing,
    };
    let prepared = chat_prepare::prepare(&turn).unwrap();
    assert!(prepared
        .system
        .ends_with(&format!("\n\n{}", case.get("block").as_str().unwrap())));
    assert_eq!(prepared.parts.get(FRAME_PART), Some(part.1.as_str()));
}
