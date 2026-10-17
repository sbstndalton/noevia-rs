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

fn tree_page(entries: usize, path_bytes: usize, filler_bytes: usize) -> Vec<u8> {
    // `entries` projected entries of about `path_bytes` each, plus a field `_project` drops.
    let entry = format!(
        r#"{{"type": "file", "path": "{}.gguf", "size": 1, "oid": "{}"}}"#,
        "p".repeat(path_bytes),
        "o".repeat(filler_bytes)
    );
    format!("[{}]", vec![entry; entries].join(", ")).into_bytes()
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

/// noevia#1973: pages each under the page cap whose projected total passes the input cap fail the
/// lookup on the page that passes it; the next page is never asked for (nothing more is held),
/// and fields `_project` drops do not count. (noevia#1985: every page and string is within
/// `PAGE_CAP` and `PAGE_STRING_CHARS`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projected_total_over_the_cap_fails_on_the_page_that_passes_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let models = one_section(&root);
    // 160 entries of 60 000-character paths: about 9.2 MiB projected per page.
    let (entries, path) = (160, 60_000);
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
    // Page 1: a 12 MiB page whose projection is tiny (dropped `oid`), so it does not count
    // (were it counted, page 2 would already pass the cap).
    let first = tree_page(200, 8, 64_000);
    assert!(first.len() > 12 * 1024 * 1024 && first.len() <= PAGE_CAP);
    answers.insert(
        format!("{tree}?recursive=true&expand=true"),
        page(first, Some("2")),
    );
    answers.insert(
        format!("{tree}?cursor=2"),
        page(tree_page(entries, path, 0), Some("3")),
    );
    answers.insert(
        format!("{tree}?cursor=3"),
        page(tree_page(entries, path, 0), Some("4")),
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

// ---- noevia#1985: memory under a hostile Hub page ---------------------------------------------

const MIB: usize = 1024 * 1024;

/// Heap bytes a parsed tree holds, from its buffer capacities (deterministic: no allocator
/// hooks, which `unsafe_code = "forbid"` rules out). Each allocation is charged as glibc's
/// malloc does: at least 24 usable bytes, plus an 8-byte header, in 16-byte steps.
fn charged(bytes: usize) -> usize {
    if bytes == 0 {
        0
    } else {
        (bytes.max(24) + 8).div_ceil(16) * 16
    }
}

fn heap(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) | Value::Float(_) => 0,
        // `BigInt` keeps its digits in a `String` reserved exactly (`try_reserve_exact`).
        Value::Int(i) => charged(i.to_string().trim_start_matches('-').len()),
        Value::Str(s) => charged(s.capacity()),
        Value::Arr(items) => {
            charged(items.capacity() * std::mem::size_of::<Value>())
                + items.iter().map(heap).sum::<usize>()
        }
        Value::Obj(pairs) => {
            charged(pairs.capacity() * std::mem::size_of::<(String, Value)>())
                + pairs
                    .iter()
                    .map(|(k, v)| charged(k.capacity()) + heap(v))
                    .sum::<usize>()
        }
    }
}

/// The most a page's tree may hold: per node, a slot in its parent (an object pair is the larger)
/// with the parent's doubling slack, plus its own smallest allocation; and every string byte
/// twice (strings grow by doubling as they are unescaped), at most the page's bytes.
fn tree_bound() -> usize {
    let per_node = 2 * std::mem::size_of::<(String, Value)>() + charged(1);
    PAGE_NODES * per_node + 2 * PAGE_CAP
}

/// `text` repeated into exactly `len` bytes between `open` and `close`.
fn filled(open: &str, unit: &str, close: &str, len: usize) -> Vec<u8> {
    let mut page = String::with_capacity(len);
    page.push_str(open);
    while page.len() + unit.len() + close.len() <= len {
        page.push_str(unit);
    }
    while page.len() + close.len() < len {
        page.push(' ');
    }
    page.push_str(close);
    assert_eq!(page.len(), len);
    page.into_bytes()
}

/// Streams `page` in 48 KiB chunks through the capped reader, as a Hub response arrives (not a
/// power of two, so plain doubling would overshoot the cap: 12 MiB, then 24 MiB).
async fn read_page(page: &[u8]) -> Option<Vec<u8>> {
    let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = page
        .chunks(48 * 1024)
        .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
        .collect();
    read_capped(futures_util::stream::iter(chunks), PAGE_CAP).await
}

