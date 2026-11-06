//! noevia#1318: native models.ini writers against the actual Python answers and files.
//!
//! `fixtures/section-writes-python.v1.json` comes from `tools/gen-section-writes-oracle.py`, which
//! runs the real services app step by step on synthetic models folders (backup names pinned to one
//! UTC instant) and records every answer, the safe-defaults plan the child gives for the same
//! state, and the files left behind. Each case is rebuilt here and replayed against the native
//! router with a fake child serving those plans; the answers and the final files must match.
#![cfg(feature = "native-section-writes")]
#![allow(
    unused_imports,
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use noevia_model_manager::{
    section_writes::SectionWrites, Container, DockerRead, Redactor, StateData,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    future::Future,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};
use tower::ServiceExt;

const TOKEN: &str = "synthetic-model-loader-token-0123456789abcdef";

type Job<'a, T> = Pin<Box<dyn Future<Output = Result<T, &'static str>> + Send + 'a>>;
struct NoDocker;
impl DockerRead for NoDocker {
    fn containers(&self) -> Job<'_, Vec<Container>> {
        Box::pin(async { Err("not used") })
    }
    fn logs<'a>(&'a self, _: &'a str, _: usize) -> Job<'a, Vec<u8>> {
        Box::pin(async { Err("not used") })
    }
}

fn state(writes: SectionWrites, _legacy_trap_base: &str) -> Arc<StateData> {
    Arc::new(StateData {
        #[cfg(feature = "native-diagnose")]
        diagnosis: None,
        prompts: None,
        #[cfg(feature = "native-settings")]
        settings: None,
        #[cfg(feature = "native-model-updates")]
        model_updates: None,
        #[cfg(feature = "native-model-detail")]
        model_detail: None,
        #[cfg(feature = "native-model-list")]
        model_list: None,
        #[cfg(feature = "native-download-targets")]
        download_targets: None,
        #[cfg(feature = "native-badges")]
        badges: None,
        #[cfg(feature = "native-bench-runs")]
        bench_runs: None,
        section_writes: Some(Arc::new(writes)),
        #[cfg(feature = "native-settings-write")]
        settings_write: None,
        #[cfg(feature = "native-benchmark-overview")]
        benchmark_overview: None,
        #[cfg(feature = "native-check-updates")]
        check_updates: None,
        #[cfg(feature = "native-downloads")]
        downloads: None,
        #[cfg(feature = "native-model-delete")]
        model_delete: None,
        #[cfg(feature = "native-draft-heads")]
        draft_heads: None,
        #[cfg(feature = "native-search")]
        search: None,
        #[cfg(feature = "native-backends")]
        backend_stats: None,
        #[cfg(feature = "native-benchmark-runner")]
        bench_runner: None,
        #[cfg(feature = "native-backend-control")]
        backends: None,
        sections: None,
        embedding: None,
        host: None,
        token: TOKEN.into(),
        configured: vec![],
        docker: Arc::new(NoDocker),
        redactor: Redactor::new().unwrap(),
        permits: tokio::sync::Semaphore::new(4),
        idle_unload: None,
        #[cfg(feature = "native-section-autoconfig")]
        section_autoconfig: None,
    })
}

// A fake child: `/api/v1/internal/startup-ready` advertises completion; the plan call answers whatever `plan` holds.
async fn child(plan: Arc<Mutex<Option<(u16, String)>>>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let plan = plan.clone();
            tokio::spawn(async move {
                let mut seen = Vec::new();
                let mut buf = [0u8; 4096];
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    seen.extend_from_slice(&buf[..n]);
                }
                let line = String::from_utf8_lossy(&seen)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_owned();
                let (status, body) = if line.starts_with("GET /api/v1/internal/startup-ready ") {
                    (200, r#"{"startup_ready":1}"#.to_owned())
                } else if line.starts_with("GET /api/v1/internal/safe-defaults-plan?") {
                    plan.lock().unwrap().clone().unwrap_or((599, String::new()))
                } else {
                    (404, String::new())
                };
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
            });
        }
    });
    base
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/section-writes-python.v1.json")).unwrap()
}

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

