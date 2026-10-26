//! Native account routing configuration; does not execute chats or access providers.
pub mod input;
pub mod storage;
use input::Json;
use serde_json::{json, Value};
use server_store::{Reader, RustRoutingMode, StoreError, Writer};
pub const BODY_LIMIT: usize = 1024 * 1024;
pub const FILE: &str = "routing-mode.json";
pub const KEY: &str = "routing_modes_allowed";
pub const FEATURE_KEY: &str = server_features::spec(server_features::Feature::RoutingModes).setting;
pub const MODES: [&str; 3] = ["local", "cloud", "hybrid"];
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub status: u16,
    pub message: String,
}
fn err(status: u16, msg: impl Into<String>) -> Error {
    Error {
        status,
        message: msg.into(),
    }
}
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(|c| matches!(c,'\u{9}'..='\u{d}'|'\u{20}'|'\u{a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}'))
}
pub fn feature_env(raw: Option<&str>) -> Result<Option<bool>, Error> {
    server_features::parse_env(raw).map_err(|message| err(503, message))
}
pub fn enabled(reader: &Reader<'_>, env: Option<bool>) -> Result<bool, StoreError> {
    if let Some(value) = env {
        return Ok(server_features::evaluate(
            server_features::Feature::RoutingModes,
            Some(value),
            None,
            true,
        )
        .enabled);
    }
    let stored = reader
        .row(
            "SELECT CASE WHEN typeof(value)='text' AND value IN ('true','false') THEN value ELSE NULL END FROM settings WHERE key=?1",
            [FEATURE_KEY],
            |r| r.get::<_, Option<String>>(0),
        )?
        .flatten();
    Ok(server_features::evaluate(
        server_features::Feature::RoutingModes,
        env,
        stored.as_deref(),
        true,
    )
    .enabled)
}
pub fn allowed(reader: &Reader<'_>) -> Result<Vec<String>, StoreError> {
    // Bound allocation at SQL boundary; do not materialize an unbounded settings value.
    let row=reader.row("SELECT typeof(value),length(CAST(value AS BLOB)),substr(CAST(value AS TEXT),1,1048577) FROM settings WHERE key='routing_modes_allowed'",[],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?)))?;
    let Some((kind, len, text)) = row else {
        return Ok(all_modes());
    };
    if len > BODY_LIMIT as i64 {
        return Err(StoreError::NotOwned("routing state over limit".into()));
    }
    if kind != "text" {
        return Ok(all_modes());
    }
    Ok(allowed_text(&text))
}
pub fn all_modes() -> Vec<String> {
    MODES.into_iter().map(str::to_owned).collect()
}
pub fn allowed_text(text: &str) -> Vec<String> {
    Json::parse(text.as_bytes())
        .ok()
        .and_then(|j| j.array())
        .map(|a| {
            MODES
                .into_iter()
                .filter(|m| a.iter().any(|v| v.string().as_deref() == Some(*m)))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(all_modes)
}
pub fn validate_allowed(body: &Json) -> Result<Vec<String>, Error> {
    let fail = || err(400, "allowed must list 'local', 'cloud' and/or 'hybrid'");
    let a = body
        .get("allowed")
        .and_then(|j| j.array())
        .ok_or_else(fail)?;
    if a.iter()
        .any(|j| !j.string().is_some_and(|s| MODES.contains(&s.as_str())))
    {
        return Err(fail());
    }
    Ok(MODES
        .into_iter()
        .filter(|m| a.iter().any(|v| v.string().as_deref() == Some(*m)))
        .map(str::to_owned)
        .collect())
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub mode: Option<String>,
    pub sensitive: String,
    pub provider: String,
    pub models: [String; 3],
}
fn provider(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn model(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/@+-".contains(&b))
}
fn field(j: Option<Json>, key: &str) -> Option<String> {
    j.and_then(|j| j.get(key)).and_then(|j| j.string())
}
pub fn normalize(raw: Option<&Json>) -> Setting {
    let j = raw.filter(|j| j.object()).cloned();
    let mode = field(j.clone(), "mode").filter(|s| MODES.contains(&s.as_str()));
    let sensitive = field(j.clone(), "whenSensitive")
        .filter(|s| ["ask", "local"].contains(&s.as_str()))
        .unwrap_or_else(|| "ask".into());
    let cloud = j.and_then(|j| j.get("cloud"));
    let provider = field(cloud.clone(), "providerId")
        .filter(|s| provider(s))
        .unwrap_or_default();
    let models = ["fast", "smart", "code"].map(|r| {
        field(cloud.clone(), r)
            .map(|s| js_trim(&s).to_owned())
            .filter(|s| model(s))
            .unwrap_or_default()
    });
    Setting {
        mode,
        sensitive,
        provider,
        models,
    }
}
pub fn validate(body: &Json, allowed: &[String]) -> Result<Setting, Error> {
    if !body.object() {
        return Err(err(400, "request body must be a JSON object"));
    }
    let mode = body.get("mode");
    if !mode
        .as_ref()
        .is_some_and(|m| m.null() || m.string().is_some_and(|s| MODES.contains(&s.as_str())))
    {
        return Err(err(400, "mode must be 'local', 'cloud', 'hybrid' or null"));
    }
    if mode
        .and_then(|m| m.string())
        .is_some_and(|s| !allowed.contains(&s))
    {
        return Err(err(
            403,
            "An administrator has not allowed this routing mode.",
        ));
    }
    if body.get("whenSensitive").is_some_and(|j| {
        !j.string()
            .is_some_and(|s| ["ask", "local"].contains(&s.as_str()))
    }) {
        return Err(err(400, "whenSensitive must be 'ask' or 'local'"));
    }
    let cloud = body.get("cloud");
    if cloud.as_ref().is_some_and(|j| !j.object()) {
        return Err(err(400, "cloud must be an object"));
    }
    if cloud
        .as_ref()
        .and_then(|j| j.get("providerId"))
        .is_some_and(|j| !j.string().is_some_and(|s| s.is_empty() || provider(&s)))
    {
        return Err(err(400, "cloud.providerId is not a provider id"));
    }
    for role in ["fast", "smart", "code"] {
        if cloud.as_ref().and_then(|j| j.get(role)).is_some_and(|j| {
            !j.string()
                .is_some_and(|s| s.is_empty() || model(js_trim(&s)))
        }) {
            return Err(err(400, format!("cloud.{role} must be a model id")));
        }
    }
    let next = normalize(Some(body));
    if next.mode.as_deref() == Some("cloud")
        && (next.provider.is_empty() || next.models.iter().all(String::is_empty))
    {
        return Err(err(
            400,
            "Cloud routing needs a cloud provider and at least one model.",
        ));
    }
    Ok(next)
}
impl Setting {
    pub fn value(&self) -> Value {
        let [fast, smart, code] = &self.models;
        json!({"mode":self.mode,"whenSensitive":self.sensitive,"cloud":{"providerId":self.provider,"fast":fast,"smart":smart,"code":code}})
    }
    pub fn bytes(&self) -> String {
        // Fields are enum/ASCII after normalization; exact Node compact property order.
        let [fast, smart, code] = &self.models;
        format!("{{\"mode\":{},\"whenSensitive\":{},\"cloud\":{{\"providerId\":{},\"fast\":{},\"smart\":{},\"code\":{}}}}}",json!(self.mode),json!(self.sensitive),json!(self.provider),json!(fast),json!(smart),json!(code))
    }
    pub fn view(&self, allowed: Vec<String>, admin: bool) -> Value {
        let mut v = self.value();
        let effective = self.mode.as_ref().map(|m| {
            if allowed.contains(m) {
                m.as_str()
            } else {
                "local"
            }
        });
        if let Some(v) = v.as_object_mut() {
            v.insert("mode".into(), json!(effective));
            v.insert("storedMode".into(), json!(self.mode));
            v.insert("enabled".into(), json!(true));
            v.insert("allowed".into(), json!(allowed));
            v.insert("admin".into(), json!(admin));
        }
        v
    }
}
pub fn put_allowed(
    writer: &Writer,
    cap: RustRoutingMode,
    actor: &str,
    modes: Vec<String>,
    now: i64,
) -> Result<Value, StoreError> {
    let text = json!(modes).to_string();
    // Deliberately two commits, matching actual Node setAllowedModes THEN audit.
    writer.write(|tx| tx.set_routing_allowed(cap, &text))?;
    writer.write(|tx|tx.execute("INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,'routing.allowed',?2,?3)",server_store::rusqlite::params![actor,json!({"allowed":modes}).to_string(),now]))?;
    Ok(json!({"allowed":modes}))
}
