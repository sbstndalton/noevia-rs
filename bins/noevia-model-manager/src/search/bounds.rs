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
    rig_full(answers, child_down, Duration::from_secs(60)).await
}

async fn rig_full(answers: J, child_down: bool, listings: Duration) -> Rig {
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
            listings,
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

/// Non-ASCII names are judged (the oracle compares them with Python's); the judging of one that
/// this side cannot follow exactly would be refused with the candidate named in the log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_ascii_names_are_judged_not_refused() {
    let r = rig(one_model(&[file("\u{6a21}\u{578b}-Q4_K_M.gguf", GB)])).await;
    let body = body_of(r.search.search(ALL).await);
    assert!(body.contains("\u{6a21}\u{578b}-Q4_K_M.gguf"), "{body}");
    let r = rig(json!({MODELS: ok(&json!([item("lab-one/\u{fc}-7B-GGUF")]))})).await;
    let body = body_of(r.search.search(ALL).await);
    assert!(body.contains(r#""params":7.0"#), "{body}");
    // The repository route shows such names too.
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
        search.remember(Epoch(Some(0)), &format!("o/r{i}"), Arc::new(Vec::new()));
    }
    // 401 entries stand; the next insert finds more than 400 and clears first.
    assert_eq!(lock(&search.files).map.len(), 1);
    // A listing of paths over the bound is not kept at all.
    let huge = Arc::new(vec![File {
        path: "x".repeat(FILES_CACHE_BYTES + 1),
        size: 1,
    }]);
    search.remember(Epoch(Some(0)), "o/huge", huge);
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
    search.remember(Epoch(Some(0)), &a, files);
    let (b, files) = big("o/b");
    search.remember(Epoch(Some(0)), &b, files);
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

/// Completing a response can admit a successor before the fake server task exits.
/// Park every completed handler to expose that handoff without a scheduler race.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_hub_handlers_do_not_inflate_listing_admission() {
    let mut answers = serde_json::Map::new();
    let ids: Vec<String> = (0..8).map(|i| format!("lab-one/handoff{i}-GGUF")).collect();
    answers.insert(
        MODELS.into(),
        ok(&J::Array(ids.iter().map(|id| item(id)).collect())),
    );
    for id in &ids {
        let mut answer = ok(&json!([file("m-Q4_K_M.gguf", GB)]));
        answer["hold"] = json!(true);
        answer["hold_after_publication"] = json!(true);
        answers.insert(tree(id), answer);
    }
    let r = rig(J::Object(answers)).await;
    let search = r.search.clone();
    let request = tokio::spawn(async move { search.search(ALL).await });
    hub_asked(&r, "/tree/", CONCURRENT_LOOKUPS).await;
    // The original real admission cap is exercised, not merely a serial client.
    assert_eq!(r.gauge.live.load(Ordering::SeqCst), CONCURRENT_LOOKUPS);
    r.gauge.released.store(true, Ordering::SeqCst);
    let body = body_of(request.await.unwrap());
    // Wait for every publication hook, not for completed server tasks to return.
    tokio::time::timeout(Duration::from_secs(5), async {
        while r.gauge.published.load(Ordering::SeqCst) != ids.len() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let peak = r.gauge.peak.load(Ordering::SeqCst);
    let live = r.gauge.live.load(Ordering::SeqCst);
    // Release before assertions so a failing control also lets server tasks finish.
    r.gauge.finish_published.add_permits(ids.len());
    for id in &ids {
        assert!(body.contains(id), "missing {id}: {body}");
    }
    assert_eq!(tree_requests(&r), ids.len());
    assert!(peak <= CONCURRENT_LOOKUPS, "{peak}");
    assert!(peak >= 1);
    assert_eq!(live, 0, "completed handlers remain counted");
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

// ---- the listing deadline, the header cache bound, the token ---------------------------------

fn tree_requests(r: &Rig) -> usize {
    r.seen
        .lock()
        .unwrap()
        .iter()
        .filter(|row| row["path"].as_str().unwrap().contains("/tree/"))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_phase_past_its_deadline_counts_as_failed_listings() {
    let mut answers = serde_json::Map::new();
    let ids: Vec<String> = (0..3).map(|i| format!("lab-one/m{i}-GGUF")).collect();
    answers.insert(
        MODELS.into(),
        ok(&J::Array(ids.iter().map(|id| item(id)).collect())),
    );
    for (i, id) in ids.iter().enumerate() {
        let mut listing = ok(&json!([file("m-Q4_K_M.gguf", GB)]));
        if i > 0 {
            listing["delay"] = json!(6.0);
        }
        answers.insert(tree(id), listing);
    }
    let r = rig_full(J::Object(answers), false, Duration::from_millis(400)).await;
    let started = std::time::Instant::now();
    let body = body_of(r.search.search(ALL).await);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    // The listing that finished counts, the others are "no files" and are not cached.
    assert_eq!(
        body.matches("no GGUF model files in this repository")
            .count(),
        2,
        "{body}"
    );
    assert!(body.contains(r#""path":"m-Q4_K_M.gguf""#));
    assert_eq!(lock(&r.search.files).map.len(), 1);
    // The repository route: one listing, the same bound, and what a timeout says.
    let slow = json!({tree("lab-one/m-GGUF"): padded_delay(ok(&json!([])), 6.0)});
    let r = rig_full(slow, false, Duration::from_millis(400)).await;
    let started = std::time::Instant::now();
    let body = body_of(r.search.repo("repo=lab-one%2Fm-GGUF").await);
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(
        body,
        r#"{"repo":"lab-one/m-GGUF","error":"Network error: ","groups":[]}"#
    );
}

fn padded_delay(mut answer: J, seconds: f64) -> J {
    answer["delay"] = json!(seconds);
    answer
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failures_gates_and_refused_tokens_stay_within_the_header_cache_bound() {
    // 70 headers the Hub does not have, 70 gated ones.
    let mut answers = serde_json::Map::new();
    for i in 0..70 {
        answers.insert(format!("/o/gate{i}/resolve/main/g.gguf"), ans(401, b"{}"));
    }
    let r = rig(J::Object(answers)).await;
    for i in 0..70 {
        assert!(r
            .search
            .gguf_header("o/r", &format!("f{i}.gguf"))
            .await
            .unwrap()
            .is_none());
        assert!(lock(&r.search.headers).map.len() <= HEADER_CACHE_MAX);
    }
    // 64 stood; the 65th found 64 and cleared; then 5 more.
    assert_eq!(lock(&r.search.headers).map.len(), 6);
    for i in 0..70 {
        assert!(r
            .search
            .gguf_header(&format!("o/gate{i}"), "g.gguf")
            .await
            .unwrap()
            .is_none());
        assert!(lock(&r.search.headers).map.len() <= HEADER_CACHE_MAX);
    }
    assert_eq!(lock(&r.search.headers).gated.len(), 70);
    // A token h11 refuses is remembered as no header, within the same bound.
    let db = rusqlite::Connection::open(r.dir.path().join("model_loader.db")).unwrap();
    db.execute("INSERT INTO kv(key, value) VALUES('hf_token', 'tok ')", [])
        .unwrap();
    for i in 0..70 {
        assert!(r
            .search
            .gguf_header("o/r", &format!("t{i}.gguf"))
            .await
            .unwrap()
            .is_none());
        assert!(lock(&r.search.headers).map.len() <= HEADER_CACHE_MAX);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_was_learnt_under_another_token_is_dropped() {
    let header = gguf(&[("arch.context_length", 4, 4096u32.to_le_bytes().to_vec())]);
    let mut hub = one_model(&[file("a-Q4_K_M.gguf", GB)]);
    hub["/lab-one/m-GGUF/resolve/main/a-Q4_K_M.gguf"] = ans(206, &header);
    let r = rig(hub).await;
    body_of(r.search.search(ALL).await);
    let first = tree_requests(&r);
    assert_eq!(first, 1);
    // Cached: no listing, and the header is read once.
    body_of(r.search.search(ALL).await);
    assert_eq!(tree_requests(&r), first);
    assert!(r
        .search
        .gguf_header("lab-one/m-GGUF", "a-Q4_K_M.gguf")
        .await
        .unwrap()
        .is_some());
    assert!(!lock(&r.search.headers).map.is_empty());
    // The same token again changes nothing.
    let db = rusqlite::Connection::open(r.dir.path().join("model_loader.db")).unwrap();
    db.execute(
        "INSERT INTO kv(key, value) VALUES('hf_token', 'first-token')",
        [],
    )
    .unwrap();
    body_of(r.search.search(ALL).await);
    assert_eq!(
        tree_requests(&r),
        first + 1,
        "the first token differs from none"
    );
    body_of(r.search.search(ALL).await);
    assert_eq!(tree_requests(&r), first + 1);
    // Another token: listings and headers are dropped.
    db.execute(
        "UPDATE kv SET value = 'second-token' WHERE key = 'hf_token'",
        [],
    )
    .unwrap();
    body_of(r.search.search(ALL).await);
    assert_eq!(tree_requests(&r), first + 2);
    assert!(lock(&r.search.headers).map.is_empty() && lock(&r.search.headers).gated.is_empty());
    let auth: Vec<String> = r
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter_map(|row| row["authorization"].as_str().map(str::to_owned))
        .collect();
    assert!(
        auth.contains(&"Bearer first-token".to_owned())
            && auth.contains(&"Bearer second-token".to_owned())
    );
}

// ---- a token that changes while a request is under way ---------------------------------------

fn set_token(r: &Rig, token: &str) {
    let db = rusqlite::Connection::open(r.dir.path().join("model_loader.db")).unwrap();
    db.execute("DELETE FROM kv WHERE key = 'hf_token'", [])
        .unwrap();
    db.execute("INSERT INTO kv(key, value) VALUES('hf_token', ?1)", [token])
        .unwrap();
}

/// Waits until the Hub has been asked for `needle` (it answers slowly: the request is under way).
async fn hub_asked(r: &Rig, needle: &str, count: usize) {
    for _ in 0..500 {
        let asked = r
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row["path"].as_str().unwrap().contains(needle))
            .count();
        if asked >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the Hub was not asked for {needle}");
}

fn held(mut answer: J) -> J {
    answer["hold"] = json!(true);
    answer
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_fetched_under_the_old_token_is_not_remembered_after_a_change() {
    let mut hub = one_model(&[file("a-Q4_K_M.gguf", GB)]);
    hub[tree("lab-one/m-GGUF")]["hold"] = json!(true);
    let r = rig(hub).await;
    set_token(&r, "first-token");
    let search = r.search.clone();
    let slow = tokio::spawn(async move { search.search(ALL).await });
    // The listing is on its way under the first token, and the Hub holds its answer.
    hub_asked(&r, "/tree/", 1).await;
    set_token(&r, "second-token");
    // A request under the second token moves the generation on; only then is the answer let go.
    r.search.gguf_header("o/r", "other.gguf").await.unwrap();
    r.gauge.released.store(true, Ordering::SeqCst);
    let body = body_of(slow.await.unwrap());
    // The caller still gets what it fetched ...
    assert!(body.contains(r#""path":"a-Q4_K_M.gguf""#), "{body}");
    // ... and the cache holds nothing the first token fetched.
    assert!(lock(&r.search.files).map.is_empty());
    let before = tree_requests(&r);
    body_of(r.search.search(ALL).await);
    assert_eq!(tree_requests(&r), before + 1);
    assert_eq!(
        lock(&r.search.files).map.len(),
        1,
        "a request under the current token is kept"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_header_fetched_under_the_old_token_is_not_remembered_after_a_change() {
    let header = gguf(&[("arch.context_length", 4, 4096u32.to_le_bytes().to_vec())]);
    let hub = json!({
        "/o/r/resolve/main/slow.gguf": held(ans(206, &header)),
        "/o/gate/resolve/main/slow.gguf": held(ans(401, b"{}")),
    });
    let r = rig(hub).await;
    set_token(&r, "first-token");
    let (a, b) = (r.search.clone(), r.search.clone());
    let slow = tokio::spawn(async move { a.gguf_header("o/r", "slow.gguf").await });
    let gated = tokio::spawn(async move { b.gguf_header("o/gate", "slow.gguf").await });
    hub_asked(&r, "/resolve/", 2).await;
    set_token(&r, "second-token");
    r.search.gguf_header("o/r", "fast.gguf").await.unwrap();
    r.gauge.released.store(true, Ordering::SeqCst);
    assert!(
        slow.await.unwrap().unwrap().is_some(),
        "the caller still gets it"
    );
    assert!(gated.await.unwrap().unwrap().is_none());
    let cache = lock(&r.search.headers);
    assert!(!cache.map.contains_key("o/r/slow.gguf"));
    assert!(cache.gated.is_empty(), "{:?}", cache.gated);
    assert!(cache.map.contains_key("o/r/fast.gguf"));
}

/// The answer says what the Hub said even when the cache kept nothing of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gated_repository_is_reported_when_its_read_raced_a_token_change() {
    let hub = json!({
        tree("lab-one/m-GGUF"): ok(&json!([file("m-Q4_K_M.gguf", GB)])),
        "/lab-one/m-GGUF/resolve/main/m-Q4_K_M.gguf": held(ans(401, b"{}")),
    });
    let r = rig(hub).await;
    repo_script(&r, 200, r#"{"groups":[{"fit":[]}]}"#);
    set_token(&r, "first-token");
    let search = r.search.clone();
    let slow = tokio::spawn(async move { search.repo("repo=lab-one%2Fm-GGUF").await });
    hub_asked(&r, "/resolve/", 1).await;
    set_token(&r, "second-token");
    r.search.gguf_header("o/r", "other.gguf").await.unwrap();
    r.gauge.released.store(true, Ordering::SeqCst);
    let body = body_of(slow.await.unwrap());
    assert!(
        body.ends_with(r#""gated":"needs a Hugging Face token (set one in Settings)"}"#),
        "{body}"
    );
    assert!(lock(&r.search.headers).gated.is_empty(), "nothing is kept");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_that_ends_late_does_not_move_the_token_back() {
    let r = rig(one_model(&[file("a-Q4_K_M.gguf", GB)])).await;
    set_token(&r, "new-token");
    body_of(r.search.search(ALL).await);
    assert_eq!(lock(&r.search.files).map.len(), 1);
    let generation = r.search.generation.load(Ordering::SeqCst);
    // A read that began before the newest one (its ticket is lower) ends now, having seen the
    // old token: it neither clears nor moves the ledger, and stores nothing.
    lock(&r.search.last_auth).applied = 1_000;
    *r.search.tickets.lock().await = 10;
    set_token(&r, "old-token");
    let (_, epoch) = r.search.auth().await.unwrap();
    assert!(epoch.0.is_none());
    assert_eq!(r.search.generation.load(Ordering::SeqCst), generation);
    assert_eq!(lock(&r.search.files).map.len(), 1);
    assert!(lock(&r.search.last_auth).last == Some(Token::Value(b"Bearer new-token".to_vec())));
    // A late read of the current token is as good as any.
    set_token(&r, "new-token");
    let (_, epoch) = r.search.auth().await.unwrap();
    assert_eq!(epoch.0, Some(generation));
}

/// Tickets are issued in the order of the sqlite reads: a read waiting for its turn is behind the
/// one that holds it, whatever started first, and waits as a task: no blocking job exists yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tickets_follow_the_order_of_the_reads_and_waiters_hold_no_blocking_slot() {
    use crate::docker::BLOCKING_HOOK;
    let r = rig(one_model(&[file("a-Q4_K_M.gguf", GB)])).await;
    set_token(&r, "first-token");
    let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = started.clone();
    let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    });
    BLOCKING_HOOK
        .scope(hook, async {
            let guard = r.search.tickets.lock().await;
            // Many reads wait for their turn ...
            let waiting = futures_util::future::join_all((0..64).map(|_| r.search.auth()));
            assert!(
                tokio::time::timeout(Duration::from_millis(200), waiting)
                    .await
                    .is_err(),
                "the reads wait for their turn"
            );
            // ... and none of them has taken a blocking slot.
            assert_eq!(started.load(Ordering::SeqCst), 0);
            // The token changes while they wait: a read sees what is stored in its turn.
            set_token(&r, "second-token");
            drop(guard);
            let (auth, _) = r.search.auth().await.unwrap();
            assert!(matches!(auth, Auth::Header(v) if v.as_bytes() == b"Bearer second-token"));
            assert_eq!(started.load(Ordering::SeqCst), 1);
            // The reads dropped while they waited took no number.
            assert_eq!(*r.search.tickets.lock().await, 1);
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_the_client_refuses_drops_what_was_learnt_before_it_and_after() {
    let hub = json!({"/o/r/resolve/main/a.gguf": ans(404, b"{}")});
    let r = rig(hub).await;
    set_token(&r, "good-token");
    r.search.gguf_header("o/r", "a.gguf").await.unwrap();
    assert_eq!(lock(&r.search.headers).map.len(), 1);
    // A token h11 refuses: what the good one learnt goes, and the refusal is learnt.
    set_token(&r, "tok ");
    r.search.gguf_header("o/r", "b.gguf").await.unwrap();
    {
        let cache = lock(&r.search.headers);
        assert!(!cache.map.contains_key("o/r/a.gguf"));
        assert!(cache.map.contains_key("o/r/b.gguf"));
    }
    // Back to a good one: nothing stored under the refused one survives.
    set_token(&r, "good-token");
    r.search.gguf_header("o/r", "a.gguf").await.unwrap();
    let cache = lock(&r.search.headers);
    assert!(!cache.map.contains_key("o/r/b.gguf"));
    assert!(cache.map.contains_key("o/r/a.gguf"));
}
