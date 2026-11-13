//! What the Node fixtures cannot show: the [`Blocking`] driver on a plain std thread and on a
//! `spawn_blocking` thread, a cancel winning over a body that is always ready, and the D4 caps
//! on the SSE buffers (stricter than Node, see the crate doc).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use js_json::JValue;
use role_engine::*;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

const MODEL: &str = "qa-coder-q4";

/// A body that hands out `chunks`, then (when `forever` is set) the last chunk again and again;
/// on read number `cancel_at` it cancels `cancel` (and stays ready).
struct ScriptBody {
    chunks: VecDeque<Vec<u8>>,
    forever: Option<Vec<u8>>,
    reads: Arc<AtomicU32>,
    cancel_at: Option<(u32, Cancel)>,
    cancelled: Arc<Mutex<bool>>,
}

impl Body for ScriptBody {
    async fn read(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        let n = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some((at, c)) = &self.cancel_at {
            if n == *at {
                c.cancel();
            }
        }
        // A runaway guard: an engine that never sees the cancel ends here instead of hanging.
        if n > 100_000 {
            return Ok(None);
        }
        Ok(self.chunks.pop_front().or_else(|| self.forever.clone()))
    }
    fn cancel(&mut self) {
        *self.cancelled.lock().unwrap() = true;
    }
}

/// One local engine whose POST answers with the body `make` builds.
struct Local {
    make: Box<dyn Fn() -> ScriptBody + Send + Sync>,
    log: Mutex<Vec<JValue>>,
}

impl Deps for Local {
    type Body = ScriptBody;
    fn engine(&self) -> Option<Endpoint> {
        Some(Endpoint {
            base_url: Some("http://engine.invalid/v1".into()),
            model: Some(MODEL.into()),
            ..Endpoint::default()
        })
    }
    async fn fetch(
        &self,
        _req: Request,
        _cancel: Cancel,
    ) -> Result<Response<ScriptBody>, TransportError> {
        Ok(Response {
            status: 200,
            body: (self.make)(),
        })
    }
    async fn keep(&self) -> Option<Vec<String>> {
        Some(Vec::new())
    }
    async fn loaded_models(&self) -> Option<Result<JValue, ()>> {
        Some(Ok(JValue::Arr(vec![MODEL.into()])))
    }
    fn system_roles(&self) -> HashMap<String, String> {
        HashMap::new()
    }
    fn log(&self, entry: JValue) {
        self.log.lock().unwrap().push(entry);
    }
    fn now(&self) -> f64 {
        0.0
    }
}

fn delta(content: &str) -> Vec<u8> {
    format!("data: {{\"choices\":[{{\"delta\":{{\"content\":{content:?}}}}}]}}\n\n").into_bytes()
}

fn reasoning() -> Vec<u8> {
    b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n\n".to_vec()
}

fn body(chunks: Vec<Vec<u8>>) -> ScriptBody {
    ScriptBody {
        chunks: chunks.into(),
        forever: None,
        reads: Arc::default(),
        cancel_at: None,
        cancelled: Arc::default(),
    }
}

fn state() -> JValue {
    js_json::wtf16::parse(
        r#"{"taskId":"qa-task-1","tenantId":"qa-tenant","revision":1,"request":"Add a median helper to the QA repo.","projectInstructions":"Keep functions small.","capabilities":[{"name":"read"}],"plan":{"goal":"median helper","steps":[{"do":"write median","done_when":"tests pass"}]}}"#,
    )
    .unwrap()
}

fn schema() -> JValue {
    js_json::wtf16::parse(
        r#"{"type":"object","additionalProperties":false,"required":["answer"],"properties":{"answer":{"type":"string"}}}"#,
    )
    .unwrap()
}

/// Pins and makes one planner call; `Ok(text)` or `Err(code)`.
async fn one_call(engine: &RoleEngine<Local>, cancel: Option<Cancel>) -> Result<String, String> {
    let pin = PinRequest {
        task_id: "qa-task-1".into(),
        ..PinRequest::default()
    };
    let session = engine.pin_model(pin).await.map_err(|f| f.code)?;
    let mut req = CallRequest::new("planner", state(), "Act as the QA planner.", schema());
    req.cancel = cancel;
    session.call(req).await.map(|a| a.text).map_err(|f| f.code)
}

