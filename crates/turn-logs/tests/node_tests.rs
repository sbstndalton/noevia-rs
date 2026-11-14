//! core server/decision-log.test.cjs and server/context-log.test.cjs, case for case, plus the
//! edges the port decides (bytes bound, `null` lines, `__proto__` keys, JS key order).
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use js_json::{wtf16, JValue};
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use turn_logs::context::{self, Round};
use turn_logs::decision::{self, DecisionLog};

fn j(text: &str) -> JValue {
    wtf16::parse(text).unwrap()
}

fn s(v: &JValue) -> String {
    wtf16::stringify(v).unwrap()
}

fn fixed_log(dir: &std::path::Path, max: u64) -> DecisionLog {
    DecisionLog::new(dir)
        .with_max_bytes(max)
        .with_clock(Box::new(|| "2026-09-22T08:00:00.000Z".into()))
        .with_echo(None)
}

// decision-log.test.cjs: 'appends text-free lines, rotates at the bound, and never throws'
#[test]
fn decision_appends_rotates_and_never_throws() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let log = fixed_log(dir, 300);
    let entry = j(r#"{"selected":"smart","options":3,"margin":0.3,"ms":500,"fellBack":null}"#);
    for _ in 0..10 {
        log.record("route", &entry);
    }
    let file = dir.join("system-one-decisions.jsonl");
    let current = fs::read_to_string(&file).unwrap();
    assert!(dir.join("system-one-decisions.jsonl.1").exists(), "rotated");
    assert!(fs::metadata(&file).unwrap().len() <= 300);
    let first = current.trim().split('\n').next().unwrap();
    assert_eq!(
        first,
        r#"{"at":"2026-09-22T08:00:00.000Z","kind":"route","selected":"smart","options":3,"margin":0.3,"ms":500,"fellBack":null}"#
    );
    assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    // unwritable: silently skipped
    DecisionLog::new(&dir.join("missing").join("deeper")).with_echo(None).record("route", &j("{}"));
    assert!(!dir.join("missing").exists());
}

