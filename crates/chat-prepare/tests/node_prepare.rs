//! The recorded Node output (`fixtures/node-prepare.v1.json`, written by `record_prepare.cjs`):
//! every account part, memory part, fixed-now reading, time part, replayed history and whole
//! no-project turn must be the bytes noevia-core produced. The fixture is read with
//! `js_json::wtf16`, so lone surrogates reach the port as Node held them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use chat_prepare::history::{self, Entry, ReplayOptions};
use chat_prepare::time::{self, FixedNow};
use chat_prepare::{account, memory, PartKind, Turn};
use js_json::{wtf16, JValue};
use std::collections::HashSet;

const FIXTURE: &str = include_str!("fixtures/node-prepare.v1.json");

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

fn opt_str(v: &JValue) -> Option<String> {
    v.as_str().map(str::to_string)
}

fn names(v: &JValue) -> HashSet<String> {
    match v {
        JValue::Arr(items) => items.iter().filter_map(opt_str).collect(),
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

fn shown(part: Option<String>) -> String {
    stringify(&part.map_or(JValue::Null, JValue::Str))
}

#[test]
fn the_fixture_names_its_node_sources() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    for file in [
        "server/account-instructions.cjs",
        "server/account-memory.cjs",
        "server/current-time-part.cjs",
        "server/chat.cjs",
        "server/chat-sources.cjs",
        "server/prompt-framing.cjs",
        "server/tools-mode.cjs",
        "server/wasm/dav-parse.wasm",
    ] {
        let sha = f.get("sources").get(file).as_str().unwrap();
        assert!(
            sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
            "{file}"
        );
    }
}

#[test]
fn account_records_and_parts_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in items(&f, "account") {
        let file = bytes(case.get("fileHex"));
        let record = account::read(file.as_deref());
        d.check(
            &format!("{} record", name(case)),
            case.get("record").as_str().unwrap(),
            &stringify(&record),
        );
        let part = account::part(&record).expect("a read record never throws");
        d.check(
            &format!("{} part", name(case)),
            &stringify(case.get("part")),
            &shown(part),
        );
    }
    d.done("account", 150);
}

#[test]
fn account_parts_of_raw_settings_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in items(&f, "accountRaw") {
        let got = match account::part(case.get("settings")) {
            Ok(part) => shown(part),
            Err(_) => "throws TypeError".into(),
        };
        let want = match case.get("throws").as_str() {
            Some(kind) => format!("throws {kind}"),
            None => stringify(case.get("part")),
        };
        d.check(name(case), &want, &got);
    }
    d.done("accountRaw", 10);
}

#[test]
fn memory_records_and_parts_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in items(&f, "memory") {
        let file = bytes(case.get("fileHex"));
        let record = memory::read(file.as_deref());
        d.check(
            &format!("{} record", name(case)),
            case.get("record").as_str().unwrap(),
            &stringify(&record),
        );
        let part = memory::part(&record, case.get("projectMemories"));
        d.check(
            &format!("{} part", name(case)),
            &stringify(case.get("part")),
            &shown(part),
        );
    }
    d.done("memory", 40);
}

#[test]
fn fixed_now_reads_the_knob_as_node_does() {
    let f = fixture();
    let mut d = Diffs::default();
    // current-time-part.cjs warns once per value, for the life of the process.
    let mut warned = HashSet::new();
    for case in items(&f, "fixedNow") {
        let raw = opt_str(case.get("raw"));
        let read = time::fixed_now(raw.as_deref());
        let pinned = match &read {
            FixedNow::Pinned { ms, .. } => JValue::from(*ms),
            _ => JValue::Null,
        };
        let value = match &read {
            FixedNow::Pinned { value, .. } | FixedNow::Ignored { value } => Some(value.clone()),
            FixedNow::Unset => None,
        };
        let warnings: Vec<JValue> = match (value, read.warning()) {
            (Some(v), Some(w)) if warned.insert(v.clone()) => vec![JValue::Str(w)],
            _ => Vec::new(),
        };
        let what = format!("{raw:?}");
        d.check(&what, &stringify(case.get("pinned")), &stringify(&pinned));
        d.check(
            &format!("{what} warnings"),
            &stringify(case.get("warnings")),
            &stringify(&JValue::Arr(warnings)),
        );
    }
    d.done("fixedNow", 60);
}

#[test]
fn time_parts_are_nodes() {
    let f = fixture();
    let mut d = Diffs::default();
    for case in items(&f, "time") {
        let ms = case.get("ms").as_num().unwrap() as i64;
        let zone = case.get("zone").as_str().unwrap();
        let zone_name = case.get("zoneName").as_bool().unwrap();
        let got = time::part(ms, zone, zone_name);
        d.check(name(case), &stringify(case.get("part")), &shown(got));
    }
    d.done("time", 300);
}

