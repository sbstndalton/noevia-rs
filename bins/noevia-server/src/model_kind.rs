//! core chat-model-kind.cjs `isChatGenerationModel(name, labels)` and model-system.cjs
//! `isSystemModel(name)` (sbstndalton/noevia#2191 round 9, the project configuration route): which
//! served models can answer a chat prompt. Embedding and reranking models return vectors or
//! scores, and Laya is the internal routing model; pinning one as a project's model would send
//! every message to it (#409, #206).
//!
//! The labels are the router's: an entry of its `/models` listing whose `status.args` carry
//! `--embedding(s)` or `--rerank(ing)` (llamacpp-manager.cjs `nativeLabels`). A model the listing
//! does not name, or a listing that could not be read, is judged by name alone, as core does with
//! an unreadable catalogue. The explicit roles are `models.roles.json` beside `models.ini`
//! (`LLAMACPP_PRESET_PATH`): a section named there is a system model when its role is
//! `system-routing` (or the legacy `laya`); only a name without a role falls back to the
//! deprecated Laya name rule.

use prompt_framing::json::Value;
use std::collections::HashMap;
use std::path::Path;

const ROLES_FILE: &str = "models.roles.json";

/// The explicit section roles beside the preset file (trimmed, lower-cased); none when the file is
/// missing, unreadable or not an object (core logs an unreadable one and goes on without roles).
pub fn roles_beside(presets: Option<&Path>) -> HashMap<String, String> {
    let Some(file) = presets.and_then(|p| p.parent()).map(|d| d.join(ROLES_FILE)) else {
        return HashMap::new();
    };
    let Ok(text) = std::fs::read_to_string(file) else {
        return HashMap::new();
    };
    roles_of(&text)
}

/// model-system.cjs `sectionRolesOf(text)`.
pub fn roles_of(text: &str) -> HashMap<String, String> {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(text) else {
        return HashMap::new();
    };
    map.into_iter()
        .filter(|(name, _)| name != "*")
        .filter_map(|(name, role)| {
            let role = role.as_str()?.trim().to_lowercase();
            (!role.is_empty()).then_some((name, role))
        })
        .collect()
}

/// `/^laya(?:[_.-]|$)/i` on the trimmed value.
fn laya_token(value: &str) -> bool {
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    lower
        .strip_prefix("laya")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['_', '.', '-']))
}

/// `isSystemModel(id)` with no model path.
pub fn is_system_model(roles: &HashMap<String, String>, id: &str) -> bool {
    let key = id.trim();
    if !key.is_empty() {
        if let Some(role) = roles.get(key) {
            return role == "system-routing" || role == "laya";
        }
    }
    laya_token(key)
}

/// The labels the router's listing gives `model` (`nativeLabels`), `None` when the listing does not
/// name it or is not a listing.
pub fn labels_of(listing: &Value, model: &str) -> Option<Vec<String>> {
    let Some(Value::Arr(rows)) = listing.get("data") else {
        return None;
    };
    let same = |s: &[u16]| s.iter().copied().eq(model.encode_utf16());
    let entry = rows
        .iter()
        .find(|m| matches!(m.get("id"), Some(Value::Str(id)) if same(id)))?;
    let args: Vec<Vec<u16>> = match entry.get("status").and_then(|s| s.get("args")) {
        Some(Value::Arr(items)) => items
            .iter()
            .filter_map(|a| a.as_str().map(<[u16]>::to_vec))
            .collect(),
        _ => Vec::new(),
    };
    let has = |names: [&str; 2]| {
        args.iter()
            .any(|a| names.iter().any(|n| a.iter().copied().eq(n.encode_utf16())))
    };
    let mut labels = Vec::new();
    if has(["--embedding", "--embeddings"]) {
        labels.push("embeddings".to_string());
    }
    if has(["--rerank", "--reranking"]) {
        labels.push("reranking".to_string());
    }
    Some(labels)
}

/// `isChatGenerationModel(name, labels)`.
pub fn is_chat_generation_model(
    roles: &HashMap<String, String>,
    name: &str,
    labels: Option<&[String]>,
) -> bool {
    if name.is_empty() || is_system_model(roles, name) {
        return false;
    }
    let lower = name.to_lowercase();
    if lower.contains("embed") || lower.contains("rerank") {
        return false;
    }
    !labels.unwrap_or_default().iter().any(|l| {
        matches!(
            l.to_lowercase().as_str(),
            "embedding" | "embeddings" | "rerank" | "reranking" | "reranker"
        )
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn roles(text: &str) -> HashMap<String, String> {
        roles_of(text)
    }

    #[test]
    fn names_roles_and_labels_decide() {
        let none = HashMap::new();
        for chat in ["gemma-chat", "qwen3", "Llama"] {
            assert!(is_chat_generation_model(&none, chat, None), "{chat}");
        }
        for not in [
            "",
            "embeddinggemma",
            "BGE-Reranker",
            "laya",
            "Laya_v2",
            "laya.x",
            " laya-1 ",
        ] {
            assert!(!is_chat_generation_model(&none, not, None), "{not:?}");
        }
        assert!(
            is_chat_generation_model(&none, "layaway", None),
            "only the token Laya"
        );
        let labelled = vec!["embeddings".to_string()];
        assert!(!is_chat_generation_model(
            &none,
            "plain-name",
            Some(&labelled)
        ));
        let vision = vec!["vision".to_string()];
        assert!(is_chat_generation_model(&none, "plain-name", Some(&vision)));
        // An explicit role decides before the name rule, both ways.
        let r = roles(
            r#"{"laya":"chat","routing-x":" System-Routing ","old":"laya","*":"system-routing","bad":5}"#,
        );
        assert!(
            is_chat_generation_model(&r, "laya", None),
            "a chat role beats the name rule"
        );
        assert!(!is_chat_generation_model(&r, "routing-x", None));
        assert!(!is_chat_generation_model(&r, "old", None));
        assert!(is_chat_generation_model(&r, "bad", None));
        assert!(roles("not json").is_empty() && roles("[1]").is_empty());
    }

    #[test]
    fn the_labels_are_native_labels_of_the_listing() {
        let parse = |t: &str| prompt_framing::json::parse_utf8(t.as_bytes(), 64).unwrap();
        let listing = parse(
            r#"{"data":[{"id":"e","status":{"args":["--embeddings","--x"]}},{"id":"r","status":{"args":["--reranking"]}},{"id":"c","status":{"args":["-m","f"]}},{"id":"n"}]}"#,
        );
        assert_eq!(
            labels_of(&listing, "e"),
            Some(vec!["embeddings".to_string()])
        );
        assert_eq!(
            labels_of(&listing, "r"),
            Some(vec!["reranking".to_string()])
        );
        assert_eq!(labels_of(&listing, "c"), Some(vec![]));
        assert_eq!(labels_of(&listing, "n"), Some(vec![]));
        assert_eq!(labels_of(&listing, "missing"), None);
        assert_eq!(labels_of(&parse(r#"{"data": 5}"#), "e"), None);
        assert_eq!(labels_of(&parse("null"), "e"), None);
    }

    #[test]
    fn the_roles_file_is_read_beside_the_preset_file() {
        let dir = tempfile::tempdir().unwrap();
        let ini = dir.path().join("models.ini");
        std::fs::write(dir.path().join(ROLES_FILE), r#"{"sys":"system-routing"}"#).unwrap();
        let r = roles_beside(Some(&ini));
        assert!(is_system_model(&r, "sys"));
        assert!(roles_beside(None).is_empty());
        assert!(roles_beside(Some(&dir.path().join("nowhere").join("models.ini"))).is_empty());
    }
}
