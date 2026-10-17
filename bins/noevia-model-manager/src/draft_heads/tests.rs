#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
//! The actual Python differential: every case of `tests/fixtures/draft-heads-python.v1.json`
//! (recorded from the services app by `tools/gen-draft-heads-oracle.py`) is rebuilt in a
//! temporary folder with its store, and every request is replayed against a fake Hub that gives
//! the recorded answers: the status, the body and the Hub requests (path, query and token) must
//! match. Every name is synthetic.
use super::*;
use serde_json::Value as J;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// sha256 of the services sources the fixture came from.
fn fixture() -> J {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/draft-heads-python.v1.json"
    ))
    .unwrap()
}

fn gguf_bytes(kv: &serde_json::Map<String, J>) -> Vec<u8> {
    let mut out = b"GGUF".to_vec();
    out.extend(3u32.to_le_bytes());
    out.extend(0u64.to_le_bytes());
    out.extend((kv.len() as u64).to_le_bytes());
    for (key, spec) in kv {
        out.extend((key.len() as u64).to_le_bytes());
        out.extend(key.as_bytes());
        let (kind, value) = (spec[0].as_str().unwrap(), &spec[1]);
        match kind {
            "str" => {
                let v = value.as_str().unwrap();
                out.extend(8u32.to_le_bytes());
                out.extend((v.len() as u64).to_le_bytes());
                out.extend(v.as_bytes());
            }
            "u32" => {
                out.extend(4u32.to_le_bytes());
                out.extend(
                    u32::try_from(value.as_u64().unwrap())
                        .unwrap()
                        .to_le_bytes(),
                );
            }
            "i32" => {
                out.extend(5u32.to_le_bytes());
                out.extend(
                    i32::try_from(value.as_i64().unwrap())
                        .unwrap()
                        .to_le_bytes(),
                );
            }
            "f32" => {
                out.extend(6u32.to_le_bytes());
                #[allow(clippy::cast_possible_truncation)]
                out.extend((value.as_f64().unwrap() as f32).to_le_bytes());
            }
            "bool" => {
                out.extend(7u32.to_le_bytes());
                out.push(u8::from(value.as_bool().unwrap()));
            }
            "u64" => {
                out.extend(10u32.to_le_bytes());
                out.extend(value.as_u64().unwrap().to_le_bytes());
            }
            other => panic!("{other}"),
        }
    }
    out
}

