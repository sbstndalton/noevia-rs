//! Native host projection around the existing code-actions policy. A policy refusal retains
//! the same human-approval floor and never acquires standing authority.
use js_json::{wtf16, JValue};
fn object(rows: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(rows.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn text(s: &str) -> JValue {
    JValue::Str(s.into())
}
fn strings(values: &[String]) -> JValue {
    JValue::Arr(values.iter().map(|s| text(s)).collect())
}
fn set(value: &mut JValue, key: &str, item: JValue) {
    if let JValue::Obj(rows) = value {
        if let Some(row) = rows.iter_mut().find(|(k, _)| k == key) {
            row.1 = item;
        } else {
            rows.push((key.into(), item));
        }
    }
}
const COMMANDS: &[&str] = &["command", "cmd", "script", "shell", "commandLine", "args"];
fn call(op: u8, args: Vec<JValue>) -> Result<JValue, String> {
    let encoded = wtf16::stringify(&JValue::Arr(args)).ok_or_else(|| "input".to_owned())?;
    let mut wire = vec![op];
    wire.extend_from_slice(encoded.as_bytes());
    let (status, reply) = code_actions::call(&wire);
    let decoded = wtf16::parse(&String::from_utf8_lossy(&reply)).map_err(|_| "input".to_owned())?;
    if status == 0 {
        Ok(decoded)
    } else {
        Err(decoded.get("error").as_str().unwrap_or("input").into())
    }
}
fn projection(value: &JValue) -> JValue {
    match value {
        JValue::Str(_) => value.clone(),
        JValue::Arr(rows) => JValue::Arr(
            rows.iter()
                .map(|v| match v {
                    JValue::Str(_) | JValue::Bool(_) | JValue::Null => v.clone(),
                    JValue::Num(n) if n.is_finite() => v.clone(),
                    _ => object(vec![]),
                })
                .collect(),
        ),
        _ => JValue::Null,
    }
}
fn project(tool: &JValue) -> JValue {
    let raw = tool.get("rawInput");
    let raw = if matches!(raw, JValue::Obj(_) | JValue::Arr(_)) {
        let mut out = object(vec![]);
        for key in COMMANDS {
            if raw.get(key) != &JValue::Undefined {
                set(&mut out, key, projection(raw.get(key)));
            }
        }
        set(
            &mut out,
            "noeviaOutsideWorkspace",
            JValue::Bool(raw.get("noeviaOutsideWorkspace") == &JValue::Bool(true)),
        );
        out
    } else {
        JValue::Null
    };
    let locations = match tool.get("locations") {
        JValue::Arr(rows) => rows
            .iter()
            .map(|v| {
                if v.get("path").as_str().is_some() {
                    v.get("path").clone()
                } else {
                    JValue::Null
                }
            })
            .collect(),
        _ => vec![],
    };
    object(vec![
        (
            "kind",
            if tool.get("kind").as_str().is_some() {
                tool.get("kind").clone()
            } else {
                JValue::Null
            },
        ),
        ("rawInput", raw),
        ("locations", JValue::Arr(locations)),
    ])
}
fn command(tool: &JValue) -> String {
    let raw = tool.get("rawInput");
    for key in COMMANDS {
        let candidate = match raw.get(key) {
            JValue::Str(s) if *key != "args" => s.clone(),
            JValue::Arr(rows) if !rows.is_empty() => {
                let values: Result<Vec<_>, _> = rows.iter().map(js_json::to_js_string).collect();
                let Ok(values) = values else {
                    return String::new();
                };
                values.join(" ")
            }
            _ => continue,
        };
        let trimmed = server_account::util::js_trim(&candidate);
        if !trimmed.is_empty() {
            return trimmed.into();
        }
    }
    String::new()
}
fn why(fault: &str) -> &str {
    match fault {
        "too_large" => "too large or complex to check",
        "input" => "its arguments have a form that cannot be checked",
        "ambiguous" => "it names a host that cannot be read exactly",
        _ => "the checker did not answer",
    }
}
pub fn classify(tool: &JValue) -> JValue {
    let result = if tool == &JValue::Null {
        Err("unexpected".to_owned())
    } else {
        call(1, vec![project(tool)])
    };
    match result {
        Ok(result) => result,
        Err(fault) => {
            let command = command(tool);
            let kind = tool.get("kind").as_str().unwrap_or("");
            let kind_class = match kind {
                "read" | "search" => "read_repository",
                "edit" | "move" => "edit_file",
                "delete" => "delete",
                "fetch" => "network",
                "think" => "none",
                _ => "execute_command",
            };
            let mut actions = vec!["external_account".to_owned()];
            if kind == "execute" || !command.is_empty() {
                actions.push("execute_command".into());
            }
            if !actions.iter().any(|s| s == kind_class) {
                actions.push(kind_class.into());
            }
            let mut paths = Vec::new();
            if let JValue::Arr(rows) = tool.get("locations") {
                for row in rows {
                    if let Some(path) = row.get("path").as_str().filter(|s| !s.is_empty()) {
                        if !paths.iter().any(|p| p == path) {
                            paths.push(path.to_owned());
                        }
                    }
                }
            }
            let mut out = object(vec![
                ("action", text("external_account")),
                ("approval", text("always")),
                ("command", text(&command)),
                ("paths", strings(&paths)),
                ("readable", JValue::Bool(false)),
                ("actions", strings(&actions)),
                ("simple", JValue::Bool(false)),
                ("standable", JValue::Bool(false)),
                ("unverified", JValue::Bool(true)),
                ("unverifiedWhy", text(why(&fault))),
            ]);
            if tool.get("rawInput").get("noeviaOutsideWorkspace") == &JValue::Bool(true) {
                set(&mut out, "outsideWorkspace", JValue::Bool(true));
            }
            out
        }
    }
}
fn verdict(decision: &str, reason: &str) -> JValue {
    object(vec![("decision", text(decision)), ("reason", text(reason))])
}
fn floor(classified: &JValue, capabilities: &[String], inside: Option<bool>) -> Option<JValue> {
    let action = classified.get("action").as_str().unwrap_or("");
    let all = match classified.get("actions") {
        JValue::Arr(rows) if !rows.is_empty() => {
            rows.iter().filter_map(JValue::as_str).collect::<Vec<_>>()
        }
        _ => vec![action],
    };
    let paths = matches!(classified.get("paths"),JValue::Arr(rows)if !rows.is_empty());
    if all.iter().any(|s| matches!(*s, "edit_file" | "delete")) && paths && inside == Some(false) {
        return Some(verdict(
            "deny",
            "The path is outside this task’s workspace.",
        ));
    }
    if let Some(missing) = all.into_iter().find(|s| {
        *s != "none"
            && *s != "read_repository"
            && !capabilities.is_empty()
            && !capabilities.iter().any(|c| c == s)
    }) {
        let suffix = if classified.get("unverified") == &JValue::Bool(true) {
            " (This command could not be checked, so it is treated as the most consequential kind of action.)"
        } else {
            ""
        };
        return Some(verdict(
            "deny",
            &format!(
                "This task was not granted {}.{suffix}",
                missing.replace('_', " ")
            ),
        ));
    }
    None
}
pub fn decide(
    classified: &JValue,
    capabilities: &[String],
    domains: &[String],
    inside: Option<bool>,
) -> JValue {
    let unverified = classified.get("unverified") == &JValue::Bool(true);
    let args = || {
        vec![
            classified.clone(),
            strings(capabilities),
            strings(domains),
            inside.map_or(JValue::Null, JValue::Bool),
        ]
    };
    let fault = if unverified {
        None
    } else {
        match call(2, args()) {
            Ok(result) => return result,
            Err(fault) => Some(fault),
        }
    };
    if let Some(denied) = floor(classified, capabilities, inside) {
        return denied;
    }
    if fault.as_deref() == Some("ambiguous")
        && classified.get("approval").as_str() == Some("capability")
    {
        let mut projected = classified.clone();
        set(&mut projected, "command", text(""));
        if let Ok(again) = call(
            2,
            vec![
                projected,
                strings(capabilities),
                strings(domains),
                inside.map_or(JValue::Null, JValue::Bool),
            ],
        ) {
            if again.get("decision").as_str() == Some("deny") {
                return again;
            }
        }
    }
    let reason = if unverified {
        let where_ = if classified.get("outsideWorkspace") == &JValue::Bool(true) {
            " It reaches outside the task’s workspace."
        } else {
            " The paths its command would touch are unknown, so the task’s workspace limits could not be checked either."
        };
        format!(
            "This action could not be checked ({}), so a person has to approve it.{where_}",
            classified
                .get("unverifiedWhy")
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or("the checker did not answer")
        )
    } else {
        "This action could not be verified, so a person has to approve it.".into()
    };
    let mut out = verdict("ask", &reason);
    set(&mut out, "standable", JValue::Bool(false));
    out
}
pub fn pick(options: &JValue, wanted: &str) -> JValue {
    let rows = match options {
        JValue::Arr(rows) => rows
            .iter()
            .map(|row| {
                if row.get("optionId").as_str().is_some() {
                    object(vec![
                        ("optionId", row.get("optionId").clone()),
                        (
                            "kind",
                            if row.get("kind").as_str().is_some() {
                                row.get("kind").clone()
                            } else {
                                JValue::Null
                            },
                        ),
                    ])
                } else {
                    JValue::Null
                }
            })
            .collect(),
        _ => vec![],
    };
    call(3, vec![JValue::Arr(rows), text(wanted)])
        .unwrap_or_else(|_| object(vec![("outcome", text("cancelled"))]))
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn policy_projection_and_refusal_floor_match_node() {
        let fixture = wtf16::parse(include_str!("../tests/fixtures/policy-node.json")).unwrap();
        let JValue::Arr(cases) = fixture.get("cases") else {
            panic!()
        };
        assert_eq!(cases.len(), 117);
        for (index, case) in cases.iter().enumerate() {
            let classified = classify(case.get("call"));
            assert_eq!(
                classified,
                *case.get("classified"),
                "classification {index}"
            );
            let list = |key| {
                let JValue::Arr(rows) = case.get(key) else {
                    panic!()
                };
                rows.iter()
                    .map(|s| s.as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                decide(
                    &classified,
                    &list("capabilities"),
                    &list("domains"),
                    case.get("inside").as_bool()
                ),
                *case.get("verdict"),
                "verdict {index}"
            );
        }
        let JValue::Arr(picks) = fixture.get("picks") else {
            panic!()
        };
        for row in picks {
            assert_eq!(
                pick(row.get("options"), row.get("wanted").as_str().unwrap()),
                *row.get("output")
            );
        }
    }
}
