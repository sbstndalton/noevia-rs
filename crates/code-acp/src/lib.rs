//! noevia-core `server/code-acp.cjs` in Rust (R7): the minimal Agent Client Protocol client a
//! Code task's harness is driven through. JSON-RPC 2.0, one JSON object per line, over the
//! sandbox's TCP endpoint (`socket`: one noevia start line first, then the ACP stream) or a child
//! process's stdio (`spawn`, for a workstation). noevia is the client: it answers the agent's
//! permission requests, file reads and writes, and takes its session updates, through
//! [`Handlers`]; nothing here decides anything (the Code harness does). What this module does
//! decide is how the agent stops: `session/cancel`, then the connection closed (socket) or the
//! process group sent SIGTERM and, after a grace period, SIGKILL (spawn).
//!
//! Every line written is `JSON.stringify(message)` in Node's key order, so a transcript of this
//! client and of Node's is the same bytes (tests/replay.rs, recorded from code-acp.cjs).
//!
//! **Differences from Node** (tested where they can be):
//! 1. The 16 MiB line cap counts bytes, not UTF-16 units (Node measured the decoded buffer).
//! 2. A line `JSON.parse` reads but this port cannot hold (a character of U+10F800..U+10FFFF, a
//!    number past the `f64` range, nesting deeper than about 128 levels) is logged as not
//!    JSON-RPC and dropped.
//! 3. The claude-code harness's pinned settings are read without following a link below the
//!    task's home (the harness owns it), and only up to 1 MiB; Node followed any link it found
//!    there and sent what it read to the agent.
//! 4. A port past 65535 is refused as a bad endpoint (Node threw when connecting); connecting is
//!    bounded by the handshake deadline and stops at a cancel; a task cancelled before it connects
//!    never connects.
//! 5. At most [`MAX_IN_FLIGHT`] of the agent's requests wait on a handler at once; the rest are
//!    answered `-32603` "Too many requests at once." (Node started one async handler per request,
//!    without limit).
//! 6. Only a string method names a handler: a method that is an object property name in JS
//!    (`constructor`, `toString`, …) or a one-element array is unsupported (`-32601`), and a
//!    `null` line is ignored rather than logged with a stack trace. Replies to requests handled at
//!    the same time may go out in either order.
//! 7. A spawned agent is put in its own process group, not its own session (`setsid` needs code
//!    this crate does not allow); its exit, not its stdout closing, ends the connection, and it is
//!    reaped.
//!
//! Writes go through a queue and a writer thread, as Node's socket buffers them (unbounded, as
//! there: an agent that stops reading while flooding requests grows it): a write blocked on an
//! agent that stopped reading never holds up a cancel. A stop gives queued lines (a
//! `session/cancel`) a moment to go out, then closes. Dropping a [`Connection`] stops the agent.
//!
//! Requests from the agent are each answered on their own thread, as Node's async handlers run
//! concurrently: a permission waiting for a person does not hold up session updates.

#![forbid(unsafe_code)]

pub mod env;

use js_json::{wtf16, JValue};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use env::{
    agent_env, read_agent, read_claude_settings, session_metadata, split_endpoint, Proxy,
};

/// ACP `protocolVersion` noevia speaks.
pub const PROTOCOL_VERSION: f64 = 1.0;
/// From `session/cancel` to SIGKILL for a spawned agent.
pub const GRACE: Duration = Duration::from_secs(5);
/// How long the handshake may take.
pub const HANDSHAKE: Duration = Duration::from_secs(30);
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
const INVALID_PARAMS: f64 = -32602.0;
const INTERNAL: f64 = -32603.0;

/// A failure, as Node's `Error` carried it: the message, the public message a route may show,
/// a status for refusals before any connection, and the JSON-RPC `code` of an agent's error reply.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    pub message: String,
    pub public_message: Option<String>,
    pub status: Option<u16>,
    pub code: Option<JValue>,
}

