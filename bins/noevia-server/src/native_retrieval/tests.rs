#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

//! The native research retrieve over an index Node wrote (crates/project-retrieval F2-F6, S1), with
//! the query vector Node bound answered by a stub: the first five of Node's hits, and every gate in
//! front of the search.

use super::*;
use project_retrieval::rerank::{self, Chain};
use serde_json::{json, Value};
use server_store::rusqlite;
use std::sync::atomic::{AtomicUsize, Ordering};

const FIXTURE: &str =
    include_str!("../../../../crates/project-retrieval/tests/fixtures/F2-F6-search.json");
const USER: &str = "0123abcd-0000-4000-8000-0000000000a1";
const START: &str = "qa-start-r4";
const NOW: f64 = 1_800_000_000_000.0;

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

fn search_of(f: &Value, scenario: &str, note: &str) -> Value {
    f["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == scenario)
        .unwrap()["searches"]
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["note"] == note)
        .unwrap()
        .clone()
}

fn cell(v: &Value) -> rusqlite::types::Value {
    match v {
        Value::Null => rusqlite::types::Value::Null,
        Value::Number(n) if n.is_i64() => rusqlite::types::Value::Integer(n.as_i64().unwrap()),
        Value::String(s) => rusqlite::types::Value::Text(s.clone()),
        other => panic!("unexpected cell {other}"),
    }
}

/// Rebuilds a Node-written index from its dump (as crates/project-retrieval/tests/search_parity.rs).
fn rebuild(path: &Path, dump: &Value) {
    embedding_worker::sqlite_vec_loader::register().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(path).unwrap();
    for sql in dump["schema"].as_array().unwrap() {
        conn.execute_batch(sql.as_str().unwrap()).unwrap();
    }
    for row in dump["chunks"].as_array().unwrap() {
        let cols = [
            "id",
            "file",
            "chunk_no",
            "body",
            "content_hash",
            "version",
            "direct",
            "kind",
            "meta",
        ];
        conn.execute(
            "INSERT INTO chunks (id, file, chunk_no, body, content_hash, version, direct, kind, meta) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params_from_iter(cols.iter().map(|c| cell(&row[*c]))),
        )
        .unwrap();
    }
    for row in dump["vec_meta"].as_array().unwrap() {
        conn.execute(
            "INSERT INTO vec_meta (key, value) VALUES (?, ?)",
            rusqlite::params![row["key"].as_str().unwrap(), row["value"].as_str()],
        )
        .unwrap();
    }
    for (table, rows) in dump["vectors"].as_object().unwrap() {
        for r in rows.as_array().unwrap() {
            let blob: Vec<u8> = r["embedding"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|x| (x.as_f64().unwrap() as f32).to_le_bytes())
                .collect();
            let id = r["chunk_id"].as_i64().unwrap();
            conn.execute(
                &format!("INSERT INTO {table} (chunk_id, embedding) VALUES ({id}, ?)"),
                [blob],
            )
            .unwrap();
        }
    }
    for (table, rows) in dump["maps"].as_object().unwrap() {
        for r in rows.as_array().unwrap() {
            conn.execute(
                &format!("INSERT INTO {table} (chunk_id, content_hash) VALUES (?, ?)"),
                rusqlite::params![
                    r["chunk_id"].as_i64().unwrap(),
                    r["content_hash"].as_str().unwrap()
                ],
            )
            .unwrap();
        }
    }
}

struct World {
    dir: tempfile::TempDir,
    query: Vec<f32>,
    want: Vec<(String, String)>,
    target: Value,
}

/// A data directory holding S1's index for project `qa-proj-1` and a saved embedding configuration
/// whose active target is S1's, answering the S1 search `note`.
fn world_for(note: &str) -> World {
    world_in("S1-map-slot", note)
}

