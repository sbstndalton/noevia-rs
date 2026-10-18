#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! The actual Python differential: every case of `tests/fixtures/search-python.v1.json` (recorded
//! from the services app by `tools/gen-search-oracle.py`) is rebuilt in a temporary store, and every
//! request is replayed against a fake Hub and a fake Python child that give the recorded answers.
//! The status, the body, the Hub requests (path and query, token, range) and the avatar cache must
//! match what Python did. Every name is synthetic.
use super::*;
use serde_json::Value as J;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CHILD_TOKEN: &str = "synthetic-child-token-0123456789abcdef";
/// The figure every client waits at most, as in the generator.
const WAIT: Duration = Duration::from_millis(1000);

/// `NOW0` of the generator.
const NOW0: f64 = 1_800_000_000.5;

static CLOCK: AtomicU64 = AtomicU64::new(0);

fn clock() -> f64 {
    f64::from_bits(CLOCK.load(Ordering::SeqCst))
}

fn set_clock(seconds: f64) {
    CLOCK.store(seconds.to_bits(), Ordering::SeqCst);
}

pub(super) fn fixture() -> J {
    serde_json::from_str(include_str!("../../tests/fixtures/search-python.v1.json")).unwrap()
}

/// Asserts two texts equal, showing only where they first differ.
pub(super) fn same(label: &str, got: &str, want: &str) {
    if got == want {
        return;
    }
    let at = got
        .char_indices()
        .zip(want.char_indices())
        .find(|((_, a), (_, b))| a != b)
        .map_or(got.len().min(want.len()), |((i, _), _)| i);
    let window = |s: &str| -> String {
        let mut start = at.saturating_sub(150).min(s.len());
        while !s.is_char_boundary(start) {
            start -= 1;
        }
        let mut end = (at + 250).min(s.len());
        while !s.is_char_boundary(end) {
            end += 1;
        }
        s[start..end].to_owned()
    };
    panic!(
        "{label}: first difference at byte {at} (lengths {} and {})
  got:  {}
  want: {}",
        got.len(),
        want.len(),
        window(got),
        window(want)
    );
}

/// Asserts two JSON arrays equal, listing the elements only one of them has.
pub(super) fn same_rows(label: &str, got: &J, want: &J) {
    if got == want {
        return;
    }
    let (g, w) = (got.as_array().unwrap(), want.as_array().unwrap());
    let only = |a: &[J], b: &[J]| -> Vec<String> {
        a.iter()
            .filter(|x| !b.contains(x))
            .map(ToString::to_string)
            .collect()
    };
    panic!(
        "{label}: {} rows, wanted {}
  only got:  {:#?}
  only want: {:#?}",
        g.len(),
        w.len(),
        only(g, w),
        only(w, g)
    );
}

pub(super) fn hex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

pub(super) type Seen = Arc<Mutex<Vec<J>>>;

async fn read_head(sock: &mut tokio::net::TcpStream) -> Option<Vec<u8>> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if sock.read(&mut byte).await.unwrap_or(0) == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    Some(head)
}

fn header_of(head: &str, name: &str) -> Option<String> {
    head.split("\r\n")
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().to_owned())
}

/// The recorded answers of a fake Hub, which a case may change between two requests.
pub(super) type Answers = Arc<Mutex<J>>;

/// Requests a fake Hub is serving now, and the most it served at once.
#[derive(Default)]
pub(super) struct Gauge {
    pub live: AtomicUsize,
    pub peak: AtomicUsize,
}

struct Serving(Arc<Gauge>);

