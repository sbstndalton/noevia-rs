//! Every row of tests/fixtures/code-harness.v1.json, recorded from noevia-core's code-meta.cjs and
//! code-tool-schemas.cjs by tools/gen-fixtures.cjs, replayed through the port and compared as
//! JSON text in the fixture's tagged form (so `undefined`, NaN, the infinities and -0 count too):
//! every return value, every thrown error (name and message), and for the guard every `event`,
//! `log` and `onBlocked` call in order with what it received. A row with `port` is a named
//! difference (the crate docs): the port must give `port`, Node's answer on the defining input.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use code_harness::{
    check_fs_call, check_tool_call, coding_identity, correction_for, fs_schemas, raw_input_schemas,
    read_agent, read_context, read_exit_code, read_usage, reject_outcome, summarize,
    tool_call_schema, violation_error, violation_text, BlockedError, ExecutorGuard,
    ExecutorGuardFlag, GuardHooks, GuardOptions, RpcError, Thrown, ACP_KINDS, CODE_META_SHA256,
    CODE_TOOL_SCHEMAS_SHA256, MAX_VIOLATIONS, PATH_KEYS,
};
use js_json::{wtf16, JValue};
use std::collections::BTreeMap;

const TAG: &str = "$js";

fn fixture() -> JValue {
    wtf16::parse(include_str!("fixtures/code-harness.v1.json")).expect("fixture JSON")
}

fn rows<'a>(f: &'a JValue, key: &str) -> &'a [JValue] {
    match f.get(key) {
        JValue::Arr(rows) => rows,
        _ => panic!("no {key}"),
    }
}

fn obj<const N: usize>(pairs: [(&str, JValue); N]) -> JValue {
    JValue::obj(pairs)
}

fn s(t: &str) -> JValue {
    JValue::Str(t.into())
}

/// The fixture's tagged form back to JS values.
fn decode(v: &JValue) -> JValue {
    match v {
        JValue::Arr(items) => JValue::Arr(items.iter().map(decode).collect()),
        JValue::Obj(items) => {
            if let Some(tag) = v.get(TAG).as_str() {
                return match tag {
                    "undefined" => JValue::Undefined,
                    "NaN" => JValue::Num(f64::NAN),
                    "Infinity" => JValue::Num(f64::INFINITY),
                    "-Infinity" => JValue::Num(f64::NEG_INFINITY),
                    "-0" => JValue::Num(-0.0),
                    "repeat" => {
                        let n = v.get("n").as_num().unwrap() as usize;
                        JValue::Str(v.get("s").as_str().unwrap().repeat(n))
                    }
                    other => panic!("unknown tag {other}"),
                };
            }
            JValue::Obj(items.iter().map(|(k, x)| (k.clone(), decode(x))).collect())
        }
        other => other.clone(),
    }
}

/// JS values to the fixture's tagged form.
fn encode(v: &JValue) -> JValue {
    let tag = |t: &str| obj([(TAG, s(t))]);
    match v {
        JValue::Undefined => tag("undefined"),
        JValue::Num(n) if n.is_nan() => tag("NaN"),
        JValue::Num(n) if *n == f64::INFINITY => tag("Infinity"),
        JValue::Num(n) if *n == f64::NEG_INFINITY => tag("-Infinity"),
        JValue::Num(n) if *n == 0.0 && n.is_sign_negative() => tag("-0"),
        JValue::Arr(items) => JValue::Arr(items.iter().map(encode).collect()),
        JValue::Obj(items) => {
            JValue::Obj(items.iter().map(|(k, x)| (k.clone(), encode(x))).collect())
        }
        other => other.clone(),
    }
}

fn text(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "<undefined>".into())
}

fn thrown(t: &Thrown) -> JValue {
    match t {
        Thrown::Error { name, message } => obj([("name", s(name)), ("message", s(message))]),
        Thrown::Value(v) => obj([("value", encode(v))]),
    }
}

/// A Rust result as the generator writes Node's: `{out}` or `{throws}`.
fn outcome(r: Result<JValue, Thrown>) -> JValue {
    match r {
        Ok(v) => obj([("out", encode(&v))]),
        Err(t) => obj([("throws", thrown(&t))]),
    }
}

/// Node's answer in a row (`{out}` or `{throws}`).
fn node(row: &JValue) -> JValue {
    match (row.get("out"), row.get("throws")) {
        (JValue::Undefined, t) => obj([("throws", t.clone())]),
        (o, _) => obj([("out", o.clone())]),
    }
}