/// noevia#1985 (the peak-buffer assertion #1973 asked for): hostile pages at the page cap are
/// refused, and nothing on the way holds more than a fixed bound. The raw buffer never grows past
/// `PAGE_CAP`; a page one byte over is not read. The parse stops at the first node or string
/// character over its limit, so what a refused page allocated is a prefix of a page the limits
/// admit; the largest admitted trees of each shape (bare numbers, object keys, long strings,
/// nested single-element arrays) are measured by their buffer capacities and stay under
/// `tree_bound()` (68 MiB). While a page is read and parsed, the listing projected from the
/// earlier pages is held too (at most `MAX_INPUT_BYTES`, see `projected_entries_…`), so that
/// stage is at most page + tree + listing = 16 + 68 + 16 = 100 MiB (noevia#1995); projecting it
/// (tree + listing) is less. The final parse of the listing and its file list are
/// `final_stage_is_bounded`. It was 1-2 GiB per page.
#[tokio::test]
async fn hostile_pages_are_refused_within_a_bound() {
    // Escaped non-ASCII text (`\u00e9` is 6 page bytes for one character) and raw 4-byte text.
    let hostile = [
        ("bare numbers", filled("[1", ",1", "]", PAGE_CAP)),
        ("deep nesting", filled("", "[", "", PAGE_CAP)),
        ("escaped text", filled("[\"", "\\u00e9", "\"]", PAGE_CAP)),
        (
            "raw 4-byte text",
            filled("[\"", "\u{1F600}", "\"]", PAGE_CAP),
        ),
        (
            "object keys",
            filled("{\"k\": 1", ", \"k\": 1", "}", PAGE_CAP),
        ),
        ("empty arrays", filled("[[]", ", []", "]", PAGE_CAP)),
    ];
    for (name, page) in hostile {
        let body = read_page(&page).await.unwrap_or_else(|| panic!("{name}"));
        assert_eq!(body.len(), PAGE_CAP, "{name}");
        assert!(body.capacity() <= PAGE_CAP, "{name}: {}", body.capacity());
        assert!(parse_page(body).is_none(), "{name} was parsed");
    }
    let mut over = filled("[1", ",1", "]", PAGE_CAP);
    over.push(b' ');
    assert!(read_page(&over).await.is_none());

    let largest = [
        ("bare numbers", filled("[1", ",1", "]", 2 * PAGE_NODES - 1)),
        (
            "object keys",
            format!("{{{}}}", vec!["\"k\": 1"; PAGE_NODES / 2 - 1].join(", ")).into_bytes(),
        ),
        // noevia#1995: nested single-element arrays, the most heap per node (each level a
        // `Vec` of 4 values), 501 levels deep.
        (
            "nested arrays",
            filled(
                "[",
                &format!("{}1{}, ", "[".repeat(500), "]".repeat(500)),
                "1]",
                1003 * (PAGE_NODES / 501 - 1) + 3,
            ),
        ),
        // Strings one character past a power of two: the most doubling slack.
        (
            "long strings",
            filled(
                "[\"\"",
                &format!(", \"{}\"", "\u{1F600}".repeat(8 * 1024 + 1)),
                "]",
                PAGE_CAP,
            ),
        ),
    ];
    let bound = tree_bound();
    assert!(
        bound + PAGE_CAP + model_files::MAX_INPUT_BYTES <= 100 * MIB,
        "{bound}"
    );
    for (name, page) in largest {
        let body = read_page(&page).await.unwrap_or_else(|| panic!("{name}"));
        assert!(body.capacity() <= PAGE_CAP, "{name}");
        let tree = parse_page(body).unwrap_or_else(|| panic!("{name}"));
        let held = heap(&tree);
        assert!(held <= bound, "{name}: {held} > {bound}");
        eprintln!(
            "{name}: tree {:.1} MiB (bound {:.1} MiB)",
            held as f64 / MIB as f64,
            bound as f64 / MIB as f64
        );
    }
    // One node or one character more is refused.
    let numbers = filled("[1", ",1", "]", 2 * PAGE_NODES + 1);
    assert!(parse_page(numbers).is_none());
    let long = format!("[\"{}\"]", "a".repeat(PAGE_STRING_CHARS + 1)).into_bytes();
    assert!(parse_page(long).is_none());
    let longest = format!("[\"{}\"]", "a".repeat(PAGE_STRING_CHARS)).into_bytes();
    assert!(parse_page(longest).is_some());
    // A UTF-16 page (decoded, the raw bytes dropped before the parse) still reads.
    let utf16: Vec<u8> = "[1, \"x\"]"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    assert_eq!(
        parse_page(utf16),
        Some(Value::Arr(vec![
            Value::Int(model_files::BigInt::from_u64(1)),
            Value::Str("x".into())
        ]))
    );
}

