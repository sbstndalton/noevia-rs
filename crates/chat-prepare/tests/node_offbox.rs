//! The off-box wire changes (slice P5d, part 2) replayed against `fixtures/node-offbox.v1.json`,
//! recorded from noevia-core by `record_offbox.cjs`: whole free-chat turns through
//! `createChatHandler`. Each turn's wire is prepared with [`chat_prepare::prepare`], its route
//! target is the one the recorded routing log entry names, and [`OffBox`] is driven with the
//! recorded seams; the request Node sent must match byte for byte.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashSet;

use chat_prepare::history::{self, Entry};
use chat_prepare::modes::RouteTarget;
use chat_prepare::offbox::{
    settle, with_system_note, OffBox, OffBoxTurn, Reply, Seam, Settled, Step,
    SKILL_INDEX_LEAD_NO_READER,
};
use chat_prepare::skills::SKILL_INDEX_LEAD;
use chat_prepare::time::{self, FixedNow};
use chat_prepare::vision::CODE_ROUTE_NOTE;
use chat_prepare::{account, js, memory, PartKind, Prepared, Turn};
use js_json::{wtf16, JValue};

const FIXTURE: &str = include_str!("fixtures/node-offbox.v1.json");
const MODES_FIXTURE: &str = include_str!("fixtures/node-modes.v1.json");
const EFFORT: &str = "Answer directly and concisely; skip step-by-step reasoning.";
const PIN: &str = "2026-11-13T23:30:00Z";

fn fixture() -> JValue {
    wtf16::parse(FIXTURE).expect("the fixture is JSON")
}

