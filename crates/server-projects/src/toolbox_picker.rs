//! #2281: the toolbox picker presentation, from the live chat process's raw discovery facts.
//! Account connection state is supplied by the Rust front, never by the facts producer or browser.
use js_json::{wtf16, JValue};
use std::collections::HashSet;

#[derive(Debug)]
pub struct InvalidFacts;

fn array<'a>(v: &'a JValue, key: &str) -> Result<&'a [JValue], ()> {
    match v.get(key) {
        JValue::Arr(a) => Ok(a),
        _ => Err(()),
    }
}
fn text<'a>(v: &'a JValue, key: &str) -> Result<&'a str, ()> {
    v.get(key).as_str().ok_or(())
}
fn or_null(v: &JValue) -> JValue {
    if v.truthy() {
        v.clone()
    } else {
        JValue::Null
    }
}

/// Node's `toolboxSummaries` and `describeMcpServers`, preserving order and UTF-16 string semantics.
pub fn compose(facts: &JValue, connected: &HashSet<String>) -> Result<JValue, InvalidFacts> {
    compose_inner(facts, connected).map_err(|()| InvalidFacts)
}

fn compose_inner(facts: &JValue, connected: &HashSet<String>) -> Result<JValue, ()> {
    let connector: HashSet<&str> = array(facts, "connectorBoxes")?
        .iter()
        .map(|x| x.as_str().ok_or(()))
        .collect::<Result<_, _>>()?;
    let mut boxes = Vec::new();
    for b in array(facts, "boxes")? {
        let id = text(b, "id")?;
        let is_connector = connector.contains(id);
        if is_connector && !connected.contains(id) {
            continue;
        }
        let tools = array(b, "tools")?;
        let mut row = vec![("id".to_owned(), JValue::from(id))];
        if let JValue::Obj(fields) = b {
            for key in ["label", "description", "source"] {
                if let Some((_, v)) = fields.iter().find(|(k, _)| k == key) {
                    row.push((key.to_owned(), v.clone()));
                }
            }
        } else {
            return Err(());
        }
        row.push((
            "inApp".into(),
            JValue::Bool(
                b.get("source").as_str() == Some("builtin")
                    || b.get("inApp").as_bool() == Some(true),
            ),
        ));
        row.push(("toolCount".into(), JValue::Num(tools.len() as f64)));
        let tokens = if tools.is_empty() {
            0.0
        } else {
            let json = wtf16::stringify(&JValue::Arr(tools.to_vec())).ok_or(())?;
            (wtf16::len(&json) as f64 / 3.6).round()
        };
        row.push(("estTokens".into(), JValue::Num(tokens)));
        if is_connector {
            row.push(("connector".into(), JValue::Bool(true)));
        }
        boxes.push(JValue::Obj(row));
    }
    let prefill = facts.get("prefill");
    let target = prefill
        .get("targetMs")
        .as_num()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .ok_or(())?;
    if !matches!(prefill.get("models"), JValue::Obj(_)) {
        return Err(());
    }
    let mcp = facts.get("mcp");
    let enabled = mcp.get("enabled").as_bool().ok_or(())?;
    let mut status = vec![("configured".to_owned(), JValue::Bool(enabled))];
    if enabled {
        let tools = array(mcp, "tools")?;
        let states = array(mcp, "states")?;
        let manifest = array(mcp, "manifest")?;
        let mut servers = Vec::new();
        for server in array(mcp, "servers")? {
            let id = text(server, "id")?;
            let auth = text(server, "auth")?;
            if ![
                "none",
                "bearer",
                "nextcloud",
                "personal",
                "oauth",
                "internal",
                "directory",
            ]
            .contains(&auth)
            {
                return Err(());
            }
            let state = states.iter().find(|s| s.get("id").as_str() == Some(id));
            if state.is_some_and(|s| !matches!(s.get("error"), JValue::Null | JValue::Str(_))) {
                return Err(());
            }
            let mut expected = HashSet::new();
            for box_ in manifest
                .iter()
                .filter(|b| b.get("server").as_str() == Some(id))
            {
                for name in array(box_, "tools")? {
                    expected.insert(name.as_str().ok_or(())?);
                }
            }
            let missing = expected
                .iter()
                .filter(|name| {
                    !tools.iter().any(|t| {
                        t.get("name").as_str() == Some(**name)
                            && t.get("serverId").as_str() == Some(id)
                    })
                })
                .count();
            let mut row = vec![
                ("id".to_owned(), JValue::from(id)),
                ("auth".to_owned(), JValue::from(auth)),
                (
                    "error".to_owned(),
                    state.map_or(JValue::Null, |s| or_null(s.get("error"))),
                ),
                (
                    "discovered".to_owned(),
                    state.map_or(JValue::Num(0.0), |s| {
                        if s.get("toolCount").truthy() {
                            s.get("toolCount").clone()
                        } else {
                            JValue::Num(0.0)
                        }
                    }),
                ),
                (
                    "checkedAt".to_owned(),
                    state.map_or(JValue::Null, |s| or_null(s.get("discoveredAt"))),
                ),
                ("missingCurated".to_owned(), JValue::Num(missing as f64)),
                (
                    "directory".to_owned(),
                    JValue::Bool(server.get("directory").truthy()),
                ),
            ];
            if let Some(title) = server.get("title").as_str().filter(|t| !t.is_empty()) {
                row.push(("title".into(), JValue::Str(wtf16::slice(title, 80))));
            }
            servers.push(JValue::Obj(row));
        }
        if !matches!(mcp.get("error"), JValue::Null | JValue::Str(_)) {
            return Err(());
        }
        status.extend([
            ("error".into(), mcp.get("error").clone()),
            ("discovered".into(), JValue::Num(tools.len() as f64)),
            ("servers".into(), JValue::Arr(servers)),
        ]);
    }
    Ok(JValue::obj([
        ("toolboxes", JValue::Arr(boxes)),
        (
            "prefill",
            JValue::obj([
                ("targetMs", JValue::Num(target)),
                ("models", prefill.get("models").clone()),
            ]),
        ),
        ("mcp", JValue::Obj(status)),
    ]))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    #[test]
    fn matches_real_nodes_picker_summary_status_and_utf16_costs() {
        let fixture =
            wtf16::parse(include_str!("../tests/fixtures/toolbox-picker-node.json")).unwrap();
        let JValue::Arr(cases) = fixture else {
            panic!("fixture array");
        };
        for case in cases {
            let connected = array(&case, "connected")
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap().to_owned())
                .collect();
            let result = compose(case.get("facts"), &connected).unwrap();
            assert_eq!(
                wtf16::stringify(&result),
                wtf16::stringify(case.get("expected"))
            );
            let json = wtf16::stringify(&result).unwrap();
            assert!(!json.contains("never-expose") && !json.contains("not-for-clients"));
        }
    }
    #[test]
    fn extra_credential_configuration_is_never_a_public_field() {
        let fixture =
            wtf16::parse(include_str!("../tests/fixtures/toolbox-picker-node.json")).unwrap();
        let JValue::Arr(cases) = fixture else {
            panic!("fixture array");
        };
        let mut facts = cases[0].get("facts").clone();
        fn field_mut<'a>(value: &'a mut JValue, key: &str) -> &'a mut JValue {
            let JValue::Obj(fields) = value else {
                panic!("object");
            };
            &mut fields.iter_mut().find(|(k, _)| k == key).unwrap().1
        }
        let JValue::Arr(servers) = field_mut(field_mut(&mut facts, "mcp"), "servers") else {
            panic!("servers");
        };
        for server in servers {
            let JValue::Obj(fields) = server else {
                panic!("server object");
            };
            for key in ["url", "headers", "token", "accessToken", "clientSecret"] {
                fields.push((
                    key.into(),
                    JValue::from(
                        "https://user:never-expose@example.test/mcp?token=not-for-clients",
                    ),
                ));
            }
        }
        let body = compose(&facts, &HashSet::new()).unwrap();
        let text = wtf16::stringify(&body).unwrap();
        assert!(!text.contains("never-expose") && !text.contains("not-for-clients"));
    }
    #[test]
    fn malformed_facts_are_not_a_successful_empty_picker() {
        for text in [
            "null",
            "{}",
            r#"{"connectorBoxes":[],"boxes":[{"id":"x","tools":null}]}"#,
        ] {
            assert!(compose(&wtf16::parse(text).unwrap(), &HashSet::new()).is_err());
        }
    }
}