/// noevia#1985: an entry is measured before it is appended. One whose projection would pass the
/// input cap (escaped non-ASCII text dumps to 3 times its page bytes) is refused with nothing
/// appended or allocated for it, and the payload buffer never grows past the cap. The payload is
/// still byte for byte `json.dumps([_project(e) ...])`.
#[test]
fn projected_entries_are_measured_before_they_are_appended() {
    // A page entry of about 6 MiB of raw 4-byte characters in a field `_project` keeps: 18 MiB
    // projected.
    let strings: Vec<Value> = (0..96)
        .map(|_| Value::Str("\u{1F600}".repeat(16 * 1024)))
        .collect();
    let entry = Value::Obj(vec![
        ("type".into(), Value::Str("file".into())),
        ("path".into(), Value::Str("x.gguf".into())),
        ("size".into(), Value::Arr(strings)),
    ]);
    let mut size = Count::default();
    dump_projected(&entry, &mut size);
    assert!(size.bytes > model_files::MAX_INPUT_BYTES, "{}", size.bytes);
    let mut fresh = Projected::default();
    assert!(fresh.push(&entry).is_none());
    assert_eq!((fresh.out.len(), fresh.out.capacity()), (0, 0));
    let small = Value::Obj(vec![("path".into(), Value::Str("a.gguf".into()))]);
    let mut running = Projected::default();
    running.push(&small).unwrap();
    let before = (running.out.clone(), running.out.capacity());
    assert!(running.push(&entry).is_none());
    assert_eq!((running.out.clone(), running.out.capacity()), before);

    // Entries up to the cap: the buffer ends at most at the cap, never past it.
    let piece = Value::Obj(vec![("path".into(), Value::Str("p".repeat(60_000)))]);
    let mut full = Projected::default();
    let mut pushed = 0usize;
    while full.push(&piece).is_some() {
        pushed += 1;
        assert!(full.out.capacity() <= model_files::MAX_INPUT_BYTES);
    }
    assert!(pushed > 250, "{pushed}");
    assert!(full.out.capacity() <= model_files::MAX_INPUT_BYTES);
    assert!(full.finish().len() <= model_files::MAX_INPUT_BYTES);

    // The contract: what the running payload writes is `json.dumps` of the projected entries,
    // with repeated keys, non-ASCII and astral characters, odd `lfs` and non-object entries.
    let parsed = model_files::json::parse(
        r#"[{"type": "file", "oid": "x", "path": "aé😀\n\"b.gguf", "size": 1.5e300, "lfs": {"size": 7, "x": 1}},
            {"path": "p", "path": "q", "size": NaN, "lfs": 3}, "str", 12, [true, null, -0.0, 1e400],
            {"lfs": {}}, {"type": "file", "type": "dir", "a": {"k": 1, "k": 2}},
            {"size": {"b": 1, "a": 2, "b": 3}, "lfs": {"size": {"z": [1, {"y": "\u0001"}]}}}]"#,
    )
    .unwrap();
    let Value::Arr(items) = parsed else { panic!() };
    let mut running = Projected::default();
    for item in &items {
        running.push(item).unwrap();
    }
    let whole = py_dumps_ascii(&Value::Arr(items.iter().map(project).collect()));
    assert_eq!(running.finish(), whole);
    assert!(
        whole.contains(r#""path": "a\u00e9\ud83d\ude00\n\"b.gguf""#),
        "{whole}"
    );
    assert!(
        whole.contains(
            r#"{"size": {"b": 3, "a": 2}, "lfs": {"size": {"z": [1, {"y": "\u0001"}]}}}"#
        ),
        "{whole}"
    );
}

