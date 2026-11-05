//! `POST /api/diary/local-exchange` against a synthetic sidecar: core `routes/diary.cjs` and
//! `diary-stream.cjs` answer for answer, the admission future's place in the order, and what is held
//! until the response ends. Nothing real is read; the admission is a stand-in for the front's lease.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod support;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use server_diary::local_exchange::{ExchangeLimits, CONNECTING, THROTTLED};
use server_diary::{ExchangeHold, ExchangeRefusal, Limits, ReplyBody, Request};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use support::*;

/// A hold that records when it is given back, and can be "lost".
struct Probe {
    admitted: Arc<AtomicUsize>,
    released: Arc<AtomicBool>,
    lose: Arc<tokio::sync::Notify>,
}

struct Kept(Arc<AtomicBool>);
impl Drop for Kept {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Probe {
    fn new() -> Probe {
        Probe {
            admitted: Arc::default(),
            released: Arc::default(),
            lose: Arc::default(),
        }
    }
    fn admit(&self) -> impl std::future::Future<Output = Result<ExchangeHold, ExchangeRefusal>> {
        let (admitted, released, lose) = (
            Arc::clone(&self.admitted),
            Arc::clone(&self.released),
            Arc::clone(&self.lose),
        );
        async move {
            admitted.fetch_add(1, Ordering::SeqCst);
            Ok(ExchangeHold {
                keep: Box::new(Kept(released)),
                lost: Box::pin(async move { lose.notified().await }),
            })
        }
    }
    fn released(&self) -> bool {
        self.released.load(Ordering::SeqCst)
    }
}

fn post() -> Request {
    Request {
        method: "POST".into(),
        target: "/api/diary/local-exchange".into(),
        content_length: None,
    }
}

fn sse(chunks: &[&[u8]]) -> Out {
    Out::Stream {
        status: 200,
        headers: vec![("content-type", "text/event-stream".into())],
        chunks: chunks.iter().map(|c| c.to_vec()).collect(),
        forever: false,
    }
}

fn limits(heartbeat: Duration) -> Limits {
    Limits {
        exchange: ExchangeLimits {
            heartbeat,
            ..ExchangeLimits::default()
        },
        ..Limits::default()
    }
}

async fn text_of(body: ReplyBody) -> String {
    match body {
        ReplyBody::Events(mut b) => {
            let mut out = Vec::new();
            while let Some(frame) = b.frame().await {
                out.extend_from_slice(&frame.unwrap().into_data().unwrap());
            }
            String::from_utf8(out).unwrap()
        }
        ReplyBody::Json(t) => t,
        ReplyBody::Zip(_) => panic!("an archive"),
    }
}

async fn settle(check: impl Fn() -> bool) -> bool {
    for _ in 0..100 {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    check()
}

const CONNECT_FRAME: &str =
    "data: {\"type\":\"status\",\"text\":\"Connecting to diary companion…\"}\n\n";

#[tokio::test]
async fn the_order_is_addon_method_admission_body() {
    let stub = Stub::start(|_, _| Out::json(200, "{}")).await;
    let d = diary(&stub, TENANT_KEY);
    let w = World::new(&[]);
    let probe = Probe::new();
    let r = d
        .local_exchange(
            w.ctx(MEMBER, false),
            post(),
            Full::new(Bytes::new()),
            probe.admit(),
        )
        .await;
    assert_eq!(
        (r.status, text_of(r.body).await),
        (404, r#"{"error":"Diary add-on is disabled"}"#.to_owned())
    );
    let mut get = post();
    get.method = "GET".into();
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            get,
            Full::new(Bytes::new()),
            probe.admit(),
        )
        .await;
    assert_eq!(
        (r.status, text_of(r.body).await),
        (405, r#"{"error":"Method not allowed"}"#.to_owned())
    );
    assert_eq!(probe.admitted.load(Ordering::SeqCst), 0, "nothing held yet");
    // A refusal is core's `{ error }` with its status, before the body is read.
    let refused = async {
        Err(ExchangeRefusal {
            status: 429,
            message: THROTTLED.into(),
        })
    };
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(b"{bad")),
            refused,
        )
        .await;
    assert_eq!(
        (r.status, text_of(r.body).await),
        (429, format!(r#"{{"error":"{THROTTLED}"}}"#))
    );
    // Admitted, then the body: invalid JSON is 400 and the hold is given back.
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(b"{bad")),
            probe.admit(),
        )
        .await;
    assert_eq!(
        (r.status, text_of(r.body).await),
        (400, r#"{"error":"invalid JSON"}"#.to_owned())
    );
    assert!(probe.released());
    assert!(stub.requests().is_empty(), "the sidecar heard nothing");
}

#[tokio::test]
async fn a_body_over_16_mib_is_413() {
    let stub = Stub::start(|_, _| Out::json(200, "{}")).await;
    let d = diary(&stub, TENANT_KEY);
    let w = World::new(&[]);
    let probe = Probe::new();
    let big = Bytes::from(vec![b' '; 16 * 1024 * 1024 + 1]);
    let r = d
        .local_exchange(w.ctx(MEMBER, true), post(), Full::new(big), probe.admit())
        .await;
    assert_eq!(
        (r.status, text_of(r.body).await),
        (413, r#"{"error":"Request exceeds size limit"}"#.to_owned())
    );
    assert!(stub.requests().is_empty());
}

#[tokio::test]
async fn the_unstreamed_exchange_forwards_the_reserialised_body_and_relays_the_reply() {
    let stub = Stub::start(|seen, _| {
        if seen.body.windows(5).any(|w| w == b"other") {
            Out::json(
                503,
                r#"{"detail":"Storage is limiting sign-ins","code":"storageThrottled","retryAfter":"7.2","extra":1}"#,
            )
        } else {
            Out::json(200, r#"{"reply":"ok","day":"2026-11-04"}"#)
        }
    })
    .await;
    let d = diary(&stub, TENANT_KEY);
    let w = World::new(&[]);
    let probe = Probe::new();
    // JSON.stringify(JSON.parse(raw)): whitespace dropped, the escape decoded and re-encoded.
    let raw = Bytes::from_static(b" { \"message\" : \"synthetic \\u00e9\", \"n\": 1.0 } ");
    let r = d
        .local_exchange(w.ctx(MEMBER, true), post(), Full::new(raw), probe.admit())
        .await;
    let status = r.status;
    let body = text_of(r.body).await;
    assert_eq!(
        (status, body.as_str()),
        (200, r#"{"reply":"ok","day":"2026-11-04"}"#)
    );
    let seen = stub.requests();
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path(), "/api/local-exchange");
    assert_eq!(
        seen[0].body,
        "{\"message\":\"synthetic é\",\"n\":1}".as_bytes()
    );
    assert_eq!(seen[0].header("x-cowork-user-id"), Some(MEMBER));
    assert!(probe.released());
    // An empty body is `{}`.
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::new()),
            Probe::new().admit(),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(stub.requests()[1].body, b"{}");
    // A failure keeps Core's two storage codes and the bounded wait only.
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(br#"{"message":"other"}"#)),
            Probe::new().admit(),
        )
        .await;
    assert_eq!(r.retry_after, Some(8));
    assert_eq!(
        (r.status, text_of(r.body).await),
        (
            503,
            r#"{"error":"Storage is limiting sign-ins","code":"storageThrottled","retryAfter":8}"#
                .to_owned()
        )
    );
}

#[tokio::test]
async fn the_stream_relays_frames_rewords_a_failure_and_holds_until_it_ends() {
    // A multi-byte character and an event split across chunks; an error while generating; a frame
    // that is not JSON; a BOM at the start.
    let stub = Stub::start(|_, _| {
        sse(&[
            b"\xEF\xBB\xBFdata: {\"type\":\"status\",\"text\":\"Opening diary context\xE2\x80",
            b"\xA6\"}\n\ndata: {\"type\":\"status\",\"text\":\"Generating companion response\xE2\x80\xA6\"}\n\n",
            b"data: {\"type\":\"error\",\"text\":\"boom\",\"reason\":\"chat_timeout\"}\n\n",
            b"event: x\ndata: [DONE]\n\n",
        ])
    })
    .await;
    let d = diary_with(&stub, TENANT_KEY, limits(Duration::from_secs(60)));
    let w = World::new(&[]);
    let probe = Probe::new();
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(br#"{"message":"hi","stream":true}"#)),
            probe.admit(),
        )
        .await;
    assert_eq!(r.status, 200);
    let text = text_of(r.body).await;
    assert_eq!(
        text,
        format!(
            "{CONNECT_FRAME}data: {{\"type\":\"status\",\"text\":\"Opening diary context…\"}}\n\n\
             data: {{\"type\":\"status\",\"text\":\"Generating companion response…\"}}\n\n\
             data: {{\"type\":\"error\",\"code\":\"modelFailed\",\"saved\":false,\"text\":\"Couldn't reach the local model — nothing was saved. You can resend.\",\"reason\":\"chat_timeout\"}}\n\n\
             event: x\ndata: [DONE]\n\n"
        )
    );
    assert!(CONNECT_FRAME.contains(CONNECTING));
    assert!(
        settle(|| probe.released()).await,
        "given back when the stream ended"
    );
    assert_eq!(
        stub.requests()[0].body,
        br#"{"message":"hi","stream":true}"#
    );
}

#[tokio::test]
async fn a_refusal_before_the_stream_says_nothing_was_saved_and_a_cut_stream_is_unknown() {
    let stub = Stub::start(|seen, _| {
        if seen.body.windows(5).any(|w| w == b"login") {
            Out::json(424, r#"{"detail":"refused"}"#)
        } else {
            sse(&[b"data: {\"type\":\"status\",\"text\":\"Saving\"}\n\ndata: {\"type\":\"answer\""])
        }
    })
    .await;
    let d = diary(&stub, TENANT_KEY);
    let w = World::new(&[]);
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(br#"{"message":"login","stream":true}"#)),
            Probe::new().admit(),
        )
        .await;
    assert_eq!(
        text_of(r.body).await,
        format!("{CONNECT_FRAME}data: {{\"type\":\"error\",\"code\":\"storageLoginRejected\",\"saved\":false,\"text\":\"Diary storage refused the saved login — nothing was saved. Reconnect storage, then resend.\"}}\n\n")
    );
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(br#"{"message":"cut","stream":true}"#)),
            Probe::new().admit(),
        )
        .await;
    assert_eq!(
        text_of(r.body).await,
        format!("{CONNECT_FRAME}data: {{\"type\":\"status\",\"text\":\"Saving\"}}\n\ndata: {{\"type\":\"error\",\"text\":\"The diary connection was interrupted. The entry may still be saving; check the saved diary before sending again.\"}}\n\n")
    );
}

#[tokio::test]
async fn an_unreachable_companion_was_never_asked() {
    // Bind and drop: nothing listens on this port.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let d = server_diary::Diary::new(server_diary::DiaryConfig::new(
        format!("http://127.0.0.1:{port}"),
        BEARER,
        TENANT_KEY,
        String::new,
    ))
    .unwrap();
    let w = World::new(&[]);
    let probe = Probe::new();
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(br#"{"stream":true}"#)),
            probe.admit(),
        )
        .await;
    assert_eq!(
        text_of(r.body).await,
        format!("{CONNECT_FRAME}data: {{\"type\":\"error\",\"code\":\"diaryUnavailable\",\"saved\":false,\"text\":\"Couldn't reach the Diary companion — nothing was saved. You can resend.\"}}\n\n")
    );
    // Unstreamed, the same is core's 503 diaryUnavailable.
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::new()),
            Probe::new().admit(),
        )
        .await;
    assert_eq!(
        (r.status, text_of(r.body).await),
        (
            503,
            r#"{"error":"Diary is unavailable right now. Try again shortly.","code":"diaryUnavailable"}"#
                .to_owned()
        )
    );
}