fn items<'a>(v: &'a JValue, key: &str) -> &'a [JValue] {
    match v.get(key) {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

fn names(v: &JValue) -> HashSet<String> {
    match v {
        JValue::Arr(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => HashSet::new(),
    }
}

fn strings(v: &JValue) -> Vec<String> {
    match v {
        JValue::Arr(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

fn json(v: &JValue) -> String {
    wtf16::stringify(v).unwrap()
}

fn at(v: &JValue, k: usize) -> &JValue {
    match v {
        JValue::Arr(a) => a.get(k).unwrap_or(&JValue::Undefined),
        _ => &JValue::Undefined,
    }
}

fn name(t: &JValue) -> &str {
    t.get("name").as_str().unwrap()
}

/// The routing log entry of P5d's first part (the first `log` seam), if routing modes ran.
fn route_entry(t: &JValue) -> Option<&JValue> {
    items(t, "calls")
        .iter()
        .find(|c| at(c, 0).as_str() == Some("log"))
        .map(|c| at(c, 1))
}

/// `routeTarget` as chat.cjs builds it from the decision (~984-989), read back from the case.
fn target_of(t: &JValue, providers: &JValue) -> Option<RouteTarget> {
    let entry = route_entry(t)?;
    let spec = t.get("spec");
    let settings = spec.get("modes").get("settings");
    let route = if entry.get("route").as_str() == Some("cloud") {
        "cloud"
    } else {
        "local"
    };
    let reason = entry.get("reason").clone();
    let flags = &items(spec, "freeChats")[0];
    let choice = spec.get("modes").get("choice");
    let remembered_cloud =
        choice.get("remember").truthy() && choice.get("choice").as_str() == Some("cloud");
    let granted = flags.get("allowCloud") == &JValue::Bool(true)
        || remembered_cloud
        || reason.as_str() == Some("user-choice");
    let cloud = settings.get("cloud").clone();
    let row = (route == "cloud").then(|| {
        providers
            .get(cloud.get("providerId").as_str().unwrap())
            .clone()
    });
    Some(RouteTarget {
        route,
        reason,
        detail: None,
        cloud,
        row,
        hybrid_ungranted: settings.get("mode").as_str() == Some("hybrid")
            && route == "cloud"
            && !granted,
    })
}

/// chat.cjs ~1115-1119: the classifier's role, or smart/fast when the account has no such role.
fn routed_role(spec: &JValue) -> String {
    let roles = spec.get("roles");
    let wanted = spec
        .get("classification")
        .get("role")
        .as_str()
        .unwrap_or("fast");
    if roles.get(wanted).truthy() {
        wanted.to_string()
    } else if roles.get("smart").truthy() {
        "smart".to_string()
    } else {
        "fast".to_string()
    }
}

struct Replayed {
    settled: Box<Settled>,
    asked: Vec<Seam>,
}

fn prepare(
    t: &JValue,
    carry: &(dyn Fn(&str) -> bool + Sync),
    known: &(dyn Fn(&str) -> bool + Sync),
) -> Prepared {
    let spec = t.get("spec");
    let body = t.get("body");
    let instructions = account::read(None);
    let memory = memory::read(None);
    let now = time::fixed_now(Some(PIN));
    assert!(matches!(now, FixedNow::Pinned { .. }));
    let brain = spec.get("brain").as_str().map(String::from);
    let trailing: Vec<(PartKind, String)> = brain
        .into_iter()
        .map(|b| (PartKind::Other("brain"), b))
        .collect();
    let compact = body.get("compactOnly").truthy();
    chat_prepare::prepare(&Turn {
        instructions: &instructions,
        memory: &memory,
        now_ms: now.now(0),
        time_zone: spec.get("tz").as_str().unwrap(),
        history: body.get("history"),
        history_cap: 1000,
        message: if compact {
            None
        } else {
            body.get("message").as_str()
        },
        carry_orphan_tool: carry,
        known_tool: known,
        project: None,
        trailing: &trailing,
    })
    .unwrap_or_else(|e| panic!("{}: prepare {e:?}", name(t)))
}

fn replay(t: &JValue, f: &JValue) -> Replayed {
    let facts = f.get("facts");
    let (public, known) = (names(facts.get("public")), names(facts.get("known")));
    let carry = |n: &str| public.contains(n);
    let knows = |n: &str| known.contains(n);
    let spec = t.get("spec");
    let body = t.get("body");
    let providers = facts.get("providers");
    let prepared = prepare(t, &carry, &knows);
    let mapped: Vec<Entry> = history::map_history(body.get("history"), 1000);
    let target = target_of(t, providers);
    let external = items(t, "external");
    let provider = match target.as_ref().and_then(RouteTarget::provider) {
        Some(row) => row,
        None => providers.get("default").clone(),
    };
    let role = routed_role(spec);
    let brain = spec.get("brain").as_str().unwrap_or("");
    let turn = OffBoxTurn {
        wants_auto: true,
        routed_role: Some(&role),
        compact_only: body.get("compactOnly").truthy(),
        default_provider_id: "default",
        brain_block: brain,
        now_ms: time::fixed_now(Some(PIN)).now(0),
        time_zone: spec.get("tz").as_str().unwrap(),
        replay_source: &mapped,
        message: body.get("message").as_str().unwrap_or(""),
        carry_orphan_tool: &carry,
        known_tool: &knows,
        project_files: false,
    };
    let first_external = at(&external[0], 1) == &JValue::Bool(true);
    let machine = OffBox::new(turn, prepared, target, provider, first_external);
    let wanted = strings(body.get("turnToolboxes"))
        .iter()
        .any(|id| id == "qa-notes");
    let mut asked = Vec::new();
    let mut provider_now: Option<(JValue, bool)> = None;
    let settled = settle(machine, |seam| {
        asked.push(seam.clone());
        match seam {
            Seam::Wanted => Reply::Wanted(wanted),
            Seam::AutoRoles => Reply::Roles(spec.get("roles").clone()),
            Seam::DefaultProvider => {
                // The move's external answer is the second one Node recorded.
                let row = providers.get("default").clone();
                let ext = at(&external[1], 1) == &JValue::Bool(true);
                provider_now = Some((row.clone(), ext));
                Reply::Provider { row, external: ext }
            }
            Seam::Log(_) | Seam::Line(_) => Reply::Done,
            // No Inference machines: the provider stays as it is.
            Seam::Settled => Reply::Provider {
                row: provider_now.clone().map_or_else(
                    || match target_of(t, providers)
                        .as_ref()
                        .and_then(RouteTarget::provider)
                    {
                        Some(row) => row,
                        None => providers.get("default").clone(),
                    },
                    |p| p.0,
                ),
                external: provider_now.as_ref().map_or(first_external, |p| p.1),
            },
            Seam::Boxes { .. } => Reply::Boxes(strings(t.get("egressBoxes"))),
        }
    })
    .expect("every seam answered in order");
    Replayed { settled, asked }
}

#[test]
fn the_recorded_turns_send_what_node_sent() {
    let f = fixture();
    let turns = items(&f, "turns");
    assert!(turns.len() >= 80, "{} turns", turns.len());
    let (mut moved, mut dropped, mut refiltered, mut stripped) = (0, 0, 0, 0);
    let (mut compact, mut refused) = (0, 0);
    for t in turns {
        let n = name(t);
        assert!(t.get("throws").is_nullish(), "{n}: threw");
        if !t.get("refused").is_nullish() {
            refused += 1; // refused by P5d's first part, before anything here runs
            continue;
        }
        let reply = t.get("reply");
        let Replayed { settled, asked } = replay(t, &f);
        assert_eq!(
            settled.boxes,
            strings(t.get("routedBoxes")),
            "{n}: the boxes"
        );
        stripped += usize::from(settled.private_stripped);
        if reply.is_nullish() {
            // compactOnly: the summary request is a later slice; the boxes are what is recorded.
            assert!(t.get("body").get("compactOnly").truthy(), "{n}: no reply");
            compact += 1;
            continue;
        }
        // The effort prefix (effort.rs) is put in front of the wire later, for a model that takes it.
        let sent = match reply.get("messages") {
            JValue::Arr(a)
                if a.first().map(|m| m.get("content").as_str()) == Some(Some(EFFORT)) =>
            {
                &a[1..]
            }
            JValue::Arr(a) => &a[..],
            _ => panic!("{n}: no messages"),
        };
        let mut got = match settled.round_note() {
            Some(note) => with_system_note(&settled.prepared.messages, note),
            None => settled.prepared.messages.clone(),
        };
        if routed_role(t.get("spec")) == "code" {
            // chat.cjs ~1478 (#1604), a later step: the Code route's note after every system message.
            for m in &mut got {
                if let (Some("system"), Some(c)) =
                    (m.get("role").as_str(), m.get("content").as_str())
                {
                    *m = js::message("system", format!("{c}\n\n{CODE_ROUTE_NOTE}"));
                }
            }
        }
        assert_eq!(
            json(&JValue::Arr(got)),
            json(&JValue::Arr(sent.to_vec())),
            "{n}: the wire"
        );
        let offered = names(reply.get("tools"));
        assert_eq!(
            offered.contains("qa_notes_search"),
            settled.boxes.iter().any(|b| b == "qa-notes"),
            "{n}: the private box offered"
        );
        let meta_routing = t.get("meta").get("routing");
        let move_line =
            "[routing] mode=hybrid route=local reason=sensitive-rule flag=private-tools";
        let lines = strings(t.get("lines"));
        if settled.moved {
            moved += 1;
            let target = settled.target.as_ref().unwrap();
            assert_eq!(
                json(&target.meta_routing()),
                json(meta_routing),
                "{n}: meta.routing"
            );
            assert_eq!(
                settled.model.as_ref(),
                Some(reply.get("model")),
                "{n}: the model"
            );
            assert_eq!(
                reply.get("url").as_str(),
                Some("http://qa-default.invalid/v1/chat/completions"),
                "{n}: url"
            );
            assert!(lines.iter().any(|l| l == move_line), "{n}: the line");
            let logs: Vec<String> = items(t, "calls")
                .iter()
                .filter(|c| at(c, 0).as_str() == Some("log"))
                .map(|c| json(at(c, 1)))
                .collect();
            let want = asked.iter().find_map(|s| match s {
                Seam::Log(e) => Some(json(e)),
                _ => None,
            });
            assert_eq!(logs.get(1), want.as_ref(), "{n}: the move's log entry");
        } else {
            assert!(!lines.iter().any(|l| l == move_line), "{n}: no move line");
            assert!(
                meta_routing.get("detail").as_str() != Some("private-tools"),
                "{n}: not moved"
            );
        }
        dropped += usize::from(settled.brain_dropped);
        refiltered += usize::from(
            asked
                .iter()
                .any(|s| matches!(s, Seam::Boxes { local: false })),
        );
    }
    // The recording reaches every branch.
    assert!(
        moved >= 5 && dropped >= 5 && refiltered >= 10 && stripped >= 2 && compact == 2 && refused <= 8,
        "moved {moved}, dropped {dropped}, off-box {refiltered}, stripped {stripped}, compact {compact}, refused {refused}"
    );
}

#[test]
fn the_recording_reads_the_same_node_files_as_the_modes_recording() {
    let f = fixture();
    let modes = wtf16::parse(MODES_FIXTURE).unwrap();
    let (JValue::Obj(ours), shared) = (f.get("sources"), modes.get("sources")) else {
        panic!("no sources");
    };
    assert!(ours.iter().any(|(k, _)| k == "server/chat.cjs"));
    for (file, sha) in ours {
        if !shared.get(file).is_nullish() {
            assert_eq!(shared.get(file), sha, "{file}");
        }
    }
    assert_eq!(items(&f, "differences").len(), 0);
}

fn bare_prepared(system: &str, history: Vec<JValue>) -> Prepared {
    let mut parts = chat_prepare::SystemParts::default();
    parts.push(PartKind::Time, system.to_string());
    let mut messages = vec![js::message("system", system.to_string())];
    messages.extend(history.iter().cloned());
    Prepared {
        parts,
        time_part: "QA-time".to_string(),
        system: system.to_string(),
        history,
        messages,
    }
}

#[test]
fn a_still_ungranted_cloud_route_takes_the_reader_out_of_every_message() {
    let no = |_: &str| false;
    let sys = format!("QA-time\n\n{SKILL_INDEX_LEAD}\nQA skill. read_project_file with an offset can read more of a file.");
    let hist = vec![js::message("user", format!("QA {SKILL_INDEX_LEAD} QA"))];
    let target = RouteTarget {
        route: "cloud",
        reason: JValue::from("clear"),
        detail: None,
        cloud: JValue::Null,
        row: Some(JValue::obj([("id", JValue::from("qa-cloud"))])),
        hybrid_ungranted: true,
    };
    let turn = OffBoxTurn {
        wants_auto: true,
        routed_role: Some("fast"),
        compact_only: false,
        default_provider_id: "default",
        brain_block: "",
        now_ms: 0,
        time_zone: "UTC",
        replay_source: &[],
        message: "QA",
        carry_orphan_tool: &no,
        known_tool: &no,
        project_files: true,
    };
    let row = target.provider().unwrap();
    let machine = OffBox::new(
        turn,
        bare_prepared(&sys, hist),
        Some(target),
        row.clone(),
        true,
    );
    let mut asked = Vec::new();
    let settled = settle(machine, |seam| {
        asked.push(seam.clone());
        match seam {
            Seam::Wanted => Reply::Wanted(true),
            // No local model: the turn stays on its route and the boxes are stripped.
            Seam::AutoRoles => Reply::Roles(JValue::obj([("smart", JValue::from(""))])),
            Seam::Settled => Reply::Provider {
                row: row.clone(),
                external: true,
            },
            Seam::Boxes { .. } => {
                Reply::Boxes(vec!["core".into(), "qa-notes".into(), "web-search".into()])
            }
            other => panic!("{other:?}"),
        }
    })
    .unwrap();
    assert!(!settled.moved && settled.private_stripped);
    assert_eq!(
        settled.boxes,
        vec!["core".to_string(), "web-search".to_string()]
    );
    let system = &settled.prepared.system;
    assert!(
        system.contains(SKILL_INDEX_LEAD_NO_READER)
            && !system.contains("read_project_file with an offset")
    );
    assert_eq!(
        settled.prepared.messages[0].get("content").as_str(),
        Some(system.as_str())
    );
    // The current message keeps its text but loses the lead, as every wire message does.
    let last = settled.prepared.messages[1]
        .get("content")
        .as_str()
        .unwrap();
    assert_eq!(last, format!("QA {SKILL_INDEX_LEAD_NO_READER} QA"));
    assert!(asked.contains(&Seam::Boxes { local: false }));
}

#[test]
fn a_reply_out_of_order_settles_nothing() {
    let no = |_: &str| false;
    let turn = OffBoxTurn {
        wants_auto: true,
        routed_role: Some("fast"),
        compact_only: false,
        default_provider_id: "default",
        brain_block: "",
        now_ms: 0,
        time_zone: "UTC",
        replay_source: &[],
        message: "QA",
        carry_orphan_tool: &no,
        known_tool: &no,
        project_files: false,
    };
    let local = JValue::obj([("id", JValue::from("default"))]);
    let mut m = OffBox::new(
        turn,
        bare_prepared("QA-time", vec![]),
        None,
        local.clone(),
        false,
    );
    assert_eq!(m.step(Reply::Wanted(true)), Step::OutOfOrder);
    let mut m = OffBox::new(
        turn,
        bare_prepared("QA-time", vec![]),
        None,
        local.clone(),
        false,
    );
    // On the box with no target: no move question, straight to the settled provider.
    assert_eq!(m.step(Reply::Start), Step::Ask(Seam::Settled));
    assert_eq!(m.step(Reply::Done), Step::OutOfOrder);
}
