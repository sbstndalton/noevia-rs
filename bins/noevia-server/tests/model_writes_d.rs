//! Replay the committed Node 22 oracle against real native HTTP handlers and synthetic peers.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use support::*;
const IDLE: &str = "/api/models/idle-unload";
const PRUNE: &str = "/api/models/prune-missing";
const CSRF: &str = "X-CSRF-Token: csrf\r\n";
const LEASE: &str = "/internal/model-maintenance/lease";
fn at(stage: &str, spec: &Value, mut response: (Duration, String)) -> (Duration, String) {
    if spec["delayStage"].as_str() == Some(stage) {
        response.0 = Duration::from_millis(250);
    }
    response
}
struct Rig {
    _dir: tempfile::TempDir,
    f: Front,
    owner: Upstream,
    loader: Upstream,
    engine: Upstream,
}
async fn rig(spec: Value, prune: bool, delay: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let owner = upstream().await;
    let loader = upstream().await;
    let engine = upstream().await;
    let chosen = Arc::new(Mutex::new(1800_i64));
    let state = Arc::clone(&chosen);
    let o_spec = spec.clone();
    let effects_count = AtomicUsize::new(0);
    owner.set(move|r| {
        if r.path==LEASE && r.method=="POST" {
            if o_spec["busy"]==true {return reply("409 Conflict","{}");}
            return at("acquire",&o_spec,reply("201 Created",r#"{"lease":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","ttlMs":600000,"renewMs":200000,"hardMs":3600000}"#));
        }
        if r.path.starts_with(LEASE){return reply("200 OK","{}");}
        if r.path=="/internal/model-writes/effects"{
 let count=effects_count.fetch_add(1,Ordering::SeqCst)+1;
 if o_spec["failEffectsAt"].as_u64()==Some(count as u64){return reply("503 Unavailable","{}");}
 let stage=if j(&r.body)["op"]=="removed"{"removed"}else{"invalidate"};return at(stage,&o_spec,reply("200 OK",r#"{"ok":true}"#));}
        if r.path=="/internal/idle-unload-read/facts"{
            return at("effective",&o_spec,reply("200 OK",&json!({"seconds":if o_spec["mismatch"]==true{300}else{*state.lock().unwrap()}}).to_string()));
        }
        if r.path=="/internal/models/facts" {
            let scan=o_spec["installed"].as_array().unwrap().iter().filter(|m|m["missingFile"]!=true).map(|m|json!({"modelId":m["name"],"sections":[m["name"]]})).collect::<Vec<_>>();
            return at("facts",&o_spec,reply("200 OK",&json!({"sidecarNames":o_spec["installed"].as_array().unwrap().iter().filter(|m|m["servedElsewhere"]==true).map(|m|m["name"].clone()).collect::<Vec<_>>(),"sidecarLoaded":[],"scan":scan}).to_string()));
        }
        reply("599 Proxy","{\"error\":\"unexpected Node executor\"}")
    });
    let l_spec = spec.clone();
    let state = Arc::clone(&chosen);
    loader.set(move|r|{
        if r.path=="/api/v1/idle-unload"{
            let status=l_spec["status"].as_u64().unwrap_or(200);
            if status!=200{return reply(&format!("{status} Synthetic"),"{}");}
            if l_spec["unexpected"]==true{return reply("200 OK","{}");}
            if r.method=="PUT"{*state.lock().unwrap()=j(&r.body)["seconds"].as_i64().unwrap_or(1800);}
            let source=if r.method=="PUT" && !j(&r.body)["seconds"].is_null(){"setting"}else{"default"};
            let body=json!({"seconds":*state.lock().unwrap(),"source":source,"fallbackSeconds":1800,"fallbackSource":"default","preset":l_spec["preset"].as_str().unwrap_or("written")});
            let mut result=reply("200 OK",&body.to_string());if delay&&r.method=="PUT"{result.0=Duration::from_millis(250);}return at(if r.method=="PUT"{"put"}else{"previous"},&l_spec,result);
        }
        if r.path=="/api/v1/models" && l_spec["scanFailure"]==true {return reply("500 Unavailable","{}");}
        if r.path=="/api/v1/models"{
            let scan=l_spec.get("scan").cloned().unwrap_or_else(||json!([{"modelId":"ok","sections":["ok"]}]));
            return at("scan",&l_spec,reply("200 OK",&json!({"models":scan}).to_string()));
        }
        if r.path=="/api/v1/sections" { return at("sections",&l_spec,reply("200 OK",&json!({"revision":"synthetic-revision","sections":l_spec["sections"]}).to_string())); }
        if r.path.starts_with("/api/v1/sections/")&&r.method=="DELETE" {
            assert!(r.path.ends_with("?baseRevision=synthetic-revision"));
            let mut result=reply("200 OK","{}");if delay{result.0=Duration::from_millis(250);}return at("delete",&l_spec,result);
        }
        reply("404 Not Found","{}")
    });
    let e_spec = spec.clone();
    engine.set(move|r|{
        if r.path.starts_with("/gate/"){return reply("404 Not Found","{}");}
        if r.path.contains("reload=1") && e_spec["pending"]==true {return reply("503 Unavailable","{}");}
        if r.path.starts_with("/models") {
            let rows=if prune {e_spec["installed"].as_array().unwrap().iter().map(|m|json!({"id":m["name"],"source":m.get("source").and_then(Value::as_str).unwrap_or("preset"),"status":{"value":"unloaded"}})).collect::<Vec<_>>()}
            else if e_spec["pending"]==true{vec![json!({"id":"active","status":{"value":"loaded"}})]}else{vec![]};
            return at(if r.path.contains("reload=1"){"reload"}else{"listing"},&e_spec,reply("200 OK",&json!({"data":rows}).to_string()));
        }
        reply("404 Not Found","{}")
    });
    let loader_base = if spec["status"] == 0 {
        "http://127.0.0.1:9".to_owned()
    } else {
        loader.base()
    };
    let f = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", owner.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
            ("MODEL_LOADER_URL", loader_base),
            (
                "MODEL_MANAGER_KIND",
                if spec["noEngine"] == true {
                    "none".into()
                } else {
                    "llamacpp".into()
                },
            ),
            (
                "MODEL_MANAGER_BASE_URL",
                if spec["unreachable"] == true {
                    "http://127.0.0.1:9".into()
                } else {
                    engine.base()
                },
            ),
        ],
    )
    .await;
    Rig {
        _dir: dir,
        f,
        owner,
        loader,
        engine,
    }
}
fn same(actual: &Value, expected: &Value) {
    assert_eq!(
        js_json::wtf16::parse(&actual.to_string()).unwrap(),
        js_json::wtf16::parse(&expected.to_string()).unwrap()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_oracle_idle_replay() {
    let corpus = j(include_str!("fixtures/model-writes.node.json"));
    for case in corpus["idle"].as_array().unwrap() {
        let r = rig(case["spec"].clone(), false, false).await;
        let input = case["spec"]["rawBody"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| case["spec"]["body"].to_string());
        let (status, body) = raw(r.f.addr, "PUT", IDLE, ADMIN, CSRF, input.as_bytes()).await;
        assert_eq!(
            status,
            case["status"].as_u64().unwrap() as u16,
            "{}: {body}",
            case["spec"]["name"]
        );
        same(&body, &case["body"]);
        assert!(r
            .owner
            .seen()
            .iter()
            .all(|s| s.path.starts_with("/internal/")));
        assert!(r
            .loader
            .seen()
            .iter()
            .all(|s| s.path == "/api/v1/idle-unload"));
        if case["spec"]["status"] != 0 {
            assert_eq!(
                json!(r
                    .loader
                    .seen()
                    .into_iter()
                    .map(|c| c.method)
                    .collect::<Vec<_>>()),
                case["calls"]
            );
        }
        let expected_reload = usize::from(case["body"]["apply"]["state"] == "applied");
        assert_eq!(
            r.engine.count("/models?reload=1"),
            expected_reload,
            "{}",
            case["spec"]["name"]
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_oracle_prune_replay() {
    let corpus = j(include_str!("fixtures/model-writes.node.json"));
    for case in corpus["prune"].as_array().unwrap() {
        let r = rig(case["spec"].clone(), true, false).await;
        let (status, body) = raw(r.f.addr, "POST", PRUNE, ADMIN, CSRF, b"{}").await;
        assert_eq!(
            status,
            case["status"].as_u64().unwrap() as u16,
            "{}: {body}",
            case["spec"]["name"]
        );
        same(&body, &case["body"]);
        let deleted = r
            .loader
            .seen()
            .into_iter()
            .filter(|s| s.method == "DELETE")
            .count();
        assert_eq!(
            deleted,
            case["body"]["removed"].as_array().map_or(0, Vec::len)
        );
        assert_eq!(
            r.engine.count("/models?reload=1"),
            usize::from(deleted > 0),
            "{}",
            case["spec"]["name"]
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gates_and_malformed_input_precede_engine_work() {
    let r = rig(json!({"body":{"seconds":900}}), false, false).await;
    for (cookie, csrf, want) in [("", CSRF, 401), (MEMBER, CSRF, 403), (ADMIN, "", 403)] {
        for (method, path) in [("PUT", IDLE), ("POST", PRUNE)] {
            assert_eq!(
                raw(r.f.addr, method, path, cookie, csrf, b"{}").await.0,
                want
            );
        }
    }
    assert_eq!(raw(r.f.addr, "PUT", IDLE, ADMIN, CSRF, b"{").await.0, 400);
    assert!(r.owner.seen().is_empty());
    assert!(r.loader.seen().is_empty());
    assert!(r.engine.seen().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_hands_job_over_and_concurrent_write_refuses_until_release() {
    let r = rig(json!({"body":{"seconds":900}}), false, true).await;
    let addr = r.f.addr;
    let request =
        tokio::spawn(
            async move { raw(addr, "PUT", IDLE, ADMIN, CSRF, br#"{"seconds":900}"#).await },
        );
    r.loader.wait_for("/api/v1/idle-unload", 2).await;
    request.abort();
    let (status, body) = raw(addr, "PUT", IDLE, ADMIN, CSRF, br#"{"seconds":300}"#).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(
        r.loader.seen().iter().filter(|c| c.method == "PUT").count(),
        1
    );
    r.owner.wait_for(&format!("{LEASE}/"), 1).await;
    let (status, body) = raw(addr, "PUT", IDLE, ADMIN, CSRF, br#"{"seconds":300}"#).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        r.loader.seen().iter().filter(|s| s.method == "PUT").count(),
        2
    );
}
#[test]
fn exact_method_dispatch() {
    use noevia_server::routes::{dispatch, Native};
    assert_eq!(dispatch("PUT", IDLE), Some(Native::Models));
    assert_eq!(dispatch("POST", PRUNE), Some(Native::Models));
    for (m, p) in [
        ("POST", IDLE),
        ("GET", PRUNE),
        ("PUT", "/api/models/idle-unload/"),
        ("POST", "/api/models/prune-missing/"),
    ] {
        assert_eq!(dispatch(m, p), None);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_at_external_awaits_finishes_and_releases_the_same_job() {
    for (stage, peer, path, count) in [
        ("invalidate", "owner", "/internal/model-writes/effects", 1),
        ("previous", "loader", "/api/v1/idle-unload", 1),
        ("acquire", "owner", LEASE, 1),
        ("put", "loader", "/api/v1/idle-unload", 2),
        ("listing", "engine", "/models", 1),
        ("reload", "engine", "/models?reload=1", 1),
        ("effective", "owner", "/internal/idle-unload-read/facts", 1),
    ] {
        let r = rig(json!({"delayStage":stage}), false, false).await;
        let addr = r.f.addr;
        let task = tokio::spawn(async move {
            raw(addr, "PUT", IDLE, ADMIN, CSRF, br#"{"seconds":900}"#).await
        });
        match peer {
            "owner" => r.owner.wait_for(path, count).await,
            "loader" => r.loader.wait_for(path, count).await,
            _ => r.engine.wait_for(path, count).await,
        };
        task.abort();
        r.owner.wait_for(&format!("{LEASE}/"), 1).await;
        assert_eq!(
            r.loader.seen().iter().filter(|c| c.method == "PUT").count(),
            1,
            "{stage}"
        );
    }
    for (stage, peer, path) in [
        ("scan", "loader", "/api/v1/models"),
        ("facts", "owner", "/internal/models/facts"),
        ("acquire", "owner", LEASE),
        ("sections", "loader", "/api/v1/sections"),
        ("delete", "loader", "/api/v1/sections/gone?"),
        ("reload", "engine", "/models?reload=1"),
        ("removed", "owner", "/internal/model-writes/effects"),
    ] {
        let r=rig(json!({"delayStage":stage,"installed":[{"name":"gone","missingFile":true},{"name":"ok"}],"sections":[{"name":"gone","hasFile":false},{"name":"ok","hasFile":true}]}),true,false).await;
        let addr = r.f.addr;
        let task = tokio::spawn(async move { raw(addr, "POST", PRUNE, ADMIN, CSRF, b"{").await });
        let count = if stage == "removed" { 2 } else { 1 };
        match peer {
            "owner" => r.owner.wait_for(path, count).await,
            "loader" => r.loader.wait_for(path, count).await,
            _ => r.engine.wait_for(path, count).await,
        };
        task.abort();
        r.owner.wait_for(&format!("{LEASE}/"), 1).await;
        assert_eq!(
            r.loader
                .seen()
                .iter()
                .filter(|c| c.method == "DELETE")
                .count(),
            1,
            "{stage}"
        );
        let notification = r
            .owner
            .seen()
            .into_iter()
            .find(|c| c.path == "/internal/model-writes/effects" && j(&c.body)["op"] == "removed")
            .unwrap();
        assert_eq!(j(&notification.body)["names"], json!(["gone"]));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loader_setting_without_a_router_saves_without_a_hold_and_stays_pending() {
    let r = rig(json!({"noEngine":true}), false, false).await;
    let (status, body) = raw(r.f.addr, "PUT", IDLE, ADMIN, CSRF, br#"{"seconds":900}"#).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["apply"], json!({"state":"pending"}));
    assert_eq!(body["applied"], false);
    assert_eq!(r.owner.count(LEASE), 0);
    assert!(r.engine.seen().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_shared_gate_and_malformed_body_oracle_replay() {
    let corpus = j(include_str!("fixtures/model-writes.node.json"));
    let r = rig(
        json!({"installed":[{"name":"ok"}],"sections":[{"name":"ok","hasFile":true}]}),
        true,
        false,
    )
    .await;
    for case in corpus["gates"].as_array().unwrap() {
        let spec = &case["spec"];
        let cookie = match spec["role"].as_str().unwrap() {
            "none" => "",
            "member" => MEMBER,
            _ => ADMIN,
        };
        let mut extra = if spec["csrf"] == true {
            CSRF.to_owned()
        } else {
            String::new()
        };
        if let Some(origin) = spec["origin"].as_str() {
            extra.push_str(&format!("Origin: {origin}\r\n"));
        }
        let (status, body) = raw(
            r.f.addr,
            case["method"].as_str().unwrap(),
            case["path"].as_str().unwrap(),
            cookie,
            &extra,
            b"{}",
        )
        .await;
        assert_eq!(
            status,
            case["status"].as_u64().unwrap() as u16,
            "{case}: {body}"
        );
        same(&body, &case["body"]);
    }
    assert!(r.loader.seen().is_empty());
    for case in corpus["malformed"].as_array().unwrap() {
        let (status, body) = raw(
            r.f.addr,
            case["method"].as_str().unwrap(),
            case["path"].as_str().unwrap(),
            ADMIN,
            CSRF,
            case["rawText"].as_str().unwrap().as_bytes(),
        )
        .await;
        assert_eq!(
            status,
            case["status"].as_u64().unwrap() as u16,
            "{case}: {body}"
        );
        same(&body, &case["body"]);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_prunes_never_execute_deletes_together() {
    let spec = json!({"installed":[{"name":"gone","missingFile":true},{"name":"ok"}],"sections":[{"name":"gone","hasFile":false},{"name":"ok","hasFile":true}]});
    let r = rig(spec, true, true).await;
    let addr = r.f.addr;
    let first = tokio::spawn(async move { raw(addr, "POST", PRUNE, ADMIN, CSRF, b"{}").await });
    r.loader.wait_for("/api/v1/sections/gone?", 1).await;
    let (status, body) = raw(addr, "POST", PRUNE, ADMIN, CSRF, b"{}").await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "maintenance_held");
    assert_eq!(
        r.loader
            .seen()
            .iter()
            .filter(|c| c.method == "DELETE")
            .count(),
        1
    );
    assert_eq!(first.await.unwrap().0, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_consumer_acknowledgments_fail_closed_before_or_after_commit() {
    for n in [1, 2] {
        let r = rig(json!({"failEffectsAt":n}), false, false).await;
        let (status, body) = raw(r.f.addr, "PUT", IDLE, ADMIN, CSRF, br#"{"seconds":900}"#).await;
        assert_eq!(status, 503, "{body}");
        assert_eq!(
            r.loader.seen().iter().filter(|c| c.method == "PUT").count(),
            if n == 1 { 0 } else { 1 }
        );
        if n == 2 {
            assert_eq!(r.owner.count(&format!("{LEASE}/")), 1);
        }
    }
    let r=rig(json!({"failEffectsAt":2,"installed":[{"name":"gone","missingFile":true},{"name":"ok"}],"sections":[{"name":"gone","hasFile":false},{"name":"ok","hasFile":true}]}),true,false).await;
    assert_eq!(
        raw(r.f.addr, "POST", PRUNE, ADMIN, CSRF, b"{}").await.0,
        503
    );
    assert_eq!(r.owner.count(&format!("{LEASE}/")), 1);
}
