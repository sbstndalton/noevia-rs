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
//!
//! Strings are handled as core handles them (sbstndalton/noevia#2584): `String.prototype.trim`'s
//! white space (U+FEFF yes, U+0085 no), the name and label patterns' ASCII-only `/i` folding, and
//! the roles file decoded as `readFileSync(file, 'utf8')` + `JSON.parse` read it (a bad byte is
//! U+FFFD, a lone-surrogate escape is kept), so one odd entry does not drop every role.
//! tests/model_kind_node.rs replays core's own answers.

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
    let Ok(bytes) = std::fs::read(file) else {
        return HashMap::new();
    };
    // `readFileSync(file, 'utf8')`: invalid sequences decode to U+FFFD, as WHATWG UTF-8 does.
    roles_of(&String::from_utf8_lossy(&bytes))
}

/// model-system.cjs `sectionRolesOf(text)`.
pub fn roles_of(text: &str) -> HashMap<String, String> {
    use prompt_framing::js;
    let units: Vec<u16> = text.encode_utf16().collect();
    // `JSON.parse`; depth 1 is enough: a nested value is not a string and is skipped.
    let Some(Value::Obj(pairs)) = prompt_framing::json::parse(&units, 1) else {
        return HashMap::new();
    };
    let mut roles = HashMap::new();
    for (name, role) in pairs {
        let Value::Str(role) = role else { continue };
        let role = js::trim(&role);
        // A name holding a lone surrogate can never equal a served model's name; its role
        // (here or elsewhere in the file) still counts for every other entry.
        let Ok(name) = String::from_utf16(&name) else {
            continue;
        };
        if role.is_empty() || name == "*" {
            continue;
        }
        // A role with a lone surrogate is kept (lossily): it is a role, and never a system one.
        roles.insert(name, js::lossy(&js::to_lower(role)));
    }
    roles
}

/// `String.prototype.trim` (core trims with JS white space, not Unicode `White_Space`).
fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| u16::try_from(u32::from(c)).is_ok_and(mcp_frame::is_js_space))
}

/// `/^laya(?:[_.-]|$)/i` on the trimmed value.
fn laya_token(value: &str) -> bool {
    let v = js_trim(value);
    let lower = v.to_ascii_lowercase();
    lower
        .strip_prefix("laya")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['_', '.', '-']))
}

/// `isSystemModel(id)` with no model path.
pub fn is_system_model(roles: &HashMap<String, String>, id: &str) -> bool {
    let key = js_trim(id);
    if !key.is_empty() {
        if let Some(role) = roles.get(key) {
            return role == "system-routing" || role == "laya";
        }
    }
    laya_token(key)
}

/// `isSystemModel(id, modelPath)`: an explicit role or the name decides, then the path's segments.
pub fn is_system_model_at(roles: &HashMap<String, String>, id: &str, path: Option<&str>) -> bool {
    let key = js_trim(id);
    if (!key.is_empty() && roles.contains_key(key)) || laya_token(key) {
        return is_system_model(roles, id);
    }
    path.is_some_and(|p| {
        p.split(['/', '\\']).filter(|s| !s.is_empty()).any(|seg| {
            let lower = seg.to_ascii_lowercase();
            let token = match lower.strip_suffix(".gguf") {
                Some(_) => seg.get(..seg.len() - 5).unwrap_or(seg),
                None => seg,
            };
            laya_token(token)
        })
    })
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
    // `/embed|rerank/i` and the label pattern fold ASCII only (no `u` flag): the Kelvin sign
    // is not a `k` there.
    let lower = name.to_ascii_lowercase();
    if lower.contains("embed") || lower.contains("rerank") {
        return false;
    }
    !labels.unwrap_or_default().iter().any(|l| {
        matches!(
            l.to_ascii_lowercase().as_str(),
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
