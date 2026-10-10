//! M6 catalogue/default setting only. No task classification, chat selection or provider policy.
pub mod input;
use serde_json::{json, Value};
use server_store::{Reader, RustSamplingSettings, StoreError, Writer};
pub const KEY: &str = "auto_sampling_presets_enabled";
pub const BODY_LIMIT: usize = 1024 * 1024;
pub fn presets() -> Value {
    json!({"general":{},"coding":{"temperature":0.2,"top_p":0.9,"repeat_penalty":1.05},"creative":{"temperature":0.9,"top_p":0.95},"reasoning":{"temperature":0.5}})
}
pub fn get(reader: &Reader<'_>, admin: bool) -> Result<Value, StoreError> {
    // SQLite may carry NULL/numbers/blobs despite the nominal text schema. Only exact text false
    // disables, matching Node's strict comparison; neither coercion nor NULL changes the default.
    let enabled = reader.row("SELECT CASE WHEN typeof(value)='text' AND value='false' THEN 0 ELSE 1 END FROM settings WHERE key=?1", [KEY], |r| r.get::<_, bool>(0))?.unwrap_or(true);
    Ok(json!({"enabled":enabled,"presets":presets(),"version":1,"admin":admin}))
}
pub fn put(
    writer: &Writer,
    sampling: RustSamplingSettings,
    actor: &str,
    enabled: bool,
    now: i64,
) -> Result<Value, StoreError> {
    // The setting and authenticated audit append commit in one SQLite transaction.
    writer.write(|tx| {
        tx.set_sampling_enabled(sampling, enabled)?;
        tx.execute("INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,NULL,'sampling.autoPresets',?2,?3)", server_store::rusqlite::params![actor,json!({"enabled":enabled}).to_string(),now])?;
        Ok(json!({"enabled":enabled}))
    })
}