fn arg(args: &JValue, i: usize) -> JValue {
    match args {
        JValue::Arr(items) => items.get(i).map(decode).unwrap_or(JValue::Undefined),
        _ => panic!("args"),
    }
}

fn rpc_error(e: &RpcError) -> JValue {
    obj([(
        "error",
        obj([
            ("name", s("Error")),
            ("message", s(&e.message)),
            (
                "props",
                obj([
                    ("code", JValue::Num(f64::from(e.code))),
                    ("data", e.data.clone()),
                ]),
            ),
        ]),
    )])
}

fn blocked_error(e: &BlockedError) -> JValue {
    obj([(
        "error",
        obj([
            ("name", s("Error")),
            ("message", s(&e.message)),
            (
                "props",
                obj([
                    ("code", s(e.code)),
                    ("publicMessage", s(&e.public_message)),
                    ("result", e.result.clone()),
                ]),
            ),
        ]),
    )])
}

/// Checks one row: the port's answer must be Node's, or for a named difference `port`. Returns the
/// difference and whether Node's answer really differs from `port` there.
fn check_row(i: usize, row: &JValue, port: impl Fn(&JValue) -> JValue) -> Option<(String, bool)> {
    let args = row.get("args");
    let ours = text(&port(args));
    match row.get("diff").as_str() {
        None => {
            assert_eq!(ours, text(&node(row)), "row {i}: {}", text(row));
            None
        }
        Some(d) => {
            let expected = text(row.get("port"));
            assert_eq!(ours, expected, "row {i} ({d}): {}", text(row));
            Some((d.to_string(), expected != text(&node(row))))
        }
    }
}

fn tally(diffs: &mut BTreeMap<String, (usize, usize)>, d: Option<(String, bool)>) {
    if let Some((d, differs)) = d {
        let e = diffs.entry(d).or_default();
        e.0 += 1;
        e.1 += usize::from(differs);
    }
}

#[test]
fn the_fixture_names_the_sources_this_port_follows() {
    let f = fixture();
    assert_eq!(f.get("version"), &JValue::Num(1.0));
    let source = f.get("source");
    assert_eq!(source.get("code-meta.cjs").as_str(), Some(CODE_META_SHA256));
    assert_eq!(
        source.get("code-tool-schemas.cjs").as_str(),
        Some(CODE_TOOL_SCHEMAS_SHA256)
    );
}

#[test]
fn constants_and_schemas_are_nodes() {
    let f = fixture();
    let c = f.get("constants");
    assert_eq!(
        c.get("MAX_VIOLATIONS"),
        &JValue::Num(f64::from(MAX_VIOLATIONS))
    );
    let strings = |items: &[&str]| JValue::Arr(items.iter().map(|t| s(t)).collect());
    assert_eq!(text(c.get("ACP_KINDS")), text(&strings(&ACP_KINDS)));
    assert_eq!(text(c.get("PATH_KEYS")), text(&strings(&PATH_KEYS)));
    assert_eq!(text(c.get("RAW_INPUT_SCHEMAS")), text(&raw_input_schemas()));
    assert_eq!(text(c.get("FS_SCHEMAS")), text(&fs_schemas()));
    // Every export of both modules has its port (the flag pair is ExecutorGuardFlag).
    let ported = [
        "readUsage",
        "readContext",
        "readExitCode",
        "readAgent",
        "codingIdentity",
        "summarize",
        "MAX_VIOLATIONS",
        "ACP_KINDS",
        "PATH_KEYS",
        "RAW_INPUT_SCHEMAS",
        "FS_SCHEMAS",
        "toolCallSchema",
        "checkToolCall",
        "checkFsCall",
        "correctionFor",
        "violationError",
        "violationText",
        "rejectOutcome",
        "createExecutorGuard",
        "executorGuardFlag",
        "useExecutorGuard",
    ];
    let exports = c.get("exports");
    for module in ["meta", "schemas"] {
        let JValue::Arr(names) = exports.get(module) else {
            panic!("exports")
        };
        for name in names {
            let name = name.as_str().unwrap();
            assert!(
                ported.contains(&name),
                "{module} exports {name}, not ported"
            );
        }
    }
}

