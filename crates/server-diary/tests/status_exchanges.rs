//! `GET /api/diary/storage-status` and `GET`/`POST /api/diary/exchanges` (sbstndalton/noevia#2206)
//! against a synthetic sidecar and a synthetic workspace: the status the sidecar gives plus the
//! storage-login latch, and the Diary tab's recovery records with the set of running exchanges Node
//! publishes. Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::type_complexity
)]
mod support;

use bytes::Bytes;
use http_body_util::Full;
use js_json::{wtf16, JValue};
use server_diary::{Diary, Held, Reply, ReplyBody, Request, Route, StorageIdentity, StorageLatch};
use std::sync::{Arc, Mutex};
use support::*;

async fn run(
    d: &Diary,
    ctx: server_diary::Ctx,
    route: Route,
    method: &str,
    target: &str,
    body: &[u8],
) -> (u16, String) {
    let reply: Reply = d
        .serve(
            route,
            ctx,
            Request {
                method: method.into(),
                target: target.into(),
                content_length: None,
            },
            Full::new(Bytes::copy_from_slice(body)),
        )
        .await;
    match reply.body {
        ReplyBody::Json(t) => (reply.status, t),
        ReplyBody::Zip(_) => panic!("a stream"),
    }
}

fn webdav(secret: &str) -> Conn {
    Conn {
        kind: "webdav",
        base_url: "https://cloud.example/dav",
        secret: secret.into(),
    }
}

/// A latch that answers with a fixed hold and remembers what it was asked about (never the secret
/// itself in a place a test prints).
struct Latch {
    hold: Option<Held>,
    asked: Mutex<Vec<(String, String, String, String, String, bool)>>,
}

impl Latch {
    fn new(hold: Option<Held>) -> Arc<Latch> {
        Arc::new(Latch {
            hold,
            asked: Mutex::new(Vec::new()),
        })
    }
}

impl StorageLatch for Latch {
    fn held(&self, user_id: &str, s: &StorageIdentity<'_>) -> Option<Held> {
        self.asked.lock().unwrap().push((
            user_id.to_owned(),
            s.kind.to_owned(),
            s.base_url.to_owned(),
            s.bucket.to_owned(),
            s.username.to_owned(),
            *s.secret == SECRET,
        ));
        self.hold
    }
}

fn held() -> Held {
    Held {
        kind: "login",
        next_attempt_at: 1_800_000_000_000,
    }
}

// ── storage status ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_status_is_the_sidecars_and_names_the_hold_on_automatic_storage_calls() {
    let stub = Stub::start(|_, _| {
        Out::json(
            200,
            r#"{"mode":"legacy","ok":true,"automaticStorage":"x","tail":1}"#,
        )
    })
    .await;
    let w = World::new(&[(MEMBER, webdav(SECRET))]);
    let d = diary(&stub, TENANT_KEY);
    let latch = Latch::new(Some(held()));
    let (status, text) = run(
        &d,
        w.ctx_with_latch(MEMBER, latch.clone()),
        Route::StorageStatus,
        "GET",
        "/api/diary/storage-status",
        b"",
    )
    .await;
    assert_eq!(status, 200);
    // The hold replaces a field of the same name where it stood; the other fields keep their order.
    assert_eq!(
        text,
        r#"{"mode":"legacy","ok":true,"automaticStorage":{"held":true,"kind":"login","nextAttemptAt":1800000000000},"tail":1}"#
    );
    let seen = &stub.requests()[0];
    assert_eq!(
        (seen.method.as_str(), seen.target.as_str()),
        ("GET", "/api/storage-status")
    );
    assert_eq!(seen.header("x-cowork-user-id"), Some(MEMBER));
    // The latch is asked about this person's own saved connection, secret included (to fingerprint
    // it), and nobody else's.
    assert_eq!(
        *latch.asked.lock().unwrap(),
        vec![(
            MEMBER.to_owned(),
            "webdav".to_owned(),
            "https://cloud.example/dav".to_owned(),
            String::new(),
            "ada".to_owned(),
            true
        )]
    );
}

