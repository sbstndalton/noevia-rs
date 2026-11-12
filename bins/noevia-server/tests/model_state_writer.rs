//! R3-0: the model-state writers, native. The loader sections client (the same two calls as Core's
//! presetSections/removePresetSection, server/routes/models.cjs) and the front's one writer of the
//! last-loaded model and the identity cache, with its internal route `POST /v1/model-state/forget`.
//! Synthetic upstreams and QA names only; no engine, no models.ini.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#[path = "mcp_support/mod.rs"]
mod support;
use noevia_server::{config::Config, model_manager_scan::Loader, App};
use serde_json::json;
use std::collections::HashMap;
use support::*;

fn config(env: &[(&str, String)]) -> Config {
    let mut env: HashMap<String, String> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    env.entry("NOEVIA_LEGACY_UPSTREAM".into())
        .or_insert_with(|| "http://127.0.0.1:9".into());
    Config::from_lookup(|k| env.get(k).cloned()).unwrap()
}

fn loader_of(env: &[(&str, String)]) -> Loader {
    let env: HashMap<String, String> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    Loader::from_lookup(|k| env.get(k).cloned()).unwrap()
}

async fn loader(listing: &str) -> (Upstream, Loader) {
    let up = upstream().await;
    let listing = listing.to_owned();
    up.set(move |r| {
        if r.method == "GET" && r.path == "/api/v1/sections" {
            reply("200 OK", &listing)
        } else if r.method == "DELETE" {
            reply("200 OK", r#"{"ok":true}"#)
        } else {
            reply("404 Not Found", "{}")
        }
    });
    let cfg = loader_of(&[
        ("MODEL_LOADER_URL", format!("{}/", up.base())),
        ("MODEL_LOADER_TOKEN", "synthetic-loader-key".into()),
    ]);
    (up, cfg)
}

#[tokio::test]
async fn sections_lists_strings_and_objects_with_the_revision() {
    let (up, cfg) = loader(
        r#"{"revision":"rev-1","sections":["qa-plain",{"name":"qa-a","hasFile":false},{"name":"qa-b","hasFile":true},{"name":"qa-c"},{"hasFile":false}]}"#,
    )
    .await;
    let got = cfg.sections().await.unwrap();
    assert_eq!(got.revision.as_deref(), Some("rev-1"));
    let names: Vec<_> = got
        .sections
        .iter()
        .map(|s| (s.name.as_deref(), s.has_file))
        .collect();
    assert_eq!(
        names,
        [
            (Some("qa-plain"), None),
            (Some("qa-a"), Some(false)),
            (Some("qa-b"), Some(true)),
            (Some("qa-c"), None),
            (None, Some(false)),
        ]
    );
    let seen = up.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].header("x-model-loader-token").as_deref(),
        Some("synthetic-loader-key")
    );
}

