#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! `PUT /api/routing-default` replayed on recorded Node output (`fixtures/node-routing-default-put.v1.json`,
//! written by `record_put.cjs` from core's own models.cjs route over a real workspace store): the reply,
//! preferences.json byte for byte, and the projects.json text Node saved. The front's route test replays the
//! same file through HTTP; this one checks the crate's parts. Cases marked `difference` must fail closed here.
use js_json::{wtf16, JValue};
use serde_json::Value;
use server_routing_default::put::{self, Refusal};
use server_routing_default::Root;

const FIXTURE: &str = include_str!("fixtures/node-routing-default-put.v1.json");

fn decode_hex(s: &str) -> Vec<u8> {
    s.as_bytes()
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}

fn bytes_of(v: &Value) -> Option<Vec<u8>> {
    if v.is_null() {
        return None;
    }
    if let Some(n) = v["repeatSpacesTo"].as_u64() {
        let mut b = v["text"].as_str().unwrap().as_bytes().to_vec();
        b.resize(usize::try_from(n).unwrap(), b' ');
        return Some(b);
    }
    Some(match v["text"].as_str() {
        Some(t) => t.as_bytes().to_vec(),
        None => decode_hex(v["hex"].as_str().unwrap()),
    })
}

/// server-projects `store::read` as the front's update closure sees the list: `projects` or none,
/// non-object `files` entries dropped (#1236).
fn stored_projects(bytes: &[u8]) -> Option<Vec<JValue>> {
    let parsed = wtf16::parse(&String::from_utf8_lossy(bytes)).ok()?;
    let mut list = match parsed.get("projects") {
        JValue::Arr(items) => items.clone(),
        JValue::Undefined | JValue::Null => Vec::new(),
        _ => return None,
    };
    for p in list.iter_mut() {
        if let JValue::Obj(fields) = p {
            if let Some((_, JValue::Arr(files))) = fields.iter_mut().find(|(k, _)| k == "files") {
                files.retain(|f| matches!(f, JValue::Obj(_) | JValue::Arr(_)));
            }
        }
    }
    Some(list)
}

#[test]
fn recorded_node_put_matrix() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let user = fixture["user"].as_str().unwrap();
    let now = fixture["now"].as_i64().unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 60, "corpus shrank to {}", cases.len());
    let mut differences = 0;
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let expected = &case["expected"];
        assert!(expected["threw"].is_null(), "{name}: Node threw");
        let body = bytes_of(&case["body"]).unwrap();
        let over = body.len() > put::BODY_LIMIT;
        let dir = tempfile::tempdir().unwrap();
        let tenant = dir.path().join("users").join(user);
        std::fs::create_dir_all(&tenant).unwrap();
        let prefs = bytes_of(&case["preferences"]);
        if let Some(p) = &prefs {
            std::fs::write(tenant.join("preferences.json"), p).unwrap();
        }
        let status = expected["status"].as_u64().unwrap();
        let reply = expected["reply"].as_str().unwrap();
        let parsed = put::parse(&body, over);
        if case["difference"] == "unholdable-body" {
            differences += 1;
            assert_eq!(status, 200, "{name}");
            assert_eq!(parsed, Err(Refusal::InvalidJson), "{name}");
            continue;
        }
        let req = match parsed {
            Err(refusal) => {
                assert_eq!(status, 400, "{name}");
                assert_eq!(
                    reply,
                    format!(
                        "{{\"error\":{}}}",
                        serde_json::to_string(refusal.message()).unwrap()
                    ),
                    "{name}"
                );
                // Nothing written.
                assert_eq!(
                    std::fs::read(tenant.join("preferences.json")).ok(),
                    prefs,
                    "{name}"
                );
                continue;
            }
            Ok(req) => req,
        };
        assert_eq!(status, 200, "{name}");
        let root = Root::open(dir.path()).unwrap();
        let wrote = root.write_preferences(user, req.routing);
        if case["difference"] == "unholdable-preferences" {
            differences += 1;
            assert!(wrote.is_err(), "{name}: must fail closed");
            assert_eq!(
                std::fs::read(tenant.join("preferences.json")).ok(),
                prefs,
                "{name}: left as stored"
            );
            continue;
        }
        wrote.unwrap();
        let after = std::fs::read(tenant.join("preferences.json")).unwrap();
        assert_eq!(
            after,
            decode_hex(expected["preferences"].as_str().unwrap()),
            "{name}: preferences.json"
        );
        // No temporary file is left beside it.
        let names: Vec<_> = std::fs::read_dir(&tenant)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["preferences.json".to_string()], "{name}");
        // The projects pass, on the list as the front's store reads it.
        let projects = bytes_of(&case["projects"]);
        let mut updated = 0;
        let mut saved = None;
        if req.apply_to_existing {
            // A list Node cannot parse loads as empty there: nothing is updated or saved.
            if let Some(mut list) = projects
                .as_deref()
                .map_or(Some(Vec::new()), stored_projects)
            {
                updated = put::apply(&mut list, req.routing, now).unwrap();
                if updated > 0 {
                    saved = Some(
                        wtf16::stringify_pretty(&JValue::obj([("projects", JValue::Arr(list))]))
                            .unwrap(),
                    );
                }
            }
        }
        assert_eq!(reply, put::answer(req.routing, updated), "{name}");
        assert_eq!(
            saved.as_deref(),
            expected["projectsSaved"].as_str(),
            "{name}: projects.json"
        );
    }
    assert_eq!(differences, 3, "the documented differences");
}

/// The recorder's sources are pinned: a changed Node file needs a fresh recording (`record_put.cjs --check`).
#[test]
fn recorder_sources_are_recorded() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    for file in [
        "server/routes/models.cjs",
        "server/workspace.cjs",
        "server/rust-projects.cjs",
        "server/http.cjs",
    ] {
        let sha = fixture["sources"][file].as_str().unwrap();
        assert_eq!(sha.len(), 64, "{file}");
    }
}

#[cfg(unix)]
#[test]
fn write_refuses_links_and_missing_tenants() {
    use server_routing_default::Routing;
    let user = "aaaaaaaa-1111-4aaa-8aaa-aaaaaaaaaaaa";
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    // No tenant directory: refused, nothing made.
    assert!(root.write_preferences(user, Routing::Manual).is_err());
    assert!(!dir.path().join("users").exists());
    let tenant = dir.path().join("users").join(user);
    std::fs::create_dir_all(&tenant).unwrap();
    // A symlinked preferences.json is never followed or replaced.
    let outside = dir.path().join("outside.json");
    std::fs::write(&outside, "{}").unwrap();
    std::os::unix::fs::symlink(&outside, tenant.join("preferences.json")).unwrap();
    assert!(root.write_preferences(user, Routing::Manual).is_err());
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "{}");
    std::fs::remove_file(tenant.join("preferences.json")).unwrap();
    // A hard-linked one neither.
    std::fs::hard_link(&outside, tenant.join("preferences.json")).unwrap();
    assert!(root.write_preferences(user, Routing::Manual).is_err());
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "{}");
    std::fs::remove_file(tenant.join("preferences.json")).unwrap();
    // A bad tenant id never names a directory.
    assert!(root.write_preferences("../x", Routing::Manual).is_err());
    // The written file is 0600 and read back by the GET.
    root.write_preferences(user, Routing::Manual).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(tenant.join("preferences.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let cancel = std::sync::atomic::AtomicBool::new(false);
    assert_eq!(root.read(user, &cancel).unwrap(), Routing::Manual);
}
