//! Native administrator reasoning default write. Provider capability reads remain Node-owned.
pub mod input;
use serde_json::{json, Value};
use server_store::{RustReasoningSettings, StoreError, Writer};
pub const BODY_LIMIT: usize = 1024 * 1024;
pub fn put(
    writer: &Writer,
    capability: RustReasoningSettings,
    actor: &str,
    effort: &str,
    now: i64,
) -> Result<Value, StoreError> {
    writer.write(|tx| {
        tx.set_reasoning_default(capability, effort)?;
        tx.execute("INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,NULL,'reasoning.default',?2,?3)", server_store::rusqlite::params![actor,json!({"effort":effort}).to_string(),now])?;
        Ok(json!({"default":effort}))
    })
}