impl Error {
    fn new(message: impl Into<String>) -> Error {
        Error {
            message: message.into(),
            public_message: None,
            status: None,
            code: None,
        }
    }
    pub(crate) fn with_status(status: u16, message: impl Into<String>) -> Error {
        Error {
            status: Some(status),
            ..Error::new(message)
        }
    }
    fn public(message: impl Into<String>, public: &str) -> Error {
        Error {
            public_message: Some(public.to_string()),
            ..Error::new(message)
        }
    }
}

/// A handler's refusal: `code` -32602 (with optional `data` for the agent) is "invalid params",
/// anything else is answered as internal.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub code: Option<f64>,
    pub message: String,
    pub data: Option<JValue>,
}

/// What the agent calls on noevia (code-harness.cjs's session handlers).
pub trait Handlers: Send + Sync {
    /// `session/request_permission`: the selected outcome (`{ outcome, optionId }`); the
    /// protocol layer wraps it as `{ outcome: … }`.
    fn request_permission(&self, params: &JValue) -> Result<JValue, Refusal>;
    fn read_text_file(&self, params: &JValue) -> Result<JValue, Refusal>;
    fn write_text_file(&self, params: &JValue) -> Result<JValue, Refusal>;
    /// `session/update`; never answered, and a failure in it is not the task's.
    fn session_update(&self, update: &JValue);
}

type Hook = Box<dyn Fn() + Send + Sync>;

/// A task's cancellation: [`Cancel::cancel`] runs every registered hook once.
#[derive(Default)]
pub struct Cancel {
    cancelled: AtomicBool,
    hooks: Mutex<Vec<(u64, Hook)>>,
    next: AtomicU64,
}

impl Cancel {
    pub fn new() -> Arc<Cancel> {
        Arc::new(Cancel::default())
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    pub fn cancel(&self) {
        if self.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        let hooks: Vec<_> = self
            .hooks
            .lock()
            .map(|mut h| std::mem::take(&mut *h))
            .unwrap_or_default();
        for (_, hook) in hooks {
            hook();
        }
    }
    fn add(&self, hook: Hook) -> u64 {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut h) = self.hooks.lock() {
            h.push((id, hook));
        }
        id
    }
    fn remove(&self, id: u64) {
        if let Ok(mut h) = self.hooks.lock() {
            h.retain(|(i, _)| *i != id);
        }
    }
}

/// How the agent is reached.
#[derive(Debug, Clone)]
pub enum Transport {
    /// The sandbox's supervisor at `host:port` (CODE_HARNESS_ENDPOINT).
    Socket(String),
    /// A child process (CODE_HARNESS_COMMAND and its arguments).
    Spawn { command: String, args: Vec<String> },
}

/// `connectAcp`'s options.
pub struct Options {
    pub transport: Option<Transport>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub home: Option<String>,
    pub permission: Option<JValue>,
    pub harness: Option<String>,
    pub proxy: Option<Proxy>,
    pub model: Option<String>,
    /// The server's own `PATH` and `LANG` the agent gets (`None`: read from this process).
    pub base_path: Option<String>,
    pub base_lang: Option<String>,
    pub grace: Duration,
    pub handshake: Duration,
    pub log: Box<dyn Fn(&str) + Send + Sync>,
}

impl Options {
    pub fn new(transport: Transport, cwd: &str) -> Options {
        Options {
            transport: Some(transport),
            cwd: cwd.to_string(),
            env: Vec::new(),
            home: None,
            permission: None,
            harness: None,
            proxy: None,
            model: None,
            base_path: std::env::var("PATH").ok(),
            base_lang: std::env::var("LANG").ok(),
            grace: GRACE,
            handshake: HANDSHAKE,
            log: Box::new(|_| {}),
        }
    }
}

type Reply = mpsc::Sender<Result<JValue, Error>>;

/// How many of the agent's requests may be waiting on a handler at once.
pub const MAX_IN_FLIGHT: usize = 64;
/// How long a stop waits for queued lines (a `session/cancel`) to go out before closing.
const DRAIN: Duration = Duration::from_millis(250);

enum Stopper {
    Socket(TcpStream),
    Spawn {
        pid: rustix::process::Pid,
        grace: Duration,
    },
}

