//! GET /api/models/preset answers from the native preset facts: one parse and one revision, the
//! ones the conditional writes use. The legacy upstream is a recorder that must stay silent.
//! Synthetic files only, no engine calls. Ported from core's rust-preset-read.test.cjs.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "mcp_support/mod.rs"]
mod support;
use noevia_server::model_maint::ini::{Candidate, Store};
use noevia_server::model_sizing::ReadFault;
use noevia_server::preset_read_routes::{read_outcome, Outcome};
use serde_json::json;
use support::*;

const PATH: &str = "/api/models/preset";
const TEXT: &str = "[*]\nparallel=2\n[m]\nc=4096\nmodel=/private/synthetic.gguf\n";
const EMPTY_REVISION: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn fields() -> serde_json::Value {
    let all: Vec<&str> = llamacpp_presets::read::field_names().collect();
    assert_eq!(all.first(), Some(&"ctx-size"));
    assert_eq!(all.len(), 18);
    json!(all)
}

async fn rig_with(
    extra: &[(&str, String)],
    text: Option<&str>,
) -> (tempfile::TempDir, Front, Upstream) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    if let Some(text) = text {
        std::fs::write(d.path().join("models.ini"), text).unwrap();
    }
    let up = upstream().await;
    up.set(|_| reply_with("202 NodeMethod", "X-Node-Method: yes\r\n", "{}"));
    let mut env = vec![
        ("NOEVIA_LEGACY_UPSTREAM", up.base()),
        ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
    ];
    env.extend(extra.iter().map(|(k, v)| (*k, v.clone())));
    let f = front_env(d.path(), true, &env).await;
    (d, f, up)
}

