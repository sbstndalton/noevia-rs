//! The provider-row file operations the ChatGPT sign-in and the provider routes share
//! (sbstndalton/noevia#2190 round 6). Rust is the only writer of `users/<id>/providers.json` and
//! `shared-providers.json` (crates/server-projects `provider_rows`); this module has no feature
//! gate, so the token broker (`rust-chatgpt-status`) can write its provider row without the rest
//! of the provider routes (`rust-storage-reads`).

use js_json::JValue;
use server_projects::provider_rows;
use server_projects::store::{self, StoreError};
use std::path::Path;

/// core chatgpt-oauth.cjs `KIND` and `PROVIDER_ID`.
pub const CHATGPT_KIND: &str = "chatgpt-oauth";
pub const CHATGPT_ID: &str = "chatgpt-oauth";
/// core chatgpt-oauth.cjs `DEFAULTS.codexBaseUrl` (the row carries the real host even when a QA
/// run points the broker elsewhere).
pub const CHATGPT_BASE: &str = "https://chatgpt.com/backend-api/codex";

fn switch() -> server_store::RustProjects {
    server_store::RustProjects::owned()
}

/// core chatgpt-oauth.cjs `providerRow()`: the row that points an account's chats at its ChatGPT
/// connection (no credential in it).
pub fn chatgpt_row() -> JValue {
    JValue::obj([
        ("id", JValue::Str(CHATGPT_ID.into())),
        ("kind", JValue::Str(CHATGPT_KIND.into())),
        ("label", JValue::Str("ChatGPT".into())),
        ("baseUrl", JValue::Str(CHATGPT_BASE.into())),
        ("apiKey", JValue::Str(String::new())),
        ("defaultModel", JValue::Str(String::new())),
        ("shared", JValue::Bool(false)),
        ("external", JValue::Bool(true)),
    ])
}

/// Whether `row` is the account's own (not shared) ChatGPT row.
fn is_private_chatgpt(row: &JValue) -> bool {
    row.get("id").as_str() == Some(CHATGPT_ID) && !row.get("shared").truthy()
}

/// Adds the ChatGPT row to the account's private rows unless it has one: `true` when added.
/// Called when a sign-in has been saved (inside the broker's fence) and when a listing finds a
/// token row without it (a sign-in finished before this build, or a crash between the two).
pub fn ensure_chatgpt_row(data_dir: &Path, user: &str) -> Result<bool, StoreError> {
    let dir = store::user_dir(data_dir, user)?;
    provider_rows::update_user(&dir, switch(), |rows| {
        if rows.iter().any(is_private_chatgpt) {
            return (false, false);
        }
        rows.push(chatgpt_row());
        (true, true)
    })
}

/// Removes every row with `id` from the account's private rows (`shared` false) or from the shared
/// rows (`shared` true): `true` when there was one.
pub fn remove_rows(
    data_dir: &Path,
    user: &str,
    id: &str,
    shared: bool,
) -> Result<bool, StoreError> {
    let drop_id = |rows: &mut Vec<JValue>| {
        let before = rows.len();
        rows.retain(|r| r.get("id").as_str() != Some(id));
        let removed = rows.len() != before;
        (removed, removed)
    };
    if shared {
        provider_rows::update_shared(data_dir, switch(), drop_id)
    } else {
        let dir = store::user_dir(data_dir, user)?;
        provider_rows::update_user(&dir, switch(), drop_id)
    }
}

/// core routes/providers.cjs: the account's projects that pointed at a removed provider fall back
/// to the configured default (`delete project.provider; delete project.model`). One locked
/// read-modify-write of projects.json; a project with another provider is untouched.
pub fn clear_project_fallbacks(data_dir: &Path, user: &str, id: &str) -> Result<usize, StoreError> {
    let dir = store::user_dir(data_dir, user)?;
    store::update(&dir, switch(), |projects| {
        let mut changed = 0;
        for project in projects.iter_mut() {
            if project.get("provider").as_str() != Some(id) {
                continue;
            }
            if let JValue::Obj(items) = project {
                items.retain(|(k, _)| k != "provider" && k != "model");
                changed += 1;
            }
        }
        (changed, changed > 0)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const USER: &str = "11111111-2222-4333-8444-555555555555";

    fn ws() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("users").join(USER)).unwrap();
        root
    }

    #[test]
    fn the_chatgpt_row_is_cores_provider_row_and_is_added_once() {
        let root = ws();
        assert!(ensure_chatgpt_row(root.path(), USER).unwrap());
        assert!(!ensure_chatgpt_row(root.path(), USER).unwrap());
        let rows = provider_rows::read_user(&root.path().join("users").join(USER)).unwrap();
        assert_eq!(rows, vec![chatgpt_row()]);
        assert_eq!(
            js_json::stringify(&rows[0]).unwrap(),
            r#"{"id":"chatgpt-oauth","kind":"chatgpt-oauth","label":"ChatGPT","baseUrl":"https://chatgpt.com/backend-api/codex","apiKey":"","defaultModel":"","shared":false,"external":true}"#
        );
    }

    #[test]
    fn rows_leave_by_id_and_a_missing_account_is_gone() {
        let root = ws();
        ensure_chatgpt_row(root.path(), USER).unwrap();
        assert!(!remove_rows(root.path(), USER, "other", false).unwrap());
        assert!(remove_rows(root.path(), USER, CHATGPT_ID, false).unwrap());
        assert!(!remove_rows(root.path(), USER, CHATGPT_ID, false).unwrap());
        assert!(matches!(
            ensure_chatgpt_row(root.path(), "99999999-2222-4333-8444-555555555555"),
            Err(StoreError::Gone)
        ));
    }

    #[test]
    fn only_projects_on_the_removed_provider_lose_their_provider_and_model() {
        let root = ws();
        let dir = root.path().join("users").join(USER);
        std::fs::write(
            dir.join("projects.json"),
            r#"{"projects":[{"id":"a","provider":"p1","model":"m","name":"A"},{"id":"b","provider":"p2","model":"m"},{"id":"c"}]}"#,
        )
        .unwrap();
        assert_eq!(clear_project_fallbacks(root.path(), USER, "p1").unwrap(), 1);
        let text = std::fs::read_to_string(dir.join("projects.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["projects"][0], serde_json::json!({"id":"a","name":"A"}));
        assert_eq!(v["projects"][1]["provider"], "p2");
        assert_eq!(clear_project_fallbacks(root.path(), USER, "p1").unwrap(), 0);
    }
}