#[tokio::test]
async fn keep_alives_flow_a_browser_that_leaves_stops_it_and_a_lost_hold_ends_it() {
    let dropped = Arc::new(AtomicBool::new(false));
    let d2 = Arc::clone(&dropped);
    let stub = Stub::start(move |_, _| Out::Stall {
        status: 200,
        headers: vec![("content-type", "text/event-stream".into())],
        chunk: b"data: {\"type\":\"status\",\"text\":\"Opening diary context\xE2\x80\xA6\"}\n\n"
            .to_vec(),
        dropped: Arc::clone(&d2),
    })
    .await;
    let d = diary_with(&stub, TENANT_KEY, limits(Duration::from_millis(50)));
    let w = World::new(&[]);
    let probe = Probe::new();
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(br#"{"stream":true}"#)),
            probe.admit(),
        )
        .await;
    let ReplyBody::Events(mut body) = r.body else {
        panic!("a stream")
    };
    let mut seen = String::new();
    while !seen.contains(": keep-alive\n\n") {
        let frame = body.frame().await.unwrap().unwrap();
        seen.push_str(std::str::from_utf8(&frame.into_data().unwrap()).unwrap());
    }
    assert!(seen.starts_with(CONNECT_FRAME), "{seen}");
    assert!(seen.contains("Opening diary context…"), "{seen}");
    assert!(!probe.released());
    drop(body);
    assert!(
        settle(|| probe.released()).await,
        "the browser left: given back"
    );
    assert!(
        settle(|| dropped.load(Ordering::SeqCst)).await,
        "and the companion connection closed"
    );

    // A hold that is lost ends the stream as an interrupted one.
    let probe = Probe::new();
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(br#"{"stream":true}"#)),
            probe.admit(),
        )
        .await;
    let lose = Arc::clone(&probe.lose);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        lose.notify_one();
    });
    let text = text_of(r.body).await;
    assert!(
        text.ends_with("data: {\"type\":\"error\",\"text\":\"The diary connection was interrupted. The entry may still be saving; check the saved diary before sending again.\"}\n\n"),
        "{text}"
    );
    assert!(settle(|| probe.released()).await);
}