fn world_in(scenario: &str, note: &str) -> World {
    let f = fixture();
    let q = search_of(&f, scenario, note);
    let dir = tempfile::tempdir().unwrap();
    rebuild(
        &dir.path()
            .join("users")
            .join(USER)
            .join("rag")
            .join("qa-proj-1.db"),
        &f["snapshots"][q["index"].as_str().unwrap()],
    );
    let target = q["target"].clone();
    std::fs::write(
        dir.path().join("embedding-config.json"),
        json!({"version": 1, "active": target, "next": null, "previous": null}).to_string(),
    )
    .unwrap();
    World {
        query: q["query_vector"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_f64().unwrap() as f32)
            .collect(),
        want: q["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                (
                    h["file"].as_str().unwrap().to_owned(),
                    h["body"].as_str().unwrap().to_owned(),
                )
            })
            .collect(),
        target,
        dir,
    }
}

fn world() -> World {
    world_for("mixed files")
}

fn never() -> bool {
    false
}

fn inputs<'a>(w: &'a World, env: &'a HashMap<String, String>) -> Inputs<'a> {
    Inputs {
        data_dir: w.dir.path(),
        user: USER,
        project: "qa-proj-1",
        question: "alpha bravo",
        start: START,
        now_ms: NOW,
        maintenance_held: &never,
        rerank_env: Some(env),
        versions: Versions::new(),
        limit: RESEARCH_HITS,
        until: None,
    }
}

async fn run(w: &World, inputs: Inputs<'_>, calls: &AtomicUsize) -> Outcome {
    run_seen(w, inputs, calls, &Mutex::new(None)).await
}

async fn run_seen(
    w: &World,
    inputs: Inputs<'_>,
    calls: &AtomicUsize,
    observed: &Mutex<Option<Observation>>,
) -> Outcome {
    let query = w.query.clone();
    let question = inputs.question.to_owned();
    let (bench, knobs) = (Mutex::new(Chain::default()), HashMap::new());
    retrieve_with(
        &deps(observed, &bench, &knobs),
        inputs,
        None,
        |target, framed| {
            calls.fetch_add(1, Ordering::SeqCst);
            // The search holds the foreground (`foreground()`), so native passes yield to it.
            assert!(embedding_worker::foreground::busy());
            assert_eq!(target.model, "qa-embed-map");
            // The query framing Node sent (embeddinggemma-2, kind query).
            assert_eq!(
                String::from_utf16_lossy(&framed),
                format!("task: search result | query: {question}")
            );
            async move { Ok(query) }
        },
        no_reranker,
    )
    .await
    .text()
}

fn deps<'a>(
    observed: &'a Mutex<Option<Observation>>,
    bench: &'a Mutex<Chain>,
    knobs: &'a HashMap<String, String>,
) -> Deps<'a> {
    Deps {
        seed: None,
        service_url: Some("http://127.0.0.1:9"),
        observed,
        bench,
        clock: &CLOCK,
        knobs,
    }
}

fn fixed_clock() -> f64 {
    NOW
}
static CLOCK: fn() -> f64 = fixed_clock;

async fn no_reranker(_: rerank::Request, _: Option<String>) -> Option<(u16, Value)> {
    panic!("the reranker is off")
}

fn pairs(o: Outcome) -> Vec<(String, String)> {
    match o {
        Outcome::Hits(h) => h.into_iter().map(|h| (h.file, h.body)).collect(),
        Outcome::Defer(why) => panic!("deferred: {why}"),
    }
}

