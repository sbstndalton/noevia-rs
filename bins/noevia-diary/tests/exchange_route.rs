//! Shift 4, step 6: POST /v1/chat/completions served by the main process (front.rs, with
//! DIARY_RUST_EXCHANGE=on, DIARY_RUST_RECOVERY=on and the embedding owner) against the exchanges
//! recorded from the frozen Python route (tests/exchange-route-fixtures.json, from
//! tests/generate_exchange_route_fixtures.py). The stub engine replays Python's recorded replies
//! by request, so a request Rust sends differently finds no reply and fails the case. Per case:
//! the status, the JSON reply or the stream's events (exchange ids and `created` masked), the
//! chat requests, and the corpus afterwards. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use axum::body::Body;
use axum::http::{Request, Response};
use noevia_diary::front::{self, Front};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

const FIXTURE: &str = include_str!("exchange-route-fixtures.json");
const CTL: &str = "synthetic-control-token";
const SECRET: &str = "synthetic-parent-secret";

fn sha256(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The recorder's `vector`: deterministic, text-dependent, unit length.
fn vector(text: &str) -> Vec<f64> {
    use sha2::Digest;
    let mut v = vec![0.0f64; 16];
    for word in text.to_lowercase().split_whitespace() {
        let h = sha2::Sha256::digest(word.as_bytes());
        for (i, x) in v.iter_mut().enumerate() {
            *x += (f64::from(h[i]) - 127.5) / 127.5;
        }
    }
    let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    let norm = if norm == 0.0 { 1.0 } else { norm };
    v.iter().map(|x| x / norm).collect()
}

struct FakeEmbedder;

impl diary_retrieval::index::Embedder for FakeEmbedder {
    fn active(&self) -> diary_retrieval::Target {
        diary_retrieval::Target::new(None, "qa-embed", "nomic-v1")
    }
    fn embed(
        &self,
        _t: &diary_retrieval::Target,
        texts: &[&str],
        _k: diary_retrieval::index::Kind,
    ) -> Result<Vec<Vec<f64>>, String> {
        Ok(texts.iter().map(|t| vector(t)).collect())
    }
}

/// A managed tenant as ManagedCorpusBackend.activate leaves it.
fn managed_tenant(root: &Path, uid: &str, files: &serde_json::Map<String, Value>) {
    let t = root.join("users").join(uid);
    std::fs::create_dir_all(&t).unwrap();
    let db = rusqlite::Connection::open(t.join("managed-diary.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE files(path TEXT PRIMARY KEY, data BLOB NOT NULL, version TEXT NOT NULL, updated REAL NOT NULL);
         CREATE TABLE directories(path TEXT PRIMARY KEY);
         CREATE TABLE workspace_imports(fingerprint TEXT PRIMARY KEY, destination TEXT NOT NULL);
         CREATE TABLE workspace_index_outbox(path TEXT PRIMARY KEY);
         CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE backups(destination TEXT PRIMARY KEY, generation INTEGER NOT NULL DEFAULT -1,
             last_success REAL, retry_at REAL NOT NULL DEFAULT 0, failures INTEGER NOT NULL DEFAULT 0, error TEXT);",
    )
    .unwrap();
    let settings = json!({"entry_layout": "daily", "month_file_template": "{year}-{month02}.md",
                          "index_enabled": false});
    for (k, v) in [
        ("tenant", uid.to_string()),
        ("active", "1".to_string()),
        ("generation", "1".to_string()),
        ("settings", settings.to_string()),
    ] {
        db.execute("INSERT INTO meta VALUES (?,?)", rusqlite::params![k, v])
            .unwrap();
    }
    for (path, text) in files {
        let text = text.as_str().unwrap();
        db.execute(
            "INSERT INTO files VALUES (?,?,?,1.0)",
            rusqlite::params![path, text.as_bytes(), sha256(text.as_bytes())],
        )
        .unwrap();
    }
    let index = t.join("managed-index.db");
    let idx = rusqlite::Connection::open(&index).unwrap();
    idx.execute_batch(
        "CREATE TABLE dirty_documents (document TEXT PRIMARY KEY);
         CREATE TABLE journal (id TEXT PRIMARY KEY, created_at TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL,
             applied INTEGER NOT NULL DEFAULT 0, applied_at TEXT, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT);",
    )
    .unwrap();
    drop(idx);
    drop(
        diary_retrieval::index::Retriever::open(
            &index,
            FakeEmbedder,
            diary_retrieval::NoGuard,
            diary_retrieval::index::Options::default(),
        )
        .unwrap(),
    );
    std::fs::write(t.join("managed-diary.active"), uid).unwrap();
}

#[derive(Default)]
struct Stub {
    /// The recorded chat calls: (request, status, reply, stream, used).
    recorded: Vec<(Value, u16, String, bool, bool)>,
    /// The chat requests received since the last take.
    seen: Vec<Value>,
}

async fn stub_engine(stub: Arc<Mutex<Stub>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = axum::Router::new().fallback(move |req: Request<Body>| {
        let stub = stub.clone();
        async move {
            let path = req.uri().path().to_owned();
            let bytes = axum::body::to_bytes(req.into_body(), 1 << 24)
                .await
                .unwrap_or_default();
            let json_reply = |status: u16, v: Value| {
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(v.to_string()))
                    .unwrap()
            };
            if path.ends_with("/models") {
                return json_reply(
                    200,
                    json!({"data": [{"id": "qa-chat"}, {"id": "qa-aux"},
                        {"id": "qa-embed", "status": {"value": "loaded", "args": ["--embedding"]}}]}),
                );
            }
            let payload: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            if path.ends_with("/embeddings") {
                let inputs: Vec<Value> = match &payload["input"] {
                    Value::Array(a) => a.clone(),
                    other => vec![other.clone()],
                };
                let data: Vec<Value> = inputs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| json!({"index": i, "embedding": vector(t.as_str().unwrap_or(""))}))
                    .collect();
                return json_reply(200, json!({"data": data}));
            }
            let mut s = stub.lock().unwrap();
            s.seen.push(payload.clone());
            let found = s
                .recorded
                .iter_mut()
                .find(|(request, _, _, _, used)| !*used && *request == payload);
            match found {
                Some((_, status, reply, stream, used)) => {
                    *used = true;
                    Response::builder()
                        .status(*status)
                        .header(
                            "content-type",
                            if *stream {
                                "text/event-stream"
                            } else {
                                "application/json"
                            },
                        )
                        .body(Body::from(reply.clone()))
                        .unwrap()
                }
                None => json_reply(599, json!({"error": "no recorded reply for this request"})),
            }
        }
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/v1")
}

/// A stub child: anything forwarded to it is a decline the case did not expect (207).
async fn stub_child() -> u16 {
    let router = axum::Router::new().fallback(|| async {
        Response::builder()
            .status(207)
            .body(Body::from("child"))
            .unwrap()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await });
    port
}

/// The recorder's masking: each exchange id, in order of appearance, becomes XID-n.
struct Mask {
    re: regex::Regex,
    uid: String,
    ids: HashMap<String, String>,
}

impl Mask {
    fn text(&mut self, text: &str) -> String {
        let mut out = String::new();
        let mut at = 0;
        for m in self.re.find_iter(text) {
            out.push_str(&text[at..m.start()]);
            let id = m.as_str();
            if id == self.uid {
                out.push_str(id);
            } else {
                let n = self.ids.len() + 1;
                let masked = self
                    .ids
                    .entry(id.to_string())
                    .or_insert_with(|| format!("XID-{n}"));
                out.push_str(masked);
            }
            at = m.end();
        }
        out.push_str(&text[at..]);
        out
    }

    fn value(&mut self, v: &Value) -> Value {
        let mut v: Value = serde_json::from_str(&self.text(&v.to_string())).unwrap();
        if let Some(o) = v.as_object_mut() {
            if o.contains_key("created") {
                o.insert("created".into(), json!(0));
            }
        }
        v
    }
}

fn corpus(root: &Path, uid: &str, mask: &mut Mask) -> serde_json::Map<String, Value> {
    let db =
        rusqlite::Connection::open(root.join("users").join(uid).join("managed-diary.db")).unwrap();
    let mut stmt = db
        .prepare("SELECT path, data FROM files ORDER BY path")
        .unwrap();
    let rows: Vec<(String, Vec<u8>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows.into_iter()
        .map(|(p, d)| (p, json!(mask.text(&String::from_utf8(d).unwrap()))))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exchanges_answer_as_the_python_route() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let uid = fixture["uid"].as_str().unwrap().to_string();
    let token = fixture["token"].as_str().unwrap().to_string();
    let dir = tempfile::tempdir().unwrap();
    managed_tenant(
        dir.path(),
        &uid,
        fixture["corpus_before"].as_object().unwrap(),
    );
    let stub = Arc::new(Mutex::new(Stub::default()));
    for case in fixture["cases"].as_array().unwrap() {
        for call in case["chat"].as_array().unwrap() {
            stub.lock().unwrap().recorded.push((
                call["request"].clone(),
                u16::try_from(call["status"].as_u64().unwrap()).unwrap(),
                call["reply"].as_str().unwrap().to_string(),
                call["stream"].as_bool().unwrap(),
                false,
            ));
        }
    }
    let engine = stub_engine(stub.clone()).await;

    // This process's reading of the recorded config.yaml: the settings Python's state used.
    let config_dir = tempfile::tempdir().unwrap();
    let yaml = fixture["config_yaml"]
        .as_str()
        .unwrap()
        .replace("STUB", &engine)
        .replace("SCRATCH", dir.path().to_str().unwrap());
    std::fs::write(config_dir.path().join("config.yaml"), yaml).unwrap();
    let yaml_path = config_dir.path().join("config.yaml");
    let loaded = diary_operator::config::load(
        Some(yaml_path.as_path()),
        config_dir.path(),
        &std::collections::BTreeMap::new(),
    )
    .unwrap();
    let mut settings = diary_operator::server_config::exchange_settings(&loaded.data).unwrap();
    for key in ["db_path", "embedding_state_path"] {
        if let Some(r) = settings.cfg["retrieval"].as_object_mut() {
            r.insert(key.into(), json!(""));
        }
    }
    let recorded = &fixture["settings"];
    assert_eq!(settings.cfg, recorded["cfg"]);
    assert_eq!(
        settings.max_answer_tokens,
        recorded["max_answer_tokens"].as_i64().unwrap()
    );
    let templates = recorded["templates"].as_object().unwrap().clone();
    let setup = noevia_diary::exchange::Setup {
        settings,
        system_prompt: noevia_diary::exchange::Prompt::Text(
            recorded["system_prompt"].as_str().unwrap().to_string(),
        ),
        templates,
    };

    let f = Arc::new(
        Front::new(CTL.into(), SECRET.into(), stub_child().await)
            .with_write_wait(std::time::Duration::from_secs(10))
            .with_recovery_here(true),
    );
    f.set_exchange_setup(setup);
    let public = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (public_addr, control_addr) = (public.local_addr().unwrap(), control.local_addr().unwrap());
    tokio::spawn(front::serve_with(public, f.clone(), front::public));
    tokio::spawn(front::serve_with(control, f.clone(), front::control));
    let client = reqwest::Client::new();
    let config = json!({
        "native_writers": [uid], "native_routes": [],
        "port": 0, "control_token": CTL, "forward_secret": SECRET, "auth_token": token, "tenant_key": "",
        "data_root": dir.path().to_str().unwrap(), "legacy_user_id": "",
        "corpus": {"entry_layout": "daily", "entries_prefix": "Entries", "index_enabled": false,
                   "month_file_template": "{year}-{month02}.md", "monthly_prefix": "", "index_file": "INDEX.md"},
        "llm": {"main": {"base_url": engine, "chat_model": "qa-chat", "setting_key": "LLM_CHAT_MODEL",
                          "fallback_setting_key": "LLM_CHAT_FALLBACK_MODEL"},
                "aux": {"base_url": engine, "chat_model": "qa-aux", "setting_key": "LLM_AUX_MODEL",
                         "fallback_setting_key": "LLM_AUX_FALLBACK_MODEL"},
                "aux_prefers_main": true, "configured": "qa-chat"},
        "corpus_write_impl": "rust",
    });
    let r = client
        .post(format!("http://{control_addr}/internal/child/config"))
        .header("x-noevia-diary-control", CTL)
        .body(config.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let target = &fixture["embedding_target"];
    let embedder = diary_answer::embedder::HttpEmbedder::fixed(
        diary_answer::model::Config {
            base_url: engine.clone(),
            timeout_s: 20.0,
            max_retries: 1,
            ..diary_answer::model::Config::default()
        },
        diary_retrieval::Target::new(
            target["base_url"].as_str(),
            target["model"].as_str().unwrap(),
            target["profile"].as_str().unwrap(),
        ),
        tokio::runtime::Handle::current(),
    );
    assert!(f
        .app()
        .unwrap()
        .save_embedder
        .set(Arc::new(embedder))
        .is_ok());

    let mut mask = Mask {
        re: regex::Regex::new(
            r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}",
        )
        .unwrap(),
        uid: uid.clone(),
        ids: HashMap::new(),
    };
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["case"].as_str().unwrap();
        stub.lock().unwrap().seen.clear();
        let body = serde_json::to_vec(&case["request"]).unwrap();
        let resp = client
            .post(format!("http://{public_addr}/v1/chat/completions"))
            .header("authorization", format!("Bearer {token}"))
            .header("x-cowork-user-id", &uid)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        assert_eq!(
            u64::from(status),
            case["status"].as_u64().unwrap(),
            "{name}: {text}"
        );
        if let Some(expected) = case.get("events") {
            let events: Vec<Value> = text
                .split("\n\n")
                .filter_map(|f| f.strip_prefix("data: "))
                .map(|e| {
                    let v: Value = serde_json::from_str(e).unwrap();
                    mask.value(&v)
                })
                .collect();
            assert_eq!(&json!(events), expected, "{name}");
        } else {
            let v: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(mask.value(&v), case["json"], "{name}");
        }
        let seen = std::mem::take(&mut stub.lock().unwrap().seen);
        let expected: Vec<Value> = case["chat"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["request"].clone())
            .collect();
        assert_eq!(seen, expected, "{name}: the chat requests");
        // The background re-index never writes the corpus; the save is done before the reply.
        assert_eq!(
            Value::Object(corpus(dir.path(), &uid, &mut mask)),
            case["corpus"],
            "{name}: the corpus"
        );
    }
    // Every exchange was this process's (a forwarded one would have answered 207), each session
    // in this process's registry.
    assert!(f.app().unwrap().sessions.len() >= 3);
}
