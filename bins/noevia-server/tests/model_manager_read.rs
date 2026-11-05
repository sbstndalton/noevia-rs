//! Native replay of Core origin/main's synthetic manager oracle. No Node route executor.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod maint_support;
use maint_support::*;
use serde_json::Value;
use std::sync::Arc;

#[tokio::test]
async fn replay_manager_read_oracle() {
    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/model-manager-node.v1.json")).unwrap();
    for s in oracle["scenarios"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let _db = seed_database(dir.path());
        let status = s["status"].as_u64().unwrap_or(200) as u16;
        let body = s
            .get("body")
            .cloned()
            .unwrap_or(serde_json::json!({"ok":true}));
        let drop = s["drop"] == true;
        let (port, seen) = engine(Arc::new(move |_| {
            if drop {
                Answer::Drop
            } else {
                Answer::Reply(status, body.to_string())
            }
        }))
        .await;
        let mut env = vec![("MODEL_MANAGER_KIND".into(), "none".into())];
        if s["disabled"] != true {
            env.push((
                "MODEL_LOADER_URL".into(),
                format!("http://127.0.0.1:{port}/"),
            ));
            env.push(("MODEL_LOADER_TOKEN".into(), "synthetic-token".into()));
        }
        let (addr, stop) = start(dir.path(), &env).await;
        let cookie = if s["role"] == "member" { MEMBER } else { ADMIN };
        let (status, headers, body) =
            call(addr, "GET", s["path"].as_str().unwrap(), Some(cookie), b"").await;
        assert_eq!(
            status,
            s["expect"]["status"].as_u64().unwrap() as u16,
            "{}: {body}",
            s["name"]
        );
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            s["expect"]["body"],
            "{}",
            s["name"]
        );
        if s["expect"]["headers"]["cache-control"] == "no-store" {
            assert!(headers.contains("cache-control: no-store"), "{}", s["name"]);
        }
        let requests = seen.lock().unwrap();
        let expected = s["expect"]["calls"].as_array().unwrap();
        assert_eq!(requests.len(), expected.len(), "{}", s["name"]);
        for (actual, expected) in requests.iter().zip(expected) {
            assert_eq!(actual.method, "GET");
            assert_eq!(
                actual.target,
                expected["url"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("http://synthetic.invalid")
                    .unwrap()
            );
            assert_eq!(
                actual
                    .headers
                    .get("x-model-loader-token")
                    .map(String::as_str),
                Some("synthetic-token")
            );
        }
        let _ = stop.send(());
    }
}