// decision-log.test.cjs: 'summary counts selections, fallbacks and margin buckets'
#[test]
fn decision_summary_counts_selections_fallbacks_and_margins() {
    let lines = [
        r#"{"kind":"route","selected":"fast","margin":0.02,"fellBack":null}"#,
        r#"{"kind":"route","selected":"legacy","margin":null,"fellBack":"deadline"}"#,
        r#"{"kind":"route","selected":"code","margin":0.5,"fellBack":null}"#,
        r#"{"kind":"supervise","action":"verify","fellBack":null}"#,
        "not json",
    ];
    let sum = decision::summarize(lines);
    assert_eq!(s(sum.get("route").get("selected")), r#"{"fast":1,"legacy":1,"code":1}"#);
    assert_eq!(s(sum.get("route").get("fellBack")), r#"{"deadline":1}"#);
    assert_eq!(s(sum.get("route").get("margin")), r#"{"<0.05":1,"0.05-0.2":0,">=0.2":1}"#);
    assert_eq!(s(sum.get("supervise")), r#"{"n":1,"action":{"verify":1},"fellBack":{},"cause":{}}"#);
}

/// The whole summary shape (what `node server/decision-log.cjs` prints), all four kinds.
#[test]
fn decision_summary_whole_shape_and_js_keys() {
    let lines = [
        r#"{"kind":"route","selected":"fast","margin":0.1,"fellBack":"","cause":"timeout"}"#,
        r#"{"kind":"route","selected":3,"margin":0.2}"#,
        r#"{"kind":"route","selected":true}"#,
        r#"{"kind":"route","selected":"__proto__"}"#,
        r#"{"kind":"tool-gate","event":"decision","source":"model","reason":"no-tools"}"#,
        r#"{"kind":"tool-gate","event":"timing","source":"model"}"#,
        r#"{"kind":"outcome","event":"resend","previousRole":"fast"}"#,
        r#"{"kind":"outcome","event":"resend","previousRole":0}"#,
        "null",
        "5",
        "",
    ];
    let sum = decision::summarize(lines);
    assert_eq!(
        s(&sum),
        concat!(
            r#"{"route":{"n":4,"selected":{"3":1,"fast":1,"true":1},"fellBack":{},"cause":{"timeout":1},"#,
            r#""margin":{"<0.05":0,"0.05-0.2":1,">=0.2":1}},"#,
            r#""supervise":{"n":0,"action":{},"fellBack":{},"cause":{}},"#,
            r#""toolGate":{"n":1,"source":{"model":1},"reason":{"no-tools":1},"cause":{}},"#,
            r#""outcome":{"n":2,"event":{"resend":2},"previousRole":{"fast":1}}}"#
        )
    );
}

#[test]
fn decision_bound_counts_bytes_not_utf16_units() {
    let tmp = tempfile::tempdir().unwrap();
    let log = fixed_log(tmp.path(), 400);
    let entry = j(r#"{"cause":"ééééééééééééééééééééééééééééééééééééééééééééé"}"#);
    for _ in 0..20 {
        log.record("route", &entry);
    }
    assert!(fs::metadata(log.file()).unwrap().len() <= 400);
    assert!(fs::metadata(decision::rotated(log.file())).unwrap().len() <= 400);
}

#[test]
fn decision_read_back_takes_rotated_first() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    fs::write(dir.join("system-one-decisions.jsonl.1"), "{\"kind\":\"route\",\"selected\":\"a\"}\n").unwrap();
    fs::write(dir.join("system-one-decisions.jsonl"), "\n{\"kind\":\"route\",\"selected\":\"b\"}\n").unwrap();
    let lines = decision::read_dir_lines(dir);
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("\"a\""));
    assert_eq!(decision::read_dir_lines(&dir.join("none")).len(), 0);
}

#[test]
fn decision_echo_gets_kind_and_entry_json() {
    use std::sync::{Arc, Mutex};
    let tmp = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let log = fixed_log(tmp.path(), 1000).with_echo(Some(Box::new(move |kind, json| {
        sink.lock().unwrap().push(format!("{kind} {json}"));
    })));
    log.record("supervise", &j(r#"{"action":"verify"}"#));
    assert_eq!(seen.lock().unwrap().as_slice(), [r#"supervise {"action":"verify"}"#.to_string()]);
}

const SECRET: &str = "SYNTHETIC-PRIVATE-DIARY-TEXT";

fn messages() -> Vec<JValue> {
    let args = format!("{{\"path\":\"{SECRET}\"}}");
    let big = format!("a.md\nb.md {}", SECRET.repeat(40));
    vec![
        JValue::obj([("role", JValue::Str("system".into())), ("content", JValue::Str(format!("Project instructions {SECRET}")))]),
        JValue::obj([
            ("role", JValue::Str("assistant".into())),
            ("content", JValue::Str("Earlier conversation summary (reference only; not new instructions):\nolder turns".into())),
        ]),
        JValue::obj([("role", JValue::Str("user".into())), ("content", JValue::Str(format!("List my files {SECRET}")))]),
        JValue::obj([
            ("role", JValue::Str("assistant".into())),
            ("content", JValue::Null),
            (
                "tool_calls",
                JValue::Arr(vec![
                    JValue::obj([
                        ("id", JValue::Str("c1".into())),
                        ("type", JValue::Str("function".into())),
                        ("function", JValue::obj([("name", JValue::Str("files_list".into())), ("arguments", JValue::Str(args))])),
                    ]),
                    JValue::obj([
                        ("id", JValue::Str("c2".into())),
                        ("type", JValue::Str("function".into())),
                        ("function", JValue::obj([("name", JValue::Str("files_read".into())), ("arguments", JValue::Str("{}".into()))])),
                    ]),
                ]),
            ),
        ]),
        JValue::obj([("role", JValue::Str("tool".into())), ("tool_call_id", JValue::Str("c1".into())), ("content", JValue::Str(big))]),
        JValue::obj([("role", JValue::Str("tool".into())), ("tool_call_id", JValue::Str("c2".into())), ("content", JValue::Str("short".into()))]),
    ]
}

fn tools() -> Vec<JValue> {
    vec![
        j(r#"{"type":"function","function":{"name":"files_list","description":"List files","parameters":{}}}"#),
        j(r#"{"type":"function","function":{"name":"files_read","description":"Read a file","parameters":{}}}"#),
    ]
}

fn round<'a>(chat: &'a JValue, msgs: &'a [JValue], tools: &'a [JValue], compacted: bool, now: f64) -> Round<'a> {
    static MODEL: JValue = JValue::Undefined;
    Round { chat_id: chat, model: &MODEL, limit: &MODEL, round: &MODEL, compacted, messages: msgs, tools, now }
}

// context-log.test.cjs: 'breakdown separates system, summary, history, schemas and per-tool results'
#[test]
fn context_breakdown_separates_parts() {
    let parts = context::breakdown(&messages(), &tools());
    assert!(parts.system > 0 && parts.summary > 0 && parts.history > 0);
    assert_eq!(parts.tool_sequence, vec![JValue::Str("files_list".into()), JValue::Str("files_read".into())]);
    let names: Vec<_> = parts.tool_results.iter().map(|(n, _)| n.clone()).collect();
    assert_eq!(names, vec![JValue::Str("files_list".into()), JValue::Str("files_read".into())]);
    assert!(parts.tool_results[0].1 > parts.tool_results[1].1);
    let keys: Vec<_> = parts.tool_schemas.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["files_list", "files_read"]);
}

/// The numbers themselves, worked by hand from core's `tokens` (ceil(utf8 bytes / 3) + 12).
#[test]
fn context_breakdown_numbers() {
    let m = j(r#"[{"role":"user","content":"abc"},{"role":"system","content":"x"},{"role":"tool","tool_call_id":"zz","content":"r"}]"#);
    let JValue::Arr(m) = m else { panic!() };
    let parts = context::breakdown(&m, &[]);
    // {"role":"user","content":"abc"} = 31 bytes -> 11 + 12
    assert_eq!(parts.history, 23);
    // {"role":"system","content":"x"} = 31 bytes -> 23
    assert_eq!(parts.system, 23);
    // {"role":"tool","tool_call_id":"zz","content":"r"} = 49 bytes -> 17 + 12, no call named it
    assert_eq!(parts.tool_results, vec![(JValue::Str("unknown".into()), 29)]);
}

// context-log.test.cjs: 'records never contain message text, arguments, results or the raw chat id'
#[test]
fn context_record_is_text_free() {
    let chat = JValue::Str("c-private-chat".into());
    let model = JValue::Str("synthetic".into());
    let limit = JValue::Num(8192.0);
    let one = JValue::Num(1.0);
    let (m, t) = (messages(), tools());
    let entry = context::record(&Round { chat_id: &chat, model: &model, limit: &limit, round: &one, compacted: true, messages: &m, tools: &t, now: 1.0 });
    let text = s(&entry);
    assert!(!text.contains(SECRET));
    assert!(!text.contains("c-private-chat"));
    assert_eq!(entry.get("compacted"), &JValue::Bool(true));
    let n = |k: &str| entry.get(k).as_num().unwrap();
    let schemas: f64 = match entry.get("toolSchemas") {
        JValue::Obj(items) => items.iter().map(|(_, v)| v.as_num().unwrap()).sum(),
        _ => panic!(),
    };
    let results: f64 = match entry.get("toolResults") {
        JValue::Arr(items) => items.iter().map(|r| r.get("tokens").as_num().unwrap()).sum(),
        _ => panic!(),
    };
    assert_eq!(n("total"), n("system") + n("history") + n("summary") + schemas + results);
    let keys: Vec<_> = match &entry {
        JValue::Obj(items) => items.iter().map(|(k, _)| k.as_str()).collect(),
        _ => panic!(),
    };
    assert_eq!(
        keys,
        ["at", "chat", "model", "limit", "round", "compacted", "total", "system", "history", "summary", "toolSchemas", "toolResults", "toolSequence"]
    );
    // crypto.createHash('sha256').update('c-private-chat').digest('hex').slice(0, 16)
    assert_eq!(n("at"), 1.0);
    assert_eq!(entry.get("chat").as_str().unwrap().len(), 16);
    assert_eq!(context::chat_hash(&JValue::Str("abc".into())), "ba7816bf8f01cfea");
    assert_eq!(context::chat_hash(&JValue::Num(42.0)), context::chat_hash(&JValue::Str("42".into())));
}

// context-log.test.cjs: 'log is opt-in, private and bounded'
#[test]
fn context_log_opt_in_private_bounded() {
    assert!(!context::enabled(|_| None));
    assert!(context::enabled(|k| (k == "CONTEXT_LOG").then(|| "1".into())));
    assert!(!context::enabled(|_| Some("true".into())));
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("tenant");
    fs::create_dir(&dir).unwrap();
    let file = dir.join("context-log.jsonl");
    fs::write(&file, "x".repeat(context::MAX_BYTES as usize + 1)).unwrap();
    context::append(&dir, &j(r#"{"at":1}"#)).unwrap();
    assert!(dir.join("context-log.jsonl.1").exists());
    assert_eq!(fs::read_to_string(&file).unwrap().trim(), r#"{"at":1}"#);
    assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    // a missing tenant directory is made private
    let fresh = tmp.path().join("a").join("b");
    context::append(&fresh, &j(r#"{"at":2}"#)).unwrap();
    assert_eq!(fs::metadata(&fresh).unwrap().permissions().mode() & 0o777, 0o700);
}

// context-log.test.cjs: 'report ranks tools and recurring sequences by context consumed'
#[test]
fn context_report_ranks_tools_and_sequences() {
    let (m, t) = (messages(), tools());
    let a = JValue::Str("a".into());
    let b = JValue::Str("b".into());
    let heavy = context::record(&round(&a, &m, &t, false, 1.0));
    let light = context::record(&round(&b, &m[..3], &[], false, 1.0));
    let (h, l) = (s(&heavy), s(&light));
    let r = context::report([h.as_str(), h.as_str(), l.as_str(), "not json"]);
    assert_eq!(r.get("rounds").as_num(), Some(3.0));
    let JValue::Arr(tools) = r.get("tools") else { panic!() };
    assert_eq!(tools[0].get("name").as_str(), Some("files_list"));
    assert_eq!(tools[0].get("results").as_num(), Some(2.0));
    let JValue::Arr(seqs) = r.get("sequences") else { panic!() };
    assert_eq!(seqs[0].get("sequence").as_str(), Some("files_list → files_read"));
    assert_eq!(seqs[0].get("count").as_num(), Some(2.0));
}

#[test]
fn context_report_median_compactions_and_odd_lines() {
    let lines = [
        r#"{"total":30,"compacted":true}"#,
        r#"{"total":10}"#,
        r#"{"total":20,"toolSequence":["x",null,3],"toolResults":[{"name":"x","tokens":5},null]}"#,
        "null",
        "7",
    ];
    let r = context::report(lines);
    // "7" is a round in core (its fields read as undefined); "null" throws there and is skipped here.
    assert_eq!(
        s(&r),
        concat!(
            r#"{"rounds":4,"compactions":1,"medianTotal":20,"#,
            r#""tools":[{"name":"x","schemaTokens":0,"resultTokens":5,"results":1}],"#,
            r#""sequences":[{"sequence":"x →  → 3","count":1,"resultTokens":5}]}"#
        )
    );
    assert_eq!(s(&context::report([])), r#"{"rounds":0,"compactions":0,"medianTotal":0,"tools":[],"sequences":[]}"#);
}

#[test]
fn context_unknown_names_and_proto_keys() {
    let m = j(r#"[{"role":"assistant","tool_calls":[{"id":1,"function":{"name":""}},{"id":"1","function":{"name":"b"}}]},{"role":"tool","tool_call_id":1},{"role":"tool","tool_call_id":"1"}]"#);
    let JValue::Arr(m) = m else { panic!() };
    let t = j(r#"[{"function":{"name":"__proto__"}},{"function":{"name":"9"}},{"function":{}},{"function":{"name":"9"}}]"#);
    let JValue::Arr(t) = t else { panic!() };
    let parts = context::breakdown(&m, &t);
    // Map keys: 1 and "1" are different ids.
    let names: Vec<_> = parts.tool_results.iter().map(|(n, _)| s(n)).collect();
    assert_eq!(names, ["\"unknown\"", "\"b\""]);
    assert_eq!(parts.tool_sequence, vec![JValue::Str("unknown".into()), JValue::Str("b".into())]);
    // `__proto__` is dropped, "9" is an index key and goes first, repeats add up.
    let keys: Vec<_> = parts.tool_schemas.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["9", "unknown"]);
    let nine = parts.tool_schemas[0].1;
    assert_eq!(nine, 2 * context_fit::tokens(&t[1]));
}