/// noevia#1985: through the Hub client, a page over a parse limit is a failed lookup like a page
/// over the size cap, and an ordinary page after it still answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pages_over_the_parse_limits_are_failed_lookups() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let models = one_section(&root);
    let tree = "/api/models/acme/a/tree/main?recursive=true&expand=true";
    let ok = |body: Vec<u8>| serde_json::json!({"status": 200, "headers": [["content-type", "application/json"]], "body": hex_of(&body)});
    for page in [
        filled("[1", ",1", "]", 4 * PAGE_NODES),
        format!("[\"{}\"]", "\\u00e9".repeat(PAGE_STRING_CHARS + 1)).into_bytes(),
        filled("[", " ", "]", PAGE_CAP + 1),
    ] {
        let (base, _seen, server) = hub(serde_json::json!({ tree: ok(page) })).await;
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
        server.abort();
    }
    let normal = br#"[{"type": "file", "path": "a-draft-Q4_K_M.gguf", "size": 5, "oid": "x", "lfs": {"size": 5, "oid": "y"}}]"#;
    let (base, _seen, server) = hub(serde_json::json!({ tree: ok(normal.to_vec()) })).await;
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
    assert!(body.get("remoteError").is_none(), "{body}");
    assert!(body["remote"].is_array(), "{body}");
    server.abort();
}

// ---- noevia#1995: the projected listing and its final parse -----------------------------------

/// `{"size": [1, 1, …]}` with `ints` numbers: `ints + 3` nodes.
fn sized(ints: usize) -> Value {
    Value::Obj(vec![(
        "size".into(),
        Value::Arr(vec![Value::Int(model_files::BigInt::from_u64(1)); ints]),
    )])
}

/// The node count the payload keeps is the count `model_files::json` takes when it parses it: the
/// payload parses with exactly that many nodes allowed and not with one fewer.
#[test]
fn projected_nodes_are_counted_as_the_parser_counts_them() {
    let parsed = model_files::json::parse(
        r#"[{"type": "file", "oid": "x", "path": "a.gguf", "size": 1.5e300, "lfs": {"size": 7, "x": 1}},
            {"path": "p", "path": "q", "size": NaN, "lfs": 3}, "str", 12, [true, null, [[]], {}],
            {"lfs": {}}, {"type": "file", "type": "dir", "a": {"k": 1, "k": 2}},
            {"size": {"b": 1, "a": 2, "b": 3}, "lfs": {"size": {"z": [1, {"y": "w"}]}}}, {}]"#,
    )
    .unwrap();
    let Value::Arr(items) = parsed else { panic!() };
    let mut running = Projected::default();
    for item in &items {
        running.push(item).unwrap();
    }
    let nodes = running.nodes + 1;
    let payload = running.finish();
    let with = |nodes: usize| Limits {
        nodes,
        ..Limits::default()
    };
    assert!(model_files::json::parse_with_limits(&payload, with(nodes)).is_ok());
    assert!(model_files::json::parse_with_limits(&payload, with(nodes - 1)).is_err());
    let mut empty = Projected::default();
    assert_eq!(empty.nodes, 0);
    empty.push(&sized(2)).unwrap();
    assert_eq!(empty.nodes, 5);
}

/// noevia#1995: an entry that would take the payload past `PROJECTED_NODES` (with its enclosing
/// array) or past `MAX_ENTRIES` entries is refused with nothing appended.
#[test]
fn projected_nodes_and_entries_are_capped() {
    let mut running = Projected::default();
    running.push(&sized(PROJECTED_NODES - 4)).unwrap();
    assert_eq!(running.nodes + 1, PROJECTED_NODES);
    let before = (running.out.len(), running.nodes, running.entries);
    assert!(running.push(&Value::Null).is_none());
    assert_eq!((running.out.len(), running.nodes, running.entries), before);
    assert!(Projected::default()
        .push(&sized(PROJECTED_NODES - 3))
        .is_none());

    let mut many = Projected::default();
    for _ in 0..model_files::MAX_ENTRIES {
        many.push(&Value::Obj(Vec::new())).unwrap();
    }
    let len = many.out.len();
    assert!(many.push(&Value::Obj(Vec::new())).is_none());
    assert_eq!(
        (many.out.len(), many.entries),
        (len, model_files::MAX_ENTRIES)
    );
}

