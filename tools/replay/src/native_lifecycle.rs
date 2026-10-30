//! Required native project-lifecycle proof (sbstndalton/noevia#2191 round 2). Beside the paired
//! front, core publishes `project-catalogue.json` (the toolbox catalogue the front's project create
//! reads) and the front keeps each account's index outbox (`users/<id>/index-outbox.db`); a
//! Node-direct reference has neither. They leave the differential comparison COPY only after this
//! validates them: a well-formed catalogue, and every outbox fully drained (no pending, running or
//! failed job: all index work the front queued was done by core, every sweep by the front) with a tombstone only
//! for a project the account no longer has. The full snapshot keeps them.
use crate::{at, state};
use serde_json::{json, Value};
use std::path::Path;

pub const CATALOGUE: &str = "project-catalogue.json";
pub const OUTBOX: &str = "index-outbox.db";

fn strings(v: &Value) -> bool {
    v.as_array().is_some_and(|a| a.iter().all(Value::is_string))
}

/// The catalogue and the outboxes, validated; returns how many outboxes there are.
pub fn validate(dir: &Path) -> Result<usize, String> {
    let text = std::fs::read_to_string(dir.join(CATALOGUE))
        .map_err(|e| format!("native project catalogue absent: {e}"))?;
    let catalogue: Value =
        serde_json::from_str(&text).map_err(|e| format!("native project catalogue: {e}"))?;
    let ok = at(&catalogue, "version").as_u64() == Some(1)
        && strings(at(&catalogue, "selectable"))
        && strings(at(&catalogue, "icons"))
        && strings(at(&catalogue, "defaults"))
        && at(&catalogue, "defaults")
            .as_array()
            .is_some_and(|d| !d.is_empty());
    if !ok {
        return Err("native project catalogue is not version 1 with string lists".into());
    }
    let mut outboxes = 0;
    let users = std::fs::read_dir(dir.join("users")).map_err(|e| format!("users: {e}"))?;
    for user in users.filter_map(Result::ok) {
        let db = user.path().join(OUTBOX);
        if !db.is_file() {
            continue;
        }
        outboxes += 1;
        let rows = state::sqlite_json(&db, "SELECT COUNT(*) AS n FROM jobs")?;
        if at(at(&rows, 0), "n").as_u64() != Some(0) {
            return Err(format!("index outbox {} still holds jobs", db.display()));
        }
        let dead = state::sqlite_json(&db, "SELECT project FROM tombstones")?;
        let projects =
            std::fs::read_to_string(user.path().join("projects.json")).unwrap_or_default();
        let live: Value = serde_json::from_str(&projects).unwrap_or(Value::Null);
        for row in dead.as_array().into_iter().flatten() {
            let id = at(row, "project").as_str().unwrap_or("");
            let listed = at(&live, "projects")
                .as_array()
                .is_some_and(|ps| ps.iter().any(|p| at(p, "id").as_str() == Some(id)));
            if listed {
                return Err(format!(
                    "index outbox tombstones a listed project in {}",
                    db.display()
                ));
            }
        }
    }
    Ok(outboxes)
}

pub fn evidence(outboxes: usize) -> Value {
    json!({ "catalogue": CATALOGUE, "drainedOutboxes": outboxes })
}

/// Removes exactly the validated files (and the evidence) from a comparison copy.
pub fn project(snapshot: &Value, outboxes: usize) -> Result<Value, String> {
    let mut projected = snapshot.clone();
    if let Some(object) = projected.as_object_mut() {
        object.remove("nativeProjectLifecycleProof");
    }
    let files = projected
        .get_mut("files")
        .and_then(Value::as_array_mut)
        .ok_or("missing files snapshot")?;
    let before = files.len();
    files.retain(|f| {
        let s = f.as_str().unwrap_or("");
        !(s == CATALOGUE || (s.starts_with("users/") && s.ends_with(&format!("/{OUTBOX}"))))
    });
    if before - files.len() != outboxes + 1 {
        return Err("proven project lifecycle files absent/duplicated in snapshot".into());
    }
    Ok(projected)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn only_the_proven_files_leave_the_comparison_copy() {
        let snap = json!({"files": ["project-catalogue.json", "users/", "users/<id:1>/index-outbox.db", "users/<id:1>/projects.json"], "nativeProjectLifecycleProof": {}});
        let p = project(&snap, 1).unwrap();
        assert_eq!(
            p,
            json!({"files": ["users/", "users/<id:1>/projects.json"]})
        );
        assert!(project(&snap, 2).is_err(), "a missing outbox is not hidden");
        assert!(
            project(&json!({"files": ["users/"]}), 0).is_err(),
            "nor a missing catalogue"
        );
    }

    #[test]
    fn a_catalogue_that_is_absent_or_malformed_is_refused() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("users")).unwrap();
        assert!(validate(d.path()).is_err());
        std::fs::write(
            d.path().join(CATALOGUE),
            r#"{"version":1,"selectable":["core"],"defaults":[],"icons":[]}"#,
        )
        .unwrap();
        assert!(validate(d.path()).is_err(), "no defaults");
        std::fs::write(
            d.path().join(CATALOGUE),
            r#"{"version":1,"selectable":["core"],"defaults":["core"],"icons":["folder"]}"#,
        )
        .unwrap();
        assert_eq!(validate(d.path()).unwrap(), 0);
    }
}
