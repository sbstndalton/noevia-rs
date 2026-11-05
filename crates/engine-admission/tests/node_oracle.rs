//! Port contract row H: differential replay. `tests/fixtures/inference-admission-node.v1.json` (core
//! tools/gen-admission-oracle.cjs, recorded at core 3521cdc2 with Node 22 over synthetic schedules) holds
//! what Node's pre-port lease route answered each admission ask. The authority now issues the ticket and
//! Node's latch answers in the same words; whatever Node answered, the front's caller must see the same
//! admission decision it saw before: a lease (with only the gate's header) or the same refusal.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use engine_admission::{Admission, Ask, Authority, Refused};
use model_stream::Cancel;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A latch that answers `(status, body)` with `<lease>` replaced by the ticket in the path.
async fn latch(answer: Arc<Mutex<(u16, Value)>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let answer = Arc::clone(&answer);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                let first = text.lines().next().unwrap_or("").to_owned();
                let id = first
                    .split(' ')
                    .nth(1)
                    .unwrap_or("")
                    .rsplit('/')
                    .find(|p| p.len() == 32)
                    .unwrap_or("")
                    .to_owned();
                let (status, body) = if first.starts_with("PUT") {
                    let (st, b) = answer.lock().unwrap().clone();
                    (st, b.to_string().replace("<lease>", &id))
                } else if first.contains("/renew") {
                    (200, "{}".to_owned())
                } else {
                    (204, String::new())
                };
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    base
}

#[tokio::test]
async fn every_recorded_admission_decision_is_the_callers_decision_again() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/inference-admission-node.v1.json")).unwrap();
    assert_eq!(fixture["version"], 1);
    let answer = Arc::new(Mutex::new((0u16, Value::Null)));
    let base = latch(Arc::clone(&answer)).await;
    let authority = Authority::new();
    authority.node_session("oracle-boot", 1).unwrap();
    let admission =
        Arc::new(Admission::with_authority(&base, Some("secret"), authority.clone()).unwrap());
    let mut replayed = 0;
    for scenario in fixture["scenarios"].as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        // The ask's kind does not change how the client reads an answer (unit tests cover the bodies).
        for step in scenario["steps"].as_array().unwrap() {
            if step["op"] != "acquire" {
                continue;
            }
            let status = u16::try_from(step["status"].as_u64().unwrap()).unwrap();
            *answer.lock().unwrap() = (status, step["body"].clone());
            let got = admission
                .enter(Ask::Engine { model: "m" }, &Cancel::new())
                .await;
            match got {
                Ok(lease) => {
                    assert_eq!(status, 201, "{name}");
                    let want: Vec<(String, String)> = step["body"]["headers"]
                        .as_object()
                        .into_iter()
                        .flatten()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                        .collect();
                    assert_eq!(lease.headers(), want.as_slice(), "{name}");
                    assert!(lease.covered(), "{name}");
                    lease.release().await;
                }
                Err(refused) => {
                    assert_ne!(status, 201, "{name}");
                    let want = Refused {
                        status,
                        message: step["body"]["error"].as_str().unwrap().to_owned(),
                        code: step["body"]["code"].as_str().map(str::to_owned),
                        retryable: step["body"]["retryable"] == serde_json::Value::Bool(true),
                    };
                    assert_eq!(refused, want, "{name}");
                }
            }
            replayed += 1;
        }
    }
    assert!(replayed >= 30, "{replayed}");
    assert_eq!(authority.held(), 0, "every ticket was given back");
}