fn answering() -> RoleEngine<Local> {
    RoleEngine::new(Local {
        make: Box::new(|| {
            body(vec![
                delta(r#"{"answer":"#),
                delta(r#""QA"}"#),
                b"data: [DONE]\n\n".to_vec(),
            ])
        }),
        log: Mutex::default(),
    })
}

#[test]
fn blocking_drives_calls_on_a_plain_std_thread() {
    let driver = Arc::new(Blocking::new().unwrap());
    let engine = Arc::new(answering());
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let (driver, engine) = (driver.clone(), engine.clone());
            std::thread::spawn(move || {
                // The same driver serves every call (and both threads).
                (0..3)
                    .map(|_| driver.block_on(one_call(&engine, None)))
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    for t in threads {
        for r in t.join().unwrap() {
            assert_eq!(r.as_deref(), Ok(r#"{"answer":"QA"}"#));
        }
    }
}

#[test]
fn blocking_drives_calls_on_a_spawn_blocking_thread() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let engine = Arc::new(answering());
    let handle = rt.handle().clone();
    let (a, b) = rt.block_on(async move {
        let (e1, e2) = (engine.clone(), engine);
        // Inside the runtime (where Handle::try_current succeeds) on a blocking thread: an own
        // driver, and one on the surrounding runtime's handle.
        let own = tokio::task::spawn_blocking(move || {
            let driver = Blocking::new().unwrap();
            (
                driver.block_on(one_call(&e1, None)),
                driver.block_on(one_call(&e1, None)),
            )
        });
        let shared = tokio::task::spawn_blocking(move || {
            Blocking::with_handle(handle).block_on(one_call(&e2, None))
        });
        (own.await.unwrap(), shared.await.unwrap())
    });
    assert_eq!(a.0.as_deref(), Ok(r#"{"answer":"QA"}"#));
    assert_eq!(a.1.as_deref(), Ok(r#"{"answer":"QA"}"#));
    assert_eq!(b.as_deref(), Ok(r#"{"answer":"QA"}"#));
}

#[test]
fn a_cancel_wins_over_an_always_ready_reasoning_body() {
    let cancel = Cancel::new();
    let reads = Arc::new(AtomicU32::new(0));
    let cancelled = Arc::new(Mutex::new(false));
    let (c, r, k) = (cancel.clone(), reads.clone(), cancelled.clone());
    let engine = RoleEngine::new(Local {
        make: Box::new(move || ScriptBody {
            chunks: VecDeque::new(),
            forever: Some(reasoning()),
            reads: r.clone(),
            cancel_at: Some((3, c.clone())),
            cancelled: k.clone(),
        }),
        log: Mutex::default(),
    });
    let got = Blocking::new()
        .unwrap()
        .block_on(one_call(&engine, Some(cancel)));
    assert_eq!(got, Err("aborted".to_owned()));
    // Read 3 cancelled; the next loop turn sees it before reading again.
    assert_eq!(reads.load(Ordering::SeqCst), 3);
    assert!(*cancelled.lock().unwrap(), "the body is cancelled");
}

fn meter_run(chunks: Vec<Vec<u8>>) -> (SseEnd<()>, Meter, bool, u32) {
    let mut b = body(chunks);
    let mut m = Meter::default();
    let end = Blocking::new().unwrap().block_on(sse_deltas(
        &mut b,
        &mut m,
        &|| 0.0,
        &Cancel::new(),
        |_| None::<()>,
    ));
    let cancelled = *b.cancelled.lock().unwrap();
    (end, m, cancelled, b.reads.load(Ordering::SeqCst))
}

#[test]
fn d4_an_unterminated_line_past_4_mib_is_a_transport_fault() {
    let mib = vec![b'x'; 1024 * 1024];
    // Exactly 4 MiB without a newline is still held; one byte more is not.
    let mut chunks = vec![mib.clone(); 4];
    chunks.push(b"\n".to_vec());
    let (end, ..) = meter_run(chunks);
    assert_eq!(end, SseEnd::Complete);
    let mut chunks = vec![mib; 4];
    chunks.push(b"y".to_vec());
    chunks.push(delta("never read"));
    let (end, m, cancelled, reads) = meter_run(chunks);
    assert_eq!(end, SseEnd::Transport);
    assert!(cancelled);
    assert_eq!((reads, m.deltas), (5, 0));
    assert_eq!(MAX_SSE_LINE_BYTES, 4 * 1024 * 1024);
}

#[test]
fn d4_a_line_arriving_in_small_chunks_is_cut_once() {
    // 64 KiB of one-byte chunks then a delta: linear, and the line is still read.
    let mut chunks: Vec<Vec<u8>> = (0..64 * 1024).map(|_| b"x".to_vec()).collect();
    chunks.push(b"\n".to_vec());
    chunks.push(delta("QA"));
    let mut b = body(chunks);
    let mut m = Meter::default();
    let mut got = Vec::new();
    let end = Blocking::new().unwrap().block_on(sse_deltas(
        &mut b,
        &mut m,
        &|| 0.0,
        &Cancel::new(),
        |d| {
            got.push(d);
            None::<()>
        },
    ));
    assert_eq!((end, got), (SseEnd::Complete, vec!["QA".to_owned()]));
}

#[test]
fn d4_more_than_64_mib_in_one_attempt_is_a_transport_fault() {
    // Endless comment lines (each one complete): only the per-attempt total stops them.
    let mut line = vec![b':'; 1024 * 1024 - 1];
    line.push(b'\n');
    let (end, m, cancelled, reads) = meter_run(vec![line; 80]);
    assert_eq!(end, SseEnd::Transport);
    assert!(cancelled);
    assert_eq!(reads, 65);
    assert!(m.bytes > MAX_SSE_ATTEMPT_BYTES);
}

#[test]
fn d4_a_call_past_the_line_cap_fails_as_error() {
    let engine = RoleEngine::new(Local {
        make: Box::new(|| {
            let mut b = body(Vec::new());
            b.forever = Some(vec![b'x'; 1024 * 1024]);
            b
        }),
        log: Mutex::default(),
    });
    let got = Blocking::new().unwrap().block_on(one_call(&engine, None));
    assert_eq!(got, Err("error".to_owned()));
    let log = engine.deps().log.lock().unwrap();
    let failed = log
        .iter()
        .find(|e| e.get("event").as_str() == Some("role.call_failed"))
        .unwrap();
    assert_eq!(failed.get("code").as_str(), Some("transport"));
    assert_eq!(failed.get("status"), &JValue::Null);
}
