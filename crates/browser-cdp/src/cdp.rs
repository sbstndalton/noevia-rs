//! One DevTools connection: the writer behind a mutex, replies matched to callers by `id`, and one
//! reader thread handing every event to a sink. Every call has a deadline; a dropped connection
//! fails every waiter at once.

use crate::ws::{self, Incoming, MessageReader};
use browser_task::engine::{EngineError, Result};
use js_json::JValue;
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Playwright's message for a call on a page, context or browser that is gone.
pub(crate) const CLOSED: &str = "Target page, context or browser has been closed";

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn timeout_error(timeout: Duration) -> EngineError {
    EngineError::new(format!("Timeout {}ms exceeded.", timeout.as_millis()))
}

type Reply = std::result::Result<JValue, String>;

/// An event: the session it came on (`None`: the browser's own), its method and params.
pub(crate) type Sink = Box<dyn Fn(Option<&str>, &str, &JValue) + Send>;

#[derive(Default)]
struct Waiters {
    /// id → (method, session, caller).
    map: HashMap<u64, (String, Option<String>, Sender<Reply>)>,
    dead: bool,
}

pub(crate) struct Conn {
    writer: Mutex<Option<TcpStream>>,
    next_id: AtomicU64,
    waiters: Mutex<Waiters>,
}

impl Conn {
    /// A connection over an upgraded stream; [`Conn::start`] begins reading.
    pub(crate) fn new(stream: TcpStream) -> Arc<Conn> {
        Arc::new(Conn {
            writer: Mutex::new(Some(stream)),
            next_id: AtomicU64::new(1),
            waiters: Mutex::default(),
        })
    }

    /// The reader thread: replies to their callers, events to `sink`, then `gone` once it ends
    /// (however it ends: a panic in an event handler too fails every waiter).
    pub(crate) fn start(
        self: &Arc<Conn>,
        stream: TcpStream,
        sink: Sink,
        gone: Box<dyn FnOnce() + Send>,
    ) -> std::io::Result<()> {
        struct Ended(Arc<Conn>, Option<Box<dyn FnOnce() + Send>>);
        impl Drop for Ended {
            fn drop(&mut self) {
                self.0.close();
                self.0.fail_all();
                if let Some(gone) = self.1.take() {
                    gone();
                }
            }
        }
        let ended = Ended(self.clone(), Some(gone));
        std::thread::Builder::new()
            .name("browser-cdp-reader".into())
            .spawn(move || {
                let conn = ended.0.clone();
                let mut input = BufReader::with_capacity(64 * 1024, stream);
                let mut reader = MessageReader::default();
                loop {
                    match reader.next(&mut input, ws::MAX_MESSAGE) {
                        Ok(Incoming::Text(text)) => conn.dispatch(&text, &sink),
                        Ok(Incoming::Ping(payload)) => {
                            let _ = conn.write(ws::PONG, &payload);
                        }
                        Ok(Incoming::Oversize) => {}
                        Ok(Incoming::Close) | Err(_) => break,
                    }
                }
                drop(ended);
            })
            .map(|_| ())
    }

    fn dispatch(&self, text: &str, sink: &Sink) {
        let Ok(message) = js_json::wtf16::parse(text) else {
            return;
        };
        if let Some(id) = message.get("id").as_num() {
            let waiter = lock(&self.waiters).map.remove(&(id as u64));
            if let Some((method, _, tx)) = waiter {
                let error = message.get("error");
                let reply = if error.is_object() {
                    Err(format!(
                        "Protocol error ({method}): {}",
                        error.get("message").as_str().unwrap_or("unknown error")
                    ))
                } else {
                    Ok(message.get("result").clone())
                };
                let _ = tx.send(reply);
            }
        } else if let Some(method) = message.get("method").as_str() {
            sink(
                message.get("sessionId").as_str(),
                method,
                message.get("params"),
            );
        }
    }

    fn write(&self, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
        let frame = ws::encode(opcode, payload, ws::random::<4>()?);
        let mut writer = lock(&self.writer);
        let stream = writer
            .as_mut()
            .ok_or_else(|| std::io::Error::other(CLOSED))?;
        stream.write_all(&frame)
    }

    fn message(id: u64, session: Option<&str>, method: &str, params: JValue) -> Option<String> {
        let mut items = vec![
            ("id".to_string(), JValue::Num(id as f64)),
            ("method".into(), JValue::Str(method.into())),
            ("params".into(), params),
        ];
        if let Some(s) = session {
            items.push(("sessionId".into(), JValue::Str(s.into())));
        }
        js_json::wtf16::stringify(&JValue::Obj(items))
    }

    /// `method` on `session` (`None`: the browser), waiting at most `timeout` for its result.
    pub(crate) fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: JValue,
        timeout: Duration,
    ) -> Result<JValue> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let text = Self::message(id, session, method, params)
            .ok_or_else(|| EngineError::new("Unserialisable protocol message"))?;
        let (tx, rx) = channel();
        {
            let mut waiters = lock(&self.waiters);
            if waiters.dead {
                return Err(EngineError::new(CLOSED));
            }
            waiters
                .map
                .insert(id, (method.to_string(), session.map(str::to_string), tx));
        }
        if self.write(ws::TEXT, text.as_bytes()).is_err() {
            lock(&self.waiters).map.remove(&id);
            return Err(EngineError::new(CLOSED));
        }
        match rx.recv_timeout(timeout) {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(message)) => Err(EngineError::new(message)),
            Err(_) => {
                if lock(&self.waiters).map.remove(&id).is_some() {
                    return Err(timeout_error(timeout));
                }
                // Answered (or failed) between the deadline and the removal.
                match rx.try_recv() {
                    Ok(Ok(result)) => Ok(result),
                    Ok(Err(message)) => Err(EngineError::new(message)),
                    Err(_) => Err(EngineError::new(CLOSED)),
                }
            }
        }
    }

    /// `method` without waiting (its answer is dropped): for the reader thread, which must never
    /// wait on a reply it would have to read itself.
    pub(crate) fn send(&self, session: Option<&str>, method: &str, params: JValue) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        if let Some(text) = Self::message(id, session, method, params) {
            let _ = self.write(ws::TEXT, text.as_bytes());
        }
    }

    pub(crate) fn is_dead(&self) -> bool {
        lock(&self.waiters).dead
    }

    fn fail_all(&self) {
        let mut waiters = lock(&self.waiters);
        waiters.dead = true;
        for (_, (_, _, tx)) in waiters.map.drain() {
            let _ = tx.send(Err(CLOSED.to_string()));
        }
    }

    /// Fails every call waiting on `session` (it detached: no answer will come).
    pub(crate) fn fail_session(&self, session: &str) {
        let mut waiters = lock(&self.waiters);
        let ids: Vec<u64> = waiters
            .map
            .iter()
            .filter(|(_, (_, s, _))| s.as_deref() == Some(session))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some((_, _, tx)) = waiters.map.remove(&id) {
                let _ = tx.send(Err(CLOSED.to_string()));
            }
        }
    }

    /// Disconnects (a close frame, then the socket shut both ways); the reader then ends.
    pub(crate) fn close(&self) {
        let stream = lock(&self.writer).take();
        if let Some(mut stream) = stream {
            if let Ok(mask) = ws::random::<4>() {
                let _ = stream.write_all(&ws::encode(ws::CLOSE, &1000u16.to_be_bytes(), mask));
            }
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}