fn entry(v: &JValue) -> Entry {
    Entry {
        role: v.get("role").as_str().unwrap().to_string(),
        content: v.get("content").as_str().unwrap().to_string(),
        name: opt_str(v.get("name")),
        applied: v.get("applied") == &JValue::Bool(true),
        declined: v.get("declined") == &JValue::Bool(true),
        target: opt_str(v.get("target")),
        args: opt_str(v.get("args")),
    }
}

#[test]
fn replayed_histories_are_nodes() {
    let f = fixture();
    let tools = f.get("replayTools");
    let (public, known) = (names(tools.get("public")), names(tools.get("known")));
    let carry = |n: &str| public.contains(n);
    let knows = |n: &str| known.contains(n);
    let mut d = Diffs::default();
    for case in items(&f, "replay") {
        let mapped: Vec<Entry> = items(case, "mapped").iter().map(entry).collect();
        let opts = ReplayOptions {
            carry_orphan_tool: &carry,
            off_box: case.get("offBox") == &JValue::Bool(true),
            known_tool: &knows,
        };
        let message = opt_str(case.get("newMessage"));
        let got = history::normalize(&mapped, message.as_deref(), &opts);
        d.check(
            name(case),
            &stringify(case.get("out")),
            &stringify(&JValue::Arr(got)),
        );
    }
    d.done("replay", 300);
}

#[test]
fn whole_turns_without_a_project_are_nodes() {
    let f = fixture();
    let tools = f.get("turnTools");
    let (public, known) = (names(tools.get("public")), names(tools.get("known")));
    let carry = |n: &str| public.contains(n);
    let knows = |n: &str| known.contains(n);
    let mut d = Diffs::default();
    for case in items(&f, "turns") {
        let instructions = account::read(bytes(case.get("instructionsHex")).as_deref());
        let memory = memory::read(bytes(case.get("memoryHex")).as_deref());
        let pin = opt_str(case.get("pin"));
        let now = time::fixed_now(pin.as_deref());
        assert!(matches!(now, FixedNow::Pinned { .. }), "{}", name(case));
        let turn = Turn {
            instructions: &instructions,
            memory: &memory,
            now_ms: now.now(0),
            time_zone: case.get("zone").as_str().unwrap(),
            history: case.get("history"),
            history_cap: case.get("cap").as_num().unwrap() as usize,
            message: case.get("message").as_str(),
            carry_orphan_tool: &carry,
            known_tool: &knows,
            trailing: &[],
        };
        let prepared = chat_prepare::prepare(&turn).expect("prepared");
        d.check(
            name(case),
            &stringify(case.get("messages")),
            &stringify(&JValue::Arr(prepared.messages.clone())),
        );
        // The trailing parts (P4's frame and brain blocks) go after everything else, in order.
        let trailing = [
            (PartKind::Other("qa frame"), "QA frame block".to_string()),
            (PartKind::Other("qa empty"), String::new()),
            (PartKind::Other("qa brain"), "QA brain block".to_string()),
        ];
        let more = chat_prepare::prepare(&Turn {
            trailing: &trailing,
            ..turn
        })
        .expect("prepared");
        assert_eq!(
            more.system,
            format!("{}\n\nQA frame block\n\nQA brain block", prepared.system),
            "{}",
            name(case)
        );
        assert_eq!(more.parts.parts().len(), prepared.parts.parts().len() + 2);
        assert_eq!(
            prepared.parts.get(PartKind::Time),
            Some(prepared.time_part.as_str())
        );
        assert!(prepared.system.contains(&prepared.time_part));
    }
    d.done("turns", 100);
}

/// The IANA release each side's zone data is: Node's ICU (recorded) and chrono-tz's bundled copy.
/// They differ, and the zone-day differences below were derived for exactly this pair: when either
/// side moves, this fails, and the recording and `ZONE_DATA_DIFFERENCES` must be redone.
const NODE_TZDB: &str = "2026c";
const PORT_TZDB: &str = "2025b";

#[test]
fn the_zone_data_releases_are_the_pair_the_differences_were_recorded_for() {
    let f = fixture();
    let node = f.get("tz").as_str().unwrap();
    assert_eq!(
        (node, chrono_tz::IANA_TZDB_VERSION),
        (NODE_TZDB, PORT_TZDB),
        "zone data moved (Node's ICU tzdb {node}, chrono-tz {}): re-record and redo ZONE_DATA_DIFFERENCES",
        chrono_tz::IANA_TZDB_VERSION
    );
}