#[tokio::test]
async fn sections_is_none_when_unconfigured_not_ok_or_not_an_array() {
    assert!(loader_of(&[]).sections().await.is_none());
    let (up, cfg) = loader(r#"{"sections":"nope"}"#).await;
    assert!(cfg.sections().await.is_none());
    up.set(|_| reply("500 Oops", r#"{"sections":[]}"#));
    assert!(cfg.sections().await.is_none());
}

#[tokio::test]
async fn delete_sends_the_encoded_name_and_the_revision_it_listed() {
    let (up, cfg) =
        loader(r#"{"revision":"rev 1/x","sections":[{"name":"qa a/b+c'(1)","hasFile":false}]}"#)
            .await;
    let out = cfg.remove_section("qa a/b+c'(1)", true).await;
    assert!(out.removed && out.has_file.is_none());
    let del = up
        .seen()
        .into_iter()
        .find(|r| r.method == "DELETE")
        .unwrap();
    // encodeURIComponent keeps ! ~ * ' ( ) - _ . and escapes the rest.
    assert_eq!(
        del.path,
        "/api/v1/sections/qa%20a%2Fb%2Bc'(1)?baseRevision=rev%201%2Fx"
    );
    assert_eq!(
        del.header("x-model-loader-token").as_deref(),
        Some("synthetic-loader-key")
    );
}

#[tokio::test]
async fn delete_without_a_revision_has_no_query() {
    let (up, cfg) = loader(r#"{"sections":[{"name":"qa-a"}]}"#).await;
    assert!(cfg.remove_section("qa-a", false).await.removed);
    let del = up
        .seen()
        .into_iter()
        .find(|r| r.method == "DELETE")
        .unwrap();
    assert_eq!(del.path, "/api/v1/sections/qa-a");
}

#[tokio::test]
async fn an_unlisted_name_is_never_sent_anywhere() {
    let (up, cfg) =
        loader(r#"{"revision":"r","sections":[{"name":"qa-a","hasFile":false}]}"#).await;
    assert!(!cfg.remove_section("qa-other", false).await.removed);
    assert!(!cfg.remove_section("", false).await.removed);
    assert!(up.seen().iter().all(|r| r.method == "GET"));
}

#[tokio::test]
async fn only_without_file_keeps_a_section_whose_file_is_not_known_to_be_gone() {
    let (up, cfg) = loader(
        r#"{"revision":"r","sections":[{"name":"qa-has","hasFile":true},{"name":"qa-unknown"},"qa-string"]}"#,
    )
    .await;
    let has = cfg.remove_section("qa-has", true).await;
    assert!(!has.removed && has.has_file == Some(true));
    for name in ["qa-unknown", "qa-string"] {
        let out = cfg.remove_section(name, true).await;
        assert!(!out.removed, "{name}");
    }
    assert!(up.seen().iter().all(|r| r.method == "GET"));
}

#[tokio::test]
async fn a_refused_delete_or_dead_loader_is_not_removed_and_never_throws() {
    let (up, cfg) =
        loader(r#"{"revision":"r","sections":[{"name":"qa-a","hasFile":false}]}"#).await;
    up.set(|r| {
        if r.method == "DELETE" {
            reply("409 Conflict", r#"{"error":"stale revision"}"#)
        } else {
            reply(
                "200 OK",
                r#"{"revision":"r","sections":[{"name":"qa-a","hasFile":false}]}"#,
            )
        }
    });
    assert!(!cfg.remove_section("qa-a", false).await.removed);
    let dead = loader_of(&[("MODEL_LOADER_URL", "http://127.0.0.1:9".into())]);
    assert!(!dead.remove_section("qa-a", false).await.removed);
}

const SECRET: &str = "synthetic-model-state-secret-0000000000";

async fn store_front(
    node: &Upstream,
) -> (
    std::net::SocketAddr,
    std::sync::Arc<App>,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&[
        ("NOEVIA_LEGACY_UPSTREAM", node.base()),
        ("UI_DATA_DIR", dir.path().display().to_string()),
        ("NOEVIA_PROJECT_STORE_TOKEN", SECRET.into()),
    ]);
    let app = App::new(cfg);
    let store = noevia_server::project_store::Store {
        data_dir: dir.path().to_owned(),
        token: std::sync::Arc::from(SECRET),
        switch: server_store::RustProjects::owned(),
        start_id: std::sync::Arc::from("synthetic-start"),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = noevia_server::auto_roles_routes::store_route(store, Some(app.clone()));
    let task = tokio::spawn(async move {
        let _keep = dir;
        axum::serve(listener, router).await.unwrap();
    });
    (addr, app, task)
}

async fn forget(
    addr: std::net::SocketAddr,
    token: Option<&str>,
    body: &str,
) -> (u16, serde_json::Value) {
    let mut r = reqwest::Client::new()
        .post(format!("http://{addr}/v1/model-state/forget"))
        .header("content-type", "application/json")
        .body(body.to_owned());
    if let Some(t) = token {
        r = r.header("x-noevia-store-token", t);
    }
    let res = r.send().await.unwrap();
    let status = res.status().as_u16();
    (
        status,
        serde_json::from_str(&res.text().await.unwrap()).unwrap_or(json!(null)),
    )
}

#[tokio::test]
async fn forget_clears_the_last_loaded_model_only_for_the_same_name_and_tells_node() {
    let node = upstream().await;
    node.set(|_| reply("200 OK", r#"{"ok":true}"#));
    let (addr, app, task) = store_front(&node).await;
    let maint = &app.config.maint;
    assert!(maint.adopt_last_loaded("qa-gone"));
    // Node's copy is adopted too: the first model the installed list names; a second name never replaces it.
    assert!(!maint.adopt_last_loaded("qa-other"));
    assert_eq!(maint.last_loaded().as_deref(), Some("qa-gone"));

    assert_eq!(forget(addr, None, r#"{"name":"qa-gone"}"#).await.0, 401);
    assert_eq!(
        forget(addr, Some("wrong"), r#"{"name":"qa-gone"}"#).await.0,
        401
    );
    for bad in [
        "",
        "{}",
        r#"{"name":""}"#,
        r#"{"name":3}"#,
        r#"{"name":"a","x":1}"#,
        "[]",
        "null",
    ] {
        assert_eq!(forget(addr, Some(SECRET), bad).await.0, 400, "{bad}");
    }
    assert_eq!(maint.last_loaded().as_deref(), Some("qa-gone"));
    assert_eq!(node.count("/internal/model-state/"), 0);

    let (s, v) = forget(addr, Some(SECRET), r#"{"name":"qa-different"}"#).await;
    assert_eq!(
        (s, v),
        (
            200,
            json!({"lastLoadedCleared": false, "nodeCleared": true})
        )
    );
    assert_eq!(maint.last_loaded().as_deref(), Some("qa-gone"));

    let (s, v) = forget(addr, Some(SECRET), r#"{"name":"qa-gone"}"#).await;
    assert_eq!(
        (s, v),
        (200, json!({"lastLoadedCleared": true, "nodeCleared": true}))
    );
    assert_eq!(maint.last_loaded(), None);
    let asks: Vec<_> = node
        .seen()
        .into_iter()
        .filter(|r| r.path == "/internal/model-state/forget")
        .collect();
    assert_eq!(asks.len(), 2);
    assert_eq!(j(&asks[1].body), json!({"name": "qa-gone"}));
    task.abort();
}

#[tokio::test]
async fn forget_still_clears_the_front_when_node_does_not_answer_and_says_so() {
    let node = upstream().await;
    node.set(|_| reply("404 Not Found", "{}"));
    let (addr, app, task) = store_front(&node).await;
    assert!(app.config.maint.adopt_last_loaded("qa-gone"));
    let (s, v) = forget(addr, Some(SECRET), r#"{"name":"qa-gone"}"#).await;
    assert_eq!(
        (s, v),
        (
            200,
            json!({"lastLoadedCleared": true, "nodeCleared": false})
        )
    );
    assert_eq!(app.config.maint.last_loaded(), None);
    task.abort();
}
