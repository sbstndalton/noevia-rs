//! S7 re-review P2-2 (6ed3633b): checking an event line allocates nothing the shared holdback budget
//! does not cover. Measured on the process's own peak resident memory (Linux), alone in this binary so
//! nothing else moves it: a line whose key is far larger than the budget passes through the front, with
//! the budget spent (the held start and the line's end checked apart) and as a whole line with escaped
//! keys, and the peak grows by much less than that key. Synthetic data only.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use axum::body::Body;
use bytes::Bytes;
use http_body_util::BodyExt;
use noevia_server::chat_wire::{EventWire, HOLDBACK, HOLDBACK_BUDGET};
use std::time::Duration;

type Chan = http_body_util::channel::Channel<Bytes, std::io::Error>;

const KEY: usize = 96 * 1024 * 1024;
const SLACK: usize = 16 * 1024 * 1024;

fn status(field: &str) -> usize {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = s.lines().find(|l| l.starts_with(field)).unwrap();
    let kib: usize = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    kib * 1024
}

/// Resets the peak to what is resident now, and answers that.
fn reset_peak() -> usize {
    std::fs::write("/proc/self/clear_refs", "5").unwrap();
    status("VmRSS:")
}

/// What a stream sends until it has nothing more for a while (or ends).
async fn drain(body: &mut Body) -> (usize, bool) {
    let mut len = 0;
    loop {
        match tokio::time::timeout(Duration::from_millis(300), body.frame()).await {
            Err(_) => return (len, false),
            Ok(None) => return (len, true),
            Ok(Some(f)) => len += f.unwrap().into_data().map_or(0, |d| d.len()),
        }
    }
}

fn stream() -> (http_body_util::channel::Sender<Bytes, std::io::Error>, Body) {
    let (tx, body) = Chan::new(1);
    (tx, Body::new(EventWire::new(Body::new(body))))
}

#[tokio::test]
async fn checking_a_line_allocates_nothing_outside_the_budget() {
    // 1. The budget spent while a line is held: its end, carrying a key far larger than the budget,
    //    cannot join the held buffer and is checked where it is.
    let (mut tx, mut body) = stream();
    tx.send_data(Bytes::from_static(b"{\"type\":\"error\",\""))
        .await
        .unwrap();
    assert_eq!(drain(&mut body).await, (0, false), "held");
    let mut spenders = Vec::new();
    for _ in 0..HOLDBACK_BUDGET / HOLDBACK + 1 {
        let (mut stx, mut sbody) = stream();
        let mut start = b"{\"t\":\"".to_vec();
        start.resize(HOLDBACK - 64, b's');
        stx.send_data(Bytes::from(start)).await.unwrap();
        drain(&mut sbody).await;
        spenders.push((stx, sbody));
    }
    let mut end = vec![b'k'; KEY];
    end.extend_from_slice(b"\":1}\n");
    let end = Bytes::from(end);
    let base = reset_peak();
    tx.send_data(end).await.unwrap();
    drop(tx);
    let (len, ended) = drain(&mut body).await;
    let grew = status("VmHWM:").saturating_sub(base);
    assert!(ended, "the error completed the turn: a clean end");
    assert_eq!(len, 6 + 17 + KEY + 4 + 2);
    assert!(
        grew < SLACK,
        "checking the line took {grew} bytes beyond the budget"
    );
    drop((body, spenders));

    // 2. A whole line too large for a reserved copy, with escaped keys (a decoder would copy them).
    let (mut tx, mut body) = stream();
    let mut line = Vec::with_capacity(KEY + 64);
    line.extend_from_slice(b"{\"type\":\"error\",\"");
    while line.len() < KEY {
        line.extend_from_slice(b"\\n");
    }
    line.extend_from_slice(b"\":1}\n");
    let line = Bytes::from(line);
    let want = line.len() - 1 + 8;
    let base = reset_peak();
    tx.send_data(line).await.unwrap();
    drop(tx);
    let (len, ended) = drain(&mut body).await;
    let grew = status("VmHWM:").saturating_sub(base);
    assert!(ended);
    assert_eq!(len, want);
    assert!(
        grew < SLACK,
        "checking the line took {grew} bytes beyond the budget"
    );
}