fn listing(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.file_type().unwrap().is_dir() {
            listing(&entry.path(), &rel, out);
        } else if !name.contains(".tmp-") {
            out.push(rel);
        }
    }
}

#[derive(Debug)]
struct Answer {
    status: u16,
    content_type: Option<String>,
    body: String,
}

async fn send(state: Arc<StateData>, step: &Value, rev: &str) -> Answer {
    let path = step["path"].as_str().unwrap().replace("{REV}", rev);
    let mut request = Request::builder()
        .method(step["method"].as_str().unwrap())
        .uri(path)
        .header("x-model-loader-token", TOKEN);
    let body = match step["body"].as_str() {
        Some(b) => {
            if let Some(ct) = step["content_type"].as_str() {
                request = request.header("content-type", ct);
            }
            Body::from(b.replace("{REV}", rev))
        }
        None => Body::empty(),
    };
    let response = noevia_model_manager::router(state)
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    Answer {
        status: response.status().as_u16(),
        content_type: response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned()),
        body: String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap(),
    }
}

// A `MIGRATE` step: the startup cache-ram migration as the parent runs it (noevia#2010).
async fn migrate(state: &Arc<StateData>) -> Answer {
    let writes = state.section_writes.clone().unwrap();
    match writes.migrate_cache_ram_at_startup().await {
        Ok(changed) => Answer {
            status: 200,
            content_type: None,
            body: serde_json::to_string(&changed).unwrap(),
        },
        Err(_) => Answer {
            status: 500,
            content_type: None,
            body: String::new(),
        },
    }
}

#[tokio::test]
async fn non_native_shapes_never_write() {
    // Trailing slashes, other methods and an encoded slash are not the native writers: they
    // keep the existing 404/405 (or reach the child, here a dead address), and write nothing.
    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("models.ini");
    let s = state(SectionWrites::new(&ini, (1024, 2048)), "http://127.0.0.1:9");
    for (method, path) in [
        ("PUT", "/api/v1/sections/a/"),
        ("GET", "/api/v1/sections/a/rename"),
        ("PUT", "/api/v1/sections/a%2Fb"),
        ("POST", "/api/v1/models-ini"),
    ] {
        let answer = noevia_model_manager::router(s.clone())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("x-model-loader-token", TOKEN)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(!answer.status().is_success(), "{method} {path}");
    }
    assert!(!ini.exists());
}

#[cfg(feature = "native-safe-defaults")]
#[tokio::test]
async fn native_safe_defaults_never_asks_child_for_a_plan() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    std::fs::write(models.join("syn-bad.gguf"), b"not a valid header").unwrap();
    let ini = models.join("models.ini");
    let writes = SectionWrites::new(&ini, (1024, 2048));
    writes.set_safe_models(&models);
    // This child rejects any planning request. The selected native planner must bypass it.
    let base = child(Arc::new(Mutex::new(None))).await;
    let state = state(writes, &base);
    let step = serde_json::json!({"method":"POST","path":"/api/v1/sections/syn-bad/safe-defaults","body":null,"content_type":null});
    let answer = send(state.clone(), &step, "").await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert!(std::fs::read_to_string(&ini)
        .unwrap()
        .contains("ctx-size = 8192"));
    let again = send(state, &step, "").await;
    assert_eq!(again.status, 409);
}