#[test]
fn code_meta_rows_match_node() {
    let f = fixture();
    let rows = rows(&f, "meta");
    assert!(rows.len() >= 1200, "{} rows", rows.len());
    let mut diffs = BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        let name = row.get("fn").as_str().unwrap().to_string();
        let d = check_row(i, row, |args| {
            let a = arg(args, 0);
            outcome(match name.as_str() {
                "readUsage" => Ok(read_usage(&a)),
                "readContext" => Ok(read_context(&a)),
                "readExitCode" => Ok(read_exit_code(&a)),
                "readAgent" => Ok(read_agent(&a)),
                "codingIdentity" => coding_identity(&a),
                "summarize" => Ok(summarize(&a)),
                other => panic!("unknown fn {other}"),
            })
        });
        tally(&mut diffs, d);
    }
    let d1 = diffs.get("D1").copied().unwrap_or_default();
    assert!(d1.0 >= 100 && d1.1 >= 50, "D1 rows {d1:?}");
    assert_eq!(diffs.len(), 1, "{diffs:?}");
}

#[test]
fn code_tool_schemas_rows_match_node() {
    let f = fixture();
    let rows = rows(&f, "schemas");
    assert!(rows.len() >= 1000, "{} rows", rows.len());
    let mut diffs = BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        let name = row.get("fn").as_str().unwrap().to_string();
        let d = check_row(i, row, |args| {
            let a = arg(args, 0);
            let b = arg(args, 1);
            outcome(match name.as_str() {
                "checkToolCall" => Ok(check_tool_call(&a)),
                "checkFsCall" => {
                    let max_bytes = match arg(args, 2).get("maxBytes") {
                        JValue::Num(n) => *n,
                        JValue::Undefined => f64::INFINITY,
                        other => panic!("maxBytes {other:?}"),
                    };
                    Ok(check_fs_call(a.as_str().unwrap(), &b, max_bytes))
                }
                "toolCallSchema" => Ok(tool_call_schema(a.as_str().unwrap())),
                "correctionFor" => Ok(correction_for(&a)),
                "violationText" => violation_text(&a).map(JValue::Str),
                "violationError" => violation_error(&a).map(|e| rpc_error(&e)),
                "rejectOutcome" => reject_outcome(&a, &b),
                other => panic!("unknown fn {other}"),
            })
        });
        tally(&mut diffs, d);
    }
    for d in ["D2", "D3"] {
        let (n, differs) = diffs.get(d).copied().unwrap_or_default();
        assert!(n >= 5 && differs == n, "{d} rows {n}, differing {differs}");
    }
    assert_eq!(diffs.len(), 2, "{diffs:?}");
}

/// Hooks that write the generator's trace and throw where a row says.
struct Scripted {
    trace: Vec<JValue>,
    seen: BTreeMap<&'static str, usize>,
    throws_at: Vec<(String, usize, Thrown)>,
}

impl Scripted {
    fn new(throws_at: &JValue) -> Self {
        let JValue::Arr(items) = throws_at else {
            panic!("throwsAt")
        };
        let throws_at = items
            .iter()
            .map(|t| {
                let spec = t.get("thrown");
                let thrown = match spec.get("error") {
                    JValue::Str(m) => Thrown::error(m.clone()),
                    _ => Thrown::Value(decode(spec.get("value"))),
                };
                (
                    t.get("cb").as_str().unwrap().to_string(),
                    t.get("at").as_num().unwrap() as usize,
                    thrown,
                )
            })
            .collect();
        Scripted {
            trace: Vec::new(),
            seen: BTreeMap::new(),
            throws_at,
        }
    }

    fn hit(&mut self, cb: &'static str, entry: JValue) -> Result<(), Thrown> {
        self.trace.push(entry);
        let n = self.seen.entry(cb).or_default();
        let at = *n;
        *n += 1;
        match self.throws_at.iter().find(|(c, a, _)| c == cb && *a == at) {
            Some((_, _, t)) => {
                let t = t.clone();
                if let Some(JValue::Obj(items)) = self.trace.last_mut() {
                    items.push(("threw".into(), JValue::Bool(true)));
                }
                Err(t)
            }
            None => Ok(()),
        }
    }
}

impl GuardHooks for Scripted {
    fn event(&mut self, kind: &str, data: &JValue) -> Result<(), Thrown> {
        let e = obj([
            ("cb", s("event")),
            ("type", s(kind)),
            ("data", encode(data)),
        ]);
        self.hit("event", e)
    }
    fn log(&mut self, entry: &JValue) -> Result<(), Thrown> {
        let e = obj([("cb", s("log")), ("entry", encode(entry))]);
        self.hit("log", e)
    }
    fn on_blocked(&mut self) -> Result<(), Thrown> {
        self.hit("onBlocked", obj([("cb", s("onBlocked"))]))
    }
}

