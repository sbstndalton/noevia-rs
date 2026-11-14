//! Track R14 step 4 with `NOEVIA_RUST_PROJECT_CATALOGUE=1` (project_catalogue.rs): the project routes
//! take the toolbox catalogue from the front's own composition (the chat facts plus its discovery),
//! never from the `project-catalogue.json` core publishes, which here holds decoy ids. The switch is
//! read once per process: every test in this binary sets it before the front starts. Synthetic,
//! QA-named data only; no Node (an unreachable upstream).
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::{json, Value};
use support::*;

fn tool(name: &str) -> Value {
    json!({"name": name, "description": "synthetic", "write": false})
}

/// The front's inputs: core's built-in boxes (core, the Drive connector box) and one discovered box.
fn facts() -> Value {
    json!({"connectorBoxes": ["gdrive"], "defaults": ["core"], "boxes": [
        {"id": "core", "label": "core", "description": "d", "source": "builtin", "tools": []},
        {"id": "gdrive", "label": "gdrive", "description": "d", "source": "builtin", "tools": [tool("qa_drive_read")]},
        {"id": "qa-curated", "label": "QA curated", "description": "d", "source": "mcp", "inApp": true, "server": "qa-mcp", "tools": [tool("qa_curated_read")]}
    ]})
}

/// What core would have published: ids the native catalogue does not have.
fn decoy(dir: &std::path::Path) {
    std::fs::write(
        dir.join("project-catalogue.json"),
        r#"{"version":1,"selectable":["core","qa-decoy"],"defaults":["qa-decoy"],"icons":["qa-decoy-icon"],"known":["core","qa-decoy"]}"#,
    )
    .unwrap();
}

async fn rig(with_facts: bool) -> (tempfile::TempDir, Front) {
    std::env::set_var("NOEVIA_RUST_PROJECT_CATALOGUE", "1");
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let f = front_env(
        d.path(),
        true,
        &[("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into())],
    )
    .await;
    decoy(d.path());
    if with_facts {
        write_snapshot(&f, d.path(), facts());
    }
    (d, f)
}

fn stored(dir: &std::path::Path) -> Vec<Value> {
    let text = std::fs::read_to_string(dir.join("users").join(A).join("projects.json")).unwrap();
    serde_json::from_str::<Value>(&text).unwrap()["projects"]
        .as_array()
        .unwrap()
        .clone()
}

fn by_name(dir: &std::path::Path, name: &str) -> Value {
    stored(dir)
        .into_iter()
        .find(|p| p["name"] == name)
        .unwrap_or_else(|| panic!("no project {name}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_and_configuration_use_the_fronts_catalogue_not_the_file() {
    let (d, f) = rig(true).await;
    // The selection is judged against the native ids: the discovered box is kept, the file's decoy and
    // the connector box are dropped.
    let (s, v) = write(
        f.addr,
        "POST",
        "/api/projects",
        ADMIN,
        r#"{"name":"QA picked","toolboxes":["qa-curated","qa-decoy","gdrive","core"]}"#,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        by_name(d.path(), "QA picked")["toolboxes"],
        json!(["qa-curated", "core"])
    );
    // No selection: the facts' defaults, not the file's.
    let (s, v) = write(
        f.addr,
        "POST",
        "/api/projects",
        ADMIN,
        r#"{"name":"QA plain"}"#,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    let plain = by_name(d.path(), "QA plain");
    assert_eq!(plain["toolboxes"], json!(["core"]));
    let id = plain["id"].as_str().unwrap().to_owned();
    let config = format!("/api/projects/{id}/config");
    // Configuration: toolboxes and icons from the native catalogue.
    let (s, v) = write(
        f.addr,
        "POST",
        &config,
        ADMIN,
        r#"{"toolboxes":["qa-decoy","qa-curated"]}"#,
    )
    .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        by_name(d.path(), "QA plain")["toolboxes"],
        json!(["qa-curated"])
    );
    let (s, _) = write(
        f.addr,
        "POST",
        &config,
        ADMIN,
        r#"{"icon":"qa-decoy-icon"}"#,
    )
    .await;
    assert_eq!(s, 400);
    let (s, v) = write(f.addr, "POST", &config, ADMIN, r#"{"icon":"book"}"#).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(by_name(d.path(), "QA plain")["icon"], json!("book"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn not_ready_is_not_published_even_with_a_file_present() {
    let (d, f) = rig(false).await;
    // A create waits its 10 s for the native catalogue, then refuses; the decoy file is not used.
    let started = std::time::Instant::now();
    let (s, _) = write(
        f.addr,
        "POST",
        "/api/projects",
        ADMIN,
        r#"{"name":"QA wait"}"#,
    )
    .await;
    assert_eq!(s, 503);
    let took = started.elapsed();
    assert!(took >= std::time::Duration::from_secs(9), "{took:?}");
    assert!(took < std::time::Duration::from_secs(14), "{took:?}");
    assert!(!d
        .path()
        .join("users")
        .join(A)
        .join("projects.json")
        .exists());
    // Ready while a create waits: that create is served (and did wait).
    let started = std::time::Instant::now();
    let waiting = tokio::spawn({
        let addr = f.addr;
        async move {
            write(
                addr,
                "POST",
                "/api/projects",
                ADMIN,
                r#"{"name":"QA late"}"#,
            )
            .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    write_snapshot(&f, d.path(), facts());
    let (s, v) = waiting.await.unwrap();
    assert_eq!(s, 200, "{v}");
    assert!(started.elapsed() >= std::time::Duration::from_millis(400));
    let id = by_name(d.path(), "QA late")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let config = format!("/api/projects/{id}/config");
    // Not ready again (the facts gone): a patch that reads the catalogue refuses, one that does not is
    // answered without composing it.
    std::fs::remove_file(d.path().join("toolbox-chat-facts.json")).unwrap();
    let (s, _) = write(f.addr, "POST", &config, ADMIN, r#"{"icon":"book"}"#).await;
    assert_eq!(s, 503);
    // Composing would touch the demand marker (and wait for a fresh facts file): without the marker,
    // a patch that does not read the catalogue leaves it absent.
    let marker = d.path().join("snapshot-demand");
    std::fs::remove_file(&marker).unwrap();
    let started = std::time::Instant::now();
    let (s, v) = write(f.addr, "POST", &config, ADMIN, r#"{"goal":"QA goal"}"#).await;
    assert_eq!(s, 200, "{v}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    assert!(!marker.exists());
    assert_eq!(by_name(d.path(), "QA late")["goal"], json!("QA goal"));
}
