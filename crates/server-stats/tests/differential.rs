#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! Replays the actual-Node corpus (`fixtures/node-stats.v1.json`, written by `oracle.cjs` from the
//! pinned core's own handler and adapters): every case's synthetic engine is served again on
//! loopback, and the native reads must give Node's status and exact body bytes, ask the engine
//! only for what Node asked it (the same paths, headers and model queries), and, for llama.cpp,
//! read every model's metrics Node read.
use serde_json::Value;
use server_stats::{Engine, Kind, Timeouts};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const OVERSIZE: usize = 8 * 1024 * 1024 + 1;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Seen {
    path: String,
    authorization: Option<String>,
    content_type: Option<String>,
}

fn decode_hex(s: &str) -> Vec<u8> {
    s.as_bytes()
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}

fn reason(status: u64) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        _ => "X",
    }
}

/// The case's engine: each request target answered from `routes`, 404 otherwise.
async fn engine(routes: Value) -> (u16, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let routes = routes.clone();
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    let Ok(n) = s.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    head.extend_from_slice(&buf[..n]);
                }
                let text = String::from_utf8_lossy(&head).to_string();
                let mut lines = text.split("\r\n");
                let target = lines.next().unwrap().split(' ').nth(1).unwrap().to_string();
                let headers: HashMap<String, String> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                log.lock().unwrap().push(Seen {
                    path: target.clone(),
                    authorization: headers.get("authorization").cloned(),
                    content_type: headers.get("content-type").cloned(),
                });
                let r = &routes[target.as_str()];
                if r.is_null() {
                    let _ = s
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found")
                        .await;
                    return;
                }
                if r["close"] == Value::Bool(true) {
                    return;
                }
                let status = r["status"].as_u64().unwrap();
                let body: Vec<u8> = if r["oversize"] == Value::Bool(true) {
                    vec![b' '; OVERSIZE]
                } else if let Some(hex) = r["hex"].as_str() {
                    decode_hex(hex)
                } else {
                    r["text"].as_str().unwrap_or("").as_bytes().to_vec()
                };
                let mut out = format!("HTTP/1.1 {status} {}\r\n", reason(status));
                if let Some(h) = r["headers"].as_object() {
                    for (k, v) in h {
                        out.push_str(&format!("{k}: {}\r\n", v.as_str().unwrap()));
                    }
                }
                out.push_str(&format!(
                    "Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                ));
                let _ = s.write_all(out.as_bytes()).await;
                let _ = s.write_all(&body).await;
                let _ = s.shutdown().await;
            });
        }
    });
    (port, seen)
}

fn fixture() -> Value {
    let text = std::env::var("NOEVIA_STATS_FIXTURE")
        .map(|p| std::fs::read_to_string(p).unwrap())
        .unwrap_or_else(|_| include_str!("fixtures/node-stats.v1.json").into());
    serde_json::from_str(&text).unwrap()
}

fn seen_of(v: &Value) -> Seen {
    Seen {
        path: v["path"].as_str().unwrap().to_string(),
        authorization: v["authorization"].as_str().map(str::to_string),
        content_type: v["contentType"].as_str().map(str::to_string),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_node_stats_matrix() {
    let fixture = fixture();
    // The corpus must come from the pinned core (contracts/http/core.ref), never another checkout.
    assert_eq!(
        fixture["core"],
        include_str!("../../../contracts/http/core.ref").trim()
    );
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 300, "corpus shrank to {}", cases.len());
    let mut kinds: HashMap<&str, usize> = HashMap::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let kind_name = case["kind"].as_str().unwrap();
        *kinds.entry(kind_name).or_default() += 1;
        let kind = match kind_name {
            "llamacpp" => Kind::LlamaCpp,
            "lemonade" => Kind::Lemonade,
            "none" => Kind::None,
            other => panic!("{name}: kind {other}"),
        };
        let (port, seen) = engine(case["routes"].clone()).await;
        let base = format!("http://127.0.0.1:{port}{}", case["base"].as_str().unwrap());
        let key = case["apiKey"].as_str().filter(|k| !k.is_empty());
        let e = Engine::new(kind, &base, key, Timeouts::default()).unwrap();
        let reply = e.read().await;
        assert_eq!(
            (reply.status, reply.body.as_str()),
            (
                case["expected"]["status"].as_u64().unwrap() as u16,
                case["expected"]["body"].as_str().unwrap()
            ),
            "{name}"
        );
        let mut ours = seen.lock().unwrap().clone();
        ours.sort();
        let mut node: Vec<Seen> = case["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(seen_of)
            .collect();
        // Every request the port made is one Node made (same target, Authorization, Content-Type).
        for r in &ours {
            let at = node
                .iter()
                .position(|n| n == r)
                .unwrap_or_else(|| panic!("{name}: Node never asked {r:?}"));
            node.remove(at);
        }
        match kind {
            // Node's extra reads are health() and listModels(): /models again and /props per model.
            Kind::LlamaCpp => {
                for n in &node {
                    assert!(
                        n.path.ends_with("/models") || n.path.contains("/props?"),
                        "{name}: the port skipped {n:?}"
                    );
                }
            }
            _ => assert!(node.is_empty(), "{name}: the port skipped {node:?}"),
        }
    }
    assert!(kinds["llamacpp"] >= 200 && kinds["lemonade"] >= 90 && kinds["none"] >= 1);
}
