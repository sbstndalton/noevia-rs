//! GET-only auto-tune status composition from bounded in-memory owner data.
use crate::autotune_settings as domain;
use js_json::JValue;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    InvalidFacts,
}
pub fn keys(model: &str) -> Vec<String> {
    let mut out = vec![model.to_owned()];
    if !model.is_empty() {
        out.push(domain::long_id(model));
    }
    out
}
fn hidden(key: &str) -> bool {
    key.starts_with('_')
        || matches!(
            key,
            "originalText" | "lastRevision" | "originalRevision" | "beforeText"
        )
}
fn http_config(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        normalized.as_str(),
        "headers"
            | "authorization"
            | "apikey"
            | "accesstoken"
            | "refreshtoken"
            | "httpconfig"
            | "requestconfig"
            | "baseurl"
            | "url"
            | "credentials"
            | "password"
            | "secret"
            | "token"
            | "clientsecret"
    )
}
fn tree(value: &JValue) -> Result<JValue, Fault> {
    match value {
        JValue::Obj(fields) => {
            let mut out = Vec::new();
            for (k, v) in fields {
                if hidden(k) {
                    continue;
                }
                if http_config(k) {
                    return Err(Fault::InvalidFacts);
                }
                out.push((k.clone(), tree(v)?));
            }
            Ok(JValue::Obj(out))
        }
        JValue::Arr(a) => Ok(JValue::Arr(
            a.iter().map(tree).collect::<Result<Vec<_>, _>>()?,
        )),
        v => Ok(v.clone()),
    }
}
const JOB_KEYS: &[&str] = &[
    "id",
    "model",
    "bulk",
    "mode",
    "promptBudgetSeconds",
    "status",
    "phase",
    "startedAt",
    "log",
    "models",
    "queueProgress",
    "finishedAt",
    "error",
    "failedPhase",
    "failedStep",
    "restored",
    "skipped",
    "waiting",
    "result",
    "queue",
    "steps",
];
const HISTORY_KEYS: &[&str] = &[
    "at",
    "kv",
    "context",
    "plan",
    "probes",
    "kvFallback",
    "spec",
    "specLabel",
    "generation",
    "acceptance",
    "ubatch",
    "promptPerSecond",
    "quality",
    "extensions",
    "sampling",
    "baseline",
    "serving",
    "loaded",
    "version",
    "signature",
];
fn selected(value: &JValue, keys: &[&str]) -> Result<JValue, Fault> {
    if !matches!(value, JValue::Obj(_)) {
        return Err(Fault::InvalidFacts);
    }
    tree(&JValue::Obj(
        keys.iter()
            .filter_map(|k| {
                let v = value.get(k);
                if matches!(v, JValue::Undefined) {
                    None
                } else {
                    Some(((*k).to_owned(), v.clone()))
                }
            })
            .collect(),
    ))
}
fn history(value: &JValue) -> Result<JValue, Fault> {
    if !value.truthy() {
        return Ok(JValue::Arr(vec![]));
    }
    let JValue::Arr(entries) = value else {
        return Err(Fault::InvalidFacts);
    };
    Ok(JValue::Arr(
        entries
            .iter()
            .take(10)
            .map(|v| selected(v, HISTORY_KEYS))
            .collect::<Result<Vec<_>, _>>()?,
    ))
}
pub fn response(model: &str, facts: &JValue) -> Result<JValue, Fault> {
    let JValue::Arr(entries) = facts.get("entries") else {
        return Err(Fault::InvalidFacts);
    };
    let requested = keys(model);
    if entries.len() != requested.len() {
        return Err(Fault::InvalidFacts);
    }
    for (entry, key) in entries.iter().zip(&requested) {
        if entry.get("key").as_str() != Some(key.as_str()) {
            return Err(Fault::InvalidFacts);
        }
    }
    let row = entries.first().ok_or(Fault::InvalidFacts)?;
    let allow = row.get("allowQ5Kv").as_bool().ok_or(Fault::InvalidFacts)?;
    let below = facts
        .get("allowBelowQ5Kv")
        .as_bool()
        .ok_or(Fault::InvalidFacts)?;
    let mut out = Vec::new();
    match facts
        .get("jobPresent")
        .as_bool()
        .ok_or(Fault::InvalidFacts)?
    {
        true => {
            let job = facts.get("job");
            out.push((
                "job".to_owned(),
                if matches!(job, JValue::Null) {
                    JValue::Null
                } else {
                    selected(job, JOB_KEYS)?
                },
            ));
        }
        false => {
            if !matches!(facts.get("job"), JValue::Undefined | JValue::Null) {
                return Err(Fault::InvalidFacts);
            }
        }
    }
    out.push((
        "history".into(),
        if model.is_empty() {
            JValue::Arr(vec![])
        } else {
            history(row.get("history"))?
        },
    ));
    out.push(("kvCandidates".into(), domain::kv_candidates(allow, below)));
    out.push((
        "settings".into(),
        JValue::obj([("allowQ5Kv", JValue::Bool(allow))]),
    ));
    out.push(("modes".into(), domain::modes()));
    if !model.is_empty() {
        out.push(("longId".into(), JValue::Str(domain::long_id(model))));
        out.push((
            "longHistory".into(),
            history(entries.get(1).ok_or(Fault::InvalidFacts)?.get("history"))?,
        ));
    }
    Ok(JValue::Obj(out))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use js_json::wtf16;
    #[test]
    fn actual_old_get_responses_match_without_engine_or_worker_calls() {
        let JValue::Arr(cases) =
            wtf16::parse(include_str!("../tests/fixtures/autotune-read-node.json")).unwrap()
        else {
            panic!("fixture")
        };
        assert_eq!(cases.len(), 13);
        for case in cases {
            let model = case.get("model").as_str().unwrap();
            let state = case.get("state");
            let entries = keys(model)
                .into_iter()
                .map(|key| {
                    JValue::obj([
                        ("key", JValue::Str(key.clone())),
                        ("history", state.get("history").get(&key).clone()),
                        (
                            "allowQ5Kv",
                            JValue::Bool(
                                state.get("settings").get(&key).get("allowQ5Kv").as_bool()
                                    == Some(true),
                            ),
                        ),
                    ])
                })
                .collect();
            let facts = JValue::obj([
                (
                    "jobPresent",
                    JValue::Bool(!matches!(state.get("job"), JValue::Undefined)),
                ),
                ("job", state.get("job").clone()),
                ("entries", JValue::Arr(entries)),
                ("allowBelowQ5Kv", case.get("below").clone()),
            ]);
            let actual = response(model, &facts).unwrap();
            let expected = case.get("expected").get("body");
            let (JValue::Obj(a), JValue::Obj(b)) = (&actual, expected) else {
                panic!("response object")
            };
            assert_eq!(a.len(), b.len(), "{:?}", case.get("name"));
            for (key, value) in b {
                assert_eq!(actual.get(key), value, "{:?}/{key}", case.get("name"));
            }
        }
    }
    fn facts(history: JValue) -> JValue {
        JValue::obj([
            ("jobPresent", JValue::Bool(false)),
            (
                "entries",
                JValue::Arr(vec![
                    JValue::obj([
                        ("key", JValue::from("m")),
                        ("history", history),
                        ("allowQ5Kv", JValue::Bool(false)),
                    ]),
                    JValue::obj([
                        ("key", JValue::from("m-long")),
                        ("history", JValue::Null),
                        ("allowQ5Kv", JValue::Bool(false)),
                    ]),
                ]),
            ),
            ("allowBelowQ5Kv", JValue::Bool(false)),
        ])
    }
    #[test]
    fn native_owns_history_bound_and_empty_model_omits_long_fields() {
        let many = JValue::Arr(
            (0..12)
                .map(|n| {
                    JValue::obj([
                        ("at", JValue::Num(n as f64)),
                        ("context", JValue::Num(8192.0)),
                    ])
                })
                .collect(),
        );
        let body = response("m", &facts(many)).unwrap();
        let JValue::Arr(history) = body.get("history") else {
            panic!("history")
        };
        assert_eq!(history.len(), 10);
        let empty = JValue::obj([
            ("jobPresent", JValue::Bool(false)),
            (
                "entries",
                JValue::Arr(vec![JValue::obj([
                    ("key", JValue::from("")),
                    ("history", JValue::Null),
                    ("allowQ5Kv", JValue::Bool(false)),
                ])]),
            ),
            ("allowBelowQ5Kv", JValue::Bool(false)),
        ]);
        let body = response("", &empty).unwrap();
        assert!(matches!(body.get("job"), JValue::Undefined));
        assert!(matches!(body.get("longId"), JValue::Undefined));
        assert!(matches!(body.get("longHistory"), JValue::Undefined));
    }
    #[test]
    fn malformed_credentials_and_wrong_selectors_are_never_public() {
        let secret =
            wtf16::parse(r#"[{"at":1,"quality":{"Authorization":"never-public"}}]"#).unwrap();
        assert!(response("m", &facts(secret)).is_err());
        assert!(response("other", &facts(JValue::Null)).is_err());
        let mut f = facts(JValue::Null);
        if let JValue::Obj(fields) = &mut f {
            fields.retain(|(k, _)| k != "allowBelowQ5Kv");
        }
        assert!(response("m", &f).is_err());
    }
}
