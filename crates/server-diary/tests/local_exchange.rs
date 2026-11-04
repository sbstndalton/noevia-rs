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
