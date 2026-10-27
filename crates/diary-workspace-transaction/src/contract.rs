//! Closed immutable mutation requests and cached result contracts. Validation
//! never consults current corpus/config/clock state and is not a signature or
//! provenance proof for an arbitrary attacker-authored database.
use super::{canonical_uuid, Condition, Fault, Reply, Request, MAX_REQUEST};
use super::{json::Number, Json};
use diary_workspace_rules as rules;
use std::collections::BTreeMap as Map;
use std::collections::HashSet;

pub(super) fn request(r: &Request) -> Result<(), Fault> {
    for value in [
        Some(r.db_path.as_str()),
        r.operation_namespace.as_deref(),
        r.source_version.value().map(String::as_str),
        r.destination_version.value().map(String::as_str),
    ]
    .into_iter()
    .flatten()
    {
        if value.len() > MAX_REQUEST {
            return Err(Fault::Frame);
        }
    }
    if !r.operation_namespace.as_deref().is_some_and(canonical_uuid) {
        return Err(Fault::Invalid);
    }
    if r.db_path.contains('\0') {
        return Err(Fault::Frame);
    }
    let source = r
        .source
        .as_deref()
        .map(|s| rules::rel_path(s, false).map_err(|_| Fault::Invalid))
        .transpose()?;
    if r.destination
        .as_deref()
        .map(|s| rules::rel_path(s, false))
        .transpose()
        .is_err()
    {
        return Err(Fault::Invalid);
    }
    if r.op != "restore" && source.is_none() {
        return Err(Fault::Invalid);
    }
    if matches!(r.op.as_str(), "move" | "copy") && r.destination.is_none() {
        return Err(Fault::Invalid);
    }
    if r.op == "trash" {
        rules::safe_file_path(r.source.as_deref().ok_or(Fault::Invalid)?)
            .map_err(|_| Fault::Invalid)?;
    }
    if r.capsule_id.as_deref().is_some_and(|s| !canonical_uuid(s)) {
        return Err(Fault::Invalid);
    }
    if matches!(r.op.as_str(), "preserve" | "trash" | "restore") && r.capsule_id.is_none() {
        return Err(Fault::Invalid);
    }
    Ok(())
}
fn object<'a>(body: &'a Json, keys: &[&str]) -> Result<&'a Map<String, Json>, Fault> {
    let object = body.as_object().ok_or(Fault::Schema)?;
    if object.len() != keys.len() || keys.iter().any(|key| !object.contains_key(*key)) {
        return Err(Fault::Schema);
    }
    Ok(object)
}
fn text<'a>(object: &'a Map<String, Json>, key: &str) -> Result<&'a str, Fault> {
    object.get(key).and_then(Json::as_str).ok_or(Fault::Schema)
}
fn boolean(object: &Map<String, Json>, key: &str) -> Result<bool, Fault> {
    object.get(key).and_then(Json::as_bool).ok_or(Fault::Schema)
}
fn relative(value: Option<&str>) -> Result<String, Fault> {
    rules::rel_path(value.ok_or(Fault::Schema)?, false).map_err(|_| Fault::Schema)
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn tag(condition: &Condition<String>) -> Result<&str, Fault> {
    condition
        .value()
        .map(String::as_str)
        .filter(|s| digest(s))
        .ok_or(Fault::Schema)
}
fn capsule(r: &Request) -> Result<&str, Fault> {
    r.capsule_id
        .as_deref()
        .filter(|id| canonical_uuid(id))
        .ok_or(Fault::Schema)
}
fn equals(a: &str, b: &str) -> Result<(), Fault> {
    if a == b {
        Ok(())
    } else {
        Err(Fault::Schema)
    }
}

pub(super) fn receipt(r: &Request, result: &Reply, created: Option<f64>) -> Result<(), Fault> {
    if result.status() != 200 {
        let body = object(result.body(), &["error"])?;
        let pair = (result.status(), text(body, "error")?);
        // Trash routes deliberately map protection/missing/precondition faults
        // to Conflict. Capacity may also arise from bounded output sealing.
        let allowed = match r.op.as_str() {
            "trash" | "restore" => matches!(pair, (409, "conflict") | (507, "capacity")),
            "delete" | "move" | "copy" | "preserve" => matches!(
                pair,
                (403, "protected")
                    | (404, "missing")
                    | (409, "conflict")
                    | (412, "precondition")
                    | (428, "required")
                    | (507, "capacity")
            ),
            _ => false,
        };
        return if allowed { Ok(()) } else { Err(Fault::Schema) };
    }
    match r.op.as_str() {
        "delete" => {
            tag(&r.source_version)?;
            let body = object(result.body(), &["deleted", "trash"])?;
            equals(text(body, "deleted")?, &relative(r.source.as_deref())?)?;
            let ids = body
                .get("trash")
                .and_then(Json::as_array)
                .ok_or(Fault::Schema)?;
            if ids.len() > rules::MAX_ENTRIES {
                return Err(Fault::Schema);
            }
            let mut unique = HashSet::new();
            for id in ids {
                let id = id
                    .as_str()
                    .filter(|id| canonical_uuid(id))
                    .ok_or(Fault::Schema)?;
                if !unique.insert(id) {
                    return Err(Fault::Schema);
                }
            }
            Ok(())
        }
        "move" | "copy" => {
            if r.op == "move" || r.source_version.is_present() {
                tag(&r.source_version)?;
            }
            let body = object(result.body(), &["path", "destination", "replaced"])?;
            equals(text(body, "path")?, &relative(r.source.as_deref())?)?;
            equals(
                text(body, "destination")?,
                &relative(r.destination.as_deref())?,
            )?;
            if boolean(body, "replaced")? {
                if r.overwrite.value() != Some(&true) {
                    return Err(Fault::Schema);
                }
                tag(&r.destination_version)?;
                capsule(r)?;
            }
            Ok(())
        }
        "preserve" => {
            tag(&r.source_version)?;
            let body = object(result.body(), &["path", "trash"])?;
            equals(text(body, "path")?, &relative(r.source.as_deref())?)?;
            equals(text(body, "trash")?, capsule(r)?)
        }
        "trash" | "restore" => {
            let body = object(
                result.body(),
                &[
                    "id",
                    "path",
                    "version",
                    "state",
                    "trashedAt",
                    "alreadyApplied",
                ],
            )?;
            equals(text(body, "id")?, capsule(r)?)?;
            rules::safe_file_path(text(body, "path")?).map_err(|_| Fault::Schema)?;
            if !digest(text(body, "version")?) {
                return Err(Fault::Schema);
            }
            let already = boolean(body, "alreadyApplied")?;
            let state = text(body, "state")?;
            // Legacy summaries preserve timestamp JSON types. The outer sealed
            // payload budget and decoder bound remain in force; do not coerce.
            if r.op == "trash" {
                // A newly written capsule uses the original receipt clock.
                // Already-applied legacy summaries retain their historical JSON
                // timestamp type; retries never consult the new request clock.
                if !already {
                    let timestamp = body
                        .get("trashedAt")
                        .and_then(Json::as_number)
                        .filter(|n| n.is_f64())
                        .and_then(Number::as_f64)
                        .filter(|n| n.is_finite())
                        .ok_or(Fault::Schema)?;
                    if Some(timestamp) != created {
                        return Err(Fault::Schema);
                    }
                }
                equals(
                    text(body, "path")?,
                    r.source.as_deref().ok_or(Fault::Schema)?,
                )?;
                equals(text(body, "version")?, tag(&r.source_version)?)?;
                if state != "trashed" && !(already && state == "restored") {
                    return Err(Fault::Schema);
                }
            } else if state != "restored" {
                return Err(Fault::Schema);
            }
            Ok(())
        }
        _ => Err(Fault::Schema),
    }
}