impl Serving {
    fn begin(gauge: &Arc<Gauge>) -> Self {
        let live = gauge.live.fetch_add(1, Ordering::SeqCst) + 1;
        gauge.peak.fetch_max(live, Ordering::SeqCst);
        Self(gauge.clone())
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A fake Hub: the recorded answer for a path and query, else a 404. `other` is the address
/// `@CDN@` stands for.
pub(super) async fn hub(
    answers: Answers,
    other: String,
) -> (String, Seen, tokio::task::JoinHandle<()>) {
    let (base, seen, task, _) = hub_gauged(answers, other).await;
    (base, seen, task)
}

/// [`hub`], counting the requests it serves at once.
pub(super) async fn hub_gauged(
    answers: Answers,
    other: String,
) -> (String, Seen, tokio::task::JoinHandle<()>, Arc<Gauge>) {
    let gauge = Arc::new(Gauge::default());
    let counted = gauge.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let (b, s) = (base.clone(), seen.clone());
    let task = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let (answers, base, seen, other) =
                (answers.clone(), b.clone(), s.clone(), other.clone());
            let gauge = counted.clone();
            tokio::spawn(async move {
                let Some(head) = read_head(&mut sock).await else {
                    return;
                };
                let _serving = Serving::begin(&gauge);
                let text = String::from_utf8_lossy(&head).into_owned();
                let path = text
                    .split("\r\n")
                    .next()
                    .unwrap()
                    .split(' ')
                    .nth(1)
                    .unwrap()
                    .to_owned();
                seen.lock().unwrap().push(serde_json::json!({
                    "path": path,
                    "authorization": header_of(&text, "authorization").map_or(J::Null, J::String),
                    "range": header_of(&text, "range").map_or(J::Null, J::String),
                }));
                let found = answers.lock().unwrap().get(&path).cloned();
                let (status, headers, body, extra) = match found {
                    Some(a) => (
                        a["status"].as_u64().unwrap(),
                        a["headers"].as_array().unwrap().clone(),
                        {
                            let mut body = hex(a["body"].as_str().unwrap());
                            body.resize(body.len() + a["zeros"].as_u64().unwrap_or(0) as usize, 0);
                            body
                        },
                        a.clone(),
                    ),
                    None => (
                        404,
                        vec![serde_json::json!(["content-type", "application/json"])],
                        br#"{"error":"Repository not found"}"#.to_vec(),
                        J::Null,
                    ),
                };
                if let Some(delay) = extra["delay"].as_f64() {
                    tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                }
                if extra["mode"] == "drop" {
                    return;
                }
                let mut out = format!("HTTP/1.1 {status} X\r\n");
                for h in headers {
                    out.push_str(&format!(
                        "{}: {}\r\n",
                        h[0].as_str().unwrap(),
                        h[1].as_str()
                            .unwrap()
                            .replace("@HUB@", &base)
                            .replace("@CDN@", &other)
                    ));
                }
                let declared = body.len() + if extra["mode"] == "partial" { 100 } else { 0 };
                out.push_str(&format!(
                    "content-length: {declared}\r\nconnection: close\r\n\r\n"
                ));
                let _ = sock.write_all(out.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                if extra["mode"] == "partial" {
                    let _ = sock.shutdown().await;
                }
            });
        }
    });
    (base, seen, task, gauge)
}

/// What the fake child answers: the recorded runtime of the request being replayed.
#[derive(Default)]
pub(super) struct ChildScript {
    pub search: Option<J>,
    pub repo: Option<J>,
    /// Mismatches found by the fake (an unexpected call, token or body).
    pub problems: Vec<String>,
    pub calls: Vec<String>,
}

pub(super) type Script = Arc<Mutex<ChildScript>>;

/// A fake Python child with the two private calls.
pub(super) async fn child() -> (String, Script, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let script: Script = Arc::new(Mutex::new(ChildScript::default()));
    let s = script.clone();
    let task = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let script = s.clone();
            tokio::spawn(async move {
                let Some(head) = read_head(&mut sock).await else {
                    return;
                };
                let text = String::from_utf8_lossy(&head).into_owned();
                let mut first = text.split("\r\n").next().unwrap().split(' ');
                let (method, path) = (first.next().unwrap(), first.next().unwrap().to_owned());
                let length: usize = header_of(&text, "content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut body = vec![0u8; length];
                sock.read_exact(&mut body).await.unwrap();
                let token = header_of(&text, "x-model-loader-token");
                let (status, answer) = {
                    let mut script = script.lock().unwrap();
                    script.calls.push(format!("{method} {path}"));
                    if token.as_deref() != Some(CHILD_TOKEN) {
                        script.problems.push(format!("token {token:?} on {path}"));
                    }
                    match (method, path.as_str()) {
                        ("GET", RUNTIME_PATH) => match script.search.clone() {
                            Some(r) => (
                                r["status"].as_u64().unwrap(),
                                r["body"].as_str().unwrap().to_owned(),
                            ),
                            None => {
                                script.problems.push("unexpected search-runtime".into());
                                (500, String::new())
                            }
                        },
                        ("POST", REPO_RUNTIME_PATH) => match script.repo.clone() {
                            Some(r) => {
                                let got: J = serde_json::from_slice(&body).unwrap_or(J::Null);
                                if got != r["request"] {
                                    script.problems.push(format!(
                                        "repo-runtime body {got} != {}",
                                        r["request"]
                                    ));
                                }
                                (
                                    r["status"].as_u64().unwrap(),
                                    r["body"].as_str().unwrap().to_owned(),
                                )
                            }
                            None => {
                                script
                                    .problems
                                    .push("unexpected search-repo-runtime".into());
                                (500, String::new())
                            }
                        },
                        _ => (404, String::new()),
                    }
                };
                let out = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{answer}",
                    answer.len()
                );
                let _ = sock.write_all(out.as_bytes()).await;
            });
        }
    });
    (base, script, task)
}