#[tokio::test]
async fn an_older_companion_answers_one_json_reply() {
    let stub = Stub::start(|_, _| {
        Out::json(
            200,
            r#"{"reply":"Synthetic answer","reasoning":"why","day":"2026-11-04"}"#,
        )
    })
    .await;
    let d = diary(&stub, TENANT_KEY);
    let w = World::new(&[]);
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(br#"{"stream":true}"#)),
            Probe::new().admit(),
        )
        .await;
    assert_eq!(
        text_of(r.body).await,
        format!("{CONNECT_FRAME}data: {{\"type\":\"reasoning\",\"text\":\"why\"}}\n\ndata: {{\"type\":\"answer\",\"text\":\"Synthetic answer\"}}\n\ndata: {{\"type\":\"diary\",\"reply\":\"Synthetic answer\",\"reasoning\":\"why\",\"day\":\"2026-11-04\"}}\n\ndata: {{\"type\":\"done\"}}\n\n")
    );
}

// ── review of b5792c5: each test below failed against it ───────────────────────

/// A sidecar that sends the head and `chunk` of its reply, then nothing until it is let go.
fn stall(ctype: &str, chunk: &[u8], dropped: &Arc<AtomicBool>) -> Out {
    Out::Stall {
        status: 200,
        headers: vec![("content-type", ctype.to_owned())],
        chunk: chunk.to_vec(),
        dropped: Arc::clone(dropped),
    }
}

