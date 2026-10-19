#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use super::*;
use serde_json::Value as V;
fn fixture() -> V {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/download-plan-python.v1.json"
    ))
    .unwrap()
}
fn files(c: &V) -> Vec<HfFile> {
    c["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| HfFile {
            path: f["path"].as_str().unwrap().into(),
            size: model_files::BigInt::from_ascii_decimal(&f["size"].to_string()).unwrap(),
            quant: None,
            shard_base: f["shard_base"].as_str().unwrap().into(),
            shard_index: f["shard_index"].as_u64().map(|n| n as u32),
            shard_total: f["shard_total"].as_u64().map(|n| n as u32),
        })
        .collect()
}
fn jobs(plan: &Plan) -> V {
    serde_json::Value::Array(plan.jobs.iter().map(|p|serde_json::json!({"repo_id":p.repo_id,"filename":p.filename,"url":p.url,"total_bytes":p.total_bytes})).collect())
}
fn temporary() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("qa-target")).unwrap();
    dir
}
#[test]
fn actual_python_repository_plans_match_responses_jobs_and_all_destinations() {
    let fx = fixture();
    let dir = temporary();
    for c in fx["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["section"].is_null() && c["body"]["repo"].is_string())
    {
        let body = crate::prompts::parse_json(&c["body"].to_string()).unwrap();
        let target = text(&body, "target").unwrap();
        let prefix = if target.is_empty() {
            String::new()
        } else {
            format!("{target}/")
        };
        let plan = validate(
            dir.path().to_str().unwrap(),
            repository_plan(
                &text(&body, "repo").unwrap(),
                &text(&body, "path").unwrap(),
                &text(&body, "shardBase").unwrap(),
                &prefix,
                &files(c),
            )
            .unwrap(),
            true,
        )
        .unwrap();
        assert_eq!(
            plan.status,
            c["status"].as_u64().unwrap() as u16,
            "{}",
            c["id"]
        );
        assert_eq!(
            serde_json::from_str::<V>(&plan.body).unwrap(),
            c["response"],
            "{}",
            c["id"]
        );
        assert_eq!(jobs(&plan), c["jobs"], "{}", c["id"]);
    }
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "planning created files"
    );
}
#[tokio::test]
async fn actual_python_draft_head_source_and_bounds() {
    let fx = fixture();
    for c in fx["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["section"].is_string())
    {
        let dir = temporary();
        std::fs::create_dir(dir.path().join("qa-folder")).unwrap();
        std::fs::write(dir.path().join("qa-folder/qa-main.gguf"), b"qa").unwrap();
        std::fs::write(dir.path().join("qa-folder/unknown.gguf"), b"qa").unwrap();
        let ini = dir.path().join("models.ini");
        std::fs::write(&ini,"[qa]\nmodel = /models/qa-folder/qa-main.gguf\n[unknown]\nmodel = /models/qa-folder/unknown.gguf\n").unwrap();
        let db = rusqlite::Connection::open(dir.path().join("model_loader.db")).unwrap();
        db.execute_batch("CREATE TABLE kv(key TEXT PRIMARY KEY,value TEXT); CREATE TABLE download_history(id INTEGER PRIMARY KEY,repo_id TEXT,filename TEXT,status TEXT);INSERT INTO download_history VALUES(1,'qa-owner/qa-repo','qa-main.gguf','done')").unwrap();
        let listing = c["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| serde_json::json!({"type":"file","path":f["path"],"size":f["size"]}))
            .collect::<Vec<_>>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}/api", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/api/models/qa-owner/qa-repo/tree/main",
                axum::routing::get(move || {
                    let listing = listing.clone();
                    async move { axum::Json(listing) }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let mut planner =
            Planner::new(dir.path().to_str().unwrap(), &ini, dir.path(), "qa-target").unwrap();
        planner.remote = DraftHeads::planning_with_hub(dir.path(), &ini, dir.path(), &api).unwrap();
        let body = crate::prompts::parse_json(&c["body"].to_string()).unwrap();
        let plan = planner
            .plan(Some(c["section"].as_str().unwrap().into()), body)
            .await
            .unwrap();
        assert_eq!(
            plan.status,
            c["status"].as_u64().unwrap() as u16,
            "{}",
            c["id"]
        );
        assert_eq!(
            serde_json::from_str::<V>(&plan.body).unwrap(),
            c["response"],
            "{}",
            c["id"]
        );
        assert_eq!(jobs(&plan), c["jobs"], "{}", c["id"]);
        server.abort();
    }
}
#[test]
fn a_late_bad_destination_and_oversized_plan_never_partially_validate() {
    let dir = temporary();
    let good = Planned {
        repo_id: "qa-owner/repo".into(),
        filename: "qa-folder/qa.gguf".into(),
        url: "https://example.test/file".into(),
        total_bytes: 1,
    };
    let mut bad = good.clone();
    bad.filename = "../outside".into();
    let plan = validate(
        dir.path().to_str().unwrap(),
        Plan {
            status: 200,
            body: "{}".into(),
            jobs: vec![good.clone(), bad],
        },
        true,
    )
    .unwrap();
    assert_eq!(plan.status, 400);
    assert!(plan.jobs.is_empty());
    assert!(!dir.path().join("qa-folder").exists());
    assert!(validate(
        dir.path().to_str().unwrap(),
        Plan {
            status: 200,
            body: "{}".into(),
            jobs: vec![good; crate::downloads::PLAN_FILES_CAP + 1]
        },
        true,
    )
    .is_err());
}

#[tokio::test]
async fn real_http_heads_are_anonymous_bounded_and_keep_python_best_effort_sizes() {
    use axum::{
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::head,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let app = axum::Router::new().route(
        "/{kind}",
        head(
            move |axum::extract::Path(kind): axum::extract::Path<String>, headers: HeaderMap| {
                let seen = seen.clone();
                async move {
                    assert!(headers.get("authorization").is_none());
                    seen.fetch_add(1, Ordering::SeqCst);
                    match kind.as_str() {
                        "size" => (StatusCode::OK, [("content-length", "27")]).into_response(),
                        "error" => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                        "redirect" => (StatusCode::FOUND, [("location", "/size")]).into_response(),
                        "loop" => (StatusCode::FOUND, [("location", "/loop")]).into_response(),
                        "slow" => {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            StatusCode::OK.into_response()
                        }
                        _ => StatusCode::OK.into_response(),
                    }
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = temporary();
    let db = rusqlite::Connection::open(dir.path().join("model_loader.db")).unwrap();
    db.execute_batch("CREATE TABLE kv(key TEXT PRIMARY KEY,value TEXT); INSERT INTO kv VALUES('hf_token','qa-stored-token')").unwrap();
    let planner = Planner::new(
        dir.path().to_str().unwrap(),
        &dir.path().join("models.ini"),
        dir.path(),
        "qa-target",
    )
    .unwrap();
    for (path, expected) in [
        ("size", 27),
        ("error", 0),
        ("redirect", 27),
        ("loop", 0),
        ("missing", 0),
    ] {
        assert_eq!(
            planner
                .head_size(reqwest::Url::parse(&format!("{base}/{path}")).unwrap())
                .await
                .unwrap(),
            expected
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 26);
    let before = hits.load(Ordering::SeqCst);
    let bad=crate::prompts::parse_json(&serde_json::json!({"target":"unoffered","url":format!("{base}/size"),"filename":"qa.gguf"}).to_string()).unwrap();
    assert_eq!(planner.plan(None, bad).await.unwrap().status, 400);
    assert_eq!(hits.load(Ordering::SeqCst), before);
    // Cancelling a blocked plan releases its admission permit and never creates directories.
    let body = crate::prompts::parse_json(
        &serde_json::json!({"url":format!("{base}/slow"),"filename":"qa.gguf"}).to_string(),
    )
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(40), planner.plan(None, body))
            .await
            .is_err()
    );
    assert_eq!(planner.permits.available_permits(), 2);
    assert!(!dir.path().join("qa").exists());
    server.abort();
}
#[tokio::test]
async fn url_cases_match_actual_python_validation_before_transport() {
    let fx = fixture();
    let dir = temporary();
    let db = rusqlite::Connection::open(dir.path().join("model_loader.db")).unwrap();
    db.execute_batch("CREATE TABLE kv(key TEXT PRIMARY KEY,value TEXT); INSERT INTO kv VALUES('hf_token','qa-stored-token')").unwrap();
    let planner = Planner::new(
        dir.path().to_str().unwrap(),
        &dir.path().join("models.ini"),
        dir.path(),
        "qa-target",
    )
    .unwrap();
    for c in fx["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["id"].as_str().unwrap().starts_with("url") && c["status"] == 400)
    {
        let body = crate::prompts::parse_json(&c["body"].to_string()).unwrap();
        let url = model_autoconfig::pyval::py_strip(&text(&body, "url").unwrap()).to_owned();
        let plan = planner.url(&body, &url, "").await.unwrap();
        assert_eq!(plan.status, 400, "{}", c["id"]);
        assert_eq!(
            serde_json::from_str::<V>(&plan.body).unwrap(),
            c["response"]
        );
        assert!(plan.jobs.is_empty());
    }
    assert!(validate(
        dir.path().to_str().unwrap(),
        Plan {
            status: 200,
            body: "x".repeat(crate::downloads::PLAN_CAP),
            jobs: vec![]
        },
        true
    )
    .is_err());
}

#[tokio::test]
async fn native_start_validates_auth_then_downloads_synthetic_bytes_without_a_child() {
    use axum::{body::Body, http::Request, routing::get};
    use http_body_util::BodyExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/qa.gguf;parameter",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/qa.gguf;parameter",
                get(move || {
                    let seen = seen.clone();
                    async move {
                        seen.fetch_add(1, Ordering::SeqCst);
                        "qa!"
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let dir = temporary();
    let root = dir.path().to_str().unwrap();
    let schema: V = serde_json::from_str(include_str!(
        "../../tests/fixtures/downloads-python.v1.json"
    ))
    .unwrap();
    let db = rusqlite::Connection::open(dir.path().join("model_loader.db")).unwrap();
    for sql in schema["schema"].as_array().unwrap() {
        db.execute_batch(sql.as_str().unwrap()).unwrap();
    }
    let table = crate::downloads::Downloads::new(crate::downloads::Config::production(
        root.into(),
        root,
        1,
    ))
    .unwrap();
    table
        .set_planner(
            Planner::new(
                root,
                &dir.path().join("models.ini"),
                dir.path(),
                "qa-target",
            )
            .unwrap(),
        )
        .unwrap();
    let mut state = crate::test_support::test_state("http://127.0.0.1:9");
    state.downloads = Some(table.clone());
    state.token = "qa-auth".into();
    let app = crate::router(Arc::new(state));
    let body = serde_json::json!({"url":url}).to_string();
    let request = |token: &str, body: &str| {
        Request::builder()
            .method("POST")
            .uri("/api/v1/downloads")
            .header("content-type", "application/json")
            .header("x-model-loader-token", token)
            .body(Body::from(body.to_owned()))
            .unwrap()
    };
    assert_eq!(
        app.clone()
            .oneshot(request("wrong", &body))
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    for invalid in [
        serde_json::json!({"url":"http://[broken]/x.gguf"}),
        serde_json::json!({"url":"http://[broken]/x.gguf","filename":"qa.gguf"}),
    ] {
        let response = app
            .clone()
            .oneshot(request("qa-auth", &invalid.to_string()))
            .await
            .unwrap();
        assert_eq!(response.status(), 500);
        assert_eq!(table.list(), "{\"jobs\":[]}");
        assert!(!dir.path().join("qa").exists());
        assert!(!dir.path().join("x").exists());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }
    let response = app.oneshot(request("qa-auth", &body)).await.unwrap();
    assert_eq!(response.status(), 200);
    let response = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        serde_json::from_slice::<V>(&response).unwrap(),
        serde_json::json!({"queued":["qa.gguf"]})
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if std::fs::read(dir.path().join("qa/qa.gguf")).is_ok_and(|b| b == b"qa!") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    server.abort();
}

#[test]
fn actual_python_url_success_responses_and_job_names_match() {
    let fx = fixture();
    for c in fx["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["body"]["url"].is_string() && c["status"] == 200)
    {
        let body = crate::prompts::parse_json(&c["body"].to_string()).unwrap();
        let url = model_autoconfig::pyval::py_strip(&text(&body, "url").unwrap()).to_owned();
        let total = c["jobs"][0]["total_bytes"].as_i64().unwrap();
        let mut plan = url_plan(&body, &url, "", total).unwrap();
        finish_url_plan(&url, &mut plan).unwrap();
        assert_eq!(
            serde_json::from_str::<V>(&plan.body).unwrap(),
            c["response"],
            "{}",
            c["id"]
        );
        assert_eq!(jobs(&plan), c["jobs"], "{}", c["id"]);
    }
}

#[tokio::test]
async fn authenticated_hub_redirects_refuse_without_contacting_a_second_origin() {
    use axum::{
        http::{HeaderMap, StatusCode},
        routing::get,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = temporary();
    let db = rusqlite::Connection::open(dir.path().join("model_loader.db")).unwrap();
    db.execute_batch("CREATE TABLE kv(key TEXT PRIMARY KEY,value TEXT); INSERT INTO kv VALUES('hf_token','qa-stored-token')").unwrap();
    let second = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination = format!("http://{}/capture", second.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let second_server = tokio::spawn(async move {
        axum::serve(
            second,
            axum::Router::new().fallback(get(move || {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    "[]"
                }
            })),
        )
        .await
        .unwrap();
    });
    let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}/api", first.local_addr().unwrap());
    let first_server = tokio::spawn(async move {
        axum::serve(
            first,
            axum::Router::new().fallback(get(move |headers: HeaderMap| {
                let destination = destination.clone();
                async move {
                    assert_eq!(
                        headers.get("authorization").unwrap(),
                        "Bearer qa-stored-token"
                    );
                    (StatusCode::FOUND, [("location", destination)])
                }
            })),
        )
        .await
        .unwrap();
    });
    let remote =
        DraftHeads::planning_with_hub(dir.path(), &dir.path().join("models.ini"), dir.path(), &api)
            .unwrap();
    assert!(remote.planning_files("qa-owner/qa-repo").await.is_err());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    first_server.abort();
    second_server.abort();
}

#[tokio::test]
async fn actual_python_urlparse_failures_and_head_order_never_queue_or_create_files() {
    let fx = fixture();
    let dir = temporary();
    for c in fx["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["body"]["url"].is_string())
    {
        let body = crate::prompts::parse_json(&c["body"].to_string()).unwrap();
        let url = model_autoconfig::pyval::py_strip(&text(&body, "url").unwrap()).to_owned();
        let mut calls = Vec::new();
        let plan = url_plan_with_head(&body, &url, "", || {
            calls.push(serde_json::json!(["head", url]));
            let total = if c["head_status"].as_u64().unwrap() >= 400 {
                Ok(0)
            } else {
                c["head_length"]
                    .as_str()
                    .unwrap_or("0")
                    .parse::<i64>()
                    .map_err(|_| Error::Failed)
            };
            std::future::ready(total)
        })
        .await;
        assert_eq!(serde_json::json!(calls), c["calls"], "{}", c["id"]);
        if c["status"] == 500 {
            assert!(matches!(plan, Err(Error::Failed)), "{}", c["id"]);
        } else {
            let plan = validate(dir.path().to_str().unwrap(), plan.unwrap(), false).unwrap();
            assert_eq!(
                plan.status,
                c["status"].as_u64().unwrap() as u16,
                "{}",
                c["id"]
            );
            assert_eq!(
                serde_json::from_str::<V>(&plan.body).unwrap(),
                c["response"],
                "{}",
                c["id"]
            );
            assert_eq!(jobs(&plan), c["jobs"], "{}", c["id"]);
        }
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "{} created files",
            c["id"]
        );
    }
}