pub(super) fn store(data: &Path, schema: &J, seed: &J) {
    let db = rusqlite::Connection::open(data.join("model_loader.db")).unwrap();
    for sql in schema.as_array().unwrap() {
        db.execute_batch(sql.as_str().unwrap()).unwrap();
    }
    db.execute("DELETE FROM kv", []).unwrap();
    for sql in seed.as_array().into_iter().flatten() {
        db.execute_batch(sql.as_str().unwrap()).unwrap();
    }
}

fn avatars(data: &Path) -> J {
    let db = rusqlite::Connection::open(data.join("model_loader.db")).unwrap();
    let mut statement = db
        .prepare("SELECT owner, url, fetched_at FROM avatar_cache ORDER BY owner")
        .unwrap();
    let rows: Vec<J> = statement
        .query_map([], |row| {
            let cell = |i: usize| -> rusqlite::Result<J> {
                use rusqlite::types::ValueRef;
                Ok(match row.get_ref(i)? {
                    ValueRef::Integer(n) => serde_json::json!(n),
                    ValueRef::Real(f) => serde_json::json!(f),
                    ValueRef::Text(t) => J::String(String::from_utf8(t.to_vec()).unwrap()),
                    other => panic!("{other:?}"),
                })
            };
            Ok(J::Array(vec![cell(0)?, cell(1)?, cell(2)?]))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    J::Array(rows)
}

fn sorted(seen: &Seen) -> J {
    let mut rows = seen.lock().unwrap().clone();
    let key = |r: &J| {
        (
            r["path"].as_str().unwrap().to_owned(),
            r["authorization"].as_str().unwrap_or("").to_owned(),
            r["range"].as_str().unwrap_or("").to_owned(),
        )
    };
    rows.sort_by_key(key);
    J::Array(rows)
}

fn timeouts() -> Timeouts {
    Timeouts {
        search: WAIT,
        tree: WAIT,
        header: WAIT,
        avatar: WAIT,
        listings: Duration::from_secs(60),
    }
}

pub(super) fn closed_port() -> String {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", closed.local_addr().unwrap().port());
    drop(closed);
    base
}

/// Replays every request of one case; returns the number of requests.
async fn replay(case: &J, schema: &J) -> usize {
    let id = case["id"].as_str().unwrap();
    let dir = tempfile::tempdir().unwrap();
    store(dir.path(), schema, &case["seed"]);
    let answers: Answers = Arc::new(Mutex::new(case["hub"].clone()));
    let cdn_answers: Answers = Arc::new(Mutex::new(case["cdn"].clone()));
    let (cdn_base, cdn_seen, cdn_task) = hub(cdn_answers, String::new()).await;
    let (base, seen, hub_task) = hub(answers.clone(), cdn_base).await;
    let (child_base, script, child_task) = child().await;
    let hub_base = if case["hub_down"].as_bool() == Some(true) {
        closed_port()
    } else {
        base
    };
    let search = Search::with(
        dir.path(),
        Hub {
            api: format!("{hub_base}/api"),
            resolve: hub_base.clone(),
        },
        &child_base,
        CHILD_TOKEN,
        clock,
        timeouts(),
    )
    .unwrap();
    let mut count = 0;
    for (index, step) in case["results"].as_array().unwrap().iter().enumerate() {
        let get = step["get"].as_str().unwrap();
        set_clock(step["now"].as_f64().unwrap());
        seen.lock().unwrap().clear();
        cdn_seen.lock().unwrap().clear();
        if index > 0 {
            if let Some(more) = case["then_hub"].as_object() {
                let mut all = answers.lock().unwrap();
                for (k, v) in more {
                    all[k.as_str()] = v.clone();
                }
            }
        }
        {
            let mut script = script.lock().unwrap();
            script.calls.clear();
            script.problems.clear();
            let runtime = &step["runtime"];
            if get.starts_with("/api/v1/search/repo") {
                script.search = None;
                script.repo = (!runtime.is_null()).then(|| runtime.clone());
            } else {
                script.repo = None;
                script.search = (!runtime.is_null()).then(|| runtime.clone());
            }
        }
        let (path, query) = get.split_once('?').unwrap_or((get, ""));
        let outcome = if path == "/api/v1/search/repo" {
            search.repo(query).await
        } else {
            search.search(query).await
        };
        if case["native"] == "refused" {
            assert_eq!(outcome, Outcome::Refused, "{id}: {get}");
            continue;
        }
        let (status, body) = match outcome {
            Outcome::Json(status, body) => (status, String::from_utf8(body).unwrap()),
            Outcome::Failed => (500, "Internal Server Error".to_owned()),
            other => panic!("{id}: {get}: {other:?}"),
        };
        assert_eq!(
            u64::from(status),
            step["status"].as_u64().unwrap(),
            "{id}: {get}: status"
        );
        same(
            &format!("{id}: {get}: body"),
            &body,
            step["body"].as_str().unwrap(),
        );
        if case["native_requests"] != "unchecked" {
            same_rows(
                &format!("{id}: {get}: Hub requests"),
                &sorted(&seen),
                &step["requests"],
            );
        }
        if case["cdn"].is_object() {
            same_rows(
                &format!("{id}: {get}: CDN requests"),
                &sorted(&cdn_seen),
                &step["cdn_requests"],
            );
        }
        same_rows(
            &format!("{id}: {get}: avatar cache"),
            &avatars(dir.path()),
            &step["avatars"],
        );
        let script = script.lock().unwrap();
        assert!(
            script.problems.is_empty(),
            "{id}: {get}: {:?}",
            script.problems
        );
        count += 1;
    }
    hub_task.abort();
    cdn_task.abort();
    child_task.abort();
    count
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_recorded_request_is_answered_as_python_answered_it() {
    let fixture = fixture();
    let schema = &fixture["schema"];
    let mut total = 0;
    for case in fixture["cases"].as_array().unwrap() {
        total += replay(case, schema).await;
    }
    assert!(total > 0);
}

// ---- the grammars and numbers ---------------------------------------------------------------

#[test]
fn query_values_are_parsed_as_pydantic_parses_them() {
    use params::Parsed::{Invalid, Unknown, Value};
    let fixture = fixture();
    let g = &fixture["grammar"];
    let accepted = &g["accepted"];
    let corpus = g["corpus"].as_array().unwrap();
    assert!(corpus.len() > 5000);
    let mut problems: Vec<String> = Vec::new();
    // Values read, values known to be errors, values left to the refusal.
    let mut seen = [[0usize; 3]; 3];
    for entry in corpus {
        let s = entry.as_str().unwrap();
        let python = |kind: &str| accepted[kind].get(s).and_then(J::as_str);
        match (params::parse_int(s), python("int")) {
            (Value(v), Some(want)) => {
                seen[0][0] += 1;
                let digits = want.trim_start_matches('-').len();
                let expected = if digits <= 18 {
                    want.to_owned()
                } else if want.starts_with('-') {
                    (-i64::MAX).to_string()
                } else {
                    i64::MAX.to_string()
                };
                if v.to_string() != expected {
                    problems.push(format!("int {s:?}: {v}, python {want}"));
                }
            }
            (Invalid(params::Failure::IntSize), Some("size")) => seen[0][1] += 1,
            (Invalid(params::Failure::Int), None) => seen[0][1] += 1,
            (Unknown, _) => seen[0][2] += 1,
            (got, want) => problems.push(format!("int {s:?}: {got:?}, python {want:?}")),
        }
        match (params::parse_float(s), python("float")) {
            (Value(v), Some(want)) => {
                seen[1][0] += 1;
                if gguf::py_float_repr(v) != want {
                    problems.push(format!(
                        "float {s:?}: {}, python {want}",
                        gguf::py_float_repr(v)
                    ));
                }
            }
            (Invalid(params::Failure::Float), None) => seen[1][1] += 1,
            (Unknown, _) => seen[1][2] += 1,
            (got, want) => problems.push(format!("float {s:?}: {got:?}, python {want:?}")),
        }
        match (params::parse_bool(s), python("bool")) {
            (Value(v), Some(want)) => {
                seen[2][0] += 1;
                if (if v { "true" } else { "false" }) != want {
                    problems.push(format!("bool {s:?}: {v}, python {want}"));
                }
            }
            (Invalid(params::Failure::Bool), None) => seen[2][1] += 1,
            (got, want) => problems.push(format!("bool {s:?}: {got:?}, python {want:?}")),
        }
    }
    problems.truncate(40);
    assert!(
        problems.is_empty(),
        "{} shown:
{}",
        problems.len(),
        problems.join(
            "
"
        )
    );
    // Each kind reads many values, and refuses only a small part of the corpus.
    for (kind, row) in ["int", "float", "bool"].iter().zip(seen) {
        assert!(row[0] > 20 && row[1] > 1000, "{kind}: {row:?}");
        assert!(row[2] * 4 < corpus.len(), "{kind}: {row:?}");
    }
}

#[test]
fn the_whitespace_sets_are_pythons() {
    let fixture = fixture();
    let spaces = &fixture["grammar"]["spaces"];
    let set = |name: &str| -> Vec<u32> {
        spaces[name]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as u32)
            .collect()
    };
    let rust_trim: Vec<u32> = (0..0x11_0000u32)
        .filter(|c| char::from_u32(*c).is_some_and(char::is_whitespace))
        .collect();
    // pydantic trims int and float text as Rust's `str::trim` does; a bool is not trimmed.
    assert_eq!(set("int"), rust_trim);
    assert_eq!(set("float"), rust_trim);
    assert!(set("bool").is_empty());
    // `str.strip()` strips what `str.isspace()` is true for.
    let py_strip: Vec<u32> = (0..0x11_0000u32)
        .filter(|c| char::from_u32(*c).is_some_and(py::is_space))
        .collect();
    assert_eq!(set("strip"), py_strip);
}

#[test]
fn int_of_text_is_pythons() {
    let fixture = fixture();
    let g = &fixture["grammar"];
    for entry in g["corpus"].as_array().unwrap() {
        let s = entry.as_str().unwrap();
        let want = g["accepted"]["int_py"].get(s).and_then(J::as_str);
        let exotic = s.chars().any(|c| !c.is_ascii() && !py::is_space(c));
        match (py::parse_int(s), want) {
            (Ok(Ok(v)), Some(want)) => assert_eq!(v.to_string(), want, "{s:?}"),
            (Ok(Err(())), None) => {}
            // A value past 64 bits, or text this side does not read (other scripts' digits).
            (Err(py::Unsupported), Some(want)) => {
                assert!(
                    exotic || want.trim_start_matches('-').len() >= 18,
                    "{s:?}: {want}"
                );
            }
            (Err(py::Unsupported), None) => assert!(exotic, "{s:?}"),
            (got, want) => panic!("int({s:?}): {got:?}, python {want:?}"),
        }
    }
}

#[test]
fn iso_dates_age_as_python_ages_them() {
    let fixture = fixture();
    let now = py::instant_of_unix(NOW0).unwrap();
    let rows = fixture["iso"].as_array().unwrap();
    assert!(rows.len() > 3000);
    let (mut aged, mut fallback, mut unsupported) = (0, 0, 0);
    for row in rows {
        let (text, want) = (row[0].as_str().unwrap(), row[1].as_str().unwrap());
        match py::age_days(text, now) {
            Ok(age) => {
                assert_eq!(gguf::py_float_repr(age), want, "{text:?}");
                if want == "3650.0" {
                    fallback += 1;
                } else {
                    aged += 1;
                }
            }
            Err(py::Unsupported) => {
                unsupported += 1;
                assert!(
                    !text.bytes().all(|b| (0x20..=0x7e).contains(&b)),
                    "{text:?}"
                );
            }
        }
    }
    assert!(
        aged > 50 && fallback > 100 && unsupported > 5,
        "{aged} {fallback} {unsupported}"
    );
}

#[test]
fn numbers_are_rounded_and_written_as_python_does() {
    let fixture = fixture();
    let numbers = &fixture["numbers"];
    for row in numbers["floats"].as_array().unwrap() {
        let bytes = hex(row[0].as_str().unwrap());
        let x = f64::from_le_bytes(bytes.try_into().unwrap());
        let text = |i: usize| row[i].as_str().unwrap();
        assert_eq!(
            gguf::py_float_repr(py::round_to(x, 2)),
            text(1),
            "round({x:e}, 2)"
        );
        assert_eq!(
            gguf::py_float_repr(py::round_to(x, 1)),
            text(2),
            "round({x:e}, 1)"
        );
        assert_eq!(py::fixed(x, 1), text(3), "{x:e} .1f");
        assert_eq!(py::fixed(x, 2), text(4), "{x:e} .2f");
    }
    for row in numbers["human_bytes"].as_array().unwrap() {
        let n = i128::from(row[0].as_i64().unwrap());
        assert_eq!(
            py::human_bytes(n),
            row[1].as_str().unwrap(),
            "human_bytes({n})"
        );
    }
    for row in numbers["ratios"].as_array().unwrap() {
        let n = i128::from(row[0].as_i64().unwrap());
        let got = py::ratio(n, 1_000_000).unwrap();
        assert_eq!(
            gguf::py_float_repr(got),
            row[1].as_str().unwrap(),
            "{n} / 10**6"
        );
    }
}

// ---- the Unicode classes ---------------------------------------------------------------------

fn bitmap(ranges: &J) -> Vec<bool> {
    let mut map = vec![false; 0x11_0000];
    for r in ranges.as_array().unwrap() {
        for c in r[0].as_u64().unwrap()..=r[1].as_u64().unwrap() {
            map[c as usize] = true;
        }
    }
    map
}

/// The classes `discover.rs` spells out are Python's `\w`, `\d`, `\s` and `re.I` letters over every
/// character Python's Unicode database (the version the fixture names) assigns.
#[test]
fn unicode_classes_are_pythons() {
    let fixture = fixture();
    let u = &fixture["unicode"];
    let (assigned, word, digit, space) = (
        bitmap(&u["assigned"]),
        bitmap(&u["word"]),
        bitmap(&u["digit"]),
        bitmap(&u["space"]),
    );
    let nd = regex::Regex::new(&format!("^{}$", discover::DIGIT)).unwrap();
    let blank = regex::Regex::new(&format!("^{}$", discover::SPACE)).unwrap();
    let mut problems: Vec<String> = Vec::new();
    let mut counted = 0;
    for c in (0..0x11_0000u32).filter(|c| assigned[*c as usize]) {
        let ch = char::from_u32(c).unwrap();
        let text = ch.to_string();
        counted += 1;
        if discover::is_word(ch).unwrap() != word[c as usize] {
            problems.push(format!("\\w {c:#x}: python {}", word[c as usize]));
        }
        if nd.is_match(&text) != digit[c as usize]
            || discover::digit_value(ch).is_some() != digit[c as usize]
        {
            problems.push(format!("\\d {c:#x}: python {}", digit[c as usize]));
        }
        if blank.is_match(&text) != space[c as usize] || py::is_space(ch) != space[c as usize] {
            problems.push(format!("\\s {c:#x}: python {}", space[c as usize]));
        }
    }
    for pair in u["digit_values"].as_array().unwrap() {
        let (c, v) = (
            pair[0].as_u64().unwrap() as u32,
            pair[1].as_u64().unwrap() as u32,
        );
        if discover::digit_value(char::from_u32(c).unwrap()) != Some(v) {
            problems.push(format!("digit {c:#x} = {v}"));
        }
    }
    // A letter under re.I: its two cases and these.
    for (letter, python) in u["letters"].as_object().unwrap() {
        let l = letter.chars().next().unwrap();
        let mut ours: Vec<u64> = discover::folded(l)
            .iter()
            .map(|c| u64::from(u32::from(*c)))
            .collect();
        let mut theirs: Vec<u64> = python
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_u64().unwrap())
            .collect();
        ours.sort_unstable();
        theirs.sort_unstable();
        if ours != theirs {
            problems.push(format!("re.I {letter}: {ours:?}, python {theirs:?}"));
        }
    }
    problems.truncate(30);
    assert!(
        counted > 140_000 && problems.is_empty(),
        "{counted}: {problems:#?}"
    );
}