fn lose_after(probe: &Probe, wait: Duration) {
    let lose = Arc::clone(&probe.lose);
    tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        lose.notify_one();
    });
}

const BUSY: &str = r#"{"error":"The model is busy. Try again shortly."}"#;
const UNAVAILABLE: &str =
    r#"{"error":"Diary is unavailable right now. Try again shortly.","code":"diaryUnavailable"}"#;

/// P1: a lease lost while the body is still arriving ends the request; nothing reaches the sidecar.
#[tokio::test]
async fn review_p1_a_lease_lost_while_the_body_arrives_ends_it() {
    let stub = Stub::start(|_, _| Out::json(200, "{}")).await;
    let d = diary(&stub, TENANT_KEY);
    let w = World::new(&[]);
    let probe = Probe::new();
    let (_still_sending, body) = http_body_util::channel::Channel::<Bytes, std::io::Error>::new(1);
    lose_after(&probe, Duration::from_millis(100));
    let answer = tokio::time::timeout(
        Duration::from_secs(3),
        d.local_exchange(w.ctx(MEMBER, true), post(), body, probe.admit()),
    )
    .await
    .expect("answered as soon as the lease was lost");
    assert_eq!(
        (answer.status, text_of(answer.body).await),
        (503, BUSY.to_owned())
    );
    assert!(probe.released());
    assert!(stub.requests().is_empty());
}

