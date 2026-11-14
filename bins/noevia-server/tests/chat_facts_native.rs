//! Track R14 step 7 with `NOEVIA_RUST_CHAT_FACTS=1` (chat_facts_native.rs): the toolbox catalogue the
//! front composes (here `GET /api/toolboxes/permitted`) takes the built-in boxes from the front's own copy
//! of core's definitions, never from the `toolbox-chat-facts.json` core publishes (here it holds a decoy),
//! and needs no such file at all. The switch is read once per process: every test in this binary sets it
//! before the front starts. Synthetic, QA-named data only.
#![cfg(feature = "rust-storage-reads")]
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::{json, Value};
use support::*;

const PERMITTED: &str = "/api/toolboxes/permitted";

fn tool(name: &str, write: bool) -> Value {
    json!({"name": name, "description": format!("{name} (synthetic)"), "write": write})
}

/// What core would have published (a decoy built-in box with a decoy tool) and what the front discovered.
fn published() -> Value {
    json!({
        "harnessEnabled": true, "hasRepositories": true,
        "boxes": [
            {"id": "qa-decoy", "label": "QA decoy", "description": "d", "source": "builtin", "tools": [tool("qa_decoy_tool", false)]},
            {"id": "qa-curated", "label": "QA curated", "description": "d", "source": "mcp", "inApp": true, "server": "qa-mcp", "tools": [tool("qa_curated_read", false)]},
        ],
    })
}

async fn rig() -> (tempfile::TempDir, Front) {
    std::env::set_var("NOEVIA_RUST_CHAT_FACTS", "1");
    // The Offline Wikipedia box: KIWIX_URL (read from the process) and the kiwix construction flag
    // (a restart flag: its selected value, here the env override the front's config reads).
    std::env::set_var("KIWIX_URL", "http://qa-kiwix.invalid:8080");
    let d = tempfile::tempdir().unwrap();
    let _db = seed(d.path());
    let f = front_env(d.path(), true, &[("NOEVIA_FEATURE_KIWIX", "true".into())]).await;
    write_snapshot(&f, d.path(), published());
    (d, f)
}

fn ids(body: &Value) -> Vec<String> {
    body["boxes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_catalogue_takes_the_fronts_built_in_boxes_and_needs_no_core_file() {
    let (d, f) = rig().await;
    let (s, body) = get(f.addr, &format!("{PERMITTED}?mode=chat"), ADMIN).await;
    assert_eq!(s, 200, "{body}");
    let got = ids(&body);
    assert!(!got.iter().any(|i| i == "qa-decoy"), "{got:?}");
    assert_eq!(
        got.iter().take(3).map(String::as_str).collect::<Vec<_>>(),
        ["core", "offline-wikipedia", "gdrive"],
        "core's built-in boxes in core's order, the kiwix one included"
    );
    assert!(got.iter().any(|i| i == "qa-curated"), "{got:?}");
    // core's own tool definitions (contracts/builtin-toolboxes.json), not the decoy's.
    let core = &body["boxes"][0];
    let names: Vec<&str> = core["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["get_current_time", "read_project_file"]);
    // Without core's file, and without asking core to publish (no demand marker), the same answer.
    std::fs::remove_file(d.path().join("toolbox-chat-facts.json")).unwrap();
    let marker = d.path().join("snapshot-demand");
    let _ = std::fs::remove_file(&marker);
    let (s, again) = get(f.addr, &format!("{PERMITTED}?mode=chat"), ADMIN).await;
    assert_eq!(s, 200, "{again}");
    assert_eq!(ids(&again), got);
    assert!(!marker.exists());
}
