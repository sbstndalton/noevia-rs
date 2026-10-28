#![cfg(target_os = "linux")]
use backend_runtime::{
    project_conversations::ProjectConversations, state::Conversations, Control, Fault,
};
use serde_json::{json, Value};
use server_projects::{chat_move, transcript};
use std::time::Duration;
const USER: &str = "01234567-89ab-4cde-8fab-0123456789ab";
const START: &str = "0123456789abcdef0123456789abcdef";
fn save(data: &std::path::Path, history: &[Value]) -> Result<(), Box<dyn std::error::Error>> {
    let current = transcript::read(data, USER, "chat1").map_err(|e| format!("{e:?}"))?;
    let js_json::JValue::Arr(values) =
        js_json::wtf16::parse(&serde_json::to_string(history)?).map_err(|_| "parse")?
    else {
        return Err("array".into());
    };
    assert!(matches!(
        transcript::write(
            data,
            USER,
            "chat1",
            &values,
            &transcript::Base::Revision(current.revision),
            None
        )
        .map_err(|e| format!("{e:?}"))?,
        transcript::Saved::Saved { .. }
    ));
    Ok(())
}
#[test]
fn browser_presave_before_and_during_turn_preserves_ids_and_fences_other_edits(
) -> Result<(), Box<dyn std::error::Error>> {
    for timing in ["before", "during", "conflict", "repeat"] {
        let dir = tempfile::tempdir()?;
        let user = dir.path().join("users").join(USER);
        std::fs::create_dir_all(&user)?;
        std::fs::write(
            user.join(server_projects::store::PROJECTS_FILE),
            r#"{"projects":[]}"#,
        )?;
        std::fs::write(
            user.join(server_projects::chats::FREE_CHATS_FILE),
            r#"[{"id":"chat1","title":"test"}]"#,
        )?;
        server_projects::journal::set_start_id(START);
        chat_move::grant_context(dir.path(), USER, "chat1", START).map_err(|e| format!("{e:?}"))?;
        let old = json!({"id":"old","role":"user","content":"same"});
        let sent = json!({"id":"new","role":"user","content":"same"});
        let request = json!({"role":"user","content":"same"});
        let previous = vec![json!({"role":"user","content":"same"})];
        save(dir.path(), &[old.clone()])?;
        if timing == "before" {
            save(dir.path(), &[old.clone(), sent.clone()])?;
        }
        let store = ProjectConversations::open(dir.path(), START, &dir.path().join("journal.db"))?;
        let binding = store.context(USER, "chat1")?;
        let control = Control::new(Duration::from_secs(5))?;
        let (revision, mut messages) = store.begin_browser(
            &binding,
            "11111111-1111-4111-8111-111111111111",
            &request,
            &previous,
            &control,
        )?;
        assert_eq!(messages.len(), 1, "{timing}");
        messages.push(request);
        messages.push(json!({"role":"assistant","content":"answer"}));
        if timing == "during" {
            save(dir.path(), &[old.clone(), sent.clone()])?;
        }
        if timing == "conflict" {
            save(
                dir.path(),
                &[json!({"role":"user","content":"changed"}), sent.clone()],
            )?;
        }
        let result = store.finish(
            &binding,
            "11111111-1111-4111-8111-111111111111",
            &revision,
            &messages,
            &control,
        );
        if timing == "conflict" {
            assert_eq!(result, Err(Fault::Conflict));
            continue;
        }
        result?;
        let saved = transcript::read(dir.path(), USER, "chat1").map_err(|e| format!("{e:?}"))?;
        let value: Value =
            serde_json::from_str(&js_json::wtf16::stringify(&saved.history).ok_or("encode")?)?;
        assert_eq!(value.as_array().ok_or("array")?.len(), 3);
        assert_eq!(value.pointer("/0/id"), Some(&json!("old")));
        if timing != "repeat" {
            assert_eq!(value.pointer("/1/id"), Some(&json!("new")));
        }
    }
    Ok(())
}
