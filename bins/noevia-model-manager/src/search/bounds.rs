#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! What the native side does that Python does not (no oracle): what it refuses, the bounds it
//! keeps, its caches and its admission. Every name is synthetic.
use super::tests::{
    child, closed_port, fixture, hex, hub_gauged, store, Answers, Gauge, Script, Seen,
};
use super::*;
use serde_json::{json, Value as J};
use std::sync::atomic::Ordering;

const GB: u64 = 1_000_000_000;
const MODELS: &str = "/api/models?limit=30&sort=trendingScore&direction=-1&filter=gguf";
const RUNTIME: &str =
    r#"{"budget":"24.0","downloaded":{},"quantisers":["quant-one"],"labs":["lab-one"]}"#;

fn fixed() -> f64 {
    1_800_000_000.5
}

fn tree(repo: &str) -> String {
    format!("/api/models/{repo}/tree/main?recursive=true&expand=true")
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A recorded answer for the fake Hub.
fn ans(status: u16, body: &[u8]) -> J {
    json!({"status": status, "headers": [["content-type", "application/json"]], "body": to_hex(body)})
}

fn ok(body: &J) -> J {
    ans(200, body.to_string().as_bytes())
}

/// `n` zero bytes after `body`, without holding them in the fixture.
fn padded(mut answer: J, zeros: usize) -> J {
    answer["zeros"] = json!(zeros);
    answer
}

fn item(id: &str) -> J {
    json!({"id": id, "downloads": 10, "likes": 1, "lastModified": "2026-01-10T00:00:00.000Z",
           "tags": ["gguf"]})
}

fn file(path: &str, size: u64) -> J {
    json!({"type": "file", "path": path, "size": 100, "lfs": {"size": size}})
}

struct Rig {
    search: Arc<Search>,
    seen: Seen,
    script: Script,
    answers: Answers,
    gauge: Arc<Gauge>,
    dir: tempfile::TempDir,
    hub_base: String,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn rig_with(answers: J, child_down: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    store(dir.path(), &fixture()["schema"], &J::Null);
    let answers: Answers = Arc::new(Mutex::new(answers));
    let (hub_base, seen, hub_task, gauge) = hub_gauged(answers.clone(), String::new()).await;
    let (child_base, script, child_task) = child().await;
    script.lock().unwrap().search = Some(json!({"status": 200, "body": RUNTIME}));
    let search = Search::with(
        dir.path(),
        Hub {
            api: format!("{hub_base}/api"),
            resolve: hub_base.clone(),
        },
        &if child_down {
            closed_port()
        } else {
            child_base
        },
        "synthetic-child-token-0123456789abcdef",
        fixed,
        Timeouts {
            search: Duration::from_secs(5),
            tree: Duration::from_secs(5),
            header: Duration::from_secs(5),
            avatar: Duration::from_secs(5),
        },
    )
    .unwrap();
    Rig {
        search: Arc::new(search),
        seen,
        script,
        answers,
        gauge,
        dir,
        hub_base,
        tasks: vec![hub_task, child_task],
    }
}

async fn rig(answers: J) -> Rig {
    rig_with(answers, false).await
}

/// A hub that lists one model, `lab-one/m-GGUF`, with the given tree entries.
fn one_model(entries: &[J]) -> J {
    json!({MODELS: ok(&json!([item("lab-one/m-GGUF")])),
           tree("lab-one/m-GGUF"): ok(&J::Array(entries.to_vec()))})
}

const ALL: &str = "trustedOnly=false&showUnsuitable=true";

fn body_of(outcome: Outcome) -> String {
    match outcome {
        Outcome::Json(200, body) => String::from_utf8(body).unwrap(),
        other => panic!("{other:?}"),
    }
}

// ---- what is refused ------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_outside_printable_ascii_is_refused_where_pythons_patterns_apply() {
    // A file name.
    let r = rig(one_model(&[file("\u{e9}-Q4_K_M.gguf", GB)])).await;
    assert_eq!(r.search.search(ALL).await, Outcome::Refused);
    // A control character in a file name.
    let r = rig(one_model(&[file("a\u{1}-Q4_K_M.gguf", GB)])).await;
    assert_eq!(r.search.search(ALL).await, Outcome::Refused);
    // A repository id (it is not asked of the Hub, so the judged repository has no files).
    let r = rig(json!({MODELS: ok(&json!([item("lab-one/\u{fc}-GGUF")]))})).await;
    assert_eq!(r.search.search(ALL).await, Outcome::Refused);
    // The same names are fine when the route does not apply a pattern: a repository listing.
    let hub = json!({tree("lab-one/m-GGUF"): ok(&json!([file("\u{4e2d}-Q4_K_M.gguf", GB)]))});
    let r = rig(hub).await;
    r.script.lock().unwrap().repo = Some(json!({
        "request": {"groups": [{"bytes": GB, "estimate": false}], "mmprojBytes": null, "summary": null},
        "status": 200, "body": r#"{"groups":[{"fit":[]}]}"#}));
    let body = body_of(r.search.repo("repo=lab-one%2Fm-GGUF").await);
    assert!(body.contains("\u{4e2d}-Q4_K_M.gguf"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn numbers_and_fields_this_side_does_not_follow_are_refused() {
    for downloads in [
        json!(1e30),
        json!("99999999999999999999"),
        serde_json::from_str::<J>("1180591620717411303424").unwrap(),
        json!(-5e18),
    ] {
        let mut odd = item("lab-one/m-GGUF");
        odd["downloads"] = downloads.clone();
        let r = rig(json!({MODELS: ok(&json!([odd]))})).await;
        assert_eq!(r.search.search(ALL).await, Outcome::Refused, "{downloads}");
    }
    for (field, value) in [
        ("lastModified", json!([1])),
        ("lastModified", json!({"a": 1})),
        ("tags", json!(["gguf", [1]])),
        ("tags", json!(["gguf", {"a": 1}])),
    ] {
        let mut odd = item("lab-one/m-GGUF");
        odd[field] = value.clone();
        let hub = json!({MODELS: ok(&json!([odd])),
                         tree("lab-one/m-GGUF"): ok(&json!([file("m-Q4_K_M.gguf", GB)]))});
        let r = rig(hub).await;
        assert_eq!(
            r.search.search(ALL).await,
            Outcome::Refused,
            "{field} {value}"
        );
    }
    // A size past 62 bits.
    let r = rig(one_model(&[file("m-Q4_K_M.gguf", 1 << 62)])).await;
    assert_eq!(r.search.search(ALL).await, Outcome::Refused);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_page_past_a_bound_is_refused_for_the_search_and_the_repository() {
    let nodes = format!("[{}1]", "1,".repeat(300_000));
    let deep = format!("{}{}", "[".repeat(600), "]".repeat(600));
    let long = format!("[\"{}\"]", "x".repeat(70_000));
    let pages = [
        padded(ans(200, b""), 17 * 1024 * 1024),
        ans(200, nodes.as_bytes()),
        ans(200, deep.as_bytes()),
        ans(200, long.as_bytes()),
        // The surrogate Python's strings can hold and Rust's cannot.
        ans(200, br#"["\ud800"]"#),
    ];
    for page in pages {
        // The model search itself.
        let r = rig(json!({MODELS: page.clone()})).await;
        assert_eq!(r.search.search(ALL).await, Outcome::Refused);
        // One repository's listing.
        let r = rig(json!({tree("lab-one/m-GGUF"): page.clone()})).await;
        assert_eq!(
            r.search.repo("repo=lab-one%2Fm-GGUF").await,
            Outcome::Refused
        );
        // A listing inside the search is a failed lookup: the repository has no files, and the
        // failure is not cached.
        let r = rig(json!({MODELS: ok(&json!([item("lab-one/m-GGUF")])),
                           tree("lab-one/m-GGUF"): page.clone()}))
        .await;
        let body = body_of(r.search.search(ALL).await);
        assert!(
            body.contains("no GGUF model files in this repository"),
            "{body}"
        );
        let before = r.seen.lock().unwrap().len();
        body_of(r.search.search(ALL).await);
        assert!(r.seen.lock().unwrap().len() > before + 1, "listed again");
    }
    // A page that is not JSON at all is Python's 500, in both routes.
    let r = rig(json!({MODELS: ans(200, b"{")})).await;
    assert_eq!(r.search.search(ALL).await, Outcome::Failed);
    let r = rig(json!({tree("lab-one/m-GGUF"): ans(200, b"{")})).await;
    assert_eq!(
        r.search.repo("repo=lab-one%2Fm-GGUF").await,
        Outcome::Failed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_next_link_to_another_origin_is_not_followed() {
    let other: Answers = Arc::new(Mutex::new(json!({"/leak": ans(200, b"[]")})));
    let (other_base, other_seen, other_task, _) = hub_gauged(other, String::new()).await;
    let link = json!([["link", format!("<{other_base}/leak>; rel=\"next\"")]]);
    let mut first = ans(
        200,
        json!([file("m-Q4_K_M.gguf", GB)]).to_string().as_bytes(),
    );
    first["headers"]
        .as_array_mut()
        .unwrap()
        .extend(link.as_array().unwrap().clone());
    let hub = json!({MODELS: ok(&json!([item("lab-one/m-GGUF")])),
                     tree("lab-one/m-GGUF"): first});
    let mut r = rig(hub).await;
    r.tasks.push(other_task);
    // In the repository route the listing is refused; in the search it is a failed lookup.
    assert_eq!(
        r.search.repo("repo=lab-one%2Fm-GGUF").await,
        Outcome::Refused
    );
    let body = body_of(r.search.search(ALL).await);
    assert!(body.contains("no GGUF model files"), "{body}");
    assert!(
        other_seen.lock().unwrap().is_empty(),
        "the token's origin only"
    );
}

// ---- the Python child -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_childs_failures_are_a_502_or_pythons_500() {
    let hub = one_model(&[file("m-Q4_K_M.gguf", GB)]);
    // Down.
    let r = rig_with(hub.clone(), true).await;
    assert_eq!(r.search.search(ALL).await, Outcome::Unavailable);
    // Its own 500 is Python's 500; anything else it says is a 502.
    for (status, body, expected) in [
        (500, "", Outcome::Failed),
        (404, "", Outcome::Unavailable),
        (503, RUNTIME, Outcome::Unavailable),
        (200, "not json", Outcome::Unavailable),
        (200, "{}", Outcome::Unavailable),
        (
            200,
            r#"{"budget":"x","downloaded":{},"quantisers":[],"labs":[]}"#,
            Outcome::Unavailable,
        ),
        (
            200,
            r#"{"budget":"1","downloaded":[],"quantisers":[],"labs":[]}"#,
            Outcome::Unavailable,
        ),
        (
            200,
            r#"{"budget":1,"downloaded":{},"quantisers":[],"labs":[]}"#,
            Outcome::Unavailable,
        ),
    ] {
        let r = rig(hub.clone()).await;
        r.script.lock().unwrap().search = Some(json!({"status": status, "body": body}));
        assert_eq!(r.search.search(ALL).await, expected, "{status} {body}");
    }
    // A figure Python prints for a non-finite budget.
    for budget in ["nan", "inf", "-inf", "1e+16"] {
        let r = rig(hub.clone()).await;
        let body = RUNTIME.replace("24.0", budget);
        r.script.lock().unwrap().search = Some(json!({"status": 200, "body": body}));
        body_of(r.search.search(ALL).await);
    }
}

fn repo_script(r: &Rig, status: u64, body: &str) {
    r.script.lock().unwrap().repo = Some(json!({
        "request": {"groups": [{"bytes": GB, "estimate": false}], "mmprojBytes": null, "summary": null},
        "status": status, "body": body}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_repository_runtime_answer_must_fit_the_groups() {
    let hub = json!({tree("lab-one/m-GGUF"): ok(&json!([file("m-Q4_K_M.gguf", GB)]))});
    for (status, body, expected) in [
        (500, "", Outcome::Failed),
        (404, "", Outcome::Unavailable),
        (200, "[]", Outcome::Unavailable),
        (200, "{}", Outcome::Unavailable),
        (200, r#"{"groups":[]}"#, Outcome::Unavailable),
        (200, r#"{"groups":[{}]}"#, Outcome::Unavailable),
        (
            200,
            r#"{"groups":[{"fit":[]},{"fit":[]}]}"#,
            Outcome::Unavailable,
        ),
        (
            200,
            r#"{"groups":[{"fit":[],"reason":5}]}"#,
            Outcome::Unavailable,
        ),
    ] {
        let r = rig(hub.clone()).await;
        repo_script(&r, status, body);
        assert_eq!(
            r.search.repo("repo=lab-one%2Fm-GGUF").await,
            expected,
            "{status} {body}"
        );
    }
    let r = rig(hub).await;
    repo_script(&r, 200, r#"{"groups":[{"fit":[{"verdict":"fits"}]}]}"#);
    let body = body_of(r.search.repo("repo=lab-one%2Fm-GGUF").await);
    assert!(body.contains(r#""fit":[{"verdict":"fits"}]"#), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repository_without_groups_does_not_ask_the_child() {
    let r = rig(json!({tree("lab-one/m-GGUF"): ok(&json!([]))})).await;
    let body = body_of(r.search.repo("repo=lab-one%2Fm-GGUF").await);
    assert_eq!(body, r#"{"repo":"lab-one/m-GGUF","groups":[],"gated":""}"#);
    assert!(r.script.lock().unwrap().calls.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn more_groups_than_the_child_takes_are_refused() {
    let entries: Vec<J> = (0..4097)
        .map(|i| file(&format!("m{i}-Q4_K_M.gguf"), GB))
        .collect();
    let r = rig(json!({tree("lab-one/m-GGUF"): ok(&J::Array(entries))})).await;
    assert_eq!(
        r.search.repo("repo=lab-one%2Fm-GGUF").await,
        Outcome::Refused
    );
    assert!(r.script.lock().unwrap().calls.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_store_is_pythons_500() {
    let r = rig(one_model(&[file("m-Q4_K_M.gguf", GB)])).await;
    std::fs::remove_file(r.dir.path().join("model_loader.db")).unwrap();
    assert_eq!(r.search.search(ALL).await, Outcome::Failed);
    assert_eq!(
        r.search.repo("repo=lab-one%2Fm-GGUF").await,
        Outcome::Failed
    );
}

// ---- the remote header ------------------------------------------------------------------------

fn gguf(extra: &[(&str, u8, Vec<u8>)]) -> Vec<u8> {
    let mut entries: Vec<(String, u32, Vec<u8>)> = vec![(
        "general.architecture".into(),
        8,
        [&4u64.to_le_bytes()[..], b"arch"].concat(),
    )];
    for (key, kind, value) in extra {
        entries.push(((*key).into(), u32::from(*kind), value.clone()));
    }
    let mut out = b"GGUF".to_vec();
    out.extend(3u32.to_le_bytes());
    out.extend(0u64.to_le_bytes());
    out.extend((entries.len() as u64).to_le_bytes());
    for (key, kind, value) in entries {
        out.extend((key.len() as u64).to_le_bytes());
        out.extend(key.as_bytes());
        out.extend(kind.to_le_bytes());
        out.extend(value);
    }
    out
}

/// `gguf_meta._finite`: a header float that is NaN or infinite is `None` in the summary, so the
/// summary the child is handed has none (and the guard against one is exercised on its own).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_finite_header_float_is_null_in_the_summary_and_a_summary_holds_none() {
    let nan = gguf(&[("arch.rope.freq_base", 6, f32::NAN.to_le_bytes().to_vec())]);
    let hub = json!({tree("lab-one/m-GGUF"): ok(&json!([file("a-Q4_K_M.gguf", GB)])),
                     "/lab-one/m-GGUF/resolve/main/a-Q4_K_M.gguf": ans(206, &nan)});
    let r = rig(hub).await;
    // (No child script: the route ends at the child's 500, after the header was read.)
    assert_eq!(
        r.search.repo("repo=lab-one%2Fm-GGUF").await,
        Outcome::Failed
    );
    let cache = lock(&r.search.headers);
    let summary = cache.map.values().next().unwrap().as_ref().unwrap();
    assert!(summary.contains(r#""rope_freq_base":null"#), "{summary}");
    assert!(!has_nonfinite(&gguf::Json::Float(1.5)));
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let nested = gguf::Json::object([("a", gguf::Json::Array(vec![gguf::Json::Float(bad)]))]);
        assert!(has_nonfinite(&nested));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_path_httpx_would_send_differently_is_refused_or_fails_like_httpx() {
    // A backslash: the URL parser here would read it as a slash.
    let r = rig(json!({tree("lab-one/m-GGUF"): ok(&json!([file("a\\b-Q4_K_M.gguf", GB)]))})).await;
    assert_eq!(
        r.search.repo("repo=lab-one%2Fm-GGUF").await,
        Outcome::Refused
    );
    // A control character: `httpx.InvalidURL`, not an `HTTPError`.
    let r =
        rig(json!({tree("lab-one/m-GGUF"): ok(&json!([file("a\u{1}-Q4_K_M.gguf", GB)]))})).await;
    assert_eq!(
        r.search.repo("repo=lab-one%2Fm-GGUF").await,
        Outcome::Failed
    );
}

// ---- caches ---------------------------------------------------------------------------------

fn search_only() -> Search {
    let dir = tempfile::tempdir().unwrap();
    Search::with(
        dir.path(),
        Hub::default(),
        "http://127.0.0.1:1",
        "",
        fixed,
        Timeouts::default(),
    )
    .unwrap()
}

#[test]
fn the_listing_cache_is_cleared_past_400_entries_and_past_its_byte_bound() {
    let search = search_only();
    for i in 0..=401 {
        search.remember(&format!("o/r{i}"), Arc::new(Vec::new()));
    }
    // 401 entries stand; the next insert finds more than 400 and clears first.
    assert_eq!(lock(&search.files).map.len(), 1);
    // A listing of paths over the bound is not kept at all.
    let huge = Arc::new(vec![File {
        path: "x".repeat(FILES_CACHE_BYTES + 1),
        size: 1,
    }]);
    search.remember("o/huge", huge);
    assert!(!lock(&search.files).map.contains_key("o/huge"));
    // Two that do not fit together: the older goes.
    let big = |n: &str| {
        (
            n.to_owned(),
            Arc::new(vec![File {
                path: "y".repeat(FILES_CACHE_BYTES / 2 + 1024),
                size: 1,
            }]),
        )
    };
    let (a, files) = big("o/a");
    search.remember(&a, files);
    let (b, files) = big("o/b");
    search.remember(&b, files);
    let cache = lock(&search.files);
    assert!(cache.map.contains_key("o/b") && !cache.map.contains_key("o/a"));
    assert!(cache.bytes <= FILES_CACHE_BYTES);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_header_cache_is_cleared_when_it_holds_64_and_a_success_clears_a_gate() {
    let mut answers = serde_json::Map::new();
    let header = gguf(&[("arch.context_length", 4, 4096u32.to_le_bytes().to_vec())]);
    for i in 0..70 {
        answers.insert(format!("/o/r/resolve/main/f{i}.gguf"), ans(206, &header));
    }
    answers.insert("/o/gate/resolve/main/g.gguf".into(), ans(401, b"{}"));
    let r = rig(J::Object(answers)).await;
    for i in 0..70 {
        let got = r
            .search
            .gguf_header("o/r", &format!("f{i}.gguf"))
            .await
            .unwrap();
        assert!(got.is_some());
    }
    // 64 stood; the 65th found 64, cleared, and was stored; then 5 more.
    assert_eq!(lock(&r.search.headers).map.len(), 6);
    // A gated repository is remembered; a success for the same repository forgets it.
    assert!(r
        .search
        .gguf_header("o/gate", "g.gguf")
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        lock(&r.search.headers).gated.get("o/gate").copied(),
        Some("needs a Hugging Face token (set one in Settings)")
    );
    r.answers.lock().unwrap()["/o/gate/resolve/main/h.gguf"] = ans(206, &header);
    assert!(r
        .search
        .gguf_header("o/gate", "h.gguf")
        .await
        .unwrap()
        .is_some());
    assert!(!lock(&r.search.headers).gated.contains_key("o/gate"));
}

// ---- admission --------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn at_most_two_listings_run_at_once() {
    let mut answers = serde_json::Map::new();
    let ids: Vec<String> = (0..8).map(|i| format!("lab-one/m{i}-GGUF")).collect();
    answers.insert(
        MODELS.into(),
        ok(&J::Array(ids.iter().map(|id| item(id)).collect())),
    );
    for id in &ids {
        let mut slow = ok(&json!([file("m-Q4_K_M.gguf", GB)]));
        slow["delay"] = json!(0.15);
        answers.insert(tree(id), slow);
    }
    let r = rig(J::Object(answers)).await;
    body_of(r.search.search(ALL).await);
    let peak = r.gauge.peak.load(Ordering::SeqCst);
    assert!(peak <= CONCURRENT_LOOKUPS, "{peak}");
    assert!(peak >= 1);
}

/// noevia#1998: a request that is dropped while its listing is parsed gives back neither the
/// lookup nor anything else until the parse has returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_request_keeps_its_lookup_until_the_parse_returns() {
    let r = rig(json!({tree("lab-one/m-GGUF"): ok(&json!([file("m-Q4_K_M.gguf", GB)]))})).await;
    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    *r.search.parse_hook.lock().unwrap() = Some(Arc::new(move || {
        let _ = entered_tx.send(());
        let _ = release_rx.lock().unwrap().recv();
    }));
    let search = r.search.clone();
    let request = tokio::spawn(async move { search.repo("repo=lab-one%2Fm-GGUF").await });
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.search.lookups.available_permits(), CONCURRENT_LOOKUPS - 1);
    request.abort();
    let _ = request.await;
    // The request is gone; the parse still runs and still holds its lookup.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(r.search.lookups.available_permits(), CONCURRENT_LOOKUPS - 1);
    release_tx.send(()).unwrap();
    for _ in 0..200 {
        if r.search.lookups.available_permits() == CONCURRENT_LOOKUPS {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the lookup was never given back");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_that_fail_before_the_hub_answer_make_no_hub_request_for_bad_queries() {
    let r = rig(json!({})).await;
    for query in ["limit=abc", "vision=zz", "minGb=%FF"] {
        assert!(
            matches!(r.search.search(query).await, Outcome::Json(422, _)),
            "{query}"
        );
    }
    for query in ["limit=3_0", "minParams=1_0", "limit=0-1"] {
        assert_eq!(r.search.search(query).await, Outcome::Refused, "{query}");
    }
    assert!(matches!(r.search.repo("").await, Outcome::Json(422, _)));
    assert!(r.seen.lock().unwrap().is_empty());
    assert!(r.script.lock().unwrap().calls.is_empty());
    let _ = &r.hub_base;
}

#[test]
fn hex_helper_round_trips() {
    assert_eq!(hex(&to_hex(b"\x00\xffA")), b"\x00\xffA");
}
