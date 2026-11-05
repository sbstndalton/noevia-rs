//! Actual identity/dispatch against synthetic owner engines; no model transfers or runs.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#[path = "mcp_support/mod.rs"]
mod support;
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use support::*;
const PULL: &str = "/api/models/pull";
const DELETE: &str = "/api/models/delete";
fn state(r: &Seen) -> Option<(Duration, String)> {
    if r.path.starts_with("/internal/model-mutations/") {
        return Some(match r.path.as_str() {
            "/internal/model-mutations/protection" => reply(
                "200 OK",
                r#"{"sidecarNames":[],"rerankerNames":[],"protectionUnknown":false}"#,
            ),
            "/internal/model-mutations/commit" => reply("200 OK", r#"{"ok":true}"#),
            _ => reply("200 OK", r#"{"ok":true}"#),
        });
    }
    if r.path == "/internal/models/facts" {
        return Some(reply(
            "200 OK",
            r#"{"sidecarNames":[],"sidecarLoaded":[],"scan":[]}"#,
        ));
    }
    None
}
async fn front_with(up: &Upstream, enabled: bool) -> (tempfile::TempDir, Front) {
    let d = tempfile::tempdir().unwrap();
    seed(d.path());
    std::fs::write(
        d.path().join("users").join(A).join("auto-roles.json"),
        r#"{"fast":"synthetic","smart":"other"}"#,
    )
    .unwrap();
    std::fs::write(
        d.path().join("users").join(B).join("auto-roles.json"),
        r#"{"fast":"synthetic","smart":"synthetic"}"#,
    )
    .unwrap();
    let f = front_env(
        d.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", up.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
            (
                "MODEL_MANAGER_KIND",
                if enabled { "llamacpp" } else { "none" }.into(),
            ),
            ("MODEL_MANAGER_BASE_URL", up.base()),
        ],
    )
    .await;
    (d, f)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_pull_replays_the_actual_node_policy_oracle() {
    let cases: Value = serde_json::from_str(include_str!("fixtures/model-pull-node.json")).unwrap();
    for c in cases.as_array().unwrap() {
        let up = upstream().await;
        let case = c.clone();
        up.set(move |r| {
            if let Some(v) = state(r) {
                return v;
            }
            assert_eq!(r.path, "/models");
            assert_eq!(r.method, "POST");
            assert_eq!(j(&r.body), case["expected"]["request"]);
            reply(
                &format!("{} Synthetic", case["upstreamStatus"]),
                &case["upstream"].to_string(),
            )
        });
        let (_d, f) = front_with(&up, c["enabled"].as_bool().unwrap()).await;
        let (s, v) = write(f.addr, "POST", PULL, ADMIN, c["raw"].as_str().unwrap()).await;
        assert_eq!(s as u64, c["expected"]["status"], "{}: {v}", c["name"]);
        assert_eq!(v, c["expected"]["body"], "{}", c["name"]);
        if c["expected"].get("request").is_some() {
            assert_eq!(up.seen().iter().filter(|r| r.path == "/models").count(), 1);
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identity_role_and_csrf_precede_all_owner_calls() {
    let up = upstream().await;
    up.set(|_| panic!("must not reach shared owner"));
    let (_d, f) = front_with(&up, true).await;
    for path in [PULL, DELETE] {
        assert_eq!(write(f.addr, "POST", path, "", "{}").await.0, 401);
        assert_eq!(write(f.addr, "POST", path, MEMBER, "{}").await.0, 403);
        assert_eq!(
            raw_text(
                f.addr,
                "POST",
                path,
                "cowork_session=admin-token",
                "",
                b"{}"
            )
            .await
            .0,
            403
        );
    }
    assert!(up.seen().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_unloads_retries_and_commits_shared_references_only_after_success() {
    let up = upstream().await;
    let deletes = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&deletes);
    up.set(move |r| {
        if let Some(v) = state(r) {
            return v;
        }
        match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/models") => reply(
                "200 OK",
                r#"{"data":[{"id":"synthetic","status":{"value":"loaded"}}]}"#,
            ),
            ("POST", "/models/unload") => {
                assert_eq!(j(&r.body), json!({"model":"synthetic"}));
                reply("200 OK", "{}")
            }
            ("DELETE", "/models?model=synthetic") => reply(
                if count.fetch_add(1, Ordering::SeqCst) == 0 {
                    "409 Busy"
                } else {
                    "200 OK"
                },
                "{}",
            ),
            _ => panic!("unexpected {} {}", r.method, r.path),
        }
    });
    let (_d, f) = front_with(&up, true).await;
    let (s, v) = write(f.addr, "POST", DELETE, ADMIN, r#"{"name":"synthetic"}"#).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        v,
        json!({"ok":true,"unloaded":true,"rolesCleared":["fast"],"presetRemoved":false})
    );
    assert_eq!(deletes.load(Ordering::SeqCst), 2);
    assert_eq!(
        serde_json::from_str::<Value>(
            &std::fs::read_to_string(_d.path().join("users").join(A).join("auto-roles.json"))
                .unwrap()
        )
        .unwrap(),
        json!({"fast":"other","smart":"other"})
    );
    assert_eq!(
        std::fs::read_to_string(_d.path().join("users").join(B).join("auto-roles.json"))
            .unwrap()
            .trim(),
        "null"
    );
    let seen = up.seen();
    let engine: Vec<_> = seen
        .iter()
        .filter(|r| r.method != "GET" && r.path.starts_with("/models"))
        .map(|r| r.method.as_str())
        .collect();
    assert_eq!(engine, ["POST", "DELETE", "POST", "DELETE"]);
    let commit = seen
        .iter()
        .position(|r| r.path.ends_with("/commit"))
        .unwrap();
    let last_delete = seen.iter().rposition(|r| r.method == "DELETE").unwrap();
    assert!(commit > last_delete);
    for r in seen.iter().filter(|r| r.path.starts_with("/internal/")) {
        assert_eq!(
            r.header("x-noevia-store-token").as_deref(),
            Some(STORE_TOKEN)
        );
        assert!(r.header("cookie").is_none());
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_traversal_assigned_and_in_use_names_never_delete_or_clear_references() {
    for mode in ["unknown", "traversal", "assigned", "in-use"] {
        let up = upstream().await;
        let chosen = mode;
        up.set(move|r|{
   if chosen=="assigned"&&r.path=="/internal/model-mutations/protection"{return reply("200 OK",r#"{"sidecarNames":["synthetic"],"rerankerNames":["synthetic"],"protectionUnknown":false}"#);}
   if let Some(v)=state(r){return v;}
   if r.method=="GET" {return reply("200 OK",r#"{"data":[{"id":"synthetic","status":{"value":"loaded"}}]}"#);}
   assert_eq!(chosen,"in-use");reply("409 Conflict",r#"{"error":{"type":"gate_error","code":"model_in_use","message":"Busy","retryable":true}}"#)
  });
        let (_d, f) = front_with(&up, true).await;
        let name = match mode {
            "unknown" => "unknown",
            "traversal" => "../../outside",
            _ => "synthetic",
        };
        let (s, v) = write(
            f.addr,
            "POST",
            DELETE,
            ADMIN,
            &json!({"name":name}).to_string(),
        )
        .await;
        assert_eq!(
            s,
            if ["unknown", "traversal"].contains(&mode) {
                404
            } else {
                409
            },
            "{mode}: {v}"
        );
        assert!(!up.seen().iter().any(|r| r.path.ends_with("/commit")));
        if mode != "in-use" {
            assert!(!up.seen().iter().any(|r| r.method == "DELETE"));
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ended_or_unreachable_activity_refuses_before_any_engine_mutation() {
    for end_at in ["enter", "touch"] {
        let up = upstream().await;
        up.set(move |r| {
            if r.path == format!("/internal/model-mutations/{end_at}") {
                return reply("503 Unavailable", r#"{"error":"Registration ended"}"#);
            }
            if let Some(v) = state(r) {
                return v;
            }
            panic!("engine must not be mutated");
        });
        let (_d, f) = front_with(&up, true).await;
        assert_eq!(
            write(
                f.addr,
                "POST",
                PULL,
                ADMIN,
                r#"{"checkpoint":"synthetic/repo"}"#
            )
            .await
            .0,
            503
        );
    }
    let up = upstream().await;
    up.set(|r| {
        if r.path.ends_with("/invalidate") {
            reply("503 Unavailable", "{}")
        } else {
            panic!("no calls after unavailable authority")
        }
    });
    let (_d, f) = front_with(&up, true).await;
    assert_eq!(
        write(f.addr, "POST", DELETE, ADMIN, r#"{"name":"synthetic"}"#)
            .await
            .0,
        503
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborting_acquire_at_its_await_sends_leave_without_reacquiring() {
    use noevia_server::model_maint::activity::{MaintenanceActivity, NodeActivity};
    let up = upstream().await;
    up.set(|r| {
        if r.path.ends_with("/enter") {
            (
                Duration::from_millis(500),
                reply("200 OK", r#"{"ok":true}"#).1,
            )
        } else {
            state(r).unwrap()
        }
    });
    let (_d, f) = front_with(&up, true).await;
    let app = Arc::clone(&f.app);
    let task = tokio::spawn(async move { NodeActivity(app).enter().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !up.seen().iter().any(|r| r.path.ends_with("/enter")) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !up.seen().iter().any(|r| r.path.ends_with("/leave")) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        up.seen()
            .iter()
            .filter(|r| r.path.ends_with("/enter"))
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deletion_stops_at_each_lost_registration_without_reacquiring() {
    for lost_at in 1..=4 {
        let up = upstream().await;
        let touches = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&touches);
        up.set(move |r| {
            if r.path.ends_with("/touch") && calls.fetch_add(1, Ordering::SeqCst) + 1 == lost_at {
                return reply("503 Unavailable", r#"{"error":"Registration ended"}"#);
            }
            if let Some(v) = state(r) {
                return v;
            }
            match (r.method.as_str(), r.path.as_str()) {
                ("GET", "/models") => reply("200 OK", r#"{"data":[{"id":"synthetic"}]}"#),
                ("POST", "/models/unload") => reply("200 OK", "{}"),
                ("DELETE", _) => reply("409 Conflict", "{}"),
                _ => panic!("unexpected {} {}", r.method, r.path),
            }
        });
        let (_d, f) = front_with(&up, true).await;
        let (status, body) = write(f.addr, "POST", DELETE, ADMIN, r#"{"name":"synthetic"}"#).await;
        assert_eq!(status, 503, "lost_at={lost_at}: {body}");
        let seen = up.seen();
        assert_eq!(
            seen.iter().filter(|r| r.path.ends_with("/enter")).count(),
            1
        );
        assert_eq!(
            seen.iter().filter(|r| r.path == "/models/unload").count(),
            lost_at / 2
        );
        assert_eq!(
            seen.iter().filter(|r| r.method == "DELETE").count(),
            (lost_at - 1) / 2
        );
        assert!(!seen.iter().any(|r| r.path.ends_with("/commit")));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_departure_keeps_activity_until_the_started_engine_call_settles() {
    let up = upstream().await;
    up.set(|r| {
        if let Some(v) = state(r) {
            return v;
        }
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/models"));
        (Duration::from_millis(200), reply("200 OK", "{}").1)
    });
    let (_d, f) = front_with(&up, true).await;
    let addr = f.addr;
    let caller = tokio::spawn(async move {
        write(
            addr,
            "POST",
            PULL,
            ADMIN,
            r#"{"checkpoint":"synthetic/repo"}"#,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !up.seen().iter().any(|r| r.path == "/models") {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    caller.abort();
    let _ = caller.await;
    assert!(!up.seen().iter().any(|r| r.path.ends_with("/leave")));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !up.seen().iter().any(|r| r.path.ends_with("/leave")) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        up.seen()
            .iter()
            .filter(|r| r.path.ends_with("/enter"))
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_delete_replays_node_validation_errors_and_result_projection() {
    let cases =
        js_json::wtf16::parse(include_str!("fixtures/model-delete-route-node.json")).unwrap();
    let js_json::JValue::Arr(cases) = cases else {
        panic!("oracle array")
    };
    for c in cases {
        let up = upstream().await;
        let ok = c.get("ok").as_bool().unwrap();
        let status = c.get("status").as_num().unwrap() as u16;
        let body = js_json::wtf16::stringify(c.get("error"))
            .filter(|s| s != "null")
            .unwrap_or("{}".into());
        up.set(move|r|{if let Some(v)=state(r){return v;}match(r.method.as_str(),r.path.as_str()){
   ("GET","/models")=>reply("200 OK",r#"{"data":[{"id":"synthetic","status":{"value":"loaded"}},{"id":"synthetic-system","status":{"value":"unloaded"}}]}"#),
   ("POST","/models/unload")=>reply(if ok{"200 OK"}else{"409 Refused"},"{}"),
   ("DELETE",_)=>reply(&format!("{status} Synthetic"),&body),_=>panic!("unexpected {} {}",r.method,r.path),
  }});
        let d = tempfile::tempdir().unwrap();
        seed(d.path());
        std::fs::write(
            d.path().join("users").join(A).join("auto-roles.json"),
            r#"{"fast":"synthetic","smart":"other"}"#,
        )
        .unwrap();
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, "").unwrap();
        std::fs::write(
            d.path().join("models.roles.json"),
            r#"{"synthetic-system":"system-routing"}"#,
        )
        .unwrap();
        let f = front_env(
            d.path(),
            true,
            &[
                ("NOEVIA_LEGACY_UPSTREAM", up.base()),
                ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
                ("MODEL_MANAGER_KIND", "llamacpp".into()),
                ("MODEL_MANAGER_BASE_URL", up.base()),
                ("LLAMACPP_PRESET_PATH", ini.display().to_string()),
            ],
        )
        .await;
        let (s, _, body) = raw_text(
            f.addr,
            "POST",
            DELETE,
            ADMIN,
            &format!("Origin: http://{}\r\nX-CSRF-Token: csrf\r\n", f.addr),
            c.get("raw").as_str().unwrap().as_bytes(),
        )
        .await;
        assert_eq!(
            s as f64,
            c.get("expected").get("status").as_num().unwrap(),
            "{} {body}",
            c.get("name").as_str().unwrap()
        );
        assert_eq!(
            &js_json::wtf16::parse(&body).unwrap(),
            c.get("expected").get("body"),
            "{}",
            c.get("name").as_str().unwrap()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthetic_files_and_presets_use_the_existing_owner_under_distinct_activity_and_hold() {
    use std::sync::atomic::AtomicBool;
    for missing in [false, true] {
        let d = tempfile::tempdir().unwrap();
        seed(d.path());
        let model = d.path().join("synthetic.gguf");
        let outside = d.path().join("untouched");
        std::fs::write(&outside, b"keep").unwrap();
        if !missing {
            std::fs::write(&model, [0u8; 8]).unwrap();
        }
        let ini = d.path().join("models.ini");
        std::fs::write(&ini, "[synthetic]\nmodel = synthetic.gguf\n").unwrap();
        let activity = Arc::new(AtomicBool::new(false));
        let hold = Arc::new(AtomicBool::new(false));
        let section = Arc::new(AtomicBool::new(true));
        let reloads = Arc::new(AtomicUsize::new(0));
        let (a, h, s, reloads_owner) = (
            Arc::clone(&activity),
            Arc::clone(&hold),
            Arc::clone(&section),
            Arc::clone(&reloads),
        );
        let owner_model = model.clone();
        let owner_ini = ini.clone();
        let up = upstream().await;
        up.set(move|r|{
   match(r.method.as_str(),r.path.as_str()){
    ("POST","/internal/model-mutations/enter")=>{assert!(!h.load(Ordering::SeqCst));a.store(true,Ordering::SeqCst);return reply("200 OK",r#"{"ok":true}"#);},
    ("POST","/internal/model-mutations/leave")=>{a.store(false,Ordering::SeqCst);return reply("200 OK",r#"{"ok":true}"#);},
    ("POST","/internal/models/facts")=>{return reply("200 OK",&json!({"sidecarNames":[],"sidecarLoaded":[],"scan":[{"modelId":if missing{"other"}else{"synthetic"},"bytes":8}]}).to_string());},
    ("POST","/internal/model-maintenance/lease")=>{assert!(!a.load(Ordering::SeqCst));h.store(true,Ordering::SeqCst);return reply("201 Created",&json!({"lease":"a".repeat(32),"ttlMs":300000,"renewMs":10000,"hardMs":3600000}).to_string());},
    ("DELETE",p)if p.starts_with("/internal/model-maintenance/lease/")=>{h.store(false,Ordering::SeqCst);return reply("204 No Content","");},
    ("GET","/models")=>return reply("200 OK",if s.load(Ordering::SeqCst){r#"{"data":[{"id":"synthetic","source":"preset","status":{"value":"unloaded"}}]}"#}else{r#"{"data":[]}"#}),
    ("POST","/models/unload")=>{assert!(a.load(Ordering::SeqCst));return reply(if missing{"404 Missing"}else{"200 OK"},"{}");},
    ("DELETE","/models?model=synthetic")=>{assert!(a.load(Ordering::SeqCst));assert!(!h.load(Ordering::SeqCst));if !missing{std::fs::remove_file(&owner_model).unwrap();}return reply(if missing{"404 Missing"}else{"200 OK"},"{}");},
    ("GET","/api/v1/sections")=>{assert!(h.load(Ordering::SeqCst));return reply("200 OK",r#"{"sections":[{"name":"synthetic","hasFile":false}],"revision":"base"}"#);},
    ("DELETE","/api/v1/sections/synthetic?baseRevision=base")=>{assert!(h.load(Ordering::SeqCst));assert!(!owner_model.exists());std::fs::write(&owner_ini,"").unwrap();s.store(false,Ordering::SeqCst);return reply("204 No Content","");},
    ("GET","/models?reload=1")=>{assert!(h.load(Ordering::SeqCst));reloads_owner.fetch_add(1,Ordering::SeqCst);return reply("200 OK","{}");},
    _=>{}
   }
   state(r).unwrap_or_else(||panic!("unexpected {} {}",r.method,r.path))
  });
        let f = front_env(
            d.path(),
            true,
            &[
                ("NOEVIA_LEGACY_UPSTREAM", up.base()),
                ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
                ("MODEL_MANAGER_KIND", "llamacpp".into()),
                ("MODEL_MANAGER_BASE_URL", up.base()),
                ("LLAMACPP_PRESET_PATH", ini.display().to_string()),
                ("MODEL_LOADER_URL", up.base()),
            ],
        )
        .await;
        let (status, body) = write(f.addr, "POST", DELETE, ADMIN, r#"{"name":"synthetic"}"#).await;
        assert_eq!(status, 200, "missing={missing}: {body}");
        assert_eq!(body["presetRemoved"], true);
        assert_eq!(body["presetsReloaded"], true);
        assert_eq!(body["unloaded"], !missing);
        assert!(!model.exists());
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
        assert_eq!(std::fs::read_to_string(&ini).unwrap(), "");
        assert_eq!(reloads.load(Ordering::SeqCst), 1);
        assert!(!activity.load(Ordering::SeqCst));
        assert!(!hold.load(Ordering::SeqCst));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_pull_and_delete_retain_owner_dedup_and_in_use_refusal() {
    use std::sync::atomic::AtomicBool;
    let downloading = Arc::new(AtomicBool::new(false));
    let live = Arc::clone(&downloading);
    let starts = Arc::new(AtomicUsize::new(0));
    let jobs = Arc::clone(&starts);
    let up = upstream().await;
    up.set(move|r|{if let Some(v)=state(r){return v;}match(r.method.as_str(),r.path.as_str()){
  ("POST","/models")=>{if !live.swap(true,Ordering::SeqCst){jobs.fetch_add(1,Ordering::SeqCst);} (Duration::from_millis(200),reply("200 OK","{}").1)},
  ("GET","/models")=>reply("200 OK",r#"{"data":[{"id":"synthetic/repo","status":{"value":"downloading"}}]}"#),
  ("POST","/models/unload")|("DELETE",_)=>{assert!(live.load(Ordering::SeqCst));reply("409 Conflict",r#"{"error":{"type":"gate_error","code":"model_in_use","message":"Download in progress","retryable":true}}"#)},
  _=>panic!("unexpected {} {}",r.method,r.path),
 }});
    let (_d, f) = front_with(&up, true).await;
    let first = write(
        f.addr,
        "POST",
        PULL,
        ADMIN,
        r#"{"checkpoint":"synthetic/repo"}"#,
    );
    let second = write(
        f.addr,
        "POST",
        PULL,
        ADMIN,
        r#"{"checkpoint":"synthetic/repo"}"#,
    );
    let deletion = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !downloading.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        write(
            f.addr,
            "POST",
            DELETE,
            ADMIN,
            r#"{"name":"synthetic/repo"}"#,
        )
        .await
    };
    let (a, b, c) = tokio::join!(first, second, deletion);
    assert_eq!(a.0, 200);
    assert_eq!(b.0, 200);
    assert_eq!(a.1, b.1);
    assert_eq!(c.0, 409, "{}", c.1);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert!(!up.seen().iter().any(|r| r.path.ends_with("/commit")));
}
