//! Explicit sandbox launch configuration. Ambient credentials are never inherited.
use js_json::JValue;
fn object(rows: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(rows.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn set(rows: &mut Vec<(String, JValue)>, key: &str, value: JValue) {
    if let Some(row) = rows.iter_mut().find(|(k, _)| k == key) {
        row.1 = value;
    } else {
        rows.push((key.into(), value));
    }
}
pub fn environment(
    path: Option<&str>,
    lang: Option<&str>,
    home: Option<&str>,
    env: &JValue,
    proxy: &JValue,
) -> JValue {
    let mut rows = Vec::new();
    for (key, value) in [("PATH", path), ("LANG", lang)] {
        if let Some(value) = value {
            set(&mut rows, key, JValue::Str(value.into()));
        }
    }
    let private_home = home
        .filter(|s| !s.is_empty())
        .map(|s| JValue::Str(s.into()))
        .unwrap_or_else(|| env.get("HOME").clone());
    if !matches!(private_home, JValue::Undefined) {
        set(&mut rows, "HOME", private_home);
    }
    if env.get("TMPDIR").truthy() {
        set(&mut rows, "TMPDIR", env.get("TMPDIR").clone());
    }
    if proxy.truthy() {
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            set(&mut rows, key, proxy.get("url").clone());
        }
        set(
            &mut rows,
            "NO_PROXY",
            if proxy.get("noProxy").truthy() {
                proxy.get("noProxy").clone()
            } else {
                JValue::Str(String::new())
            },
        );
    }
    if let JValue::Obj(extra) = env {
        for (key, value) in extra {
            set(&mut rows, key, value.clone());
        }
    }
    set(&mut rows, "CURL_HOME", JValue::Str("/nonexistent".into()));
    set(&mut rows, "WGETRC", JValue::Str("/dev/null".into()));
    rows.retain(|(_, value)| !matches!(value, JValue::Undefined));
    JValue::Obj(rows)
}
#[derive(Debug, PartialEq, Eq)]
pub struct MissingSettings;
pub fn metadata(
    permission: &JValue,
    harness: Option<&str>,
    settings: &JValue,
) -> Result<JValue, MissingSettings> {
    let mut rows = Vec::new();
    if permission.truthy() {
        rows.push(("noevia", object(vec![("permission", permission.clone())])));
    }
    if harness == Some("claude-code") {
        if !matches!(settings, JValue::Obj(_)) {
            return Err(MissingSettings);
        }
        let options = object(vec![
            ("settingSources", JValue::Arr(vec![])),
            ("strictMcpConfig", JValue::Bool(true)),
            ("allowDangerouslySkipPermissions", JValue::Bool(false)),
            ("plugins", JValue::Arr(vec![])),
            (
                "tools",
                JValue::Arr(
                    [
                        "Read",
                        "Edit",
                        "Write",
                        "NotebookEdit",
                        "Bash",
                        "Glob",
                        "Grep",
                        "WebFetch",
                        "WebSearch",
                    ]
                    .into_iter()
                    .map(|s| JValue::Str(s.into()))
                    .collect(),
                ),
            ),
            (
                "extraArgs",
                object(vec![("disable-slash-commands", JValue::Null)]),
            ),
            ("settings", settings.clone()),
        ]);
        rows.push(("claudeCode", object(vec![("options", options)])));
    }
    Ok(if rows.is_empty() {
        JValue::Null
    } else {
        object(rows)
    })
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[test]
    fn launch_configuration_matches_node() {
        let fixture =
            js_json::wtf16::parse(include_str!("../tests/fixtures/acp-options-node.json")).unwrap();
        let JValue::Arr(cases) = fixture.get("cases") else {
            panic!()
        };
        for case in cases {
            let input = case.get("input");
            assert_eq!(
                environment(
                    None,
                    None,
                    input.get("home").as_str(),
                    input.get("env"),
                    input.get("proxy")
                ),
                *case.get("output")
            );
        }
        let JValue::Arr(sessions) = fixture.get("sessions") else {
            panic!()
        };
        for case in sessions {
            let input = case.get("input");
            assert_eq!(
                metadata(
                    input.get("permission"),
                    input.get("harness").as_str(),
                    input.get("settings")
                )
                .unwrap(),
                *case.get("output")
            );
        }
        assert_eq!(
            metadata(&JValue::Null, Some("claude-code"), &JValue::Null),
            Err(MissingSettings)
        );
    }
}
