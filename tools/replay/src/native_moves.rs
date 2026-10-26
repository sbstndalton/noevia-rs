//! Required native persisted move proof. Only validated account-root paths leave a
//! differential comparison COPY; full snapshots/backup files remain authoritative.
use crate::{at, bind::Bindings, http, Outcome};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io::Read, path::Path, process::Command};
#[derive(Debug, Clone)]
pub struct Move {
    pub user: String,
    pub raw: String,
    pub canonical: String,
    pub from: Value,
    pub to: Value,
    pub projects_version: String,
    pub free_version: String,
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn canonical(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}
fn plain(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err("native move proof needs a plain file".into());
    }
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map_err(|e| e.to_string())?;
    let stat = rustix::fs::fstat(&fd).map_err(|e| e.to_string())?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile
        || stat.st_nlink != 1
    {
        return Err("unsafe native move proof file".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::from(fd)
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > max {
        return Err("native move proof file too large".into());
    }
    Ok(bytes)
}
fn holder(v: &Value) -> Result<Value, String> {
    match v {
        Value::Null => Ok(json!({"kind":"free"})),
        Value::String(id) if !id.is_empty() => Ok(json!({"kind":"project","id":id})),
        _ => Err("invalid move response holder".into()),
    }
}
fn decode(s: &str) -> Result<String, String> {
    let mut bytes = Vec::new();
    let mut i = 0;
    let input = s.as_bytes();
    while let Some(&b) = input.get(i) {
        if b == b'%' {
            let h = input.get(i + 1..i + 3).ok_or("bad move URI")?;
            let text = std::str::from_utf8(h).map_err(|_| "bad move URI")?;
            bytes.push(u8::from_str_radix(text, 16).map_err(|_| "bad move URI")?);
            i += 3;
        } else {
            bytes.push(b);
            i += 1;
        }
    }
    String::from_utf8(bytes).map_err(|_| "bad move URI".into())
}
/// Capture successful responses/list hashes immediately, before later corpus list edits.
/// Account binding comes from the ACTUAL credential's session row, never state-file claims.
pub fn capture(
    dir: &Path,
    target: &str,
    headers: &[(String, String)],
    body: &[u8],
    response: &http::Response,
) -> Result<Move, String> {
    let encoded = target
        .strip_prefix("/api/chats/")
        .and_then(|s| s.strip_suffix("/move"))
        .filter(|s| !s.is_empty() && !s.contains('/'))
        .ok_or("bad move target")?;
    let raw = decode(encoded)?;
    let canonical = canonical(&raw);
    if canonical.is_empty() {
        return Err("empty canonical move id".into());
    }
    let cookies: Vec<_> = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("cookie"))
        .flat_map(|(_, v)| v.split(';'))
        .filter_map(|v| v.trim().strip_prefix("cowork_session="))
        .collect();
    if cookies.len() != 1 {
        return Err("native move proof requires one actual session credential".into());
    }
    let sql = format!(
        "SELECT user_id FROM sessions WHERE id_hash='{}'",
        digest(cookies.first().ok_or("missing session")?.as_bytes())
    );
    let result = Command::new("sqlite3")
        .args(["-readonly", "-json"])
        .arg(dir.join("cowork.db"))
        .arg(sql)
        .output()
        .map_err(|e| e.to_string())?;
    if !result.status.success() {
        return Err("native move session lookup failed".into());
    }
    let rows: Value = serde_json::from_slice(&result.stdout).map_err(|e| e.to_string())?;
    let user = at(at(&rows, 0), "user_id")
        .as_str()
        .filter(|s| s.len() == 36 && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
        .ok_or("missing bound move account")?
        .to_string();
    let request: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let answer: Value = serde_json::from_slice(&response.body).map_err(|e| e.to_string())?;
    if answer.get("ok") != Some(&json!(true)) || answer.get("projectId") != request.get("projectId")
    {
        return Err("move response target mismatch".into());
    }
    let account = dir.join("users").join(&user);
    Ok(Move {
        user,
        raw,
        canonical,
        from: holder(at(&answer, "from"))?,
        to: holder(at(&answer, "projectId"))?,
        projects_version: digest(&plain(&account.join("projects.json"), 20 * 1024 * 1024)?),
        free_version: digest(&plain(&account.join("free-chats.json"), 20 * 1024 * 1024)?),
    })
}
/// Validate every exact account-root state, including absence/unknown accounts.
pub fn validate(
    dir: &Path,
    outcomes: &[Outcome],
    start: &str,
    bindings: &Bindings,
) -> Result<Vec<String>, String> {
    if !hex(start, 32) {
        return Err("invalid actual replay start identity".into());
    }
    let mut accounts: BTreeMap<&str, Vec<&Move>> = BTreeMap::new();
    for m in outcomes.iter().filter_map(|o| o.native_move.as_ref()) {
        accounts.entry(&m.user).or_default().push(m);
    }
    if let Ok(meta) = std::fs::symlink_metadata(dir.join("users")) {
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err("unsafe users directory".into());
        }
    }
    let mut actual = Vec::new();
    if let Ok(users) = std::fs::read_dir(dir.join("users")) {
        for user in users {
            let user = user.map_err(|e| e.to_string())?;
            let p = user.path().join("chat-move-state.json");
            if std::fs::symlink_metadata(&p).is_ok() {
                let name = user
                    .file_name()
                    .into_string()
                    .map_err(|_| "non-UTF8 move account")?;
                if !accounts.contains_key(name.as_str()) {
                    return Err("unproven account-root move state".into());
                }
                let meta = std::fs::symlink_metadata(user.path()).map_err(|e| e.to_string())?;
                if !meta.is_dir() || meta.file_type().is_symlink() {
                    return Err("unsafe move account directory".into());
                }
                actual.push(name);
            }
        }
    }
    if actual.len() != accounts.len() {
        return Err("required native move state is missing".into());
    }
    let mut projected = Vec::new();
    for (user, moves) in accounts {
        let account = dir.join("users").join(user);
        let state: Value = serde_json::from_slice(&plain(
            &account.join("chat-move-state.json"),
            84 * 1024 * 1024,
        )?)
        .map_err(|e| e.to_string())?;
        let mut keys: Vec<_> = state
            .as_object()
            .ok_or("move state is not an object")?
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        if keys != ["completed", "generations", "pending", "version"]
            || state.get("version") != Some(&json!(1))
            || state.get("pending") != Some(&Value::Null)
        {
            return Err("invalid/unsafe native move state schema or pending intent".into());
        }
        let mut generations: BTreeMap<&str, u64> = BTreeMap::new();
        let mut expected = Vec::new();
        for m in &moves {
            let generation = generations.entry(&m.canonical).or_default();
            if m.from == m.to {
                return Err("same-holder move needs an explicit replay proof contract".into());
            }
            *generation = generation
                .checked_add(1)
                .ok_or("move proof generation overflow")?;
            expected.push((*m, generation.to_string()));
        }
        let wanted: BTreeMap<_, _> = generations
            .iter()
            .map(|(k, v)| (*k, v.to_string()))
            .collect();
        if state.get("generations") != Some(&json!(wanted)) {
            return Err("native canonical generations differ from successful corpus moves".into());
        }
        let journal = plain(&account.join("chat-changes.jsonl"), 16 * 1024 * 1024)?;
        let mut events = Vec::new();
        for line in journal.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let entry: Value = serde_json::from_slice(line).map_err(|e| e.to_string())?;
            if at(&entry, "op") == "move" {
                events.push(entry);
            }
        }
        if events.len() != expected.len() {
            return Err("move journal count differs from committed corpus moves".into());
        }
        let mut ids = std::collections::HashSet::new();
        let mut last_seq = 0;
        for (entry, (m, generation)) in events.iter().zip(&expected) {
            let event = at(entry, "eventId")
                .as_str()
                .ok_or("missing move event id")?;
            let seq = at(entry, "seq").as_u64().ok_or("invalid move sequence")?;
            if !hex(event, 32)
                || !ids.insert(event)
                || seq <= last_seq
                || at(entry, "chatId").as_str() != Some(m.canonical.as_str())
                || at(entry, "rawChatId").as_str() != Some(m.raw.as_str())
                || at(entry, "generation").as_str() != Some(generation.as_str())
                || at(entry, "start") != start
                || at(entry, "delivery") != "live"
                || at(entry, "origin") != "other"
            {
                return Err(
                    "native move journal identity/generation/start/delivery mismatch".into(),
                );
            }
            last_seq = seq;
        }
        let (m, generation) = expected.last().ok_or("no proven moves")?;
        let event = events.last().ok_or("no move journal completion")?;
        let completed = json!({"canonical":m.canonical,"generation":generation,"event":at(event,"eventId"),"from":m.from,"to":m.to,"projectsVersion":m.projects_version,"freeVersion":m.free_version});
        if state.get("completed") != Some(&completed) {
            return Err("native move completion differs from actual committed transaction".into());
        }
        projected.push(bindings.unbind(&format!("users/{user}/chat-move-state.json")));
    }
    Ok(projected)
}
/// Differential-only COPY. Never changes the snapshot, disk or backup evidence.
pub fn project(snapshot: &Value, proven: &[String]) -> Result<Value, String> {
    let mut projected = snapshot.clone();
    if let Some(object) = projected.as_object_mut() {
        object.remove("nativeMoveStateProof");
    }
    let files = projected
        .get_mut("files")
        .and_then(Value::as_array_mut)
        .ok_or("missing file snapshot")?;
    for path in proven {
        if files.iter().filter(|v| v.as_str() == Some(path)).count() != 1 {
            return Err("proven move state absent/duplicated in snapshot".into());
        }
    }
    files.retain(|v| !proven.iter().any(|p| v.as_str() == Some(p)));
    Ok(projected)
}

