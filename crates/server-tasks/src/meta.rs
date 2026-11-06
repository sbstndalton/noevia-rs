//! Defensive ACP measurements. Context occupancy and spent tokens remain separate facts.
use js_json::JValue;
fn object(rows: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(rows.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
pub(crate) fn numeric(value: &JValue) -> f64 {
    match value {
        JValue::Undefined => f64::NAN,
        JValue::Null => 0.0,
        JValue::Bool(v) => {
            if *v {
                1.0
            } else {
                0.0
            }
        }
        JValue::Num(n) => *n,
        _ => js_json::to_js_string(value)
            .ok()
            .map_or(f64::NAN, |s| server_account::util::js_number(&s)),
    }
}
fn first(source: &JValue, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        let n = numeric(source.get(key));
        (n.is_finite() && n >= 0.0).then_some(n)
    })
}
fn number(value: Option<f64>) -> JValue {
    value.map_or(JValue::Null, JValue::Num)
}
fn container(value: &JValue) -> bool {
    matches!(value, JValue::Obj(_) | JValue::Arr(_))
}
const INPUT: &[&str] = &[
    "inputTokens",
    "input_tokens",
    "promptTokens",
    "prompt_tokens",
    "input",
    "prompt",
];
const OUTPUT: &[&str] = &[
    "outputTokens",
    "output_tokens",
    "completionTokens",
    "completion_tokens",
    "output",
    "completion",
];
pub fn usage(meta: &JValue) -> JValue {
    if !container(meta) {
        return JValue::Null;
    }
    let holder = [
        meta.get("usage"),
        meta.get("tokenUsage"),
        meta.get("tokens"),
        meta.get("noevia").get("usage"),
        meta,
    ]
    .into_iter()
    .find(|c| container(c) && (first(c, INPUT).is_some() || first(c, OUTPUT).is_some()));
    let Some(holder) = holder else {
        return JValue::Null;
    };
    let input = first(holder, INPUT);
    let output = first(holder, OUTPUT);
    let total = first(holder, &["totalTokens", "total_tokens", "total"])
        .or_else(|| input.zip(output).map(|(a, b)| a + b));
    object(vec![
        ("input", number(input)),
        ("output", number(output)),
        ("total", number(total)),
    ])
}
pub fn context(source: &JValue) -> JValue {
    if !container(source) {
        return JValue::Null;
    }
    for holder in [
        source,
        source.get("usage"),
        source.get("_meta"),
        source.get("context"),
    ] {
        if !container(holder) {
            continue;
        }
        let used = first(
            holder,
            &[
                "used",
                "usedTokens",
                "used_tokens",
                "contextUsed",
                "context_used",
            ],
        );
        let size = first(
            holder,
            &["size", "contextSize", "context_size", "limit", "window"],
        );
        if let Some((used, size)) = used.zip(size).filter(|(_, size)| *size > 0.0) {
            return object(vec![
                ("used", JValue::Num(used)),
                ("size", JValue::Num(size)),
                (
                    "percent",
                    JValue::Num(((used / size) * 100.0 + 0.5).floor()),
                ),
            ]);
        }
    }
    JValue::Null
}
pub fn exit(source: &JValue) -> JValue {
    if !container(source) {
        return JValue::Null;
    }
    for holder in [
        source,
        source.get("_meta"),
        source.get("meta"),
        source.get("rawOutput"),
        source.get("_meta").get("noevia"),
    ] {
        if !container(holder) {
            continue;
        }
        for key in ["exitCode", "exit_code", "exitStatus", "status_code", "code"] {
            let n = numeric(holder.get(key));
            if n.is_finite() && n.fract() == 0.0 {
                return JValue::Num(n);
            }
        }
    }
    JValue::Null
}
pub fn summary(source: &JValue) -> JValue {
    let agent = source.get("agent");
    let measured = source.get("usage");
    let usage = if container(measured)
        && !matches!(measured.get("total"), JValue::Null | JValue::Undefined)
    {
        measured.clone()
    } else {
        JValue::Null
    };
    let exits = match source.get("exits") {
        JValue::Arr(rows) => rows
            .iter()
            .filter(|v| container(v))
            .cloned()
            .collect::<Vec<_>>(),
        _ => vec![],
    };
    let context = source.get("context");
    let context = if container(context)
        && context.get("used").as_num().is_some_and(f64::is_finite)
        && context.get("size").as_num().is_some_and(f64::is_finite)
    {
        context.clone()
    } else {
        JValue::Null
    };
    let mut limitations = Vec::new();
    if !agent.get("version").truthy() {
        limitations
            .push("The harness did not report its version, so this cannot be scoped to one.");
    }
    if usage == JValue::Null {
        limitations.push("The harness did not report token usage.");
    }
    if exits.is_empty() {
        limitations.push("No command exit codes were reported.");
    }
    let string = |v: &JValue| {
        if v.as_str().is_some() {
            v.clone()
        } else {
            JValue::Null
        }
    };
    object(vec![
        ("harness", string(agent.get("name"))),
        ("harnessVersion", string(agent.get("version"))),
        (
            "protocolVersion",
            number(
                agent
                    .get("protocolVersion")
                    .as_num()
                    .filter(|v| v.is_finite()),
            ),
        ),
        ("usage", usage),
        ("context", context),
        ("commands", JValue::Num(exits.len() as f64)),
        (
            "failedCommands",
            JValue::Num(
                exits
                    .iter()
                    .filter(|e| {
                        e.get("exitCode")
                            .as_num()
                            .is_some_and(|n| n.is_finite() && n.fract() == 0.0 && n != 0.0)
                    })
                    .count() as f64,
            ),
        ),
        (
            "exitCodes",
            JValue::Arr(exits.into_iter().take(50).collect()),
        ),
        (
            "messageChunks",
            JValue::Num(
                source
                    .get("messageChunks")
                    .as_num()
                    .filter(|v| v.is_finite())
                    .unwrap_or(0.0),
            ),
        ),
        (
            "limitations",
            JValue::Arr(
                limitations
                    .into_iter()
                    .map(|s| JValue::Str(s.into()))
                    .collect(),
            ),
        ),
    ])
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn metadata_matches_node() {
        let fixture =
            js_json::wtf16::parse(include_str!("../tests/fixtures/meta-node.json")).unwrap();
        let JValue::Arr(cases) = fixture.get("cases") else {
            panic!()
        };
        for (i, case) in cases.iter().enumerate() {
            let input = case.get("input");
            assert_eq!(usage(input), *case.get("usage"), "usage {i}");
            assert_eq!(context(input), *case.get("context"), "context {i}");
            assert_eq!(exit(input), *case.get("exit"), "exit {i}");
        }
        let JValue::Arr(cases) = fixture.get("summaries") else {
            panic!()
        };
        for case in cases {
            assert_eq!(summary(case.get("input")), *case.get("output"));
        }
    }
}