/// The llama.cpp rig: kind and preset path set at start, the file may come and go after.
async fn rig(text: Option<&str>) -> (tempfile::TempDir, Front, Upstream) {
    let d = tempfile::tempdir().unwrap();
    let ini = d.path().join("models.ini");
    seed(d.path());
    if let Some(text) = text {
        std::fs::write(&ini, text).unwrap();
    }
    let up = upstream().await;
    up.set(|_| reply_with("202 NodeMethod", "X-Node-Method: yes\r\n", "{}"));
    let f = front_env(
        d.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", up.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
            ("MODEL_MANAGER_KIND", "llamacpp".into()),
            ("MODEL_MANAGER_BASE_URL", "http://127.0.0.1:9".into()),
            ("LLAMACPP_PRESET_PATH", ini.display().to_string()),
        ],
    )
    .await;
    (d, f, up)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_get_requires_admin_and_never_asks_node() {
    let (d, f, up) = rig(Some(TEXT)).await;
    assert_eq!(get(f.addr, PATH, "").await.0, 401);
    assert_eq!(get(f.addr, PATH, MEMBER).await.0, 403);
    let (status, headers, body) = raw_text(
        f.addr,
        "GET",
        &format!("{PATH}?model=m&model=ignored"),
        ADMIN,
        "",
        b"",
    )
    .await;
    assert_eq!(status, 200);
    assert!(headers.contains("cache-control: no-store"));
    let text = std::fs::read_to_string(d.path().join("models.ini")).unwrap();
    assert_eq!(text, TEXT, "a read never changes the file");
    assert_eq!(
        j(&body),
        json!({
            "model":"m", "revision":hex(TEXT), "exists":true,
            "options":{"ctx-size":"4096"}, "defaults":{"parallel":"2"},
            "fields":fields()
        })
    );
    assert!(!body.contains("/private/"), "no file content is exported");
    assert!(
        body.find("\"model\"").unwrap() < body.find("\"revision\"").unwrap(),
        "Core's key order"
    );
    assert!(up.seen().is_empty(), "Node is not asked: {:?}", up.seen());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_names_decode_and_an_unknown_model_keeps_the_defaults() {
    let (_d, f, up) = rig(Some(TEXT)).await;
    let (status, body) = get(f.addr, &format!("{PATH}?model=a%2Bb+c"), ADMIN).await;
    assert_eq!(status, 200);
    assert_eq!(body["model"], "a+b c");
    assert_eq!(body["exists"], false);
    assert_eq!(body["options"], json!({}));
    assert_eq!(body["defaults"], json!({"parallel":"2"}));
    let (_, none) = get(f.addr, PATH, ADMIN).await;
    assert_eq!(
        (none["model"].clone(), none["exists"].clone()),
        (json!(""), json!(false))
    );
    let (_, star) = get(f.addr, &format!("{PATH}?model=*"), ADMIN).await;
    assert_eq!(
        (star["exists"].clone(), star["options"].clone()),
        (json!(true), json!({"parallel":"2"}))
    );
    assert!(up.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_valid_empty_file_is_an_answer_not_a_fault() {
    for text in ["", "\n\n", "; synthetic comment only\n"] {
        let (_d, f, up) = rig(Some(text)).await;
        let (status, body) = get(f.addr, &format!("{PATH}?model=m"), ADMIN).await;
        assert_eq!(status, 200, "{text:?}");
        assert_eq!(body["revision"], hex(text));
        assert_eq!(
            (
                body["exists"].clone(),
                body["options"].clone(),
                body["defaults"].clone()
            ),
            (json!(false), json!({}), json!({}))
        );
        assert!(up.seen().is_empty());
    }
    let (_d, f, _up) = rig(Some("")).await;
    assert_eq!(get(f.addr, PATH, ADMIN).await.1["revision"], EMPTY_REVISION);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_revision_is_the_one_a_conditional_write_compares_against() {
    let (d, f, up) = rig(Some(TEXT)).await;
    let file = d.path().join("models.ini");
    let (_, first) = get(f.addr, PATH, ADMIN).await;
    let seen = first["revision"].as_str().unwrap().to_owned();
    let store = Store::new(file.clone(), None);
    assert_eq!(store.snapshot().unwrap().revision, seen);
    // Another writer moves the file: a read says so, and a write from the old read is refused.
    let moved = "[*]\nparallel=2\n[m]\nc=8192\n";
    std::fs::write(&file, moved).unwrap();
    let (_, second) = get(f.addr, &format!("{PATH}?model=m"), ADMIN).await;
    assert_eq!(second["revision"], hex(moved));
    assert_ne!(second["revision"], first["revision"]);
    assert_eq!(second["options"], json!({"ctx-size":"8192"}));
    let stale = Candidate {
        base_revision: seen,
        text: "[m]\nc=2048\n".into(),
        before: TEXT.into(),
    };
    let err = store.commit(&stale, false).await.unwrap_err();
    assert_eq!(err.status, 409);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        moved,
        "a stale write changed nothing"
    );
    let current = Candidate {
        base_revision: hex(moved),
        text: "[m]\nc=2048\n".into(),
        before: moved.into(),
    };
    store.commit(&current, false).await.unwrap();
    let (_, third) = get(f.addr, &format!("{PATH}?model=m"), ADMIN).await;
    assert_eq!(third["revision"], hex("[m]\nc=2048\n"));
    assert_eq!(third["options"], json!({"ctx-size":"2048"}));
    assert!(up.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_faults_keep_public_status_and_hide_private_data() {
    let (d, f, up) = rig(None).await;
    let file = d.path().join("models.ini");
    let missing = (500, json!({"error":"Internal error"}));
    assert_eq!(get(f.addr, PATH, ADMIN).await, missing, "missing file");
    std::fs::write(&file, "[m]\n[m]\nmodel=/private/x\n").unwrap();
    assert_eq!(
        get(f.addr, PATH, ADMIN).await,
        (
            409,
            json!({"error":"Duplicate preset sections require operator repair"})
        )
    );
    std::fs::write(&file, "x".repeat(1024 * 1024 + 1)).unwrap();
    assert_eq!(
        get(f.addr, PATH, ADMIN).await,
        (413, json!({"error":"Preset file exceeds the editor limit"}))
    );
    std::fs::write(&file, "x".repeat(1024 * 1024)).unwrap();
    assert_eq!(
        get(f.addr, PATH, ADMIN).await.0,
        200,
        "exactly at the limit"
    );
    std::fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(d.path().join("absent.ini"), &file).unwrap();
    assert_eq!(
        get(f.addr, PATH, ADMIN).await,
        (409, json!({"error":"Preset path must be a regular file"}))
    );
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    assert_eq!(
        get(f.addr, PATH, ADMIN).await.0,
        409,
        "a directory is not a file"
    );
    assert!(up.seen().is_empty());
}

#[test]
fn missing_unreadable_and_empty_are_three_outcomes() {
    use server_stats::Kind;
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("models.ini");
    let store = Store::new(file.clone(), None);
    let outcome = || read_outcome(Kind::LlamaCpp, Some(&store), "m");
    assert_eq!(outcome(), Outcome::Missing);
    assert!(matches!(store.load(), Err(ReadFault::Missing(_))));
    std::fs::write(&file, "").unwrap();
    let Outcome::Facts(body) = outcome() else {
        panic!("a valid empty file is facts");
    };
    assert_eq!(j(&body)["revision"], EMPTY_REVISION);
    assert!(store.load().unwrap().revision == EMPTY_REVISION);
    // A path through a file cannot be opened: unreadable, not missing.
    let through = Store::new(file.join("inner.ini"), None);
    assert!(matches!(through.load(), Err(ReadFault::Unreadable(_))));
    assert_eq!(
        read_outcome(Kind::LlamaCpp, Some(&through), "m"),
        Outcome::Unreadable
    );
    std::fs::write(&file, "[a]\n[a]\n").unwrap();
    assert_eq!(outcome(), Outcome::Duplicate);
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    assert_eq!(outcome(), Outcome::NonRegular);
    // Not llama.cpp: unavailable. llama.cpp with no preset file configured: unconfigured.
    assert_eq!(
        read_outcome(Kind::None, Some(&store), "m"),
        Outcome::Unavailable
    );
    assert_eq!(
        read_outcome(Kind::Lemonade, None, "m"),
        Outcome::Unavailable
    );
    assert_eq!(
        read_outcome(Kind::LlamaCpp, None, "m"),
        Outcome::Unconfigured
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_llamacpp_or_a_preset_path_the_routes_keep_their_statuses() {
    let (_d, f, up) = rig_with(&[], Some(TEXT)).await;
    assert_eq!(
        get(f.addr, PATH, ADMIN).await,
        (404, json!({"error":"Native presets are unavailable"}))
    );
    let (_d, f, up2) = rig_with(
        &[
            ("MODEL_MANAGER_KIND", "llamacpp".into()),
            ("MODEL_MANAGER_BASE_URL", "http://127.0.0.1:9".into()),
        ],
        None,
    )
    .await;
    assert_eq!(
        get(f.addr, PATH, ADMIN).await,
        (
            501,
            json!({"error":"Native preset editing is not exposed by this llama.cpp adapter"})
        )
    );
    assert!(up.seen().is_empty() && up2.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writer_other_methods_and_trailing_path_still_proxy() {
    let (_d, f, up) = rig(Some(TEXT)).await;
    for (method, path) in [
        ("PUT", PATH),
        ("POST", PATH),
        ("HEAD", PATH),
        ("GET", "/api/models/preset/"),
    ] {
        let (status, headers, _) = raw_text(f.addr, method, path, ADMIN, "", b"").await;
        assert_eq!(status, 202, "{method} {path}");
        assert!(headers.contains("x-node-method: yes"));
    }
    assert!(up
        .seen()
        .iter()
        .all(|r| r.path != "/internal/preset-read/facts"));
}
