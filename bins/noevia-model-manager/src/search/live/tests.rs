#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! The actual Python differential of the search runtime: every search case of
//! `tests/fixtures/fit-python.v1.json` (`main._search_runtime` of the services app, recorded by
//! `tools/gen-fit-oracle.py` with synthetic trusted sets) is replayed against the same Docker
//! world, backend cache, models folder and `download_history` rows. The native body must be the
//! child's, byte for byte. Every name is synthetic.
use super::*;
use crate::backend_stats::{fit::reserve_gb, gpu_vram_map, Config};
use serde_json::Value as V;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn fixture() -> V {
    serde_json::from_str(include_str!("../../../tests/fixtures/fit-python.v1.json")).unwrap()
}

/// A child that advertises completed startup, counting requests.
async fn healthy_child() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if sock.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                assert!(String::from_utf8_lossy(&head)
                    .starts_with("GET /api/v1/internal/startup-ready "));
                let body = r#"{"startup_ready":1}"#;
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            });
        }
    });
    (base, seen)
}

fn stats(case: &V, dir: &Path) -> Arc<BackendStats> {
    let env = &case["env"];
    let meminfo = dir.join("meminfo");
    if let Some(text) = case["meminfo"].as_str() {
        std::fs::write(&meminfo, text).unwrap();
    }
    let configured = env["LLAMA_CONTAINERS"]
        .as_str()
        .unwrap_or("")
        .split(',')
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .collect();
    let stats = BackendStats::new(
        Config {
            configured,
            gpu_vram: gpu_vram_map(env["GPU_VRAM"].as_str().unwrap_or("")).unwrap(),
            default_port: Some(8080),
            drm_root: dir.join("drm"),
            legacy_base: "http://127.0.0.1:9".into(),
            token: String::new(),
            meminfo,
            ram_reserve_gb: reserve_gb(None),
        },
        Arc::new(crate::backend_stats::tests_world(
            (!case["docker"].is_null()).then(|| case["docker"].clone()),
        )),
    )
    .unwrap();
    stats
        .seed_cache_json(case["cache"].as_str().unwrap())
        .unwrap();
    Arc::new(stats)
}

fn history(data: &Path, rows: &V) {
    let db = rusqlite::Connection::open(data.join("model_loader.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE download_history (id INTEGER PRIMARY KEY AUTOINCREMENT, repo_id TEXT NOT NULL, \
         filename TEXT NOT NULL, dest_path TEXT NOT NULL, total_bytes INTEGER NOT NULL, \
         status TEXT NOT NULL, error TEXT, started_at REAL NOT NULL, completed_at REAL);",
    )
    .unwrap();
    for row in rows.as_array().unwrap() {
        db.execute(
            "INSERT INTO download_history(repo_id, filename, dest_path, total_bytes, status, started_at) \
             VALUES (?1, ?2, '/models', 4, ?3, 1.0)",
            [row[0].as_str().unwrap(), row[1].as_str().unwrap(), row[2].as_str().unwrap()],
        )
        .unwrap();
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_search_runtime_is_the_childs() {
    let fx = fixture();
    let cases = fx["search"].as_array().unwrap();
    assert_eq!(cases.len(), 5);
    for case in cases {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (models, data) = (dir.path().join("models"), dir.path().join("data"));
        std::fs::create_dir_all(&models).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        for name in case["files"].as_array().unwrap() {
            let p = models.join(name.as_str().unwrap());
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"GGUF").unwrap();
        }
        history(&data, &case["rows"]);
        let trusted_file = dir.path().join("trusted.json");
        std::fs::write(&trusted_file, case["trusted"].to_string()).unwrap();
        let (base, seen) = healthy_child().await;
        let search = Search::new(&data, &base, "").unwrap();
        search.set_live(Source {
            stats: stats(case, dir.path()),
            models: models.clone(),
            ini: models.join("models.ini"),
            trusted: load_trusted(&trusted_file).unwrap(),
            estimates_cache_cap_mib: None,
        });
        let source = search.live.get().unwrap();
        for _ in 0..2 {
            let body = search
                .native_runtime(source)
                .await
                .unwrap_or_else(|_| panic!("{id}"));
            assert_eq!(
                String::from_utf8(body).unwrap(),
                case["body"].as_str().unwrap(),
                "{id}"
            );
        }
        // Schema readiness is local; no private startup or runtime RPC is made.
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 0, "{id}");
    }
}

#[tokio::test]
async fn native_runtime_does_not_require_a_child_startup_advertisement() {
    let dir = tempfile::tempdir().unwrap();
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        base
    };
    let search = Search::new(dir.path(), &closed, "").unwrap();
    let case = serde_json::json!({"env": {}, "meminfo": null, "docker": null, "cache": "{}"});
    search.set_live(Source {
        stats: stats(&case, dir.path()),
        models: dir.path().join("models"),
        ini: dir.path().join("models.ini"),
        trusted: Trusted::default(),
        estimates_cache_cap_mib: None,
    });
    let source = search.live.get().unwrap();
    assert!(search.native_runtime(source).await.is_ok());
}

