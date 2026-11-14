#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

//! `embedOnce`'s request over the guarded boundary, against a stub engine on loopback: the address
//! and `/v1` rule, no credentials to an explicit address, the bearer to the inference engine, the
//! body, refused answers, a refused redirect, the 16 MiB cap, and a whole index run through
//! [`DocumentEmbedder`] and the [`Indexer`].

use embedding_worker::http::Http;
use project_index::embed::DocumentEmbedder;
use project_index::http_embed::{embeddings_url, Endpoint, HttpEmbed};
use project_index::indexer::{Host, Indexer};
use project_index::slots::{Target, LEGACY_PROFILE};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    authorization: Option<String>,
    body: Value,
}

type Reply = (u16, Vec<(String, String)>, Vec<u8>);
type Answer = Arc<dyn Fn(&Value) -> Reply + Send + Sync>;

/// Gives a closure the stub's answer type.
fn answer(f: impl Fn(&Value) -> Reply + Send + Sync + 'static) -> Answer {
    Arc::new(f)
}

/// A one-request-per-connection HTTP/1.1 stub; records every request it reads.
async fn stub(answer: Answer) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let answer = Arc::clone(&answer);
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let head_end = loop {
                    let n = sock.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let header = |name: &str| {
                    head.lines().find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
                    })
                };
                let len: usize = header("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                while buf.len() < head_end + len {
                    let n = sock.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body: Value = serde_json::from_slice(&buf[head_end..]).unwrap_or(Value::Null);
                let path = head.split(' ').nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(Seen {
                    path,
                    authorization: header("authorization"),
                    body: body.clone(),
                });
                let (status, headers, payload) = answer(&body);
                let mut out = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n",
                    payload.len()
                );
                for (k, v) in headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                let _ = sock.write_all(out.as_bytes()).await;
                let _ = sock.write_all(&payload).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (base, seen)
}

fn vectors_for(body: &Value) -> Reply {
    let input = body["input"].as_array().cloned().unwrap_or_default();
    // Answered in reverse order with indexes: the batch rules put them back.
    let data: Vec<Value> = input
        .iter()
        .enumerate()
        .rev()
        .map(
            |(i, t)| json!({"index": i, "embedding": [t.as_str().unwrap().len() as f64, 1.0, 0.5]}),
        )
        .collect();
    (
        200,
        vec![],
        json!({ "data": data }).to_string().into_bytes(),
    )
}

fn target() -> Target {
    Target {
        profile: LEGACY_PROFILE.into(),
        model: "qa-model".into(),
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn http() -> Http {
    Http::new("", None, "http://127.0.0.1:9/", None).unwrap()
}

/// Runs `f` on a plain thread (as the indexer's blocking threads do).
fn off_runtime<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::spawn(f).join().unwrap()
}

#[test]
fn the_url_rule_is_nodes() {
    assert_eq!(embeddings_url("http://h:1"), "http://h:1/v1/embeddings");
    assert_eq!(embeddings_url("http://h:1/v1"), "http://h:1/v1/embeddings");
    assert_eq!(
        embeddings_url("http://h:1/v1///"),
        "http://h:1/v1/embeddings"
    );
    assert_eq!(
        embeddings_url("http://h:1/api/v1/"),
        "http://h:1/api/v1/embeddings"
    );
    assert_eq!(
        embeddings_url("http://h:1/v1/v1"),
        "http://h:1/v1/v1/embeddings"
    );
    assert_eq!(
        embeddings_url("http://h:1/x/"),
        "http://h:1/x/v1/embeddings"
    );
}

#[test]
fn an_explicit_address_gets_no_credentials_and_the_engine_gets_its_bearer() {
    let rt = runtime();
    let (base, seen) = rt.block_on(stub(answer(vectors_for)));
    let handle = rt.handle().clone();
    let b = base.clone();
    let items = off_runtime(move || {
        let embed = HttpEmbed::new(http(), handle, move |_: &Target| {
            Ok(Endpoint::Explicit(format!("{b}/v1/")))
        });
        embed.call(&target(), &["alpha".into(), "bravo two".into()])
    })
    .unwrap();
    assert_eq!(items.len(), 2);
    let handle = rt.handle().clone();
    let b = base.clone();
    off_runtime(move || {
        let embed = HttpEmbed::new(http(), handle, move |_: &Target| {
            Ok(Endpoint::Inference {
                base: b.clone(),
                token: Some("qa-token".into()),
            })
        });
        embed.call(&target(), &["charlie".into()])
    })
    .unwrap();
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].path, "/v1/embeddings");
    assert_eq!(
        seen[0].authorization, None,
        "an explicit address never gets credentials"
    );
    assert_eq!(
        seen[0].body,
        json!({"model": "qa-model", "input": ["alpha", "bravo two"]})
    );
    assert_eq!(seen[1].authorization.as_deref(), Some("Bearer qa-token"));
    assert_eq!(
        seen[1].body,
        json!({"model": "qa-model", "input": ["charlie"]})
    );
}

