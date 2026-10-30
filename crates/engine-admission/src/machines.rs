//! core inference-machines.cjs `readState` then `chatTarget(role)`, read-only: the `inference:machines`
//! settings row is Node's to write (#1638, #1840). A damaged or unknown value moves nothing: every role
//! stays on this machine. The address is judged at connect time (model_stream::net), not here.
use js_json::{wtf16, JValue};

const LOCAL_ID: &str = "local";

/// A chat role on another machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub role: String,
    pub machine_id: String,
    pub machine_name: String,
    pub port: u32,
    pub model: String,
    /// `http(s)://host:port`.
    pub base_url: String,
}

fn clean_model(raw: &JValue) -> Option<String> {
    let model = raw.as_str()?;
    let model = model.trim_matches(|c: char| {
        matches!(
            c,
            '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        )
    });
    let units = wtf16::len(model);
    let ok =
        (1..=200).contains(&units) && !model.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f);
    ok.then(|| model.to_owned())
}

fn service_of(role: &str) -> Option<&'static str> {
    match role {
        "fast" => Some("chatFast"),
        "smart" => Some("chatSmart"),
        "code" => Some("chatCode"),
        "creative" => Some("chatCreative"),
        "vision" => Some("vision"),
        _ => None,
    }
}

/// `row` is the settings value's text (`None`: no row).
pub fn chat_target(row: Option<&str>, role: &str) -> Option<Target> {
    let service = service_of(role)?;
    let state = wtf16::parse(row?).ok()?;
    let assignments = state.get("assignments");
    if !state.truthy()
        || state.get("version").as_num() != Some(1.0)
        || !matches!(assignments, JValue::Obj(_))
    {
        return None;
    }
    let JValue::Arr(machines) = state.get("machines") else {
        return None;
    };
    let a = assignments.get(service);
    let machine_id = a.get("machine").as_str()?;
    let port = a.get("port").as_num().filter(|p| p.fract() == 0.0)?;
    let model = clean_model(a.get("model"))?;
    let machine = machines.iter().find(|m| {
        m.get("id").as_str() == Some(machine_id)
            && machine_id != LOCAL_ID
            && m.get("name").as_str().is_some()
            && m.get("address").as_str().is_some()
    })?;
    let address = machine.get("address").as_str()?;
    let port_text = js_json::number_to_string(port);
    Some(Target {
        role: role.to_owned(),
        machine_id: machine_id.to_owned(),
        machine_name: machine.get("name").as_str()?.to_owned(),
        port: port as u32,
        model,
        base_url: format!("{address}:{port_text}"),
    })
}

/// core chat-machines.cjs `connectError(t, err)` for a socket failure: `None` is no detail. Node's text.
pub fn connect_error(t: &Target, refused_address: bool, redirect: bool) -> String {
    let detail = if refused_address {
        ": its address is not on this network"
    } else if redirect {
        ": it answered with a redirect, which is never followed"
    } else {
        ""
    };
    let role = label(&t.role);
    format!(
        "{role} model\u{2019}s machine ({}) is unreachable{detail}. Check that it is on and serving {} on port {}, or move the {role} role back to this machine in Settings \u{2192} Inference machines.",
        t.machine_name, t.model, t.port
    )
}

fn label(role: &str) -> &str {
    match role {
        "fast" => "Fast",
        "smart" => "Smart",
        "code" => "Code",
        "creative" => "Creative",
        "vision" => "Vision",
        other => other,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const ROW: &str = r#"{"version":1,"machines":[{"id":"m1","name":"Rock","address":"http://rock.lan"},{"id":"local","name":"x","address":"http://x"}],"assignments":{"chatSmart":{"machine":"m1","port":8080,"model":" kev "},"chatFast":{"machine":"gone","port":1,"model":"x"},"chatCode":{"machine":"m1","port":1.5,"model":"x"},"chatCreative":{"machine":"m1","port":1},"vision":{"machine":"local","port":1,"model":"x"}}}"#;

    #[test]
    fn a_role_on_a_machine_and_every_unusable_assignment() {
        let t = chat_target(Some(ROW), "smart").unwrap();
        assert_eq!(t.base_url, "http://rock.lan:8080");
        assert_eq!(t.model, "kev");
        assert_eq!(t.machine_name, "Rock");
        for role in ["fast", "code", "creative", "vision", "nope"] {
            assert_eq!(chat_target(Some(ROW), role), None, "{role}");
        }
    }

    #[test]
    fn a_damaged_row_moves_nothing() {
        for row in [
            "",
            "not json",
            "null",
            "[]",
            r#"{"version":2,"machines":[],"assignments":{}}"#,
            r#"{"version":1,"machines":{},"assignments":{}}"#,
            r#"{"version":1,"machines":[],"assignments":null}"#,
        ] {
            assert_eq!(chat_target(Some(row), "smart"), None, "{row}");
        }
        assert_eq!(chat_target(None, "smart"), None);
    }

    #[test]
    fn the_connect_error_says_core_s_words() {
        let t = chat_target(Some(ROW), "smart").unwrap();
        let text = connect_error(&t, true, false);
        assert!(text.starts_with("Smart model\u{2019}s machine (Rock) is unreachable: its address is not on this network. Check"), "{text}");
        assert!(connect_error(&t, false, true).contains("a redirect"));
        assert!(connect_error(&t, false, false).contains("unreachable. Check"));
    }
}