/// Where chrono-tz's data (2025b) and Node's ICU data (2026c) give a different local date near
/// midnight: (zone, cases, first ms, last ms) of the recorded edge cases that differ. Morocco: from
/// 2026-09-20T23:59Z to the end of the recording, Node's data is at +00 where chrono-tz's is at +01.
const ZONE_DATA_DIFFERENCES: &[(&str, usize, i64, i64)] =
    &[("Africa/Casablanca", 432, 1_789_948_740_000, 1_830_211_140_000)];

#[test]
fn local_dates_near_midnight_are_nodes_except_the_named_zone_data_differences() {
    let f = fixture();
    let mut found = Vec::new();
    let mut total = 0;
    for z in items(&f, "zoneDays") {
        let name = z.get("zone").as_str().unwrap();
        let zone = time::zone(name).expect("an IANA zone");
        let mut differ: Vec<i64> = Vec::new();
        for c in items(z, "cases") {
            let JValue::Arr(pair) = c else {
                panic!("{c:?}")
            };
            let ms = pair[0].as_num().unwrap() as i64;
            let want = pair[1].as_num().unwrap() as i64;
            let (y, m, d) = time::local_date(ms, zone).unwrap();
            total += 1;
            if y * 10000 + m * 100 + d != want {
                differ.push(ms);
            }
        }
        if let (Some(first), Some(last)) = (differ.first(), differ.last()) {
            found.push((name.to_string(), differ.len(), *first, *last));
        }
    }
    assert!(total >= 20_000, "only {total} zone days recorded");
    let expected: Vec<(String, usize, i64, i64)> = ZONE_DATA_DIFFERENCES
        .iter()
        .map(|(z, n, a, b)| ((*z).to_string(), *n, *a, *b))
        .collect();
    assert_eq!(found, expected, "the zone-data differences changed");
}

/// The named differences: input Node accepts that this port does not state as Node does. Node's
/// output is recorded (non-ASCII as \u escapes, in a string); the port's documented behaviour is
/// checked instead.
#[test]
fn the_named_differences_behave_as_documented() {
    let f = fixture();
    let differences = items(&f, "differences");
    assert!(differences.len() >= 4);
    for case in differences {
        assert!(case.get("node").as_str().is_some(), "{}", name(case));
        let file = bytes(case.get("fileHex"));
        match case.get("kind").as_str().unwrap() {
            "account" => {
                let record = account::read(file.as_deref());
                assert_eq!(record, account::empty(), "{}", name(case));
                assert_eq!(account::part(&record), Ok(None), "{}", name(case));
            }
            "memory" => {
                let record = memory::read(file.as_deref());
                assert_eq!(record, memory::empty(), "{}", name(case));
                assert_eq!(memory::part(&record, &JValue::Undefined), None);
            }
            "zone_id" => {
                let ms = case.get("ms").as_num().unwrap() as i64;
                let zone = case.get("zone").as_str().unwrap();
                let part = time::part(ms, zone, true).unwrap();
                assert!(part.contains(" (UTC).\n"), "{}", name(case));
            }
            "zone_data" => {
                let ms = case.get("ms").as_num().unwrap() as i64;
                let zone = case.get("zone").as_str().unwrap();
                assert!(time::part(ms, zone, true).is_some(), "{}", name(case));
            }
            "time" => {
                let ms = case.get("ms").as_num().unwrap() as i64;
                let zone = case.get("zone").as_str().unwrap();
                assert_eq!(time::part(ms, zone, true), None, "{}", name(case));
            }
            other => panic!("unknown difference kind {other}"),
        }
    }
}

/// The recording reaches the branches that matter, so a narrower recording cannot pass silently.
#[test]
fn the_recording_covers_every_branch() {
    let f = fixture();
    let all = |section: &str| -> String {
        items(&f, section)
            .iter()
            .map(stringify)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let replay = all("replay");
    let turns = all("turns");
    for needle in [
        "Already done earlier in this chat: (a change was applied with",
        "ran after the user approved it, and it succeeded.",
        "Not run earlier in this chat: the user did not approve",
        "(The reply to the message above ended before its answer.",
        "(No reply was kept for the message above",
        "\\u2026",
    ] {
        assert!(
            replay.contains(needle) || replay.contains(&needle.replace("\\u2026", "…")),
            "replay lacks {needle}"
        );
    }
    for needle in [
        "The user's custom instructions for all chats",
        "Things you know about the user",
        "Today is",
        "(the ISO year is not the calendar year",
        "Reply in Norwegian",
    ] {
        assert!(
            turns.contains(needle) || all("time").contains(needle),
            "turns lack {needle}"
        );
    }
}