fn build(root: &Path, case: &J) -> PathBuf {
    let models = root.join("models");
    std::fs::create_dir(&models).unwrap();
    for spec in case["files"].as_array().unwrap() {
        let path = models.join(spec["name"].as_str().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        if let Some(kv) = spec.get("gguf") {
            std::fs::write(&path, gguf_bytes(kv.as_object().unwrap())).unwrap();
        } else {
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(spec["size"].as_u64().unwrap()).unwrap();
        }
    }
    for link in case["links"].as_array().into_iter().flatten() {
        std::os::unix::fs::symlink(
            link[1].as_str().unwrap(),
            models.join(link[0].as_str().unwrap()),
        )
        .unwrap();
    }
    if let Some(ini) = case["ini"].as_str() {
        std::fs::write(models.join("models.ini"), ini).unwrap();
    }
    models
}

fn store(data: &Path, schema: &J, case: &J) {
    let db = rusqlite::Connection::open(data.join("model_loader.db")).unwrap();
    for sql in schema.as_array().unwrap() {
        db.execute_batch(sql.as_str().unwrap()).unwrap();
    }
    db.execute("DELETE FROM kv", []).unwrap();
    for sql in case["seed"].as_array().into_iter().flatten() {
        db.execute_batch(sql.as_str().unwrap()).unwrap();
    }
}

type Seen = Arc<Mutex<Vec<J>>>;

/// A fake Hub: the recorded answer for a path and query, else a 404.
async fn hub(answers: J) -> (String, Seen, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let (b, s) = (base.clone(), seen.clone());
    let task = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let (answers, base, seen) = (answers.clone(), b.clone(), s.clone());
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if sock.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let text = String::from_utf8_lossy(&head).into_owned();
                let mut lines = text.split("\r\n");
                let path = lines.next().unwrap().split(' ').nth(1).unwrap().to_owned();
                let auth = lines
                    .filter_map(|l| l.split_once(':'))
                    .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                    .map(|(_, v)| J::String(v.trim().to_owned()))
                    .unwrap_or(J::Null);
                seen.lock()
                    .unwrap()
                    .push(serde_json::json!({"path": path, "authorization": auth}));
                let (status, headers, body) = match answers.get(&path) {
                    Some(a) => (
                        a["status"].as_u64().unwrap(),
                        a["headers"].as_array().unwrap().clone(),
                        hex(a["body"].as_str().unwrap()),
                    ),
                    None => (
                        404,
                        vec![serde_json::json!(["content-type", "application/json"])],
                        br#"{"error":"Repository not found"}"#.to_vec(),
                    ),
                };
                let mut out = format!("HTTP/1.1 {status} X\r\n");
                for h in headers {
                    out.push_str(&format!(
                        "{}: {}\r\n",
                        h[0].as_str().unwrap(),
                        h[1].as_str().unwrap().replace("@HUB@", &base)
                    ));
                }
                out.push_str(&format!(
                    "content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                ));
                let _ = sock.write_all(out.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (base, seen, task)
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn fixture_provenance_and_modes() {
    let fixture = fixture();
    let services = fixture["oracle"].as_object().unwrap();
    for name in [
        "api.py",
        "helpers.py",
        "ini.py",
        "autoconfig_core.py",
        "hf.py",
        "db.py",
    ] {
        assert_eq!(services[name].as_str().unwrap().len(), 64, "{name}");
    }
    assert!(fixture["cases"].as_array().unwrap().len() >= 20);
    // The constant every 200 answer carries.
    let first = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|c| c["results"].as_array().unwrap())
        .find(|r| r["status"] == 200)
        .unwrap();
    assert!(first["body"]
        .as_str()
        .unwrap()
        .contains(&format!("\"modes\":{MODES}")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_python_differential() {
    let fixture = fixture();
    let mut compared = 0;
    for case in fixture["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let models = build(&root, case);
        store(&root, &fixture["schema"], case);
        let answers = case
            .get("hub")
            .cloned()
            .unwrap_or(J::Object(Default::default()));
        let (base, seen, server) = hub(answers).await;
        let heads = DraftHeads::with(
            &models,
            &models.join("models.ini"),
            &root,
            &format!("{base}/api"),
            Root::synthetic,
        )
        .unwrap();
        for expected in case["results"].as_array().unwrap() {
            seen.lock().unwrap().clear();
            let raw = expected["name"].as_str().unwrap();
            let name = crate::lossy_unquote(raw);
            let (status, body) = match heads.get(&name).await {
                Outcome::Json(status, body) => (status, String::from_utf8(body).unwrap()),
                Outcome::Failed => (500, "Internal Server Error".to_owned()),
                Outcome::Refused => (503, REFUSAL.to_owned()),
            };
            assert_eq!(
                status,
                expected["status"].as_u64().unwrap() as u16,
                "{id} {raw}: {body}"
            );
            assert_eq!(body, expected["body"].as_str().unwrap(), "{id} {raw}");
            assert_eq!(
                J::Array(seen.lock().unwrap().clone()),
                expected["requests"],
                "{id} {raw}"
            );
            compared += 1;
        }
        server.abort();
    }
    assert!(compared >= 40, "{compared}");
}

#[test]
fn projection_dumps_as_python() {
    let parsed = model_files::json::parse(
        r#"[{"type": "file", "oid": "x", "path": "aé\n\"b.gguf", "size": 1.5e300, "lfs": {"size": 7, "x": 1}},
            {"path": "p", "path": "q", "size": NaN, "lfs": 3}, "str", 12, [true, null, -0.0, 1e400],
            {"lfs": {}}, {"type": "file", "type": "dir", "a": {"k": 1, "k": 2}}]"#,
    )
    .unwrap();
    let Value::Arr(items) = parsed else { panic!() };
    let projected: Vec<Value> = items.iter().map(project).collect();
    assert_eq!(
        py_dumps_ascii(&Value::Arr(projected)),
        r#"[{"type": "file", "path": "a\u00e9\n\"b.gguf", "size": 1.5e+300, "lfs": {"size": 7}}, {"path": "q", "size": NaN, "lfs": 3}, "str", 12, [true, null, -0.0, Infinity], {"lfs": {}}, {"type": "dir"}]"#
    );
}

#[test]
fn next_links_and_names() {
    assert_eq!(
        next_link(r#"<https://h/a?cursor=1>; rel="next""#).as_deref(),
        Some("https://h/a?cursor=1")
    );
    assert_eq!(
        next_link(r#"<https://h/p>; rel="prev", <https://h/n> ;rel="next""#).as_deref(),
        Some("https://h/n")
    );
    assert_eq!(next_link(r#"<https://h/p>; rel=next"#), None);
    assert_eq!(pathlib_name("MTP/x.gguf"), "x.gguf");
    assert_eq!(pathlib_name("a/b/./"), "b");
    assert_eq!(pathlib_suffix("x.GGUF"), ".GGUF");
    assert_eq!(pathlib_suffix(".gguf"), "");
    assert_eq!(pathlib_suffix("x.gguf."), "");
    assert_eq!(sibling("acme/m-GGUF").as_deref(), Some("acme/m-MTP-GGUF"));
    assert_eq!(sibling("acme/m-gguf").as_deref(), Some("acme/m-MTP-GGUF"));
    assert_eq!(sibling("acme/m-MTP-GGUF"), None);
    assert_eq!(sibling("acme/m"), None);
    assert_eq!(
        origin_of("http://127.0.0.1:80/x"),
        origin_of("http://127.0.0.1/y")
    );
    assert_ne!(
        origin_of("http://127.0.0.1:81/x"),
        origin_of("http://127.0.0.1/y")
    );
}

/// Deliberate differences: a next link to another origin, a malformed repository id, and a
/// token the store cannot give are failed lookups (the token never leaves the Hub's origin).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_origin_next_links_and_odd_ids_are_failed_lookups() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let case = serde_json::json!({"files": [
        {"name": "a.gguf", "gguf": {"general.architecture": ["str", "llama"]}},
        {"name": "b.gguf", "gguf": {"general.architecture": ["str", "llama"]}}]});
    let models = build(&root, &case);
    let fixture = fixture();
    store(
        &root,
        &fixture["schema"],
        &serde_json::json!({"seed": [
            "INSERT INTO download_history(repo_id, filename, dest_path, total_bytes, status, started_at) VALUES('acme/a', 'a.gguf', '/m', 1, 'done', 1.0)",
            "INSERT INTO download_history(repo_id, filename, dest_path, total_bytes, status, started_at) VALUES('acme/b c', 'b.gguf', '/m', 1, 'done', 1.0)",
            "INSERT INTO kv(key, value) VALUES('hf_token', 'hf_synthetic')"]}),
    );
    let other = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let other_port = other.local_addr().unwrap().port();
    let (base, seen, server) = hub(serde_json::json!({
        "/api/models/acme/a/tree/main?recursive=true&expand=true": {
            "status": 200,
            "headers": [["content-type", "application/json"],
                        ["link", format!("<http://127.0.0.1:{other_port}/api/next>; rel=\"next\"")]],
            "body": hex_of(b"[]")}}))
    .await;
    let heads = DraftHeads::with(
        &models,
        &models.join("models.ini"),
        &root,
        &format!("{base}/api"),
        Root::synthetic,
    )
    .unwrap();
    for name in ["a", "b"] {
        let Outcome::Json(200, body) = heads.get(name).await else {
            panic!()
        };
        let body: J = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["remoteError"], REMOTE_ERROR, "{name}");
    }
    // Only the first page was asked for; the other origin and the odd id never were.
    assert_eq!(seen.lock().unwrap().len(), 1);
    drop(other);
    server.abort();
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// noevia#1973: the payload built page by page is byte for byte `json.dumps(projected)`, and the
/// size check gives Python's `len(payload) > MODEL_FILES_STDIN_CAP` answer at the exact edge.
#[test]
fn running_projection_matches_the_whole_dump_and_its_cap() {
    let parsed = model_files::json::parse(
        r#"[{"type": "file", "path": "a", "oid": "x"}, "s", 3, {"lfs": {"size": 1}}]"#,
    )
    .unwrap();
    let Value::Arr(items) = parsed else { panic!() };
    let mut running = Projected::default();
    for item in &items {
        running.push(item).unwrap();
    }
    assert_eq!(
        running.finish(),
        py_dumps_ascii(&Value::Arr(items.iter().map(project).collect()))
    );
    assert_eq!(Projected::default().finish(), "[]");
    // `["<n x a>"]` is n + 4 bytes: exactly the cap passes, one more byte fails.
    let at_cap = Value::Str("a".repeat(model_files::MAX_INPUT_BYTES - 4));
    let mut edge = Projected::default();
    assert!(edge.push(&at_cap).is_some());
    assert_eq!(edge.finish().len(), model_files::MAX_INPUT_BYTES);
    let over = Value::Str("a".repeat(model_files::MAX_INPUT_BYTES - 3));
    assert!(Projected::default().push(&over).is_none());
}

fn tree_page(path_bytes: usize, filler_bytes: usize) -> Vec<u8> {
    // One projected entry of about `path_bytes`, plus a field `_project` drops.
    format!(
        r#"[{{"type": "file", "path": "{}.gguf", "size": 1, "oid": "{}"}}]"#,
        "p".repeat(path_bytes),
        "o".repeat(filler_bytes)
    )
    .into_bytes()
}

fn one_section(root: &Path) -> PathBuf {
    let case = serde_json::json!({"files": [
        {"name": "a.gguf", "gguf": {"general.architecture": ["str", "llama"]}}]});
    let models = build(root, &case);
    store(
        root,
        &fixture()["schema"],
        &serde_json::json!({"seed": [
            "INSERT INTO download_history(repo_id, filename, dest_path, total_bytes, status, started_at) VALUES('acme/a', 'a.gguf', '/m', 1, 'done', 1.0)"]}),
    );
    models
}

/// noevia#1973: pages each under `TREE_CAP` whose projected total passes the input cap fail the
/// lookup on the page that passes it; the next page is never asked for (nothing more is held),
/// and fields `_project` drops do not count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projected_total_over_the_cap_fails_on_the_page_that_passes_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let models = one_section(&root);
    let nine = 9 * 1024 * 1024;
    let tree = "/api/models/acme/a/tree/main";
    let page = |body: Vec<u8>, next: Option<&str>| {
        let mut headers = vec![serde_json::json!(["content-type", "application/json"])];
        if let Some(n) = next {
            headers.push(serde_json::json!([
                "link",
                format!("<@HUB@{tree}?cursor={n}>; rel=\"next\"")
            ]));
        }
        serde_json::json!({"status": 200, "headers": headers, "body": hex_of(&body)})
    };
    let mut answers = serde_json::Map::new();
    // Page 1: a 30 MiB page whose projection is tiny (dropped `oid`), so it does not count.
    answers.insert(
        format!("{tree}?recursive=true&expand=true"),
        page(tree_page(8, 30 * 1024 * 1024), Some("2")),
    );
    answers.insert(
        format!("{tree}?cursor=2"),
        page(tree_page(nine, 0), Some("3")),
    );
    answers.insert(
        format!("{tree}?cursor=3"),
        page(tree_page(nine, 0), Some("4")),
    );
    answers.insert(format!("{tree}?cursor=4"), page(b"[]".to_vec(), None));
    let (base, seen, server) = hub(J::Object(answers)).await;
    let heads = DraftHeads::with(
        &models,
        &models.join("models.ini"),
        &root,
        &format!("{base}/api"),
        Root::synthetic,
    )
    .unwrap();
    let Outcome::Json(200, body) = heads.get("a").await else {
        panic!()
    };
    let body: J = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["remoteError"], REMOTE_ERROR);
    assert_eq!(body["remote"], serde_json::json!([]));
    let paths: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|r| r["path"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        paths,
        [
            format!("{tree}?recursive=true&expand=true"),
            format!("{tree}?cursor=2"),
            format!("{tree}?cursor=3")
        ]
    );
    server.abort();
}

/// noevia#1973: at most `CONCURRENT_LOOKUPS` Hub lookups are in flight at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_lookups_are_bounded() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let models = one_section(&root);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let (now, peak, total) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    );
    // Every answer waits until all lookups that may be in flight have arrived (or 2 s pass), so
    // an unbounded reader would show more than `CONCURRENT_LOOKUPS` at once.
    let arrived = Arc::new(tokio::sync::Notify::new());
    let (n, p, t, a) = (now.clone(), peak.clone(), total.clone(), arrived.clone());
    let server = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let (n, p, t, a) = (n.clone(), p.clone(), t.clone(), a.clone());
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if sock.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let inflight = n.fetch_add(1, Ordering::SeqCst) + 1;
                p.fetch_max(inflight, Ordering::SeqCst);
                t.fetch_add(1, Ordering::SeqCst);
                a.notify_waiters();
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while n.load(Ordering::SeqCst) <= CONCURRENT_LOOKUPS {
                        a.notified().await;
                    }
                })
                .await;
                n.fetch_sub(1, Ordering::SeqCst);
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n[]")
                    .await;
                let _ = sock.shutdown().await;
            });
        }
    });
    let heads = Arc::new(
        DraftHeads::with(
            &models,
            &models.join("models.ini"),
            &root,
            &format!("{base}/api"),
            Root::synthetic,
        )
        .unwrap(),
    );
    let calls: Vec<_> = (0..6)
        .map(|_| {
            let h = heads.clone();
            tokio::spawn(async move { h.get("a").await })
        })
        .collect();
    for call in calls {
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(60), call)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, Outcome::Json(200, _)));
    }
    // Each answer is a listing without heads, then the `-GGUF` sibling rule does not apply.
    assert_eq!(total.load(Ordering::SeqCst), 6);
    assert!(
        peak.load(Ordering::SeqCst) <= CONCURRENT_LOOKUPS,
        "{peak:?}"
    );
    assert!(peak.load(Ordering::SeqCst) >= 1);
    server.abort();
}