#[tokio::test]
async fn answers_nodes_hits_cut_to_the_limit() {
    let w = world_for("ties between the two duplicate files");
    assert_eq!(w.want.len(), TOP_K, "the recorded search has a full pool");
    let env = HashMap::new();
    let calls = AtomicUsize::new(0);
    let mut i = inputs(&w, &env);
    i.question = "echo echo echo";
    assert_eq!(pairs(run(&w, i, &calls).await), w.want[..RESEARCH_HITS]);
    let mut i = inputs(&w, &env);
    i.question = "echo echo echo";
    i.limit = TOP_K;
    assert_eq!(pairs(run(&w, i, &calls).await), w.want);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn wanted_versions_filter_as_in_node() {
    let w = world();
    let env = HashMap::new();
    let calls = AtomicUsize::new(0);
    let mut i = inputs(&w, &env);
    i.versions = [(
        "qa-long-a.md".to_owned(),
        Some("qa-other-version".to_owned()),
    )]
    .into();
    let got = pairs(run(&w, i, &calls).await);
    assert!(!w.want.is_empty() && w.want.iter().all(|h| h.0 == "qa-long-a.md"));
    assert!(got.is_empty(), "every hit was of the file being re-indexed");
}

#[tokio::test]
async fn defers_while_the_reranker_settings_are_unreadable() {
    let w = world();
    let calls = AtomicUsize::new(0);
    let env = HashMap::new();
    let mut i = inputs(&w, &env);
    i.rerank_env = None;
    assert_eq!(
        run(&w, i, &calls).await,
        Outcome::Defer("reranker settings unavailable")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// With the reranker on, the cosine pool is `RAG_RERANK_POOL` wide and the reranker's order picks
/// `RAG_RERANK_KEEP` hits; a failed answer is the cosine top six; three deadline misses bench it.
#[tokio::test]
async fn the_reranker_picks_the_kept_hits_and_falls_back_to_cosine() {
    let w = world_for("ties between the two duplicate files");
    let env: HashMap<String, String> = [
        ("NOEVIA_FEATURE_RAG_RERANK", "1"),
        ("RERANK_BASE_URL", "http://qa-rerank:8080"),
        ("RERANK_BACKEND", "tei"),
        ("RERANK_API_KEY", "qa-key"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let knobs: HashMap<String, String> = [
        ("RAG_RERANK_POOL", "8"),
        ("RAG_RERANK_KEEP", "3"),
        ("RAG_RERANK_DEADLINE_MS", "100"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let (observed, bench) = (Mutex::new(None), Mutex::new(Chain::default()));
    let search = |answer: Option<Value>, hang: bool| {
        let (query, observed, bench, knobs, env) =
            (w.query.clone(), &observed, &bench, &knobs, &env);
        let i = Inputs {
            question: "echo echo echo",
            limit: TOP_K,
            ..inputs(&w, env)
        };
        async move {
            let sent = std::sync::Mutex::new(Vec::new());
            let out = retrieve_with(
                &deps(observed, bench, knobs),
                i,
                None,
                |_, _| async move { Ok(query) },
                |request: rerank::Request, bearer: Option<String>| {
                    sent.lock().unwrap().push((request, bearer));
                    async move {
                        if hang {
                            std::future::pending::<()>().await;
                        }
                        answer.map(|a| (200, a))
                    }
                },
            )
            .await
            .text();
            (pairs(out), sent.into_inner().unwrap())
        }
    };
    // Eight cosine hits go to the reranker, which reverses them; three are kept.
    let reverse = json!((0..8)
        .map(|i| json!({"index": i, "score": i}))
        .collect::<Vec<_>>());
    let (got, sent) = search(Some(reverse), false).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0.url, "http://qa-rerank:8080/rerank");
    assert_eq!(sent[0].1.as_deref(), Some("qa-key"));
    let texts: Value = serde_json::from_str(&sent[0].0.body).unwrap();
    assert_eq!(
        texts["texts"].as_array().unwrap().len(),
        8,
        "the pool is RAG_RERANK_POOL wide"
    );
    let pool: Vec<String> = texts["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(got.len(), 3);
    assert_eq!(
        got.iter().map(|g| g.1.clone()).collect::<Vec<_>>(),
        pool.iter().rev().take(3).cloned().collect::<Vec<_>>()
    );
    // A partial answer: the cosine top six, Node's recorded hits.
    let (got, _) = search(Some(json!([{"index": 0, "score": 1}])), false).await;
    assert_eq!(got, w.want);
    // Three answers past the deadline bench the reranker: the fourth search sends nothing.
    for _ in 0..3 {
        let (got, sent) = search(None, true).await;
        assert_eq!((got, sent.len()), (w.want.clone(), 1));
    }
    let (got, sent) = search(None, true).await;
    assert_eq!((got, sent.len()), (w.want.clone(), 0));
}

/// `RAG_RERANK_KEEP` above six: Node's `searchProject` answers all the kept hits (only research cuts).
#[tokio::test]
async fn kept_hits_are_not_cut_to_six() {
    let w = world_for("ties between the two duplicate files");
    let env: HashMap<String, String> = [
        ("NOEVIA_FEATURE_RAG_RERANK", "1"),
        ("RERANK_BASE_URL", "http://qa-rerank:8080"),
        ("RERANK_BACKEND", "tei"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let knobs: HashMap<String, String> = [("RAG_RERANK_POOL", "12"), ("RAG_RERANK_KEEP", "8")]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let (observed, bench) = (Mutex::new(None), Mutex::new(Chain::default()));
    let query = w.query.clone();
    let i = Inputs {
        question: "echo echo echo",
        limit: usize::MAX,
        ..inputs(&w, &env)
    };
    let got = retrieve_with(
        &deps(&observed, &bench, &knobs),
        i,
        None,
        |_, _| async move { Ok(query) },
        |request: rerank::Request, _| async move {
            let n = serde_json::from_str::<Value>(&request.body).unwrap()["texts"]
                .as_array()
                .unwrap()
                .len();
            Some((
                200,
                json!((0..n)
                    .map(|i| json!({"index": i, "score": i}))
                    .collect::<Vec<_>>()),
            ))
        },
    )
    .await
    .text();
    let got = pairs(got);
    assert_eq!(got.len(), 8);
}

#[tokio::test]
async fn defers_for_a_shared_engine_target_and_an_unreadable_configuration() {
    let w = world();
    let env = HashMap::new();
    let calls = AtomicUsize::new(0);
    let mut shared = w.target.clone();
    shared["baseUrl"] = Value::Null;
    std::fs::write(
        w.dir.path().join("embedding-config.json"),
        json!({"version": 1, "active": shared}).to_string(),
    )
    .unwrap();
    assert_eq!(
        run(&w, inputs(&w, &env), &calls).await,
        Outcome::Defer("shared engine target")
    );
    std::fs::write(w.dir.path().join("embedding-config.json"), "{damaged").unwrap();
    assert_eq!(
        run(&w, inputs(&w, &env), &calls).await,
        Outcome::Defer("embedding configuration unavailable")
    );
    std::fs::remove_file(w.dir.path().join("embedding-config.json")).unwrap();
    // No saved configuration and no usable environment seed: refused, not guessed.
    assert_eq!(
        run(&w, inputs(&w, &env), &calls).await,
        Outcome::Defer("embedding configuration unavailable")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn no_hits_where_node_has_none_and_nothing_is_embedded() {
    let w = world();
    let env = HashMap::new();
    let calls = AtomicUsize::new(0);
    // No index file for this project: nothing created (#1776).
    let mut i = inputs(&w, &env);
    i.project = "qa-proj-missing";
    assert_eq!(run(&w, i, &calls).await, Outcome::Hits(Vec::new()));
    assert!(!w
        .dir
        .path()
        .join("users")
        .join(USER)
        .join("rag")
        .join("qa-proj-missing.db")
        .exists());
    // A project id that is a path, an account id that is not one: real indexes sit where they would
    // lead, and are not read.
    let real = w
        .dir
        .path()
        .join("users")
        .join(USER)
        .join("rag")
        .join("qa-proj-1.db");
    std::fs::copy(
        &real,
        w.dir.path().join("users").join(USER).join("qa-proj-1.db"),
    )
    .unwrap();
    let mut i = inputs(&w, &env);
    i.project = "../qa-proj-1";
    assert_eq!(run(&w, i, &calls).await, Outcome::Hits(Vec::new()));
    let odd = w
        .dir
        .path()
        .join("users")
        .join("qa-not-an-account")
        .join("rag");
    std::fs::create_dir_all(&odd).unwrap();
    std::fs::copy(&real, odd.join("qa-proj-1.db")).unwrap();
    let mut i = inputs(&w, &env);
    i.user = "qa-not-an-account";
    assert_eq!(run(&w, i, &calls).await, Outcome::Hits(Vec::new()));
    // The maintenance lease is held: Node's gate refuses the query embedding.
    let held = || true;
    let mut i = inputs(&w, &env);
    i.maintenance_held = &held;
    assert_eq!(run(&w, i, &calls).await, Outcome::Hits(Vec::new()));
    // The embedding service the target is on is turned off (the worker's current snapshot says so).
    std::fs::write(
        w.dir.path().join("embedding-worker-status.json"),
        json!({"protocol": "rust-embedding-worker-v1", "start": START, "at": NOW - 1000.0,
               "service": {"managed": true, "enabled": false}, "serviceAt": NOW - 2000.0})
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        run(&w, inputs(&w, &env), &calls).await,
        Outcome::Hits(Vec::new())
    );
    // The same snapshot from another start is not this process's: the search runs.
    std::fs::write(
        w.dir.path().join("embedding-worker-status.json"),
        json!({"protocol": "rust-embedding-worker-v1", "start": "qa-start-old", "at": NOW - 1000.0,
               "service": {"managed": true, "enabled": false}, "serviceAt": NOW - 2000.0})
        .to_string(),
    )
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(pairs(run(&w, inputs(&w, &env), &calls).await), w.want);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_embedding_failure_or_a_wrong_width_is_no_hits() {
    let w = world();
    let env = HashMap::new();
    let seen = Mutex::new(None);
    let (bench, knobs) = (Mutex::new(Chain::default()), HashMap::new());
    let failed = retrieve_with(
        &deps(&seen, &bench, &knobs),
        inputs(&w, &env),
        None,
        |_, _| async { Err("embeddings 500".to_owned()) },
        no_reranker,
    )
    .await
    .text();
    assert_eq!(failed, Outcome::Hits(Vec::new()));
    let wide = w.query.iter().copied().chain([0.5]).collect::<Vec<f32>>();
    let wrong = retrieve_with(
        &deps(&seen, &bench, &knobs),
        inputs(&w, &env),
        None,
        |_, _| async move { Ok(wide) },
        no_reranker,
    )
    .await
    .text();
    assert_eq!(wrong, Outcome::Hits(Vec::new()));
}

#[test]
fn modes_and_the_shadow_line() {
    assert_eq!(Mode::parse(None), Ok(Mode::Off));
    assert_eq!(Mode::parse(Some(" ")), Ok(Mode::Off));
    assert_eq!(Mode::parse(Some("off")), Ok(Mode::Off));
    assert_eq!(Mode::parse(Some("shadow")), Ok(Mode::Shadow));
    assert_eq!(Mode::parse(Some("on")), Ok(Mode::On));
    assert_eq!(Mode::parse(Some(" Shadow ")), Ok(Mode::Shadow));
    assert_eq!(Mode::parse(Some("OFF")), Ok(Mode::Off));
    assert!(Mode::parse(Some("1")).is_err());
    let a = ("a".to_owned(), "x".to_owned());
    let b = ("b".to_owned(), "y".to_owned());
    assert_eq!(
        compare(&[a.clone(), b.clone()], &[a.clone(), b.clone()]),
        "match (2 hits)"
    );
    assert_eq!(
        compare(&[a.clone(), b.clone()], &[b.clone(), a.clone()]),
        "differs (node 2 hits, native 2 hits, first difference at 0)"
    );
    assert_eq!(
        compare(std::slice::from_ref(&a), &[a.clone(), b]),
        "differs (node 1 hits, native 2 hits, first difference at 1)"
    );
    assert_eq!(compare(&[], &[]), "match (0 hits)");
}

fn status(w: &World, service: Value, service_at: f64, at: f64) {
    std::fs::write(
        w.dir.path().join("embedding-worker-status.json"),
        json!({"protocol": "rust-embedding-worker-v1", "start": START, "at": at,
               "service": service, "serviceAt": service_at})
        .to_string(),
    )
    .unwrap();
}

/// Node keeps its last observation of the service: when the worker's snapshot stops coming, the
/// observation goes stale after 15 s and the service counts as off (no hits), until a new one.
#[tokio::test]
async fn the_last_observation_is_kept_and_goes_stale() {
    let w = world();
    let env = HashMap::new();
    let calls = AtomicUsize::new(0);
    let observed = Mutex::new(None);
    status(
        &w,
        json!({"managed": true, "enabled": true}),
        NOW - 1000.0,
        NOW - 500.0,
    );
    assert_eq!(
        pairs(run_seen(&w, inputs(&w, &env), &calls, &observed).await),
        w.want
    );
    std::fs::remove_file(w.dir.path().join("embedding-worker-status.json")).unwrap();
    let mut i = inputs(&w, &env);
    i.now_ms = NOW + 13_000.0;
    assert_eq!(
        pairs(run_seen(&w, i, &calls, &observed).await),
        w.want,
        "14 s old: still enabled"
    );
    let mut i = inputs(&w, &env);
    i.now_ms = NOW + 15_000.0;
    assert_eq!(
        run_seen(&w, i, &calls, &observed).await,
        Outcome::Hits(Vec::new()),
        "stale: off"
    );
    status(
        &w,
        json!({"managed": true, "enabled": true}),
        NOW + 15_500.0,
        NOW + 15_500.0,
    );
    let mut i = inputs(&w, &env);
    i.now_ms = NOW + 16_000.0;
    assert_eq!(pairs(run_seen(&w, i, &calls, &observed).await), w.want);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

/// A maintenance lease taken while the query was being embedded: Node's gate would have refused a
/// request started now, so the hits are dropped.
#[tokio::test]
async fn a_lease_taken_during_the_embed_drops_the_hits() {
    let w = world();
    let env = HashMap::new();
    let asked = AtomicUsize::new(0);
    let held = || asked.fetch_add(1, Ordering::SeqCst) > 0;
    let mut i = inputs(&w, &env);
    i.maintenance_held = &held;
    let calls = AtomicUsize::new(0);
    assert_eq!(run(&w, i, &calls).await, Outcome::Hits(Vec::new()));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the first check let the embed run"
    );
    assert_eq!(asked.load(Ordering::SeqCst), 2);
}

/// S8, recorded from Node's `searchProject` with the reranker on (TEI, `RAG_RERANK_KEEP` 4): the
/// cosine pool of `RAG_RERANK_POOL` (12; LIMIT 48), the reranker's order, four kept; a failing
/// reranker leaves the cosine top six. The native search must give Node's hits exactly.
#[tokio::test]
async fn the_reranker_matches_nodes_search_project() {
    let f = fixture();
    let s8 = f["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "S8-rerank")
        .unwrap()
        .clone();
    for q in s8["searches"].as_array().unwrap() {
        let note = q["note"].as_str().unwrap();
        let w = world_in("S8-rerank", note);
        let rr = &q["rerank"];
        let env: HashMap<String, String> = rr["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        let knobs: HashMap<String, String> = rr["knobs"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        let reverse = rr["answer"] == "reverse";
        let pool = rr["pool"].as_u64().unwrap() as usize;
        let (observed, bench) = (Mutex::new(None), Mutex::new(Chain::default()));
        let query = w.query.clone();
        let i = Inputs {
            question: "echo echo echo",
            limit: usize::MAX,
            ..inputs(&w, &env)
        };
        let got = retrieve_with(
            &deps(&observed, &bench, &knobs),
            i,
            None,
            |_, _| async move { Ok(query) },
            |request: rerank::Request, _| async move {
                let n = serde_json::from_str::<Value>(&request.body).unwrap()["texts"]
                    .as_array()
                    .unwrap()
                    .len();
                assert_eq!(n, pool, "the pool Node sent");
                Some(if reverse {
                    (
                        200,
                        json!((0..n)
                            .map(|i| json!({"index": i, "score": i}))
                            .collect::<Vec<_>>()),
                    )
                } else {
                    (500, json!({}))
                })
            },
        )
        .await;
        let Outcome::Hits(found) = got else {
            panic!("{note}: deferred")
        };
        let got: Vec<(String, String, f64)> = found
            .text
            .into_iter()
            .map(|h| (h.file, h.body, h.score))
            .collect();
        let want: Vec<(String, String, f64)> = q["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                (
                    h["file"].as_str().unwrap().to_owned(),
                    h["body"].as_str().unwrap().to_owned(),
                    h["score"].as_f64().unwrap(),
                )
            })
            .collect();
        assert_eq!(got, want, "{note}");
    }
}