/// P1: a lease lost during the unstreamed call stops the call (core's answer for an aborted call).
#[tokio::test]
async fn review_p1_a_lease_lost_during_the_unstreamed_call_stops_it() {
    let dropped = Arc::new(AtomicBool::new(false));
    let d2 = Arc::clone(&dropped);
    let stub = Stub::start(move |_, _| stall("application/json", b"{\"reply\":", &d2)).await;
    let d = diary(&stub, TENANT_KEY);
    let w = World::new(&[]);
    let probe = Probe::new();
    lose_after(&probe, Duration::from_millis(200));
    let answer = tokio::time::timeout(
        Duration::from_secs(3),
        d.local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(br#"{"message":"hi"}"#)),
            probe.admit(),
        ),
    )
    .await
    .expect("answered as soon as the lease was lost");
    assert_eq!(
        (answer.status, text_of(answer.body).await),
        (503, UNAVAILABLE.to_owned())
    );
    assert!(probe.released());
    assert!(
        settle(|| dropped.load(Ordering::SeqCst)).await,
        "the call was stopped"
    );
}

/// P1: a request that goes away during the unstreamed call takes the call with it.
#[tokio::test]
async fn review_p1_a_dropped_request_stops_the_unstreamed_call() {
    let dropped = Arc::new(AtomicBool::new(false));
    let d2 = Arc::clone(&dropped);
    let stub = Stub::start(move |_, _| stall("application/json", b"{\"reply\":", &d2)).await;
    let d = diary(&stub, TENANT_KEY);
    let w = World::new(&[]);
    let probe = Probe::new();
    let gone = tokio::time::timeout(
        Duration::from_millis(300),
        d.local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(br#"{"message":"hi"}"#)),
            probe.admit(),
        ),
    )
    .await;
    assert!(gone.is_err(), "still waiting for the companion");
    assert!(probe.released());
    assert!(
        settle(|| dropped.load(Ordering::SeqCst)).await,
        "the call was stopped"
    );
}

/// P2: a browser that stops reading cannot hold the companion or the lease past the deadline.
#[tokio::test]
async fn review_p2_a_browser_that_stops_reading_cannot_outlive_the_deadline() {
    let dropped = Arc::new(AtomicBool::new(false));
    let d2 = Arc::clone(&dropped);
    let stub = Stub::start(move |_, _| {
        stall(
            "text/event-stream",
            b"data: {\"type\":\"status\",\"text\":\"Opening diary context\xE2\x80\xA6\"}\n\n",
            &d2,
        )
    })
    .await;
    let d = diary_with(
        &stub,
        TENANT_KEY,
        Limits {
            exchange: ExchangeLimits {
                heartbeat: Duration::from_millis(5),
                stream_deadline: Duration::from_millis(400),
                ..ExchangeLimits::default()
            },
            ..Limits::default()
        },
    );
    let w = World::new(&[]);
    let probe = Probe::new();
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(br#"{"stream":true}"#)),
            probe.admit(),
        )
        .await;
    // Held, never read.
    let _unread = r.body;
    assert!(
        settle(|| probe.released()).await,
        "the lease was given back at the deadline"
    );
    assert!(
        settle(|| dropped.load(Ordering::SeqCst)).await,
        "and the companion connection closed"
    );
}

