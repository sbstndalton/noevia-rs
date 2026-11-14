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

/// The front's internal store listener with its feature context (the shared harness serves the store
/// without one), as core's feature-runtime.cjs reaches it.
async fn context_listener(f: &Front, dir: &std::path::Path, start: &str) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = noevia_server::project_store::Store {
        data_dir: dir.to_path_buf(),
        token: STORE_TOKEN.into(),
        switch: server_store::RustProjects::owned(),
        start_id: start.into(),
    };
    let context = f.app.feature_context.clone();
    assert!(
        context.is_some(),
        "NOEVIA_START_ID gives the front a feature context"
    );
    tokio::spawn(async move {
        axum::serve(
            listener,
            noevia_server::project_store::router_with_context(store, context),
        )
        .await
        .unwrap()
    });
    addr
}

/// One call to the front's feature-context op as core's feature-runtime.cjs makes it (store token).
async fn context(at: std::net::SocketAddr, body: Value) -> Value {
    let (s, v) = raw(
        at,
        "POST",
        "/v1/features/context",
        "",
        &format!(
            "{}: {STORE_TOKEN}\r\n",
            noevia_server::project_store::TOKEN_HEADER
        ),
        body.to_string().as_bytes(),
    )
    .await;
    assert_eq!(s, 200, "{v}");
    v
}

/// sbstndalton/noevia#2667: with core registered here (a feature context), the Offline Wikipedia box
/// follows core's own construction value (its boot facts), not the stored setting, and is not ready
/// before core's first registration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_kiwix_box_follows_cores_registered_construction_value() {
    const START: &str = "0123456789abcdef0123456789abcdef";
    std::env::set_var("NOEVIA_RUST_CHAT_FACTS", "1");
    std::env::set_var("KIWIX_URL", "http://qa-kiwix.invalid:8080");
    let d = tempfile::tempdir().unwrap();
    let db = seed(d.path());
    // The setting says off; core started with it on (boot facts below).
    db.execute(
        "INSERT INTO settings(key, value) VALUES('feature:kiwix', 'false')",
        [],
    )
    .unwrap();
    let f = front_env(d.path(), true, &[("NOEVIA_START_ID", START.into())]).await;
    write_snapshot(&f, d.path(), published());
    // No registration yet: not ready (a setting changed since core's start must not show early).
    let (s, _) = get(f.addr, &format!("{PERMITTED}?mode=chat"), ADMIN).await;
    assert_eq!(s, 503);
    let at = context_listener(&f, d.path(), START).await;
    let hello = context(
        at,
        json!({"op": "hello", "request": "00000000-0000-4000-8000-000000000001", "start": START}),
    )
    .await;
    let snapshot = f
        .app
        .writes
        .writer()
        .unwrap()
        .read(|r| {
            use server_account::feature_context::{source_fingerprint, ContextSource};
            Ok(json!({
                "sources": {
                    "decision": source_fingerprint(r, ContextSource::Decision)?,
                    "machines": source_fingerprint(r, ContextSource::InferenceMachines)?,
                },
                "availability": {"stepSupervision": null, "toolGate": null, "chatFraming": null, "framingReasoner": null, "systemOneRouting": null},
            }))
        })
        .unwrap();
    let registered = context(
        at,
        json!({"op": "register", "request": "00000000-0000-4000-8000-000000000004", "start": START,
            "incarnation": hello["incarnation"], "boot": "00000000-0000-4000-8000-000000000002",
            "session": "00000000-0000-4000-8000-000000000003", "sequence": 0,
            "facts": {"env": vec![Option::<bool>::None; 23], "restart": [false, true], "browser": "available", "code": false, "trustProxy": false},
            "snapshot": snapshot}),
    )
    .await;
    assert_eq!(registered["status"], json!("ready"), "{registered}");
    let (s, body) = get(f.addr, &format!("{PERMITTED}?mode=chat"), ADMIN).await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(
        ids(&body)
            .iter()
            .take(3)
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["core", "offline-wikipedia", "gdrive"],
        "core's registered value wins over the stored setting"
    );
}

/// sbstndalton/noevia#2667: with no feature context (no core registers here) and no env override, the
/// Offline Wikipedia box follows the stored typed setting (features.cjs `selected().value`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_registration_the_kiwix_box_follows_the_stored_setting() {
    std::env::set_var("NOEVIA_RUST_CHAT_FACTS", "1");
    std::env::set_var("KIWIX_URL", "http://qa-kiwix.invalid:8080");
    for (stored, listed) in [("'true'", true), ("'false'", false), ("1", false)] {
        let d = tempfile::tempdir().unwrap();
        let db = seed(d.path());
        db.execute(
            &format!("INSERT INTO settings(key, value) VALUES('feature:kiwix', {stored})"),
            [],
        )
        .unwrap();
        let f = front_env(d.path(), true, &[]).await;
        assert!(f.app.feature_context.is_none());
        write_snapshot(&f, d.path(), published());
        let (s, body) = get(f.addr, &format!("{PERMITTED}?mode=chat"), ADMIN).await;
        assert_eq!(s, 200, "{body}");
        assert_eq!(
            ids(&body).iter().any(|i| i == "offline-wikipedia"),
            listed,
            "stored {stored}"
        );
    }
}