struct Shared {
    /// The writer thread's queue (Node's socket buffers its writes; a blocked write never holds
    /// up a cancel). `None` once stopped.
    out: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    written: Mutex<Option<mpsc::Receiver<()>>>,
    pending: Mutex<HashMap<u64, Reply>>,
    closed: Mutex<Option<Error>>,
    next_id: AtomicU64,
    stopper: Stopper,
    stopped: AtomicBool,
    exited: Arc<AtomicBool>,
    in_flight: AtomicUsize,
    log: Box<dyn Fn(&str) + Send + Sync>,
    handlers: Arc<dyn Handlers>,
}

impl Shared {
    fn closed(&self) -> Option<Error> {
        self.closed.lock().ok().and_then(|c| c.clone())
    }

    /// The connection is gone: every waiting request fails with `error`, once.
    fn fail(&self, error: Error) {
        let Ok(mut closed) = self.closed.lock() else {
            return;
        };
        if closed.is_some() {
            return;
        }
        *closed = Some(error.clone());
        drop(closed);
        let waiting: Vec<Reply> = self
            .pending
            .lock()
            .map(|mut p| p.drain().map(|(_, r)| r).collect())
            .unwrap_or_default();
        for reply in waiting {
            let _ = reply.send(Err(error.clone()));
        }
    }

    /// Queues one line; false once the connection is closed.
    fn send(&self, message: &JValue) -> bool {
        if self.closed().is_some() {
            return false;
        }
        let Some(mut line) = wtf16::stringify(message) else {
            return false;
        };
        line.push('\n');
        let Ok(out) = self.out.lock() else {
            return false;
        };
        out.as_ref()
            .is_some_and(|tx| tx.send(line.into_bytes()).is_ok())
    }

    fn notify(&self, method: &str, params: JValue) {
        self.send(&JValue::obj([
            ("jsonrpc", JValue::Str("2.0".into())),
            ("method", JValue::Str(method.into())),
            ("params", params),
        ]));
    }

    /// Sends a request; the receiver gets its result, or the connection's failure.
    fn request(&self, method: &str, params: JValue) -> mpsc::Receiver<Result<JValue, Error>> {
        let (tx, rx) = mpsc::channel();
        if let Some(closed) = self.closed() {
            let _ = tx.send(Err(closed));
            return rx;
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut p) = self.pending.lock() {
            p.insert(id, tx.clone());
        }
        let message = JValue::obj([
            ("jsonrpc", JValue::Str("2.0".into())),
            ("id", JValue::Num(id as f64)),
            ("method", JValue::Str(method.into())),
            ("params", params),
        ]);
        if !self.send(&message) {
            if let Ok(mut p) = self.pending.lock() {
                p.remove(&id);
            }
            let why = self
                .closed()
                .unwrap_or_else(|| Error::new("The coding harness stopped."));
            let _ = tx.send(Err(why));
        }
        rx
    }

