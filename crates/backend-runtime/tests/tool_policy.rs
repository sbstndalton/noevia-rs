#[path = "../../server-auth/tests/common/mod.rs"]
mod auth_fixture;
use backend_runtime::{
    approvals::{Binding, Holder},
    session_conversations::SessionConversations,
    state::{Conversations, StateStore},
};
use policy_leaves::Mode;
use serde_json::{json, Value};
use std::sync::Arc;
#[test]
fn original_account_policy_is_live_and_revocation_fenced() -> Result<(), Box<dyn std::error::Error>>
{
    let fixtures: Value = serde_json::from_str(include_str!(
        "../../server-auth/tests/fixtures/node-auth.v1.json"
    ))?;
    let scenario = fixtures
        .get("scenarios")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .ok_or("scenario")?;
    let built = auth_fixture::build(scenario);
    let input = scenario
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("cases")?
        .iter()
        .find(|c| {
            c.pointer("/expect/csrf") == Some(&json!(true))
                && c.pointer("/expect/authn/user/id") == Some(&json!("u-member"))
        })
        .and_then(|c| c.get("input"))
        .ok_or("credentials")?;
    let now = auth_fixture::now(&fixtures, input);
    let writer = Arc::new(server_store::Writer::open(
        built.dir.path(),
        server_store::RustAuth::owned(),
    )?);
    let auth = Arc::new(server_auth::Authenticator::new(auth_fixture::config(
        scenario,
    )));
    let store = Arc::new(StateStore::create(&built.dir.path().join("native.db"))?);
    let binding = Binding {
        user: "u-member".into(),
        canonical_chat_id: "one".into(),
        scope: "free".into(),
        holder: Holder::Free,
        generation: "0".into(),
        start: "a".repeat(32),
        tombstoned: false,
    };
    store.create_chat(&binding)?;
    let session = SessionConversations::new(
        store,
        writer,
        auth,
        auth_fixture::creds(input, "POST"),
        "u-member".into(),
        move || now,
    );
    let db = rusqlite::Connection::open(server_store::db_path(built.dir.path()))?;
    assert_eq!(
        session.tool_mode(&binding, "synthetic", false)?,
        Mode::Allow
    );
    assert_eq!(session.tool_mode(&binding, "synthetic", true)?, Mode::Ask);
    db.execute_batch("CREATE TABLE tool_policies(user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE, tool TEXT NOT NULL, mode TEXT NOT NULL CHECK(mode IN ('allow','ask','block')), updated_at INTEGER NOT NULL, PRIMARY KEY(user_id,tool))")?;
    for (stored, read, write) in [
        ("block", Mode::Block, Mode::Block),
        ("ask", Mode::Ask, Mode::Ask),
        ("allow", Mode::Allow, Mode::Ask),
    ] {
        db.execute("INSERT INTO tool_policies(user_id,tool,mode,updated_at) VALUES('u-member','synthetic',?,0) ON CONFLICT(user_id,tool) DO UPDATE SET mode=excluded.mode",[stored])?;
        assert_eq!(session.tool_mode(&binding, "synthetic", false)?, read);
        assert_eq!(session.tool_mode(&binding, "synthetic", true)?, write);
    }
    db.execute("DELETE FROM sessions WHERE user_id='u-member'", [])?;
    assert!(session.tool_mode(&binding, "synthetic", false).is_err());
    Ok(())
}