fn one_call(answer: Answer) -> (Result<Vec<Value>, String>, usize) {
    let rt = runtime();
    let (base, seen) = rt.block_on(stub(answer));
    let handle = rt.handle().clone();
    let out = off_runtime(move || {
        HttpEmbed::new(http(), handle, move |_: &Target| {
            Ok(Endpoint::Explicit(base.clone()))
        })
        .call(&target(), &["alpha".into()])
    });
    let n = seen.lock().unwrap().len();
    (out, n)
}

#[test]
fn refused_answers_redirects_and_oversized_bodies_fail() {
    let (out, _) = one_call(answer(|_: &Value| {
        (500, vec![], br#"{"error":"down"}"#.to_vec())
    }));
    assert_eq!(out.unwrap_err(), "embeddings 500");

    let (out, n) = one_call(answer(|_: &Value| {
        (
            302,
            vec![("location".into(), "http://127.0.0.1:9/elsewhere".into())],
            vec![],
        )
    }));
    assert!(out.is_err(), "a redirect is refused");
    assert_eq!(n, 1, "and never followed");

    let (out, _) = one_call(answer(|_: &Value| (200, vec![], br#"{"nope":1}"#.to_vec())));
    assert_eq!(
        out.unwrap(),
        Vec::<Value>::new(),
        "no data array is no items"
    );

    let big = vec![b' '; 16 * 1024 * 1024 + 1];
    let (out, _) = one_call(answer(move |_: &Value| (200, vec![], big.clone())));
    assert!(out.is_err(), "answers past 16 MiB are cut");
}

#[test]
fn a_public_address_is_refused_before_any_request() {
    let rt = runtime();
    let handle = rt.handle().clone();
    let out = off_runtime(move || {
        HttpEmbed::new(http(), handle, |_: &Target| {
            Ok(Endpoint::Explicit("http://192.0.2.1:9/".into()))
        })
        .call(&target(), &["alpha".into()])
    });
    assert!(out.is_err());
}

struct TestHost {
    dir: PathBuf,
}

impl Host for TestHost {
    fn index_path(&self, project: &str, _user: Option<&str>) -> PathBuf {
        self.dir.join(format!("{project}.db"))
    }
    fn user_active(&self, _user: &str) -> bool {
        true
    }
    fn lifecycle_generation(&self, _user: Option<&str>) -> u64 {
        0
    }
    fn deleted_since(&self, _project: &str, _user: Option<&str>, _seen: u64) -> bool {
        false
    }
    fn active_target(&self) -> Target {
        target()
    }
    fn embedding_off(&self, _t: &Target) -> bool {
        false
    }
    fn foreground_busy(&self) -> bool {
        false
    }
}

#[test]
fn a_whole_index_run_goes_through_the_guarded_request() {
    sqlite_vec_loader::register().unwrap();
    let rt = runtime();
    let (base, seen) = rt.block_on(stub(answer(vectors_for)));
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let handle = rt.handle().clone();
    let text: String = (0..300)
        .map(|i| format!("Line {i} of the QA notes. "))
        .collect();
    let out = off_runtime(move || {
        let http = HttpEmbed::new(http(), handle, move |_: &Target| {
            Ok(Endpoint::Explicit(base.clone()))
        });
        let ix = Indexer::new(TestHost { dir });
        let mut embedder = DocumentEmbedder {
            call: |t: &Target, input: &[String]| http.call(t, input),
            off: |_: &Target| false,
        };
        ix.index_file("qa-proj", "notes.md", &text, Some("qa-user"), &mut embedder)
    });
    assert!(
        out.ok && out.stored > 2 && out.embedded == out.stored,
        "{out:?}"
    );
    let seen = seen.lock().unwrap().clone();
    let sent: usize = seen
        .iter()
        .map(|s| s.body["input"].as_array().unwrap().len())
        .sum();
    assert_eq!(sent, out.stored, "every chunk was sent once");
    assert!(seen
        .iter()
        .all(|s| s.path == "/v1/embeddings" && s.authorization.is_none()));
    let db = rusqlite::Connection::open(tmp.path().join("qa-proj.db")).unwrap();
    let chunks: i64 = db
        .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(chunks as usize, out.stored);
}