    /// Closes the queue, gives what is in it a moment to go out, then closes the connection (a
    /// socket) or stops the agent's process group (SIGTERM, then SIGKILL after the grace).
    fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut out) = self.out.lock() {
            out.take();
        }
        if let Some(rx) = self.written.lock().ok().and_then(|mut w| w.take()) {
            let _ = rx.recv_timeout(DRAIN);
        }
        match &self.stopper {
            Stopper::Socket(stream) => {
                let _ = stream.shutdown(Shutdown::Both);
            }
            Stopper::Spawn { pid, grace } => {
                // The whole group, as Node signals `-pid`, even once the leader has exited: what it
                // started may still run, and while the group has a member its id cannot be reused.
                let pid = *pid;
                if rustix::process::kill_process_group(pid, rustix::process::Signal::TERM).is_err()
                    && !self.exited.load(Ordering::SeqCst)
                {
                    let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
                }
                let grace = *grace;
                let _ = std::thread::Builder::new().spawn(move || {
                    std::thread::sleep(grace);
                    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                });
            }
        }
    }

    fn reply_error(&self, id: &JValue, code: f64, message: &str, data: Option<JValue>) {
        let mut error = vec![
            ("code".to_string(), JValue::Num(code)),
            ("message".to_string(), JValue::Str(message.to_string())),
        ];
        if let Some(d) = data {
            error.push(("data".into(), d));
        }
        self.send(&JValue::obj([
            ("jsonrpc", JValue::Str("2.0".into())),
            ("id", id.clone()),
            ("error", JValue::Obj(error)),
        ]));
    }

    /// code-acp.cjs `handleIncoming`.
    fn incoming(self: &Arc<Self>, message: JValue) {
        let id = message.get("id").clone();
        let method = message.get("method").clone();
        if !matches!(id, JValue::Undefined) && matches!(method, JValue::Undefined) {
            let JValue::Num(n) = id else {
                return;
            };
            if n < 0.0 || n.fract() != 0.0 || n > 9e15 {
                return;
            }
            let waiting = self
                .pending
                .lock()
                .ok()
                .and_then(|mut p| p.remove(&(n as u64)));
            let Some(waiting) = waiting else {
                return;
            };
            let error = message.get("error");
            let answer = if error.truthy() {
                let text = match error.get("message") {
                    m if m.truthy() => js_json::to_js_string(m).unwrap_or_default(),
                    _ => "The harness reported an error.".into(),
                };
                let code = error.get("code");
                Err(Error {
                    code: (!matches!(code, JValue::Undefined)).then(|| code.clone()),
                    ..Error::new(text)
                })
            } else {
                Ok(message.get("result").clone())
            };
            let _ = waiting.send(answer);
            return;
        }
        let params = message.get("params");
        if method.as_str() == Some("session/update") {
            let update = match params.get("update") {
                u if u.truthy() => u.clone(),
                _ => params.clone(),
            };
            self.handlers.session_update(&update);
            return;
        }
        if matches!(id, JValue::Undefined) {
            return;
        }
        let which = match method.as_str() {
            Some("session/request_permission") => 0,
            Some("fs/read_text_file") => 1,
            Some("fs/write_text_file") => 2,
            _ => {
                self.reply_error(&id, -32601.0, "Method not supported by this client", None);
                return;
            }
        };
        let params = if params.truthy() {
            params.clone()
        } else {
            JValue::Obj(Vec::new())
        };
        // Bounded: an agent cannot make noevia start threads without limit.
        if self.in_flight.fetch_add(1, Ordering::SeqCst) >= MAX_IN_FLIGHT {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.reply_error(&id, INTERNAL, "Too many requests at once.", None);
            return;
        }
        let me = self.clone();
        let failed_id = id.clone();
        let spawned = std::thread::Builder::new()
            .name("code-acp request".into())
            .spawn(move || {
                // Given back however the handler ends, a panic included.
                struct Slot(Arc<Shared>);
                impl Drop for Slot {
                    fn drop(&mut self) {
                        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                let _slot = Slot(me.clone());
                let handlers = me.handlers.clone();
                let answer = match which {
                    0 => handlers
                        .request_permission(&params)
                        .map(|outcome| JValue::obj([("outcome", outcome)])),
                    1 => handlers.read_text_file(&params),
                    _ => handlers.write_text_file(&params),
                };
                match answer {
                    Ok(result) => {
                        let result = if result.is_nullish() {
                            JValue::Null
                        } else {
                            result
                        };
                        me.send(&JValue::obj([
                            ("jsonrpc", JValue::Str("2.0".into())),
                            ("id", id),
                            ("result", result),
                        ]));
                    }
                    Err(r) => {
                        let invalid = r.code == Some(INVALID_PARAMS);
                        let message = if r.message.is_empty() {
                            "refused".to_string()
                        } else {
                            r.message
                        };
                        let data = r
                            .data
                            .filter(|d| invalid && matches!(d, JValue::Obj(_) | JValue::Arr(_)));
                        me.reply_error(
                            &id,
                            if invalid { INVALID_PARAMS } else { INTERNAL },
                            &message,
                            data,
                        );
                    }
                }
            });
        if spawned.is_err() {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            self.reply_error(&failed_id, INTERNAL, "Too many requests at once.", None);
        }
    }
}

/// `String.prototype.trim`.
pub(crate) fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| {
        matches!(
            c,
            '\u{9}'..='\u{d}'
                | ' '
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

/// Writes queued lines in order until the queue closes or a write fails.
fn write_loop(
    shared: Arc<Shared>,
    rx: mpsc::Receiver<Vec<u8>>,
    mut to: Box<dyn Write + Send>,
    done: mpsc::Sender<()>,
) {
    for line in rx {
        if let Err(e) = to.write_all(&line).and_then(|()| to.flush()) {
            shared.fail(Error::new(e.to_string()));
            break;
        }
    }
    let _ = done.send(());
}

/// Reads lines until the stream ends, dispatching each. A socket's end closes the connection; a
/// child's stdout closing does not (its exit does, as Node's `exit` event).
fn read_loop(shared: Arc<Shared>, mut from: Box<dyn Read + Send>) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = match from.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buffer.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
        if buffer.len() > MAX_LINE_BYTES {
            buffer.clear();
            (shared.log)("The harness sent an oversized message.");
            continue;
        }
        while let Some(i) = buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=i).collect();
            let text = String::from_utf8_lossy(line.get(..i).unwrap_or(&[])).into_owned();
            let text = js_trim(&text);
            if text.is_empty() {
                continue;
            }
            match wtf16::parse(text) {
                Ok(message) => shared.incoming(message),
                Err(_) => (shared.log)("The harness sent something that is not JSON-RPC."),
            }
        }
    }
    if let Stopper::Socket(_) = shared.stopper {
        shared.fail(Error::public(
            "The coding sandbox closed the connection.",
            "The coding harness stopped unexpectedly.",
        ));
    }
}