/// noevia#1995: the final parse is held to `PROJECTED_NODES` too (it used the default, no node
/// limit), so a payload over it is a failed lookup whatever built it.
#[test]
fn the_final_parse_refuses_a_payload_over_the_node_cap() {
    // `[{"size": [1, …]}]`: 4 + ints nodes.
    let payload = |ints: usize| format!("[{{\"size\": [{}]}}]", vec!["1"; ints].join(", "));
    assert_eq!(files_of(payload(PROJECTED_NODES - 4)).unwrap(), Vec::new());
    assert!(files_of(payload(PROJECTED_NODES - 3)).is_err());
}

fn files_heap(files: &[model_files::HfFile], capacity: usize) -> usize {
    charged(capacity * std::mem::size_of::<model_files::HfFile>())
        + files
            .iter()
            .map(|f| {
                charged(f.path.capacity())
                    + charged(f.shard_base.capacity())
                    + f.quant.as_ref().map_or(0, |q| charged(q.capacity()))
                    + charged(f.size.to_string().trim_start_matches('-').len())
            })
            .sum::<usize>()
}

/// noevia#1995: the final stage. The listing text (at most `MAX_INPUT_BYTES`) and its tree (at
/// most `PROJECTED_NODES` nodes: `tree_bound()`, as for a page) are 84 MiB at most; the text is
/// then dropped and the file list built beside the tree. The file list holds at most one file per
/// 5 nodes and copies of their paths (the shard base and the quant label come from each path).
/// It is largest for sharded paths just past a power of two, where both the node cap and the
/// input cap bind: the parser's path string doubles (288 characters keep 512 bytes) and the shard
/// base is reserved by characters and then doubled by the `.gguf` extend. A sweep of path lengths
/// 40-1100 (and 2049, 4096), sharded or not, peaked at 98.3 MiB for 288-character sharded paths
/// (52,428 files), so the bound is 100 MiB. The other shapes: the most files, short sharded
/// quantised names, and the longest ASCII and 4-byte paths.
#[test]
fn final_stage_is_bounded() {
    let mk = |entry: String, count: usize| format!("[{}]", vec![entry; count].join(", "));
    let fill = |path: String| {
        let entry = format!(r#"{{"type": "file", "path": "{path}", "size": 123456789012}}"#);
        let count = (model_files::MAX_INPUT_BYTES - 16) / (entry.len() + 2);
        mk(entry, count)
    };
    // `{"type": "file", "path": …}` with a `len`-character sharded quantised path, as many as both
    // caps allow.
    let sharded = |len: usize| {
        let suffix = "-Q4_K_M-00001-of-00002.gguf";
        let entry = format!(
            r#"{{"type": "file", "path": "{}{suffix}"}}"#,
            "a".repeat(len - suffix.len())
        );
        let count = ((model_files::MAX_INPUT_BYTES - 16) / (entry.len() + 2))
            .min((PROJECTED_NODES - 1) / 5);
        mk(entry, count)
    };
    let shapes = [
        ("257-character sharded paths", sharded(257)),
        (
            "288-character sharded paths (the sweep's peak)",
            sharded(288),
        ),
        ("513-character sharded paths", sharded(513)),
        (
            "most files",
            mk(
                r#"{"type": "file", "path": "a.gguf"}"#.to_owned(),
                (PROJECTED_NODES - 1) / 5,
            ),
        ),
        (
            "sharded quantised",
            mk(
                r#"{"type": "file", "path": "x-Q4_K_M-00001-of-00002.gguf", "size": 123456789012}"#
                    .to_owned(),
                (PROJECTED_NODES - 1) / 7,
            ),
        ),
        (
            "long ASCII paths",
            fill(format!(
                "{}-Q4_K_M-00001-of-00002.gguf",
                "a".repeat(model_files::json::MAX_STRING_CHARS - 30)
            )),
        ),
        (
            "long 4-byte paths",
            fill(format!(
                "{}-IQ4_XS-00001-of-00002.gguf",
                "\u{1F600}".repeat(model_files::json::MAX_STRING_CHARS - 30)
            )),
        ),
    ];
    for (name, payload) in shapes {
        assert!(payload.len() <= model_files::MAX_INPUT_BYTES, "{name}");
        let limits = Limits {
            nodes: PROJECTED_NODES,
            ..Limits::default()
        };
        let tree = model_files::json::parse_with_limits(&payload, limits).unwrap();
        let held = heap(&tree);
        assert!(held <= tree_bound(), "{name}: {held}");
        let files = model_files::files_from_tree(&tree).unwrap();
        assert!(!files.is_empty(), "{name}");
        let list = files_heap(&files, files.capacity());
        assert!(held + list <= 100 * MIB, "{name}: {held} + {list}");
        assert_eq!(files_of(payload).unwrap(), files, "{name}");
        eprintln!(
            "{name}: tree {:.1} MiB, {} files {:.1} MiB",
            held as f64 / MIB as f64,
            files.len(),
            list as f64 / MIB as f64
        );
    }
}

/// noevia#1995: the attack the review found. Every page passes the page limits, but each keeps a
/// `size` of 262,140 numbers in the projection. 21 such pages stay under the 16 MiB input cap
/// (3 bytes a number), so the final parse would build about 5.5 million numbers (some 370 MiB).
/// Now the listing fails on the second page and never asks for the third.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pages_of_kept_numbers_fail_the_lookup_at_the_node_cap() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let models = one_section(&root);
    let tree = "/api/models/acme/a/tree/main";
    let body = format!("[{{\"size\": [{}]}}]", vec!["1"; PAGE_NODES - 4].join(","));
    assert!(parse_page(body.clone().into_bytes()).is_some());
    let mut answers = serde_json::Map::new();
    let pages = 21;
    let projected = (body.len() - 15) / 2 * 3 + 15;
    assert!(pages * projected < model_files::MAX_INPUT_BYTES);
    for page in 1..=pages {
        let path = if page == 1 {
            format!("{tree}?recursive=true&expand=true")
        } else {
            format!("{tree}?cursor={page}")
        };
        let mut headers = vec![serde_json::json!(["content-type", "application/json"])];
        if page < pages {
            headers.push(serde_json::json!([
                "link",
                format!("<@HUB@{tree}?cursor={}>; rel=\"next\"", page + 1)
            ]));
        }
        answers.insert(
            path,
            serde_json::json!({"status": 200, "headers": headers, "body": hex_of(body.as_bytes())}),
        );
    }
    let (base, seen, server) = hub(J::Object(answers)).await;
    let heads = DraftHeads::with(
        &models,
        &models.join("models.ini"),
        &root,
        &format!("{base}/api"),
        Root::synthetic,
    )
    .unwrap();
    let Outcome::Json(200, answer) = heads.get("a").await else {
        panic!()
    };
    let answer: J = serde_json::from_slice(&answer).unwrap();
    assert_eq!(answer["remoteError"], REMOTE_ERROR);
    assert_eq!(seen.lock().unwrap().len(), 2);
    server.abort();
}

/// noevia#1995: `push` keeps room for the closing `]`, so `finish` never reallocates: a payload of
/// one ~15 MiB entry stays at (about) its length instead of doubling to ~30 MiB, and no finished
/// payload's buffer passes `MAX_INPUT_BYTES`.
#[test]
fn finishing_the_listing_never_reallocates() {
    let entry = Value::Str("p".repeat(15 * MIB));
    let mut one = Projected::default();
    one.push(&entry).unwrap();
    let before = one.out.capacity();
    assert!(before > one.out.len(), "no room for the closing bracket");
    let payload = one.finish();
    assert_eq!(payload.capacity(), before);
    assert!(payload.capacity() <= model_files::MAX_INPUT_BYTES);
    let at_cap = Value::Str("a".repeat(model_files::MAX_INPUT_BYTES - 4));
    let mut full = Projected::default();
    full.push(&at_cap).unwrap();
    let payload = full.finish();
    assert_eq!(payload.len(), model_files::MAX_INPUT_BYTES);
    assert_eq!(payload.capacity(), model_files::MAX_INPUT_BYTES);
}
