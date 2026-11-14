//! The native turn's reader of the sign-in row's answer over a scripted body (no network): each
//! answer kind as the engine answer the turn reads, the end and failure of a body, the cap, the
//! client leaving and the request deadline. Over real HTTP: signin_wire_tests.rs (core's 93
//! recorded cases through this reader, and the engine against a loopback backend).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use super::*;
use signin_transport::fetch::Upstream;
use signin_transport::request::Refusal;
use std::collections::VecDeque;

/// Every test's waits end by this.
const DEADLINE: Duration = Duration::from_secs(10);

/// One scripted read: a chunk, a failure, or a read that never answers.
enum Step {
    Chunk(&'static [u8]),
    Fail(&'static str),
    Hang,
}

struct QaBody(VecDeque<Step>);

impl Body for QaBody {
    async fn chunk(&mut self) -> Option<Result<Vec<u8>, String>> {
        match self.0.pop_front()? {
            Step::Chunk(c) => Some(Ok(c.to_vec())),
            Step::Fail(e) => Some(Err(e.to_owned())),
            Step::Hang => std::future::pending().await,
        }
    }
}

fn raw(status: u16, headers: &[(&str, &str)], steps: Vec<Step>) -> Answer<QaBody> {
    Answer::Passthrough(Upstream {
        status,
        labelled: true,
        headers: headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        body: Some(QaBody(steps.into())),
    })
}

async fn all(body: &mut TurnBody<QaBody>) -> Vec<Read> {
    let mut out = Vec::new();
    loop {
        let r = tokio::time::timeout(DEADLINE, read(body, std::future::pending()))
            .await
            .expect("a read answered");
        let end = !matches!(r, Read::Chunk(_));
        out.push(r);
        if end {
            return out;
        }
    }
}

#[tokio::test]
async fn an_error_answer_is_cores_labelled_json() {
    let a = turn_answer(
        Answer::<QaBody>::Error(Refusal {
            status: 409,
            message: "qa refused".into(),
            code: Some("reconnect".into()),
        }),
        None,
    )
    .unwrap();
    assert_eq!(a.status, 409);
    assert_eq!(
        a.headers,
        [
            ("content-type".to_owned(), "application/json".to_owned()),
            ("x-noevia-provider-message".to_owned(), "1".to_owned()),
        ]
    );
    let (text, capped) = text(a.body, 1024, std::future::pending()).await;
    assert_eq!(
        text,
        r#"{"error":{"message":"qa refused","code":"reconnect"}}"#
    );
    assert!(!capped);
}

#[tokio::test]
async fn a_completion_is_one_json_body() {
    let mut a = turn_answer(Answer::<QaBody>::Completion("{\"id\":\"qa\"}".into()), None).unwrap();
    assert_eq!(a.status, 200);
    assert_eq!(
        a.headers,
        [("content-type".to_owned(), "application/json".to_owned())]
    );
    assert_eq!(
        all(&mut a.body).await,
        [Read::Chunk(b"{\"id\":\"qa\"}".to_vec()), Read::End]
    );
    // Read once: later reads end.
    assert_eq!(read(&mut a.body, std::future::pending()).await, Read::End);
}

#[tokio::test]
async fn an_empty_completion_ends_at_once() {
    let mut a = turn_answer(Answer::<QaBody>::Completion(String::new()), None).unwrap();
    assert_eq!(all(&mut a.body).await, [Read::End]);
}

#[test]
fn a_rejected_fetch_is_the_requests_error() {
    let r = turn_answer(Answer::<QaBody>::Thrown("fetch failed".into()), None);
    assert_eq!(r.err().as_deref(), Some("fetch failed"));
}

#[test]
fn a_pass_through_keeps_its_status_and_allow_listed_headers_only() {
    let a = turn_answer(
        raw(
            429,
            &[
                ("Content-Type", "application/json"),
                ("x-noevia-provider-message", "1"),
                ("set-cookie", "qa=1"),
                ("www-authenticate", "Bearer"),
                ("content-encoding", "identity"),
                ("x-noevia-gate-refusal", "1"),
                ("x-backend-qa", "1"),
                ("content-length", "2"),
            ],
            vec![Step::Chunk(b"{}")],
        ),
        None,
    )
    .unwrap();
    assert_eq!(a.status, 429);
    assert_eq!(
        a.headers,
        [
            ("content-type".to_owned(), "application/json".to_owned()),
            ("x-noevia-provider-message".to_owned(), "1".to_owned()),
        ]
    );
}

#[tokio::test]
async fn a_pass_through_body_reads_to_its_end_or_its_failure() {
    let mut a = turn_answer(
        raw(200, &[], vec![Step::Chunk(b"a"), Step::Chunk(b"b")]),
        None,
    )
    .unwrap();
    assert_eq!(
        all(&mut a.body).await,
        [
            Read::Chunk(b"a".to_vec()),
            Read::Chunk(b"b".to_vec()),
            Read::End
        ]
    );
    let mut a = turn_answer(
        raw(200, &[], vec![Step::Chunk(b"a"), Step::Fail("terminated")]),
        None,
    )
    .unwrap();
    assert_eq!(
        all(&mut a.body).await,
        [
            Read::Chunk(b"a".to_vec()),
            Read::Failed("terminated".into())
        ]
    );
    // A failed body is let go: later reads end.
    assert_eq!(read(&mut a.body, std::future::pending()).await, Read::End);
}

#[tokio::test]
async fn a_pass_through_without_a_body_ends_at_once() {
    let mut a = turn_answer(
        Answer::Passthrough(Upstream::<QaBody> {
            status: 204,
            labelled: true,
            headers: Vec::new(),
            body: None,
        }),
        None,
    )
    .unwrap();
    assert_eq!(a.status, 204);
    assert_eq!(all(&mut a.body).await, [Read::End]);
}

#[tokio::test]
async fn text_is_capped_and_says_so() {
    let a = turn_answer(
        raw(
            200,
            &[],
            vec![Step::Chunk(b"abcd"), Step::Chunk(b"efgh"), Step::Hang],
        ),
        None,
    )
    .unwrap();
    // The cap is passed after the second chunk: no third read (it would hang).
    let (t, capped) = tokio::time::timeout(DEADLINE, text(a.body, 6, std::future::pending()))
        .await
        .unwrap();
    assert_eq!((t.as_str(), capped), ("abcdef", true));
    let a = turn_answer(raw(200, &[], vec![Step::Chunk(b"abcdef")]), None).unwrap();
    let (t, capped) = text(a.body, 6, std::future::pending()).await;
    assert_eq!((t.as_str(), capped), ("abcdef", false));
}

#[tokio::test]
async fn text_ends_with_what_came_when_a_read_fails() {
    let a = turn_answer(
        raw(
            500,
            &[],
            vec![Step::Chunk(b"part"), Step::Fail("terminated")],
        ),
        None,
    )
    .unwrap();
    let (t, capped) = text(a.body, 1024, std::future::pending()).await;
    assert_eq!((t.as_str(), capped), ("part", false));
}

#[tokio::test]
async fn the_client_leaving_aborts_a_waiting_read() {
    let mut a = turn_answer(raw(200, &[], vec![Step::Chunk(b"a"), Step::Hang]), None).unwrap();
    assert_eq!(
        read(&mut a.body, std::future::pending()).await,
        Read::Chunk(b"a".to_vec())
    );
    let r = tokio::time::timeout(DEADLINE, read(&mut a.body, std::future::ready(())))
        .await
        .unwrap();
    assert_eq!(r, Read::Aborted);
    // The body was let go.
    assert_eq!(read(&mut a.body, std::future::pending()).await, Read::End);
}

#[tokio::test]
async fn the_client_leaving_ends_text_with_what_came() {
    let a = turn_answer(raw(200, &[], vec![Step::Chunk(b"a"), Step::Hang]), None).unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let left = async move {
        let _ = rx.await;
    };
    let reading = tokio::spawn(async move { text(a.body, 1024, left).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = tx.send(());
    let (t, capped) = tokio::time::timeout(DEADLINE, reading)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((t.as_str(), capped), ("a", false));
}

#[tokio::test]
async fn a_read_after_the_deadline_fails_with_the_abort_text() {
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut a = turn_answer(
        raw(200, &[], vec![Step::Chunk(b"a"), Step::Hang]),
        Some(deadline),
    )
    .unwrap();
    assert_eq!(
        read(&mut a.body, std::future::pending()).await,
        Read::Chunk(b"a".to_vec())
    );
    let r = tokio::time::timeout(DEADLINE, read(&mut a.body, std::future::pending()))
        .await
        .unwrap();
    assert_eq!(r, Read::Failed(ABORTED.to_owned()));
    assert_eq!(read(&mut a.body, std::future::pending()).await, Read::End);
}

#[test]
fn debug_names_the_kind_only() {
    let a = turn_answer(Answer::<QaBody>::Completion("secret-qa".into()), None).unwrap();
    let d = format!("{:?}", a.body);
    assert!(d.contains("bytes"), "{d}");
    assert!(!d.contains("secret-qa"), "{d}");
}

#[tokio::test]
async fn a_passed_deadline_wins_over_waiting_data_every_time() {
    // The deadline is already past and data is ready: the read fails, every time (the order of a
    // tie is fixed), and the body is let go.
    for _ in 0..64 {
        let past = Instant::now();
        let mut a = turn_answer(
            raw(200, &[], vec![Step::Chunk(b"a"), Step::Chunk(b"b")]),
            Some(past),
        )
        .unwrap();
        assert_eq!(
            read(&mut a.body, std::future::pending()).await,
            Read::Failed(ABORTED.to_owned())
        );
        for _ in 0..4 {
            assert_eq!(read(&mut a.body, std::future::pending()).await, Read::End);
        }
    }
}

#[tokio::test]
async fn the_client_leaving_wins_over_a_passed_deadline_and_waiting_data() {
    for _ in 0..64 {
        let mut a =
            turn_answer(raw(200, &[], vec![Step::Chunk(b"a")]), Some(Instant::now())).unwrap();
        assert_eq!(
            read(&mut a.body, std::future::ready(())).await,
            Read::Aborted
        );
        assert_eq!(read(&mut a.body, std::future::pending()).await, Read::End);
    }
}