#[tokio::test]
async fn conditional_publication_checks_generation_at_loader_and_retains_a_queryable_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("models.ini");
    let before = "[preset]\nmodel = /models/synthetic.gguf\n";
    std::fs::write(&ini, before).unwrap();
    let base = format!("{:x}", Sha256::digest(before.as_bytes()));
    let journal = dir.path().join(".models.ini.noevia-publication.sqlite");
    let db = rusqlite::Connection::open(&journal).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE preset_authority(singleton INTEGER PRIMARY KEY,incarnation TEXT,lease TEXT,generation TEXT,active INTEGER,until_ms TEXT,recovery INTEGER); CREATE TABLE preset_operations(id TEXT PRIMARY KEY,incarnation TEXT,lease TEXT,generation TEXT,kind TEXT,expected_revision TEXT,state TEXT,revision TEXT,created_ms INTEGER); CREATE TABLE preset_revision(singleton INTEGER PRIMARY KEY,revision TEXT)").unwrap();
    db.execute(
        "INSERT INTO preset_authority VALUES(1,'front','hold','2',1,?1,0)",
        [(std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            + 60_000)
            .to_string()],
    )
    .unwrap();
    for (operation, generation) in [("old-op", "1"), ("fresh-op", "2")] {
        db.execute("INSERT INTO preset_operations VALUES(?1,'front','hold',?2,'preset-write',?3,'issued',NULL,0)",rusqlite::params![operation,generation,base]).unwrap();
    }
    drop(db);
    let app = noevia_model_manager::router(state(
        SectionWrites::new(&ini, (1024, 2048)),
        "http://127.0.0.1:9",
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let after = format!("{before}ctx-size = 8192\n");
    let proof = |operation: &str, generation: u64| serde_json::json!({"incarnation":"front","lease":"hold","generation":generation,"operation":operation,"kind":"preset-write","expectedRevision":base});
    let stale = reqwest::Client::new()
        .put(format!("{url}/api/v1/models-ini"))
        .header("x-model-loader-token", TOKEN)
        .header("content-type", "application/json")
        .body(
            serde_json::json!({"baseRevision":base,"text":after,"maintenance":proof("old-op",1)})
                .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        stale.status().as_u16(),
        409,
        "stale generation conflicts despite a matching INI revision"
    );
    assert_eq!(std::fs::read_to_string(&ini).unwrap(), before);
    let fresh = proof("fresh-op", 2);
    let response = reqwest::Client::new()
        .put(format!("{url}/api/v1/models-ini"))
        .header("x-model-loader-token", TOKEN)
        .header("content-type", "application/json")
        .body(serde_json::json!({"baseRevision":base,"text":after,"maintenance":fresh}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(std::fs::read_to_string(&ini).unwrap(), after);
    let status = reqwest::Client::new()
        .post(format!("{url}/api/v1/maintenance-operation/status"))
        .header("content-type", "application/json")
        .body(serde_json::json!({"maintenance":fresh}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(status.status().as_u16(), 200);
    let receipt: Value = serde_json::from_slice(&status.bytes().await.unwrap()).unwrap();
    assert_eq!(receipt["state"], "settled");
    assert_eq!(
        receipt["revision"],
        format!("{:x}", Sha256::digest(after.as_bytes()))
    );
    let mut forged = fresh.clone();
    forged["operation"] = serde_json::json!("unknown-op");
    let response = reqwest::Client::new()
        .post(format!("{url}/api/v1/maintenance-operation/settle"))
        .header("content-type", "application/json")
        .body(serde_json::json!({"maintenance":forged,"state":"settled"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
    let mut current = after;
    for explicit_null in [false, true] {
        let next = format!("{current}# legacy client\n");
        let mut body = serde_json::json!({"baseRevision":format!("{:x}",Sha256::digest(current.as_bytes())),"text":next});
        if explicit_null {
            body["maintenance"] = Value::Null;
        }
        let response = reqwest::Client::new()
            .put(format!("{url}/api/v1/models-ini"))
            .header("x-model-loader-token", TOKEN)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            200,
            "legacy clients need no new metadata"
        );
        assert_eq!(std::fs::read_to_string(&ini).unwrap(), next);
        current = next;
    }
    server.abort();
    let _ = server.await;
}

#[cfg(feature="native-safe-defaults")]
fn scoped_publication_rig(text:&str,kind:&str)->(tempfile::TempDir,Arc<StateData>,model_publication::Fence) {
    let dir=tempfile::tempdir().unwrap();let ini=dir.path().join("models.ini");let models=dir.path().join("models");
    std::fs::create_dir(&models).unwrap();std::fs::write(&ini,text).unwrap();
    let writes=SectionWrites::new(&ini,(1024,2048));writes.set_safe_models(&models);
    let mut state=state(writes,"http://127.0.0.1:9");
    Arc::get_mut(&mut state).unwrap().sections=Some(Arc::new(noevia_model_manager::sections::Sections::new(Arc::new(noevia_model_manager::sections::Files::new(&models,&ini).unwrap()))));
    let fence=model_publication::Fence{incarnation:"front".into(),lease:"grant".into(),generation:1,operation:"first".into(),kind:kind.into(),expected_revision:Some(format!("{:x}",Sha256::digest(text.as_bytes())))};
    let guard=model_publication::Guard::at(&dir.path().join(".models.ini.noevia-publication.sqlite")).unwrap();guard.authority().unwrap();
    guard.save_authority(&model_publication::AuthorityRecord{incarnation:"front".into(),lease:"grant".into(),generation:1,active:true,until_ms:model_publication::now_ms()+60_000,recovery:false}).unwrap();guard.begin(&fence).unwrap();guard.commit().unwrap();
    (dir,state,fence)
}

#[cfg(feature="native-safe-defaults")]
#[tokio::test]
async fn publication_refusals_finish_their_receipt_and_stale_is_typed() {
    for (name,status) in [("target",409),("bad%20name",400)] {
        let (dir,state,fence)=scoped_publication_rig("[target]\nmodel = /models/target.gguf\n","register");
        let response=noevia_model_manager::router(state.clone()).oneshot(Request::builder().method("POST").uri(format!("/api/v1/sections/{name}/safe-defaults")).header("x-model-loader-token",TOKEN).header("x-noevia-maintenance-fence",serde_json::to_string(&fence).unwrap()).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status().as_u16(),status);
        let guard=model_publication::Guard::at(&dir.path().join(".models.ini.noevia-publication.sqlite")).unwrap();
        let receipt=guard.status(&fence).unwrap().unwrap();assert_eq!(receipt.state,"settled","known refusal is terminal at the destination");assert_eq!(receipt.revision,None);guard.commit().unwrap();
        let response=noevia_model_manager::router(state).oneshot(Request::builder().method("POST").uri("/api/v1/sections/target/safe-defaults").header("x-model-loader-token",TOKEN).header("x-noevia-maintenance-fence",serde_json::to_string(&fence).unwrap()).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status().as_u16(),409);
        let body:Value=serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["code"],"maintenance_operation_stale");
    }
}

#[cfg(feature="native-safe-defaults")]
#[tokio::test]
async fn publication_predicate_checks_fresh_file_presence_with_delete_and_roles() {
    let before="[target]\nmodel = /models/target.gguf\n";
    let (dir,state,fence)=scoped_publication_rig(before,"delete");
    let file=dir.path().join("models/target.gguf");std::fs::write(&file,b"new file after the caller's discovery").unwrap();
    let roles=dir.path().join("models.roles.json");std::fs::write(&roles,"{\"code\":\"target\"}").unwrap();
    let response=noevia_model_manager::router(state.clone()).oneshot(Request::builder().method("DELETE").uri(format!("/api/v1/sections/target?baseRevision={}",fence.expected_revision.as_ref().unwrap())).header("x-model-loader-token",TOKEN).header("x-noevia-maintenance-fence",serde_json::to_string(&fence).unwrap()).header("x-noevia-publication-condition",r#"{"fileAbsent":"target"}"#).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status().as_u16(),409,"restored file prevents cleanup at publication");
    assert_eq!(std::fs::read_to_string(dir.path().join("models.ini")).unwrap(),before);assert_eq!(std::fs::read_to_string(&roles).unwrap(),"{\"code\":\"target\"}");
    std::fs::remove_file(file).unwrap();
    let next=model_publication::Fence{operation:"second".into(),..fence};
    let guard=model_publication::Guard::at(&dir.path().join(".models.ini.noevia-publication.sqlite")).unwrap();guard.begin(&next).unwrap();guard.commit().unwrap();
    let response=noevia_model_manager::router(state).oneshot(Request::builder().method("DELETE").uri(format!("/api/v1/sections/target/missing?baseRevision={}",next.expected_revision.as_ref().unwrap())).header("x-model-loader-token",TOKEN).header("x-noevia-maintenance-fence",serde_json::to_string(&next).unwrap()).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status().as_u16(),200);
    assert!(!std::fs::read_to_string(dir.path().join("models.ini")).unwrap().contains("[target]"));
}

#[cfg(feature="native-safe-defaults")]
#[tokio::test]
async fn publication_file_identity_rejects_a_swapped_symlink_before_planning() {
    use std::os::unix::fs::MetadataExt;
    let before="[other]\nmodel = /models/other.gguf\n";
    let (dir,state,fence)=scoped_publication_rig(before,"register");
    let target=dir.path().join("models/target.gguf");std::fs::write(&target,b"first").unwrap();let meta=std::fs::metadata(&target).unwrap();
    let condition=serde_json::json!({"fileIdentity":{"relative":"target.gguf","device":meta.dev(),"inode":meta.ino(),"size":meta.len(),"mtimeNs":(i128::from(meta.mtime())*1_000_000_000+i128::from(meta.mtime_nsec())).to_string()}});
    std::fs::write(dir.path().join("models/replacement.gguf"),b"replacement").unwrap();std::fs::remove_file(&target).unwrap();std::os::unix::fs::symlink("replacement.gguf",&target).unwrap();
    let response=noevia_model_manager::router(state).oneshot(Request::builder().method("POST").uri("/api/v1/sections/target/safe-defaults").header("x-model-loader-token",TOKEN).header("x-noevia-maintenance-fence",serde_json::to_string(&fence).unwrap()).header("x-noevia-publication-condition",condition.to_string()).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status().as_u16(),409,"the loader must plan only the validated file");
    assert_eq!(std::fs::read_to_string(dir.path().join("models.ini")).unwrap(),before);
}

#[cfg(feature="native-safe-defaults")]
#[tokio::test]
async fn publication_receipt_includes_the_idle_key_reconciliation() {
    let before="[target]\nmodel = /models/target.gguf\n";
    let (dir,mut state,fence)=scoped_publication_rig(before,"preset-write");
    let ini=dir.path().join("models.ini");
    Arc::get_mut(&mut state).unwrap().idle_unload=Some(Arc::new(noevia_model_manager::idle_unload::Reconciler::new(ini.clone(),Some(300))));
    let response=noevia_model_manager::router(state).oneshot(Request::builder().method("PUT").uri("/api/v1/models-ini").header("x-model-loader-token",TOKEN).header("content-type","application/json").body(Body::from(serde_json::json!({"baseRevision":fence.expected_revision,"text":format!("{before}ctx-size = 8192\n"),"maintenance":fence}).to_string())).unwrap()).await.unwrap();
    assert_eq!(response.status().as_u16(),200);
    let body:Value=serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let text=std::fs::read_to_string(&ini).unwrap();assert!(text.contains("sleep-idle-seconds = 300"));
    let revision=format!("{:x}",Sha256::digest(text.as_bytes()));assert_eq!(body["revision"],revision);
    let guard=model_publication::Guard::at(&dir.path().join(".models.ini.noevia-publication.sqlite")).unwrap();
    assert_eq!(guard.status(&fence).unwrap().unwrap().revision.as_deref(),Some(revision.as_str()),"receipt is the final committed revision, including reconciliation");
}