/// Waits for the spawned agent to exit (reaping it), then closes the connection with
/// `The harness exited (${signal || code}).`.
fn wait_loop(shared: Arc<Shared>, mut child: Child) {
    use std::os::unix::process::ExitStatusExt;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break None,
        }
    };
    shared.exited.store(true, Ordering::SeqCst);
    let what = match status.map(|s| (s.signal(), s.code())) {
        Some((Some(sig), _)) => signal_name(sig),
        Some((None, Some(code))) => code.to_string(),
        _ => "null".into(),
    };
    shared.fail(Error::public(
        format!("The harness exited ({what})."),
        "The coding harness stopped unexpectedly.",
    ));
}

/// Node's names for the signals a harness dies of.
fn signal_name(sig: i32) -> String {
    let name = match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        10 => "SIGUSR1",
        11 => "SIGSEGV",
        12 => "SIGUSR2",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        24 => "SIGXCPU",
        25 => "SIGXFSZ",
        31 => "SIGSYS",
        n => return format!("signal {n}"),
    };
    name.to_string()
}

/// Node's errno name for a failed spawn (`spawn <command> ENOENT`).
fn errno_name(e: &std::io::Error) -> String {
    let name = match e.raw_os_error() {
        Some(2) => "ENOENT",
        Some(13) => "EACCES",
        Some(20) => "ENOTDIR",
        Some(8) => "ENOEXEC",
        Some(12) => "ENOMEM",
        Some(n) => return format!("E{n}"),
        None => "EUNKNOWN",
    };
    name.to_string()
}

/// Lossy UTF-8 of a byte stream read in chunks: a character split across two reads is kept whole
/// (Node's StringDecoder), bad bytes become U+FFFD.
struct Utf8Stream {
    carry: Vec<u8>,
}

impl Utf8Stream {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.carry.extend_from_slice(bytes);
        let keep = match std::str::from_utf8(&self.carry) {
            Ok(_) => 0,
            Err(e) if e.error_len().is_none() => self.carry.len() - e.valid_up_to(),
            Err(_) => 0,
        };
        let cut = self.carry.len() - keep;
        let text = String::from_utf8_lossy(self.carry.get(..cut).unwrap_or(&[])).into_owned();
        self.carry.drain(..cut);
        text
    }
}