fn nest(depth: usize) -> String {
    format!("{}0{}", "[".repeat(depth), "]".repeat(depth))
}

/// P3: events the parsed tree could not hold are still read as Node reads them.
#[tokio::test]
async fn review_p3_events_with_huge_numbers_or_deep_nesting_get_nodes_rewrite() {
    let deep = nest(200);
    let frames = [
        // Stage start: not reworded, but the reason that is not a token is removed.
        "data: {\"type\":\"error\",\"reason\":\"not a token!\",\"extra\":1e400}\n\n".to_owned(),
        "data: {\"type\":\"status\",\"text\":\"Generating companion response\u{2026}\"}\n\n".to_owned(),
        // While generating: reworded, the token reason kept.
        "data: {\"type\":\"error\",\"text\":\"t\",\"reason\":\"chat_timeout\",\"n\":1e400}\n\n".to_owned(),
        // `saved` given: not reworded; no modelFailed code, so the reason goes; the rest as Node writes it.
        format!("data: {{\"type\":\"error\",\"saved\":false, \"reason\":\"chat_timeout\",\"deep\":{deep},\"n\":1.50}}\n\n"),
    ];
    let body: Vec<u8> = frames.concat().into_bytes();
    let stub = Stub::start(move |_, _| Out::Stream {
        status: 200,
        headers: vec![("content-type", "text/event-stream".into())],
        chunks: vec![body.clone()],
        forever: false,
    })
    .await;
    let d = diary(&stub, TENANT_KEY);
    let w = World::new(&[]);
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(br#"{"stream":true}"#)),
            Probe::new().admit(),
        )
        .await;
    assert_eq!(
        text_of(r.body).await,
        format!(
            "{CONNECT_FRAME}data: {{\"type\":\"error\",\"extra\":null}}\n\n\
             data: {{\"type\":\"status\",\"text\":\"Generating companion response…\"}}\n\n\
             data: {{\"type\":\"error\",\"code\":\"modelFailed\",\"saved\":false,\"text\":\"Couldn't reach the local model — nothing was saved. You can resend.\",\"reason\":\"chat_timeout\"}}\n\n\
             data: {{\"type\":\"error\",\"saved\":false,\"deep\":{deep},\"n\":1.5}}\n\n"
        )
    );
}