#[test]
fn the_trusted_file_is_strict_and_empty_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.json");
    // Missing: an error (the child keeps the runtime), never silently empty sets.
    assert!(load_trusted(&path).is_err());
    // Explicitly empty: empty sets.
    std::fs::write(&path, r#"{"quantisers":[],"labs":[]}"#).unwrap();
    assert_eq!(load_trusted(&path), Ok(Trusted::default()));
    // Not a regular file (a FIFO or a directory) is refused without opening it.
    let fifo = dir.path().join("fifo");
    assert!(std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    assert!(load_trusted(&fifo).is_err());
    assert!(load_trusted(dir.path()).is_err());
    // Entries are kept exactly (no case folding), as Python keeps them.
    std::fs::write(&path, r#"{"quantisers":["b","a","a","Mixed"],"labs":[]}"#).unwrap();
    assert_eq!(
        load_trusted(&path).unwrap().quantisers,
        vec!["Mixed".to_owned(), "a".into(), "b".into()]
    );
    std::fs::write(&path, r#"{"quantisers":["b","a","a"],"labs":[]}"#).unwrap();
    assert_eq!(
        load_trusted(&path),
        Ok(Trusted {
            quantisers: vec!["a".into(), "b".into()],
            labs: vec![]
        })
    );
    for bad in [
        "",
        "[]",
        r#"{"quantisers":[]}"#,
        r#"{"quantisers":[],"labs":[],"x":[]}"#,
        r#"{"quantisers":[1],"labs":[]}"#,
        r#"{"quantisers":"a","labs":[]}"#,
    ] {
        std::fs::write(&path, bad).unwrap();
        assert!(load_trusted(&path).is_err(), "{bad:?}");
    }
}

/// `discover.judge`'s trust test against Python's own (`trust` in the fixture): only the owner
/// is lower-cased, and membership is exact, so a mixed-case entry and a prefix lookalike never
/// match.
#[test]
fn trust_matching_is_pythons() {
    let fx = fixture();
    let trust = &fx["trust"];
    let set = |key: &str| -> HashSet<String> {
        trust[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    };
    let (quantisers, labs) = (set("quantisers"), set("labs"));
    let answers = trust["owners"].as_object().unwrap();
    assert!(answers.len() >= 8);
    for (owner, python) in answers {
        let candidate = super::super::discover::Candidate {
            id: format!("{owner}/synthetic-repo"),
            owner: owner.clone(),
            downloads: 0,
            likes: 0,
            last_modified: trust["last_modified"].as_str().unwrap().to_owned(),
            tags: vec![],
            files: Arc::new(vec![]),
            license: None,
        };
        let judged = super::super::discover::judge(
            &candidate,
            8.0,
            &quantisers,
            &labs,
            super::super::py::Instant(0),
        )
        .unwrap();
        assert_eq!(judged.trusted, python.as_bool().unwrap(), "{owner}");
    }
}