/// Unmasked native-only evidence accompanies the full state snapshot, never the Node
/// comparison. This state is backup authority and is not rewritten/removed on disk.
pub fn evidence(dir: &Path, outcomes: &[Outcome], start: &str) -> Result<Value, String> {
    let mut accounts = serde_json::Map::new();
    for m in outcomes.iter().filter_map(|o| o.native_move.as_ref()) {
        let path = format!("users/{}/chat-move-state.json", m.user);
        let state: Value = serde_json::from_slice(&plain(&dir.join(&path), 84 * 1024 * 1024)?)
            .map_err(|e| e.to_string())?;
        accounts.insert(path, state);
    }
    Ok(json!({"actualStart":start,"accountFiles":accounts}))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    const USER: &str = "11111111-1111-4111-8111-111111111111";
    const START: &str = "0123456789abcdef0123456789abcdef";
    fn fixture() -> (tempfile::TempDir, Vec<Outcome>, Value, Value) {
        let dir = tempfile::tempdir().unwrap();
        let account = dir.path().join("users").join(USER);
        std::fs::create_dir_all(&account).unwrap();
        let moves = vec![
            Move {
                user: USER.into(),
                raw: "qa raw!".into(),
                canonical: "qaraw".into(),
                from: json!({"kind":"free"}),
                to: json!({"kind":"project","id":"qa"}),
                projects_version: "a".repeat(64),
                free_version: "b".repeat(64),
            },
            Move {
                user: USER.into(),
                raw: "qa raw!".into(),
                canonical: "qaraw".into(),
                from: json!({"kind":"project","id":"qa"}),
                to: json!({"kind":"free"}),
                projects_version: "c".repeat(64),
                free_version: "d".repeat(64),
            },
        ];
        let events:Vec<_>=moves.iter().enumerate().map(|(i,m)|json!({"seq":i+1,"op":"move","origin":"other","chatId":m.canonical,"rawChatId":m.raw,"generation":(i+1).to_string(),"eventId":format!("{:032x}",i+1),"start":START,"delivery":"live"})).collect();
        let state = json!({"version":1,"generations":{"qaraw":"2"},"pending":null,"completed":{"canonical":"qaraw","generation":"2","event":"00000000000000000000000000000002","from":{"kind":"project","id":"qa"},"to":{"kind":"free"},"projectsVersion":"c".repeat(64),"freeVersion":"d".repeat(64)}});
        std::fs::write(account.join("chat-move-state.json"), state.to_string()).unwrap();
        std::fs::write(
            account.join("chat-changes.jsonl"),
            events
                .iter()
                .map(|e| e.to_string() + "\n")
                .collect::<String>(),
        )
        .unwrap();
        let outcomes = moves
            .into_iter()
            .map(|m| Outcome {
                native_move: Some(m),
                actual_status: Some(200),
                ..Outcome::default()
            })
            .collect();
        (dir, outcomes, state, json!(events))
    }
    #[test]
    fn exact_proof_preserves_full_snapshot_and_only_projects_the_proven_path() {
        let (dir, outcomes, _, _) = fixture();
        let b = Bindings::new("");
        let paths = validate(dir.path(), &outcomes, START, &b).unwrap();
        assert_eq!(paths, vec![format!("users/{USER}/chat-move-state.json")]);
        let snap = json!({"files":[paths[0],"users/other/chat-move-state.json","users/x/projects.json"],"db":{"keep":"sentinel"},"nativeMoveStateProof":evidence(dir.path(),&outcomes,START).unwrap()});
        let before = snap.to_string();
        let projected = project(&snap, &paths).unwrap();
        assert_eq!(
            at(&projected, "files"),
            &json!(["users/other/chat-move-state.json", "users/x/projects.json"])
        );
        assert_eq!(at(&projected, "db"), &json!({"keep":"sentinel"}));
        assert_eq!(snap.to_string(), before);
        assert!(project(&json!({"files":[]}), &paths).is_err());
        let mutated =
            json!({"files":["users/other/chat-move-state.json"],"db":{"keep":"sentinel"}});
        assert!(
            !crate::state::compare(&projected, &mutated).is_empty(),
            "shared metadata oracle still fails"
        );
    }
    #[test]
    fn missing_corrupt_reset_noncanonical_pending_and_completion_mutations_fail() {
        let (dir, outcomes, state, _) = fixture();
        let path = dir
            .path()
            .join("users")
            .join(USER)
            .join("chat-move-state.json");
        let b = Bindings::new("");
        std::fs::remove_file(&path).unwrap();
        assert!(validate(dir.path(), &outcomes, START, &b).is_err());
        std::fs::write(&path, "{").unwrap();
        assert!(validate(dir.path(), &outcomes, START, &b).is_err());
        for (pointer, value) in [
            ("/version", json!(2)),
            ("/generations/qaraw", json!("0")),
            ("/generations/qaraw", json!("02")),
            ("/pending", json!({"unsafe":"intent"})),
            ("/completed/canonical", json!("wrong")),
            ("/completed/generation", json!("1")),
            ("/completed/from", json!({"kind":"free"})),
            ("/completed/to", json!({"kind":"project","id":"foreign"})),
            ("/completed/event", json!("f".repeat(32))),
            ("/completed/projectsVersion", json!("0".repeat(64))),
            ("/completed/freeVersion", json!("0".repeat(64))),
        ] {
            let mut bad = state.clone();
            *bad.pointer_mut(pointer).unwrap() = value;
            std::fs::write(&path, bad.to_string()).unwrap();
            assert!(
                validate(dir.path(), &outcomes, START, &b).is_err(),
                "{pointer}"
            );
        }
        std::fs::write(&path, state.to_string()).unwrap();
        assert!(validate(dir.path(), &outcomes, "wrong", &b).is_err());
        let mut extra = state;
        extra["generations"]["foreign"] = "1".into();
        std::fs::write(&path, extra.to_string()).unwrap();
        assert!(validate(dir.path(), &outcomes, START, &b).is_err());
    }
    #[test]
    fn journal_count_identity_start_delivery_canonical_raw_and_order_are_required() {
        let (dir, outcomes, _, events) = fixture();
        let path = dir
            .path()
            .join("users")
            .join(USER)
            .join("chat-changes.jsonl");
        let b = Bindings::new("");
        for (pointer, value) in [
            ("/1/start", json!("f".repeat(32))),
            ("/1/delivery", json!("recovery")),
            ("/1/chatId", json!("foreign")),
            ("/1/rawChatId", json!("other alias")),
            ("/1/generation", json!("1")),
            ("/1/eventId", json!("00000000000000000000000000000001")),
            ("/1/seq", json!(1)),
            ("/1/origin", json!("history")),
        ] {
            let mut bad = events.clone();
            *bad.pointer_mut(pointer).unwrap() = value;
            std::fs::write(
                &path,
                bad.as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e.to_string() + "\n")
                    .collect::<String>(),
            )
            .unwrap();
            assert!(
                validate(dir.path(), &outcomes, START, &b).is_err(),
                "{pointer}"
            );
        }
        std::fs::write(&path, "").unwrap();
        assert!(validate(dir.path(), &outcomes, START, &b).is_err());
    }
    #[test]
    fn no_move_does_not_invent_state_and_unknown_accounts_cannot_be_hidden() {
        let empty = tempfile::tempdir().unwrap();
        let b = Bindings::new("");
        assert!(validate(empty.path(), &[], START, &b).unwrap().is_empty());
        let (dir, outcomes, _, _) = fixture();
        assert!(validate(dir.path(), &[], START, &b).is_err());
        std::fs::create_dir_all(dir.path().join("users/foreign")).unwrap();
        std::fs::write(dir.path().join("users/foreign/chat-move-state.json"), "{}").unwrap();
        assert!(validate(dir.path(), &outcomes, START, &b).is_err());
    }
    #[test]
    fn path_decode_matches_raw_identity_without_double_decoding() {
        assert_eq!(decode("qa%20raw%21").unwrap(), "qa raw!");
        assert_eq!(decode("qa%2520raw").unwrap(), "qa%20raw");
        assert!(decode("%E0%A4").is_err());
        assert!(decode("%GG").is_err());
    }
}