/// P3: a request body JSON.parse accepts is forwarded as JSON.stringify writes it, never a 500.
#[tokio::test]
async fn review_p3_a_request_with_huge_numbers_or_deep_nesting_is_forwarded() {
    let stub = Stub::start(|_, _| Out::json(200, r#"{"reply":"ok"}"#)).await;
    let d = diary(&stub, TENANT_KEY);
    let w = World::new(&[]);
    let deep = nest(200);
    let body = format!("{{\"message\":\"hi\", \"big\":1e400,\"deep\":{deep}}}");
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from(body)),
            Probe::new().admit(),
        )
        .await;
    assert_eq!(
        (r.status, text_of(r.body).await),
        (200, r#"{"reply":"ok"}"#.to_owned())
    );
    assert_eq!(
        String::from_utf8(stub.requests()[0].body.clone()).unwrap(),
        format!("{{\"message\":\"hi\",\"big\":null,\"deep\":{deep}}}")
    );
}

/// The non-streamed reply is relayed as `JSON.stringify(JSON.parse(text))` too, never a 500.
#[tokio::test]
async fn an_unstreamed_reply_with_huge_numbers_is_relayed_as_node_writes_it() {
    let stub = Stub::start(|_, _| Out::json(200, r#"{"reply":"ok", "n":1e400,"1":2}"#)).await;
    let d = diary(&stub, TENANT_KEY);
    let w = World::new(&[]);
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(b"{}")),
            Probe::new().admit(),
        )
        .await;
    assert_eq!(
        (r.status, text_of(r.body).await),
        (200, r#"{"1":2,"reply":"ok","n":null}"#.to_owned())
    );
}

// ── re-review of 0ec81d1: each test below failed against it ────────────────────

/// U+10FFFF and U+10F800, which the parsed tree keeps for lone surrogates.
const TOP: char = '\u{10FFFF}';
const LOW: char = '\u{10F800}';

/// Finding 1, stream: an event with a character from U+10F800..U+10FFFF (literal or as an escaped
/// pair, in a key or a value) still gets its reason removed, rewords, and advances the stage.
#[tokio::test]
async fn rereview_1_events_with_top_plane_characters_get_nodes_policy() {
    let frames = [
        // Stage start: the reason that is not a token is removed.
        format!("data: {{\"type\":\"error\",\"code\":\"other\",\"reason\":\"x y\",\"text\":\"{TOP}\"}}\n\n"),
        // `saved` given, an escaped pair as a key: the reason removed, the pair written as its character.
        "data: {\"type\":\"error\",\"saved\":false,\"reason\":\"bad!\",\"\\udbff\\udfff\":\"\\ud800\"}\n\n".to_owned(),
        // A status carrying one still advances the stage...
        format!("data: {{\"type\":\"status\",\"text\":\"Generating companion response\u{2026}\",\"x\":\"{LOW}\"}}\n\n"),
        // ...so this error is reworded as a model failure.
        "data: {\"type\":\"error\",\"text\":\"t\"}\n\n".to_owned(),
    ];
    let body: Vec<u8> = frames.concat().into_bytes();
    let stub = Stub::start(move |_, _| Out::Stream {
        status: 200,
        headers: vec![("content-type", "text/event-stream".into())],
        chunks: vec![body.clone()],
        forever: false,
    })
    .await;
    let d = diary(&stub, TENANT_KEY);
    let w = World::new(&[]);
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(br#"{"stream":true}"#)),
            Probe::new().admit(),
        )
        .await;
    assert_eq!(
        text_of(r.body).await,
        format!(
            "{CONNECT_FRAME}data: {{\"type\":\"error\",\"code\":\"other\",\"text\":\"{TOP}\"}}\n\n\
             data: {{\"type\":\"error\",\"saved\":false,\"{TOP}\":\"\\ud800\"}}\n\n\
             data: {{\"type\":\"status\",\"text\":\"Generating companion response…\",\"x\":\"{LOW}\"}}\n\n\
             data: {{\"type\":\"error\",\"code\":\"modelFailed\",\"saved\":false,\"text\":\"Couldn't reach the local model — nothing was saved. You can resend.\"}}\n\n"
        )
    );
}

/// Finding 1, request: such characters in a key or a value are forwarded, not a 400.
#[tokio::test]
async fn rereview_1_a_request_with_top_plane_characters_is_forwarded() {
    let stub = Stub::start(|_, _| Out::json(200, r#"{"reply":"ok"}"#)).await;
    let d = diary(&stub, TENANT_KEY);
    let w = World::new(&[]);
    let body = format!("{{\"message\":\"{LOW}\", \"k\\udbff\\udfff\":1}}");
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from(body)),
            Probe::new().admit(),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        String::from_utf8(stub.requests()[0].body.clone()).unwrap(),
        format!("{{\"message\":\"{LOW}\",\"k{TOP}\":1}}")
    );
}

/// Finding 1, replies: a reply with such a character is relayed as the object it is, and a
/// failure keeps its detail.
#[tokio::test]
async fn rereview_1_replies_with_top_plane_characters_are_read() {
    let stub = Stub::start(|seen, _| {
        if seen.body.windows(4).any(|w| w == b"fail") {
            Out::json(
                503,
                &format!(
                    "{{\"detail\":\"busy {TOP}\",\"code\":\"storageThrottled\",\"retryAfter\":3}}"
                ),
            )
        } else {
            Out::json(200, &format!("{{\"reply\":\"{TOP}\"}}"))
        }
    })
    .await;
    let d = diary(&stub, TENANT_KEY);
    let w = World::new(&[]);
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(b"{}")),
            Probe::new().admit(),
        )
        .await;
    assert_eq!(
        (r.status, text_of(r.body).await),
        (200, format!("{{\"reply\":\"{TOP}\"}}"))
    );
    let r = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(br#"{"m":"fail"}"#)),
            Probe::new().admit(),
        )
        .await;
    assert_eq!(r.retry_after, Some(3));
    assert_eq!(
        (r.status, text_of(r.body).await),
        (
            503,
            format!("{{\"error\":\"busy {TOP}\",\"code\":\"storageThrottled\",\"retryAfter\":3}}")
        )
    );
}