fn play(options: &JValue, throws_at: &JValue, steps: &JValue) -> Vec<JValue> {
    let options = decode(options);
    let mut o = GuardOptions::default();
    if let JValue::Num(n) = options.get("limit") {
        o.limit = *n;
    }
    if let JValue::Num(n) = options.get("maxBytes") {
        o.max_bytes = *n;
    }
    let mut guard = ExecutorGuard::new(Scripted::new(throws_at), o);
    let JValue::Arr(steps) = steps else {
        panic!("steps")
    };
    for (i, step) in steps.iter().enumerate() {
        let args = step.get("args");
        let r: Result<JValue, Thrown> = match step.get("op").as_str().unwrap() {
            "permission" => guard
                .permission(&arg(args, 0), &arg(args, 1))
                .map(|v| v.unwrap_or(JValue::Null)),
            "readTextFile" => guard
                .read_text_file(&arg(args, 0))
                .map(|e| e.as_ref().map_or(JValue::Null, rpc_error)),
            "writeTextFile" => guard
                .write_text_file(&arg(args, 0))
                .map(|e| e.as_ref().map_or(JValue::Null, rpc_error)),
            "blockedError" => Ok(guard
                .blocked_error()
                .as_ref()
                .map_or(JValue::Null, blocked_error)),
            "state" => Ok(obj([
                ("violations", JValue::Num(guard.violations() as f64)),
                ("blocked", JValue::Bool(guard.is_blocked())),
            ])),
            other => panic!("unknown op {other}"),
        };
        let mut entry = vec![("step".to_string(), JValue::Num(i as f64))];
        if let JValue::Obj(items) = outcome(r) {
            entry.extend(items);
        }
        guard.hooks_mut().trace.push(JValue::Obj(entry));
    }
    guard.into_hooks().trace
}

#[test]
fn guard_sequences_match_node_call_for_call() {
    let f = fixture();
    let rows = rows(&f, "guards");
    assert!(rows.len() >= 40, "{} rows", rows.len());
    let mut differs = 0;
    for row in rows {
        let name = row.get("name").as_str().unwrap();
        let (steps, expected) = match row.get("diff").as_str() {
            None => (row.get("steps"), row.get("trace")),
            Some(_) => {
                differs += 1;
                assert_ne!(text(row.get("trace")), text(row.get("portTrace")), "{name}");
                (row.get("portSteps"), row.get("portTrace"))
            }
        };
        let JValue::Arr(expected) = expected else {
            panic!("trace")
        };
        let ours = play(row.get("options"), row.get("throwsAt"), steps);
        for (k, (a, b)) in ours.iter().zip(expected.iter()).enumerate() {
            assert_eq!(text(a), text(b), "{name}: trace entry {k}");
        }
        assert_eq!(ours.len(), expected.len(), "{name}: trace length");
    }
    assert_eq!(differs, 1);
}

#[test]
fn d2_the_guard_refuses_where_node_throws() {
    // The row's own Node trace: the first call threw out of permission() and nothing was counted.
    let f = fixture();
    let row = rows(&f, "guards")
        .iter()
        .find(|r| r.get("diff").as_str() == Some("D2"))
        .unwrap();
    let JValue::Arr(trace) = row.get("trace") else {
        panic!()
    };
    assert_eq!(
        text(trace[0].get("throws").get("name")),
        "\"TypeError\"",
        "Node throws"
    );
    // The port, on the very same steps, answers it as an ordinary refusal.
    let ours = play(row.get("options"), row.get("throwsAt"), row.get("steps"));
    let JValue::Arr(port) = row.get("portTrace") else {
        panic!()
    };
    assert_eq!(ours.len(), port.len());
    for (a, b) in ours.iter().zip(port.iter()) {
        assert_eq!(text(a), text(b));
    }
}

#[test]
fn the_flag_is_off_until_a_check_answers_exactly_true() {
    let f = fixture();
    let mut flag = ExecutorGuardFlag::new();
    for row in rows(&f, "flag") {
        let install = row.get("install");
        if matches!(row.get("installNotFunction"), JValue::Obj(_)) {
            // Node's TypeError for a non-function cannot arise: install takes a function. The flag
            // keeps the check it had.
            assert_eq!(
                text(row.get("installNotFunction").get("name")),
                "\"TypeError\""
            );
        } else if !install.is_nullish() {
            let answer: Result<JValue, Thrown> = match install.get("throws") {
                JValue::Undefined => Ok(decode(install.get("returns"))),
                t => Err(match t.get("error") {
                    JValue::Str(m) => Thrown::error(m.clone()),
                    _ => Thrown::Value(decode(t.get("value"))),
                }),
            };
            flag.install(Box::new(move || answer.clone()));
        }
        assert_eq!(
            JValue::Bool(flag.enabled()),
            row.get("enabled").clone(),
            "{}",
            text(row)
        );
    }
}
