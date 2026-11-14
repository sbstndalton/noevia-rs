//! Shift 4: POST /api/external-sources/import served natively for a writer tenant against the
//! imports recorded from the frozen Python route (tests/external-import-fixtures.json, from
//! tests/generate_external_import_fixtures.py): the same synthetic source files and modification
//! times, the same requests; per case the status, the JSON reply (exchange ids masked) and the
//! corpus afterwards. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

const FIXTURE: &str = include_str!("external-import-fixtures.json");
const SECRET: &str = "synthetic-forward-secret";

fn sha256(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

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
    let idx = rusqlite::Connection::open(t.join("managed-index.db")).unwrap();
    idx.execute_batch(
        "CREATE TABLE dirty_documents (document TEXT PRIMARY KEY);
         CREATE TABLE journal (id TEXT PRIMARY KEY, created_at TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL,
             applied INTEGER NOT NULL DEFAULT 0, applied_at TEXT, attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT);",
    )
    .unwrap();
    std::fs::write(t.join("managed-diary.active"), uid).unwrap();
}

/// The recorder's source folder: the same bytes and modification times.
fn source_files(scratch: &Path, files: &serde_json::Map<String, Value>) {
    for (name, f) in files {
        let path = scratch.join("qa-source").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let data: Vec<u8> = match f["hex"].as_str() {
            Some(hex) => (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect(),
            None => f["fill"].as_str().unwrap().as_bytes()[0..1]
                .repeat(usize::try_from(f["size"].as_u64().unwrap()).unwrap()),
        };
        std::fs::write(&path, data).unwrap();
        let mtime =
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(f["mtime"].as_u64().unwrap());
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }
}

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
            if m.as_str() == self.uid {
                out.push_str(m.as_str());
            } else {
                let n = self.ids.len() + 1;
                let masked = self
                    .ids
                    .entry(m.as_str().to_string())
                    .or_insert_with(|| format!("XID-{n}"));
                out.push_str(masked);
            }
            at = m.end();
        }
        out.push_str(&text[at..]);
        out
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imports_answer_as_the_python_route() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let uid = fixture["uid"].as_str().unwrap().to_string();
    let token = fixture["token"].as_str().unwrap().to_string();
    let data = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    managed_tenant(
        data.path(),
        &uid,
        fixture["corpus_before"].as_object().unwrap(),
    );
    source_files(scratch.path(), fixture["files"].as_object().unwrap());
    let source = scratch.path().join("qa-source");
    let cfg = json!({
        "port": 0, "control_token": "ctl", "forward_secret": SECRET, "auth_token": token, "tenant_key": "k",
        "data_root": data.path().to_str().unwrap(), "legacy_user_id": "",
        "corpus": {"entry_layout": "daily", "entries_prefix": "Entries", "index_enabled": false,
                   "month_file_template": "{year}-{month02}.md", "monthly_prefix": "", "index_file": "INDEX.md"},
        "llm": {"main": {"base_url": "http://127.0.0.1:9/v1", "chat_model": "qa-chat", "setting_key": "LLM_CHAT_MODEL",
                          "fallback_setting_key": "LLM_CHAT_FALLBACK_MODEL"},
                "aux": {"base_url": "http://127.0.0.1:9/v1", "chat_model": "qa-aux", "setting_key": "LLM_AUX_MODEL",
                         "fallback_setting_key": "LLM_AUX_FALLBACK_MODEL"},
                "aux_prefers_main": true, "configured": "qa-chat"},
        "corpus_write_impl": "rust",
        "external_sources": [source.to_str().unwrap()],
    })
    .to_string();
    let app = Arc::new(noevia_diary::server::App::new(
        noevia_diary::config::parse(&cfg).unwrap(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(noevia_diary::serve(listener, app));
    let mut mask = Mask {
        re: regex::Regex::new(
            r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}",
        )
        .unwrap(),
        uid: uid.clone(),
        ids: HashMap::new(),
    };
    let scratch_text = scratch.path().to_str().unwrap().to_string();
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["case"].as_str().unwrap();
        let body = case["request"]
            .to_string()
            .replace("SCRATCH", &scratch_text);
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/api/external-sources/import"))
            .header("authorization", format!("Bearer {token}"))
            .header("x-cowork-user-id", &uid)
            .header("x-noevia-diary-forward", SECRET)
            .header("x-noevia-diary-asserted", "1")
            .header("x-noevia-diary-recovered", "1")
            .header("x-noevia-diary-writer", "1")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        assert!(
            resp.headers().get("x-noevia-diary-decline").is_none(),
            "{name}: declined"
        );
        let text = resp.text().await.unwrap();
        assert_eq!(
            u64::from(status),
            case["status"].as_u64().unwrap(),
            "{name}: {text}"
        );
        let v: Value = serde_json::from_str(&mask.text(&text)).unwrap();
        assert_eq!(v, case["json"], "{name}");
        let db = rusqlite::Connection::open(
            data.path()
                .join("users")
                .join(&uid)
                .join("managed-diary.db"),
        )
        .unwrap();
        let mut stmt = db
            .prepare("SELECT path, data FROM files ORDER BY path")
            .unwrap();
        let rows: Vec<(String, Vec<u8>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let corpus: serde_json::Map<String, Value> = rows
            .into_iter()
            .map(|(p, d)| (p, json!(mask.text(&String::from_utf8(d).unwrap()))))
            .collect();
        assert_eq!(Value::Object(corpus), case["corpus"], "{name}: the corpus");
    }
}
