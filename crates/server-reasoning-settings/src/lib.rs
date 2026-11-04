//! Native reasoning settings: the administrator's default write, and the read (core routes/reasoning-settings.cjs
//! GET, sbstndalton/noevia#2384): the stored default, the effort a project resolves to (reasoning-effort.cjs
//! `resolveEffort`) and the answer's shape. How the provider will be asked (`mode`) depends on the provider row, the
//! engine and machine facts and the set of providers that rejected the parameter during a chat turn, all of which
//! only the Node process has; the front asks it (core rust-reasoning-read-facts.cjs) and never guesses.
pub mod input;
use serde_json::{json, Value};
use server_store::{Reader, RustReasoningSettings, StoreError, Writer};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Default,
    Low,
    High,
}
impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Low => "low",
            Self::High => "high",
        }
    }
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Default),
            "low" => Some(Self::Low),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}
pub const KEY: &str = "reasoning_effort_default";
pub const BODY_LIMIT: usize = 1024 * 1024;
pub fn put(
    writer: &Writer,
    capability: RustReasoningSettings,
    actor: &str,
    effort: Effort,
    now: i64,
) -> Result<Value, StoreError> {
    let effort = effort.as_str();
    writer.write(|tx| {
        tx.set_reasoning_default(capability, effort)?;
        tx.execute("INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,NULL,'reasoning.default',?2,?3)", server_store::rusqlite::params![actor,json!({"effort":effort}).to_string(),now])?;
        Ok(json!({"default":effort}))
    })
}

/// reasoning-effort.cjs `EFFORTS`.
pub const EFFORTS: [&str; 3] = ["default", "low", "high"];

pub fn valid_effort(value: &str) -> bool {
    EFFORTS.contains(&value)
}

/// reasoning-effort.cjs `resolveEffort(project, globalDefault)`: the project's own effort when it is one of
/// [`EFFORTS`], else the default when that is, else `"default"`.
pub fn resolve_effort<'a>(project: Option<&'a str>, global_default: &'a str) -> &'a str {
    match project {
        Some(p) if valid_effort(p) => p,
        _ if valid_effort(global_default) => global_default,
        _ => "default",
    }
}

/// The route's `globalDefault()`: `settings.reasoning_effort_default || 'default'`. A row of another storage class than
/// text (a number or a blob in a hand-edited database), which Node would have echoed as it came, is refused instead.
pub fn stored_default(reader: &Reader<'_>) -> Result<Option<String>, StoreError> {
    let row = reader.row(
        "SELECT typeof(value), CASE WHEN typeof(value)='text' THEN value END FROM settings WHERE key=?1",
        [KEY],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
    )?;
    match row {
        None => Ok(Some("default".into())),
        Some((class, _)) if class == "null" => Ok(Some("default".into())),
        Some((_, Some(text))) if text.is_empty() => Ok(Some("default".into())),
        Some((_, Some(text))) => Ok(Some(text)),
        Some(_) => Ok(None),
    }
}

/// The answer: `{ default, effort, mode, admin }`, in the order the route built it.
pub fn answer(default: &str, effort: &str, mode: &str, admin: bool) -> String {
    let mut out = String::from("{\"default\":");
    quote(default, &mut out);
    out.push_str(",\"effort\":");
    quote(effort, &mut out);
    out.push_str(",\"mode\":");
    quote(mode, &mut out);
    out.push_str(if admin {
        ",\"admin\":true}"
    } else {
        ",\"admin\":false}"
    });
    out
}

/// JSON.stringify of a string.
fn quote(s: &str, out: &mut String) {
    // serde_json escapes exactly the characters JSON.stringify does (quotes, backslash, controls; not U+2028/9).
    out.push_str(&serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into()));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod read_tests {
    use super::*;

    #[test]
    fn effort_resolves_like_reasoning_effort_cjs() {
        assert_eq!(resolve_effort(Some("high"), "low"), "high");
        assert_eq!(resolve_effort(Some("bogus"), "low"), "low");
        assert_eq!(resolve_effort(None, "low"), "low");
        assert_eq!(resolve_effort(Some("HIGH"), "also-bogus"), "default");
        assert_eq!(resolve_effort(None, ""), "default");
    }

    #[test]
    fn the_answer_keeps_the_route_key_order_and_json_escapes() {
        assert_eq!(
            answer("low", "high", "hint", true),
            r#"{"default":"low","effort":"high","mode":"hint","admin":true}"#
        );
        assert_eq!(
            answer("a\"\n\u{2028}", "default", "off", false),
            "{\"default\":\"a\\\"\\n\u{2028}\",\"effort\":\"default\",\"mode\":\"off\",\"admin\":false}"
        );
    }
}