#[tokio::test]
async fn without_a_hold_or_a_latch_the_status_is_unchanged() {
    let stub = Stub::start(|_, _| Out::json(200, r#"{"mode":"managed","n":1.50}"#)).await;
    let w = World::new(&[]);
    let d = diary(&stub, TENANT_KEY);
    for ctx in [
        w.ctx_with_latch(MEMBER, Latch::new(None)),
        w.ctx(MEMBER, true),
    ] {
        let (status, text) = run(&d, ctx, Route::StorageStatus, "GET", "/x", b"").await;
        assert_eq!(
            (status, text.as_str()),
            (200, r#"{"mode":"managed","n":1.5}"#)
        );
    }
}

#[tokio::test]
async fn only_an_object_reply_carries_the_hold_and_a_failure_is_the_sidecars_detail() {
    let latch = Latch::new(Some(held()));
    let w = World::new(&[]);
    for (reply, want) in [
        (Out::json(200, r#"[1,2]"#), (200, "[1,2]")),
        (Out::json(200, r#""text""#), (200, r#""text""#)),
        (
            Out::json(502, r#"{"detail":"storage is down"}"#),
            (502, r#"{"error":"storage is down"}"#),
        ),
        (
            Out::json(500, r#"{"detail":""}"#),
            (500, r#"{"error":"Diary storage request failed"}"#),
        ),
        (
            Out::text(503, "upstream"),
            (503, r#"{"error":"Diary storage request failed"}"#),
        ),
    ] {
        let reply = Mutex::new(Some(reply));
        let stub = Stub::start(move |_, _| reply.lock().unwrap().take().unwrap()).await;
        let d = diary(&stub, TENANT_KEY);
        let (status, text) = run(
            &d,
            w.ctx_with_latch(MEMBER, latch.clone()),
            Route::StorageStatus,
            "GET",
            "/x",
            b"",
        )
        .await;
        assert_eq!((status, text.as_str()), want);
    }
    assert!(
        latch.asked.lock().unwrap().is_empty(),
        "a reply that is not an object is never asked about"
    );
}

#[tokio::test]
async fn the_status_is_gated_on_the_addon_then_the_method_and_a_dead_sidecar_is_a_generic_500() {
    let stub = Stub::start(|_, _| Out::json(200, "{}")).await;
    let w = World::new(&[]);
    let d = diary(&stub, TENANT_KEY);
    let (status, text) = run(
        &d,
        w.ctx(MEMBER, false),
        Route::StorageStatus,
        "POST",
        "/x",
        b"",
    )
    .await;
    assert_eq!(
        (status, text.as_str()),
        (404, r#"{"error":"Diary add-on is disabled"}"#)
    );
    for method in ["POST", "PUT", "DELETE"] {
        let (status, text) = run(
            &d,
            w.ctx(MEMBER, true),
            Route::StorageStatus,
            method,
            "/x",
            b"",
        )
        .await;
        assert_eq!(
            (status, text.as_str()),
            (405, r#"{"error":"Method not allowed"}"#)
        );
    }
    assert!(stub.requests().is_empty());
    let dead = Stub::start(|_, _| Out::Hangup).await;
    let d = diary(&dead, TENANT_KEY);
    let (status, text) = run(
        &d,
        w.ctx(MEMBER, true),
        Route::StorageStatus,
        "GET",
        "/x",
        b"",
    )
    .await;
    // A sidecar that drops the call is unavailable, not a bug (#2232).
    assert_eq!(
        (status, text.as_str()),
        (
            503,
            r#"{"error":"Diary is unavailable right now. Try again shortly.","code":"diaryUnavailable"}"#
        )
    );
}

#[tokio::test]
async fn the_status_call_repeats_once_with_the_secret_after_a_428_like_every_other_call() {
    let stub = Stub::start(|seen, n| {
        if n == 0 {
            assert!(!seen.storage_has_secret());
            Out::json(428, "{}")
        } else {
            assert!(seen.storage_has_secret());
            Out::json(200, r#"{"mode":"legacy"}"#)
        }
    })
    .await;
    let w = World::new(&[(MEMBER, webdav(SECRET))]);
    let d = diary(&stub, TENANT_KEY);
    let (status, _) = run(
        &d,
        w.ctx(MEMBER, true),
        Route::StorageStatus,
        "GET",
        "/x",
        b"",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(stub.requests().len(), 2);
}

trait SecretOnTheWire {
    fn storage_has_secret(&self) -> bool;
}
impl SecretOnTheWire for Seen {
    fn storage_has_secret(&self) -> bool {
        !matches!(self.storage().get("secret"), JValue::Undefined)
    }
}

// ── exchanges ──────────────────────────────────────────────────────────────────

const DAY: &str = "2026-10-30";
const A: &str = "exchange-aaaaaaaaaaaaaaaa";
const B: &str = "exchange-bbbbbbbbbbbbbbbb";
const C: &str = "exchange-cccccccccccccccc";

fn write_record(w: &World, user: &str, day: &str, id: &str, json: &str) {
    let dir = w.user_dir(user).join("diary-conversations").join(day);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{id}.json")), json).unwrap();
}

fn publish(w: &World, user: &str, start: &str, files: &[&str]) {
    let dir = w.user_dir(user).join("diary-conversations");
    std::fs::create_dir_all(&dir).unwrap();
    let files: Vec<String> = files.iter().map(|f| format!("{f:?}")).collect();
    std::fs::write(
        dir.join("active.json"),
        format!(r#"{{"start":"{start}","files":[{}]}}"#, files.join(",")),
    )
    .unwrap();
}

async fn list(d: &Diary, w: &World, user: &str, query: &str) -> (u16, JValue) {
    let (status, text) = run(
        d,
        w.ctx(user, true),
        Route::Exchanges,
        "GET",
        &format!("/api/diary/exchanges{query}"),
        b"",
    )
    .await;
    (status, wtf16::parse(&text).unwrap())
}

fn ids(v: &JValue) -> Vec<(String, String)> {
    match v.get("exchanges") {
        JValue::Arr(rows) => rows
            .iter()
            .map(|r| {
                (
                    r.get("id").as_str().unwrap().to_owned(),
                    r.get("state").as_str().unwrap().to_owned(),
                )
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

async fn quiet() -> (Stub, Diary) {
    let stub = Stub::start(|_, _| Out::json(200, "{}")).await;
    let d = diary(&stub, TENANT_KEY);
    (stub, d)
}

#[tokio::test]
async fn a_day_lists_its_records_oldest_first_and_a_bad_day_is_a_400() {
    let (stub, d) = quiet().await;
    let w = World::new(&[]);
    write_record(
        &w,
        MEMBER,
        DAY,
        B,
        r#"{"id":"exchange-bbbbbbbbbbbbbbbb","state":"complete","startedAt":20}"#,
    );
    write_record(
        &w,
        MEMBER,
        DAY,
        A,
        r#"{"id":"exchange-aaaaaaaaaaaaaaaa","state":"failed","startedAt":30,"error":"x"}"#,
    );
    write_record(
        &w,
        MEMBER,
        DAY,
        C,
        r#"{"id":"exchange-cccccccccccccccc","state":"complete","startedAt":10}"#,
    );
    // Not records: another extension, a short name, a record of another day, damaged JSON.
    write_record(
        &w,
        MEMBER,
        DAY,
        "short",
        r#"{"id":"short","state":"complete","startedAt":1}"#,
    );
    std::fs::write(
        w.user_dir(MEMBER)
            .join("diary-conversations")
            .join(DAY)
            .join("note.txt"),
        "x",
    )
    .unwrap();
    write_record(
        &w,
        MEMBER,
        "2026-10-29",
        A,
        r#"{"id":"exchange-aaaaaaaaaaaaaaaa","state":"complete","startedAt":5}"#,
    );
    write_record(&w, MEMBER, DAY, "exchange-dddddddddddddddd", "{broken");
    write_record(&w, MEMBER, DAY, "exchange-eeeeeeeeeeeeeeee", "null");
    let (status, v) = list(&d, &w, MEMBER, &format!("?day={DAY}")).await;
    assert_eq!(status, 200);
    assert_eq!(
        ids(&v),
        [(C, "complete"), (B, "complete"), (A, "failed")]
            .map(|(i, s)| (i.to_owned(), s.to_owned()))
    );
    // A day with nothing is an empty list; a day that is not a date is refused.
    let (status, v) = list(&d, &w, MEMBER, "?day=2026-10-31").await;
    assert_eq!((status, ids(&v)), (200, vec![]));
    for query in [
        "",
        "?day=",
        "?day=2026-02-30",
        "?day=yesterday",
        "?day=2026-10-30%20",
    ] {
        let (status, v) = list(&d, &w, MEMBER, query).await;
        assert_eq!(
            (status, v.get("error").as_str()),
            (400, Some("Valid diary day required.")),
            "{query}"
        );
    }
    // Another person's records are never read.
    let (_, v) = list(&d, &w, OTHER, &format!("?day={DAY}")).await;
    assert_eq!(ids(&v), vec![]);
    assert!(
        stub.requests().is_empty(),
        "the records are files, not the sidecar's"
    );
}

/// Review L4 (sbstndalton/noevia#2206): a record with no numeric `startedAt` has a place in the order
/// (before every dated one, file-name order among themselves) instead of comparing equal to all.
#[tokio::test]
async fn a_record_without_a_start_time_sorts_before_the_dated_ones_and_the_order_is_total() {
    let (_stub, d) = quiet().await;
    let w = World::new(&[]);
    let at = |n: &str| format!("exchange-{}", n.repeat(16));
    for (id, start) in [
        ("a", r#","startedAt":30"#),
        ("b", ""),
        ("c", r#","startedAt":10"#),
        ("d", r#","startedAt":"soon""#),
        ("e", r#","startedAt":20"#),
        ("f", r#","startedAt":null"#),
    ] {
        write_record(
            &w,
            MEMBER,
            DAY,
            &at(id),
            &format!(r#"{{"id":"{}","state":"complete"{start}}}"#, at(id)),
        );
    }
    let (status, got) = list(&d, &w, MEMBER, &format!("?day={DAY}")).await;
    assert_eq!(status, 200);
    let order: Vec<String> = ids(&got).into_iter().map(|(id, _)| id).collect();
    assert_eq!(
        order,
        ["b", "d", "f", "c", "e", "a"].map(at),
        "undated first (by name), then oldest to newest"
    );
}

#[tokio::test]
async fn only_the_newest_hundred_records_of_a_day_are_listed() {
    let (_stub, d) = quiet().await;
    let w = World::new(&[]);
    for n in 0..105 {
        let id = format!("exchange-{n:016}");
        write_record(
            &w,
            MEMBER,
            DAY,
            &id,
            &format!(r#"{{"id":"{id}","state":"complete","startedAt":{n}}}"#),
        );
    }
    let (_, v) = list(&d, &w, MEMBER, &format!("?day={DAY}")).await;
    let got = ids(&v);
    assert_eq!(got.len(), 100);
    assert_eq!(got[0].0, "exchange-0000000000000005");
    assert_eq!(got[99].0, "exchange-0000000000000104");
}

#[tokio::test]
async fn a_running_record_nothing_runs_for_reads_uncertain() {
    let (_stub, d) = quiet().await;
    let w = World::new(&[]);
    let running = |id: &str, at: u32| {
        format!(r#"{{"id":"{id}","state":"running","startedAt":{at},"content":"partial \ud83d"}}"#)
    };
    write_record(&w, MEMBER, DAY, A, &running(A, 1));
    write_record(&w, MEMBER, DAY, B, &running(B, 2));
    write_record(&w, MEMBER, DAY, C, &running(C, 3));
    // Nothing published at all (a Node that never ran, or one that is gone): all abandoned.
    let (_, v) = list(&d, &w, MEMBER, &format!("?day={DAY}")).await;
    assert!(ids(&v).iter().all(|(_, s)| s == "uncertain"));
    // Node says it runs B, from this start: B stays running, the others are still abandoned.
    publish(&w, MEMBER, START, &[&format!("{DAY}/{B}.json")]);
    let (_, v) = list(&d, &w, MEMBER, &format!("?day={DAY}")).await;
    assert_eq!(
        ids(&v),
        [(A, "uncertain"), (B, "running"), (C, "uncertain")]
            .map(|(i, s)| (i.to_owned(), s.to_owned()))
    );
    // The same set written by another container start names nothing: that Node is gone.
    publish(
        &w,
        MEMBER,
        "ffffffffffffffffffffffffffffffff",
        &[&format!("{DAY}/{B}.json")],
    );
    let (_, v) = list(&d, &w, MEMBER, &format!("?day={DAY}")).await;
    assert!(ids(&v).iter().all(|(_, s)| s == "uncertain"));
    // So does a set that cannot be read, and another person's set.
    std::fs::write(
        w.user_dir(MEMBER).join("diary-conversations/active.json"),
        "{broken",
    )
    .unwrap();
    let (_, v) = list(&d, &w, MEMBER, &format!("?day={DAY}")).await;
    assert!(ids(&v).iter().all(|(_, s)| s == "uncertain"));
    publish(&w, OTHER, START, &[&format!("{DAY}/{A}.json")]);
    let (_, v) = list(&d, &w, MEMBER, &format!("?day={DAY}")).await;
    assert_eq!(ids(&v)[0].1, "uncertain");
    // The record is returned as stored, its lone surrogate included.
    let (_, text) = run(
        &d,
        w.ctx(MEMBER, true),
        Route::Exchanges,
        "GET",
        &format!("/x?day={DAY}"),
        b"",
    )
    .await;
    assert!(text.contains(r#"partial \ud83d"#), "{text}");
}

#[tokio::test]
async fn a_record_that_is_not_running_is_never_touched_by_the_set() {
    let (_stub, d) = quiet().await;
    let w = World::new(&[]);
    write_record(
        &w,
        MEMBER,
        DAY,
        A,
        r#"{"id":"exchange-aaaaaaaaaaaaaaaa","state":"complete","startedAt":1}"#,
    );
    publish(&w, MEMBER, START, &[&format!("{DAY}/{A}.json")]);
    let (_, v) = list(&d, &w, MEMBER, &format!("?day={DAY}")).await;
    assert_eq!(ids(&v), [(A.to_owned(), "complete".to_owned())]);
}

#[tokio::test]
async fn the_listing_is_exchanges_only_on_get_and_post_and_the_addon_comes_before_the_day() {
    let (_stub, d) = quiet().await;
    let w = World::new(&[]);
    // Any other method matches nothing in core: the unknown-path answer, add-on or not.
    for ctx_diary in [true, false] {
        for method in ["PUT", "DELETE", "PATCH"] {
            let (status, text) = run(
                &d,
                w.ctx(MEMBER, ctx_diary),
                Route::Exchanges,
                method,
                "/x",
                b"{}",
            )
            .await;
            assert_eq!(
                (status, text.as_str()),
                (404, r#"{"error":"not found"}"#),
                "{method}"
            );
        }
    }
    for method in ["GET", "POST"] {
        let (status, text) = run(
            &d,
            w.ctx(MEMBER, false),
            Route::Exchanges,
            method,
            "/x",
            b"{}",
        )
        .await;
        assert_eq!(
            (status, text.as_str()),
            (404, r#"{"error":"Diary add-on is disabled"}"#)
        );
    }
}

// ── dismiss ────────────────────────────────────────────────────────────────────

async fn dismiss(d: &Diary, w: &World, user: &str, body: &str) -> (u16, JValue) {
    let (status, text) = run(
        d,
        w.ctx(user, true),
        Route::Exchanges,
        "POST",
        "/api/diary/exchanges",
        body.as_bytes(),
    )
    .await;
    (status, wtf16::parse(&text).unwrap())
}

fn record_path(w: &World, user: &str, id: &str) -> std::path::PathBuf {
    w.user_dir(user)
        .join("diary-conversations")
        .join(DAY)
        .join(format!("{id}.json"))
}

fn dismiss_body(id: &str) -> String {
    format!(r#"{{"action":"dismiss","day":"{DAY}","id":"{id}"}}"#)
}

#[tokio::test]
async fn dismissing_marks_an_uncertain_or_failed_record_once_and_never_rewrites_its_outcome() {
    let (_stub, d) = quiet().await;
    let w = World::new(&[]);
    write_record(
        &w,
        MEMBER,
        DAY,
        A,
        r#"{"id":"exchange-aaaaaaaaaaaaaaaa","day":"2026-10-30","state":"uncertain","startedAt":1,"error":"x\ud83d"}"#,
    );
    write_record(
        &w,
        MEMBER,
        DAY,
        B,
        r#"{"id":"exchange-bbbbbbbbbbbbbbbb","day":"2026-10-30","state":"failed","startedAt":2}"#,
    );
    let (status, v) = dismiss(&d, &w, MEMBER, &dismiss_body(A)).await;
    assert_eq!(status, 200);
    let row = v.get("exchange");
    assert_eq!(row.get("id").as_str(), Some(A));
    assert_eq!(row.get("day").as_str(), Some(DAY));
    assert_eq!(row.get("state").as_str(), Some("uncertain"));
    let first = row.get("acknowledgedAt").as_num().unwrap();
    assert!(first > 1.0e12, "a time in ms");
    // Written as core's atomicJson writes: 2-space JSON, outcome and lone surrogate kept, updatedAt set.
    let text = std::fs::read_to_string(record_path(&w, MEMBER, A)).unwrap();
    assert!(
        text.starts_with("{\n  \"id\": \"exchange-aaaaaaaaaaaaaaaa\""),
        "{text}"
    );
    let stored = wtf16::parse(&text).unwrap();
    assert_eq!(stored.get("state").as_str(), Some("uncertain"));
    assert_eq!(stored.get("acknowledgedAt").as_num(), Some(first));
    assert_eq!(stored.get("updatedAt").as_num(), Some(first));
    assert!(text.contains(r#""x\ud83d""#));
    // Idempotent: a second press keeps the first time.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (_, again) = dismiss(&d, &w, MEMBER, &dismiss_body(A)).await;
    assert_eq!(
        again.get("exchange").get("acknowledgedAt").as_num(),
        Some(first)
    );
    // A failed one can be dismissed too.
    let (status, v) = dismiss(&d, &w, MEMBER, &dismiss_body(B)).await;
    assert_eq!(
        (status, v.get("exchange").get("state").as_str()),
        (200, Some("failed"))
    );
    // Nothing else was left in the directory, and the file is private.
    let names: Vec<String> = std::fs::read_dir(record_path(&w, MEMBER, A).parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(record_path(&w, MEMBER, A))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[tokio::test]
async fn an_orphaned_running_record_is_dismissed_as_the_uncertain_one_it_reads_as() {
    let (_stub, d) = quiet().await;
    let w = World::new(&[]);
    write_record(
        &w,
        MEMBER,
        DAY,
        A,
        r#"{"id":"exchange-aaaaaaaaaaaaaaaa","day":"2026-10-30","state":"running","startedAt":1}"#,
    );
    let (status, v) = dismiss(&d, &w, MEMBER, &dismiss_body(A)).await;
    assert_eq!(
        (status, v.get("exchange").get("state").as_str()),
        (200, Some("uncertain"))
    );
    let stored =
        wtf16::parse(&std::fs::read_to_string(record_path(&w, MEMBER, A)).unwrap()).unwrap();
    assert_eq!(
        stored.get("state").as_str(),
        Some("uncertain"),
        "written as the list reports it"
    );
}

#[tokio::test]
async fn a_record_that_still_runs_is_refused_and_so_is_one_with_nothing_to_resolve() {
    let (_stub, d) = quiet().await;
    let w = World::new(&[]);
    write_record(
        &w,
        MEMBER,
        DAY,
        A,
        r#"{"id":"exchange-aaaaaaaaaaaaaaaa","state":"running","startedAt":1}"#,
    );
    write_record(
        &w,
        MEMBER,
        DAY,
        B,
        r#"{"id":"exchange-bbbbbbbbbbbbbbbb","state":"complete","startedAt":2}"#,
    );
    publish(&w, MEMBER, START, &[&format!("{DAY}/{A}.json")]);
    let before = std::fs::read(record_path(&w, MEMBER, A)).unwrap();
    let (status, v) = dismiss(&d, &w, MEMBER, &dismiss_body(A)).await;
    assert_eq!(
        (status, v.get("error").as_str()),
        (409, Some("This exchange is still running."))
    );
    assert_eq!(std::fs::read(record_path(&w, MEMBER, A)).unwrap(), before);
    let (status, v) = dismiss(&d, &w, MEMBER, &dismiss_body(B)).await;
    assert_eq!(
        (status, v.get("error").as_str()),
        (409, Some("This exchange has nothing to resolve."))
    );
}

#[tokio::test]
async fn dismiss_errors_follow_core_and_nothing_outside_the_record_is_written() {
    let (_stub, d) = quiet().await;
    let w = World::new(&[]);
    write_record(&w, MEMBER, DAY, A, "not json");
    for (body, status, error) in [
        (
            dismiss_body("exchange-zzzzzzzzzzzzzzzz"),
            404,
            "No such exchange.",
        ),
        (dismiss_body(A), 500, "This exchange record cannot be read."),
        (
            dismiss_body("../../x"),
            400,
            "Valid diary day and exchange ID required.",
        ),
        (
            dismiss_body("short"),
            400,
            "Valid diary day and exchange ID required.",
        ),
        (
            r#"{"action":"dismiss","day":"2026-02-30","id":"exchange-aaaaaaaaaaaaaaaa"}"#.into(),
            400,
            "Valid diary day and exchange ID required.",
        ),
        (
            r#"{"action":"dismiss","day":5,"id":"exchange-aaaaaaaaaaaaaaaa"}"#.into(),
            400,
            "Valid diary day and exchange ID required.",
        ),
        (
            r#"{"action":"dismiss"}"#.into(),
            400,
            "Valid diary day and exchange ID required.",
        ),
        (
            r#"{"action":"delete","day":"2026-10-30","id":"exchange-aaaaaaaaaaaaaaaa"}"#.into(),
            400,
            "Unknown action",
        ),
        ("{}".into(), 400, "Unknown action"),
        (String::new(), 400, "Unknown action"),
        ("{broken".into(), 400, "invalid JSON"),
        ("null".into(), 400, "request body must be a JSON object"),
        ("[1]".into(), 400, "request body must be a JSON object"),
        ("5".into(), 400, "request body must be a JSON object"),
    ] {
        let (got, v) = dismiss(&d, &w, MEMBER, &body).await;
        assert_eq!(
            (got, v.get("error").as_str()),
            (status, Some(error)),
            "{body}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(record_path(&w, MEMBER, A)).unwrap(),
        "not json"
    );
    // Another person's record is not reachable by name.
    let (got, v) = dismiss(&d, &w, OTHER, &dismiss_body(A)).await;
    assert_eq!(
        (got, v.get("error").as_str()),
        (404, Some("No such exchange."))
    );
}

#[tokio::test]
async fn a_dismiss_body_over_4096_bytes_is_refused_unread() {
    let (_stub, d) = quiet().await;
    let w = World::new(&[]);
    let pad = "x".repeat(4096);
    let (status, v) = dismiss(
        &d,
        &w,
        MEMBER,
        &format!(r#"{{"action":"dismiss","pad":"{pad}"}}"#),
    )
    .await;
    assert_eq!(
        (status, v.get("error").as_str()),
        (413, Some("Request exceeds size limit"))
    );
    // At the cap exactly it is read.
    let exact = format!(r#"{{"action":"x","pad":"{}"}}"#, "y".repeat(4096 - 23));
    assert_eq!(exact.len(), 4096);
    let (status, v) = dismiss(&d, &w, MEMBER, &exact).await;
    assert_eq!(
        (status, v.get("error").as_str()),
        (400, Some("Unknown action"))
    );
}