/// Forwards `body` on its own thread and runtime; `None` if that took longer than `bound`. (A
/// rewrite that never yields would hold a test runtime's only thread, so the clock is outside it.)
fn forwarded_within(body: String, bound: Duration) -> Option<(u16, usize, Duration)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let stub = Stub::start(|_, _| Out::json(200, "{}")).await;
            let d = diary(&stub, TENANT_KEY);
            let w = World::new(&[]);
            let started = std::time::Instant::now();
            let r = d
                .local_exchange(
                    w.ctx(MEMBER, true),
                    post(),
                    Full::new(Bytes::from(body)),
                    Probe::new().admit(),
                )
                .await;
            let took = started.elapsed();
            let sent = stub.requests().first().map_or(0, |s| s.body.len());
            let _ = tx.send((r.status, sent, took));
        });
    });
    rx.recv_timeout(bound).ok()
}

/// Finding 2: the rewrite is linear in nesting depth, for arrays, plain objects and objects that
/// must be reordered. Quadratic copying at a million levels is ~10^12 bytes: hours, not seconds.
#[test]
fn rereview_2_a_million_levels_are_rewritten_in_linear_time() {
    let depth = 1_000_000;
    let reorder = 300_000;
    for (name, body) in [
        (
            "arrays",
            format!("{{\"x\":{}0{}}}", "[".repeat(depth), "]".repeat(depth)),
        ),
        (
            "objects",
            format!("{}0{}", "{\"a\":".repeat(depth), "}".repeat(depth)),
        ),
        (
            "reordered objects",
            format!(
                "{}0{}",
                "{\"b\":0,\"1\":".repeat(reorder),
                "}".repeat(reorder)
            ),
        ),
    ] {
        let size = body.len();
        let (status, sent, took) = forwarded_within(body, Duration::from_secs(60))
            .unwrap_or_else(|| panic!("{name}: not forwarded within 60 s (quadratic)"));
        assert_eq!(status, 200, "{name}");
        assert!(sent > 0 && sent <= size, "{name}");
        eprintln!("{name}: {size} bytes forwarded in {took:?}");
    }
}

#[tokio::test]
async fn tls_failure_is_internal_unstreamed_and_interrupted_streamed() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut hello = [0; 4096];
            let _ = socket.read(&mut hello).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
        }
    });
    let d = server_diary::Diary::new(server_diary::DiaryConfig::new(
        format!("https://{address}"),
        BEARER,
        TENANT_KEY,
        String::new,
    ))
    .unwrap();
    let w = World::new(&[]);
    let reply = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::new()),
            Probe::new().admit(),
        )
        .await;
    assert_eq!(
        (reply.status, text_of(reply.body).await),
        (500, r#"{"error":"Internal error"}"#.to_owned())
    );
    let reply = d
        .local_exchange(
            w.ctx(MEMBER, true),
            post(),
            Full::new(Bytes::from_static(br#"{"stream":true}"#)),
            Probe::new().admit(),
        )
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(text_of(reply.body).await, format!("{CONNECT_FRAME}data: {{\"type\":\"error\",\"text\":\"The diary connection was interrupted. The entry may still be saving; check the saved diary before sending again.\"}}\n\n"));
    task.abort();
    let _ = task.await;
}