/// An open ACP session. Dropping it stops the agent.
pub struct Connection {
    shared: Arc<Shared>,
    session_id: String,
    session_value: JValue,
    agent: JValue,
    model: Option<String>,
    cancel: Arc<Cancel>,
    hook: u64,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.cancel.remove(self.hook);
        self.shared.stop();
    }
}

impl Connection {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The session id as the agent gave it (a number stays a number on the wire).
    pub fn session(&self) -> &JValue {
        &self.session_value
    }

    /// What the agent said about itself at `initialize` (`{ name, version, protocolVersion }`).
    pub fn agent(&self) -> &JValue {
        &self.agent
    }

    /// `session/prompt`: the agent's final answer. The connection is stopped afterwards either way.
    pub fn prompt(&self, text: &str) -> Result<JValue, Error> {
        let mut params = vec![
            ("sessionId".to_string(), self.session_value.clone()),
            (
                "prompt".to_string(),
                JValue::Arr(vec![JValue::obj([
                    ("type", JValue::Str("text".into())),
                    ("text", JValue::Str(text.into())),
                ])]),
            ),
        ];
        if let Some(model) = self.model.as_ref().filter(|m| !m.is_empty()) {
            params.push((
                "_meta".into(),
                JValue::obj([(
                    "noevia",
                    JValue::obj([("model", JValue::Str(model.clone()))]),
                )]),
            ));
        }
        let rx = self.shared.request("session/prompt", JValue::Obj(params));
        let answer = rx
            .recv()
            .unwrap_or_else(|_| Err(Error::new("The coding harness stopped.")));
        self.cancel.remove(self.hook);
        self.shared.stop();
        answer
    }

    pub fn stop(&self) {
        self.shared.stop();
    }
}

fn cancelled() -> Error {
    Error::public("Cancelled", "The task was cancelled.")
}

/// Connects to `host:port` within `deadline`, giving up at once on a cancel.
fn reach(host: &str, port: u16, deadline: Instant, cancel: &Cancel) -> Result<TcpStream, Error> {
    use std::net::ToSocketAddrs;
    let (tx, rx) = mpsc::channel();
    let target = format!("{host}:{port}");
    let budget = deadline.saturating_duration_since(Instant::now());
    std::thread::Builder::new()
        .spawn(move || {
            let attempt = (|| {
                let mut last = std::io::Error::other("no address");
                for addr in target.to_socket_addrs()? {
                    match TcpStream::connect_timeout(&addr, budget.max(Duration::from_millis(1))) {
                        Ok(s) => return Ok(s),
                        Err(e) => last = e,
                    }
                }
                Err(last)
            })();
            let _ = tx.send(attempt);
        })
        .map_err(|e| Error::new(e.to_string()))?;
    loop {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Error::public(
                "The coding harness did not answer.",
                "The coding harness did not answer.",
            ));
        }
        match rx.recv_timeout(left.min(Duration::from_millis(25))) {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => {
                return Err(Error::public(
                    format!("The coding sandbox is unreachable ({e})."),
                    "The coding harness stopped unexpectedly.",
                ))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::new("The coding sandbox is unreachable."))
            }
        }
    }
}

