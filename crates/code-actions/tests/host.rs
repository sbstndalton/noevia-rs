//! Every row of tests/fixtures/host-node.v1.json, recorded from noevia-core's
//! `server/code-actions.cjs` over the dav-parse.wasm its lock pins (tools/gen-host-fixtures.cjs,
//! which also records the sha256 of both files), through `code_actions::host`: the same
//! `JSON.stringify` text as Node's answer, or the same throw, or, for a row naming one of the
//! module's documented differences (D1, D2), the port's own answer.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;

use code_actions::host;
use js_json::{wtf16, JValue};

fn fixture() -> JValue {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/host-node.v1.json"
    );
    wtf16::parse(&std::fs::read_to_string(path).expect("fixture file")).expect("fixture JSON")
}

fn text(v: &JValue) -> Option<String> {
    wtf16::stringify(v)
}

fn str_of(v: &JValue) -> String {
    v.as_str().expect("a string").to_owned()
}

fn count(v: &JValue) -> usize {
    let n = v.as_num().expect("a count");
    assert!(n >= 0.0 && n.fract() == 0.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = n as usize;
    n
}

/// The generator's compact inputs (see its header).
fn expand(v: &JValue) -> JValue {
    match v {
        JValue::Arr(items) => JValue::Arr(items.iter().map(expand).collect()),
        JValue::Obj(items) if items.len() == 1 && items[0].0.starts_with('$') => {
            let (k, x) = &items[0];
            let part = |i: usize| match x {
                JValue::Arr(a) => a[i].clone(),
                _ => panic!("{k}: not an array"),
            };
            match k.as_str() {
                "$repeat" => JValue::Str(str_of(&part(0)).repeat(count(&part(1)))),
                "$concat" => {
                    let JValue::Arr(parts) = x else { panic!() };
                    JValue::Str(parts.iter().map(|p| str_of(&expand(p))).collect())
                }
                "$fill" => JValue::Arr(vec![expand(&part(0)); count(&part(1))]),
                "$undefined" => JValue::Undefined,
                "$classify" => host::classify(&expand(x)),
                _ => panic!("unknown marker {k}"),
            }
        }
        JValue::Obj(items) => {
            JValue::Obj(items.iter().map(|(k, x)| (k.clone(), expand(x))).collect())
        }
        _ => v.clone(),
    }
}

/// The answer Node gave: `want`, or `wantRuns` joined.
fn want_of(row: &JValue) -> Option<Option<String>> {
    match row.get("want") {
        JValue::Str(s) => return Some(Some(s.clone())),
        JValue::Null => return Some(None),
        _ => {}
    }
    let JValue::Arr(runs) = row.get("wantRuns") else {
        return None;
    };
    let mut out = String::new();
    for run in runs {
        let JValue::Arr(r) = run else { panic!() };
        out.push_str(&str_of(&r[0]).repeat(count(&r[1])));
    }
    Some(Some(out))
}

/// The crate, or the injected answers (a string is a refusal with that reason, null the crate).
fn port(inject: Option<Vec<JValue>>) -> impl FnMut(&[u8]) -> (u32, Vec<u8>) {
    let mut i = 0usize;
    move |input: &[u8]| {
        let Some(steps) = &inject else {
            return code_actions::call(input);
        };
        let step = &steps[i.min(steps.len() - 1)];
        i += 1;
        match step {
            JValue::Null => code_actions::call(input),
            reason @ JValue::Str(_) => (
                1,
                format!("{{\"error\":{}}}", text(reason).unwrap()).into_bytes(),
            ),
            _ => panic!("inject step"),
        }
    }
}

/// What the port answers for one row: `Ok(answer)` or `Err(what Node throws)`.
fn run(row: &JValue) -> Result<Option<String>, String> {
    let fun = str_of(row.get("fn"));
    let JValue::Arr(args) = row.get("args") else {
        panic!("args")
    };
    let args: Vec<JValue> = args.iter().map(expand).collect();
    let arg = |i: usize| args.get(i).cloned().unwrap_or(JValue::Undefined);
    let inject = match row.get("inject") {
        JValue::Arr(steps) => Some(steps.clone()),
        _ => None,
    };
    let plain = inject.is_none();
    let mut port = port(inject);
    let out = match fun.as_str() {
        "classify" => {
            let out = host::classify_with(&arg(0), &mut port);
            if plain {
                assert_eq!(host::classify(&arg(0)), out);
            }
            out
        }
        "decide" => {
            let input = arg(0);
            let out = host::decide_request_with(&input, &mut port);
            if plain {
                assert_eq!(host::decide_request(&input), out);
                // The four-argument form, wherever it can say the same thing.
                let keys = ["classified", "capabilities", "domains", "inWorkspace"];
                if let JValue::Obj(items) = &input {
                    let in_workspace = match input.get("inWorkspace") {
                        JValue::Undefined | JValue::Null => Some(None),
                        JValue::Bool(b) => Some(Some(*b)),
                        _ => None,
                    };
                    if let Some(in_workspace) = in_workspace
                        .filter(|_| items.iter().all(|(k, _)| keys.contains(&k.as_str())))
                    {
                        let four = host::decide(
                            input.get("classified"),
                            input.get("capabilities"),
                            input.get("domains"),
                            in_workspace,
                        );
                        assert_eq!(text(&four), text(&out), "decide(4) differs");
                    }
                }
            }
            out
        }
        "pickOption" => {
            let out = host::pick_option_with(&arg(0), &arg(1), &mut port);
            if plain {
                assert_eq!(host::pick_option_value(&arg(0), &arg(1)), out);
                if let JValue::Str(kind) = arg(1) {
                    assert_eq!(host::pick_option(&arg(0), &kind), out);
                }
            }
            out
        }
        "commandOf" => match host::command_of(&arg(0)) {
            Some(s) => JValue::Str(s),
            None => return Err("TypeError".into()),
        },
        "classifyInput" => match host::classify_input(&arg(0)) {
            Some(v) => v,
            None => return Err("TypeError".into()),
        },
        "decideInput" => match host::decide_input(&arg(0)) {
            Ok(v) => v,
            Err("unexpected") => return Err("TypeError".into()),
            Err(reason) => return Err(format!("Error:{reason}")),
        },
        "pickInput" => host::pick_input(&arg(0)),
        other => panic!("unknown fn {other}"),
    };
    Ok(text(&out))
}

#[test]
fn every_recorded_row_matches_node() {
    let f = fixture();
    assert_eq!(f.get("version"), &JValue::Num(1.0));
    let source = f.get("source");
    for key in [
        "code-actions.cjs",
        "dav-parse.lock",
        "DAV_PARSE_WASM_SHA256",
        "NOEVIA_RS_REF",
    ] {
        let sha = str_of(source.get(key));
        assert_eq!(
            sha.len(),
            if key == "NOEVIA_RS_REF" { 40 } else { 64 },
            "{key}"
        );
        assert!(sha.bytes().all(|b| b.is_ascii_hexdigit()), "{key}");
    }
    let JValue::Arr(rows) = f.get("cases") else {
        panic!("cases")
    };
    let mut per_fn: BTreeMap<String, usize> = BTreeMap::new();
    let mut differences: BTreeMap<String, usize> = BTreeMap::new();
    let mut failures = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let fun = str_of(row.get("fn"));
        *per_fn.entry(fun.clone()).or_default() += 1;
        let got = run(row);
        let node = match row.get("throws") {
            JValue::Str(t) => Err(t.clone()),
            _ => Ok(want_of(row).expect("want")),
        };
        let expected = if let JValue::Str(name) = row.get("difference") {
            *differences.entry(name.clone()).or_default() += 1;
            let port = Ok(Some(str_of(row.get("port"))));
            assert_ne!(port, node, "row {i}: a difference that is none");
            port
        } else {
            node
        };
        if got != expected {
            failures.push(format!(
                "row {i} ({fun}): got {:.300?}\n  want {:.300?}",
                got, expected
            ));
        }
    }
    eprintln!("host rows: {per_fn:?}; differences {differences:?}");
    assert!(
        failures.is_empty(),
        "{} of {} rows differ:\n{}",
        failures.len(),
        rows.len(),
        failures
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    let minimum = [
        ("classify", 2500),
        ("decide", 3500),
        ("pickOption", 300),
        ("commandOf", 80),
        ("classifyInput", 80),
        ("decideInput", 40),
        ("pickInput", 20),
    ];
    for (fun, min) in minimum {
        assert!(
            per_fn.get(fun).copied().unwrap_or(0) >= min,
            "{fun}: too few rows"
        );
    }
    assert!(differences.get("D1").copied().unwrap_or(0) >= 10);
    assert!(differences.get("D2").copied().unwrap_or(0) >= 10);
}

#[test]
fn constants_match_node() {
    let f = fixture();
    let c = f.get("constants");
    assert_eq!(
        text(&host::actions_object()).unwrap(),
        str_of(c.get("ACTIONS"))
    );
    assert_eq!(
        text(&host::command_keys_array()).unwrap(),
        str_of(c.get("COMMAND_KEYS"))
    );
    assert_eq!(
        text(&JValue::from(host::UNVERIFIED_ASK)).unwrap(),
        str_of(c.get("UNVERIFIED_ASK"))
    );
    // A new export in Node is a new thing to port.
    assert_eq!(
        str_of(c.get("exports")),
        r#"["ACTIONS","COMMAND_KEYS","classify","commandOf","decide","pickOption","classifyInput","decideInput","pickInput","UNVERIFIED_ASK"]"#
    );
}