/// code-acp.cjs `connectAcp`: the handshake (`initialize`, then `session/new` with no MCP
/// servers), refused or abandoned after `options.handshake` (connecting included).
pub fn connect(
    options: Options,
    handlers: Arc<dyn Handlers>,
    cancel: Arc<Cancel>,
) -> Result<Connection, Error> {
    let Some(transport) = options.transport.clone() else {
        return Err(Error::with_status(
            409,
            "No coding harness is configured on this server.",
        ));
    };
    // Read before the agent starts: a missing pin refuses, it is not a fallback.
    let settings = if options.harness.as_deref() == Some("claude-code") {
        Some(read_claude_settings(options.home.as_deref())?)
    } else {
        None
    };
    let child_env = agent_env(
        options.base_path.as_deref(),
        options.base_lang.as_deref(),
        &options.env,
        options.proxy.as_ref(),
        options.home.as_deref(),
    );
    if cancel.is_cancelled() {
        return Err(cancelled());
    }
    let deadline = Instant::now() + options.handshake;
    let log = options.log;
    let mut child_parts: Option<(Child, Option<std::process::ChildStderr>)> = None;
    let (writer, reader, stopper): (Box<dyn Write + Send>, Box<dyn Read + Send>, Stopper) =
        match transport {
            Transport::Socket(endpoint) => {
                let (host, port) = split_endpoint(&endpoint)?;
                let stream = reach(&host, port, deadline, &cancel)?;
                let broken = |e: std::io::Error| {
                    Error::public(
                        format!("The coding sandbox is unreachable ({e})."),
                        "The coding harness stopped unexpectedly.",
                    )
                };
                let w = stream.try_clone().map_err(broken)?;
                let r = stream.try_clone().map_err(broken)?;
                (Box::new(w), Box::new(r), Stopper::Socket(stream))
            }
            Transport::Spawn { command, args } => {
                let mut cmd = Command::new(&command);
                cmd.args(&args)
                    .current_dir(&options.cwd)
                    .env_clear()
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .process_group(0);
                if let JValue::Obj(items) = &child_env {
                    for (k, v) in items {
                        if let Some(v) = v.as_str() {
                            cmd.env(k, v);
                        }
                    }
                }
                let mut child = cmd.spawn().map_err(|e| {
                    Error::public(
                        format!("spawn {command} {}", errno_name(&e)),
                        "The coding harness stopped unexpectedly.",
                    )
                })?;
                let pid = rustix::process::Pid::from_child(&child);
                let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::new("The harness could not be started."));
                };
                let stderr = child.stderr.take();
                child_parts = Some((child, stderr));
                (
                    Box::new(stdin),
                    Box::new(stdout),
                    Stopper::Spawn {
                        pid,
                        grace: options.grace,
                    },
                )
            }
        };
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let shared = Arc::new(Shared {
        out: Mutex::new(Some(out_tx)),
        written: Mutex::new(Some(done_rx)),
        pending: Mutex::new(HashMap::new()),
        closed: Mutex::new(None),
        next_id: AtomicU64::new(1),
        stopper,
        stopped: AtomicBool::new(false),
        exited: Arc::new(AtomicBool::new(false)),
        in_flight: AtomicUsize::new(0),
        log,
        handlers,
    });
    let start = |what: &str, f: Box<dyn FnOnce() + Send>| {
        std::thread::Builder::new()
            .name(format!("code-acp {what}"))
            .spawn(f)
            .map(|_| ())
            .map_err(|e| Error::new(e.to_string()))
    };
    // The socket's first line is noevia's own start message; everything after it is ACP.
    if let Stopper::Socket(_) = shared.stopper {
        let mut line = wtf16::stringify(&JValue::obj([
            ("noevia", JValue::Str("start".into())),
            ("cwd", JValue::Str(options.cwd.clone())),
            ("env", child_env.clone()),
        ]))
        .unwrap_or_default();
        line.push('\n');
        if let Ok(out) = shared.out.lock() {
            if let Some(tx) = out.as_ref() {
                let _ = tx.send(line.into_bytes());
            }
        }
    }
    // A spawned child is reaped by its waiter; until that runs, a failure to start a thread
    // stops and reaps it here.
    let (child, stderr) = match child_parts {
        Some((child, stderr)) => (Some(child), stderr),
        None => (None, None),
    };
    let slot = Arc::new(Mutex::new(child));
    let give_up = |e: Error| {
        shared.stop();
        if let Some(mut child) = slot.lock().ok().and_then(|mut c| c.take()) {
            let _ = child.kill();
            let _ = child.wait();
        }
        e
    };
    let shared2 = shared.clone();
    start(
        "writer",
        Box::new(move || write_loop(shared2, out_rx, writer, done_tx)),
    )
    .map_err(give_up)?;
    let shared2 = shared.clone();
    start("reader", Box::new(move || read_loop(shared2, reader))).map_err(give_up)?;
    if let Some(mut stderr) = stderr {
        // What the agent writes to stderr is logged, a chunk at a time, as Node's onLog.
        let shared2 = shared.clone();
        start(
            "stderr",
            Box::new(move || {
                let mut buf = [0u8; 8192];
                let mut text = Utf8Stream { carry: Vec::new() };
                while let Ok(n) = stderr.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let chunk = text.push(buf.get(..n).unwrap_or(&[]));
                    if !chunk.is_empty() {
                        (shared2.log)(&wtf16::slice(&chunk, 2000));
                    }
                }
            }),
        )
        .map_err(give_up)?;
    }
    if slot.lock().map(|c| c.is_some()).unwrap_or(false) {
        let shared2 = shared.clone();
        let taken = slot.clone();
        start(
            "waiter",
            Box::new(move || {
                if let Some(child) = taken.lock().ok().and_then(|mut c| c.take()) {
                    wait_loop(shared2, child);
                }
            }),
        )
        .map_err(give_up)?;
    }

    // Cancellation is wired before the handshake: an agent that never answers has no session
    // to cancel, and this is still the way to stop it.
    let session: Arc<Mutex<Option<JValue>>> = Arc::new(Mutex::new(None));
    let hook = {
        let (shared, session) = (shared.clone(), session.clone());
        cancel.add(Box::new(move || {
            if let Some(id) = session.lock().ok().and_then(|s| s.clone()) {
                shared.notify("session/cancel", JValue::obj([("sessionId", id)]));
            }
            // Failed first, so the waiting prompt hears "Cancelled", not the broken pipe the stop
            // causes; the queued `session/cancel` still goes out as the queue drains.
            shared.fail(cancelled());
            shared.stop();
        }))
    };
    let abandon = |e: Error| {
        cancel.remove(hook);
        shared.stop();
        e
    };
    if cancel.is_cancelled() {
        shared.fail(cancelled());
        return Err(abandon(shared.closed().unwrap_or_else(cancelled)));
    }
    let wait = |rx: mpsc::Receiver<Result<JValue, Error>>| -> Result<JValue, Error> {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(answer) => answer,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(Error::public(
                "The coding harness did not answer.",
                "The coding harness did not answer.",
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(Error::new("The coding harness stopped."))
            }
        }
    };
    let init = JValue::obj([
        ("protocolVersion", JValue::Num(PROTOCOL_VERSION)),
        (
            "clientCapabilities",
            JValue::obj([
                (
                    "fs",
                    JValue::obj([
                        ("readTextFile", JValue::Bool(true)),
                        ("writeTextFile", JValue::Bool(true)),
                    ]),
                ),
                ("terminal", JValue::Bool(false)),
            ]),
        ),
        (
            "clientInfo",
            JValue::obj([
                ("name", JValue::Str("noevia".into())),
                ("version", JValue::Str("1".into())),
            ]),
        ),
    ]);
    let agent = read_agent(&wait(shared.request("initialize", init)).map_err(abandon)?);
    let meta = session_metadata(
        options.permission.as_ref(),
        options.harness.as_deref(),
        settings.as_ref(),
    )
    .map_err(abandon)?;
    let mut new = vec![
        ("cwd".to_string(), JValue::Str(options.cwd.clone())),
        ("mcpServers".to_string(), JValue::Arr(Vec::new())),
    ];
    if let Some(meta) = meta {
        new.push(("_meta".into(), meta));
    }
    let opened = wait(shared.request("session/new", JValue::Obj(new))).map_err(abandon)?;
    let value = opened.get("sessionId").clone();
    let id = match &value {
        v if v.truthy() => js_json::to_js_string(v).unwrap_or_default(),
        _ => {
            return Err(abandon(Error::public(
                "The harness did not open a session.",
                "The coding harness did not open a session.",
            )))
        }
    };
    if let Ok(mut s) = session.lock() {
        *s = Some(value.clone());
    }
    Ok(Connection {
        shared,
        session_id: id,
        session_value: value,
        agent,
        model: options.model,
        cancel,
        hook,
    })
}
