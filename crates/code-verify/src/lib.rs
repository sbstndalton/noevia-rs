//! noevia-core `server/code-verify.cjs` and `server/code-pipeline-verify.cjs` in Rust (R7):
//! server-measured verification for Code tasks (#703, part of #511), and the adapter the pipeline
//! (#705) reads it through.
//!
//! After the Executor's work is on the branch the pipeline asks one question: do the operator's
//! tests pass at exactly this commit? [`Verify`] answers it without believing anything the agent
//! said. The caller's `verify_checkout` names the SOURCE repository and checks the commit is the
//! released branch's tip; the verifier (its own container: another uid, no network, the workspaces
//! volume read-only) makes a hash-checked copy of that commit and runs the command the OPERATOR
//! configured there. This crate sends a repository name, a source path and a commit id (never a
//! command) over `CODE_VERIFY_ENDPOINT` (`unix:/path`, or `host:port`) and gets back an exit status
//! and a bounded tail of output. Without an endpoint verification is unavailable: it is never run
//! beside noevia's own state, nor in the agent's sandbox.
//!
//! # Trust rules (as in Node)
//!
//! * The only test report the pipeline may accept is the [`Report`] [`Verify::run`] returns.
//!   Node froze those objects and remembered them in a module-private `WeakSet`; here a `Report`
//!   is a type only this crate can construct (its fields are private, it has no public
//!   constructor, no `Default`, and nothing deserializes into it), and [`is_measured`] binds one
//!   to a task, commit and revision. **A report read back from a job journal, an artifact, a file
//!   in the repository or anything the agent emitted is JSON ([`Report::to_json`] is a one-way
//!   record), never a `Report`, so it cannot be measured, even if every field matches.** The
//!   pipeline must still re-check, at merge time, that the branch head is the measured head
//!   (the merge is #705's job).
//! * `passed` is computed here from the measured exit status: exit 0, no signal, no timeout.
//! * The output tail is untrusted data: re-capped here in bytes, stripped of control characters
//!   other than newline and tab ([`clean_tail`]), and never parsed for a verdict.
//! * The verifier's answer is read as ONE line of at most [`MAX_ANSWER_BYTES`]. Its commit id
//!   must equal the one asked for, and its nonce (fresh per request) the one sent.
//!
//! ```compile_fail
//! // A report cannot be built outside this crate.
//! let _ = code_verify::Report { passed: true };
//! ```
//!
//! # Retrying
//!
//! The verifier serves one request and restarts, so for a few seconds after a run it is not
//! listening, and while one runs it turns others away. A connect error (`ENOENT`, `ECONNREFUSED`,
//! `EAGAIN`, `ECONNRESET`) before any byte, or a close with no byte within one second of
//! connecting, is `not_listening`; so is an answer `{ ok: false, error: 'busy' }`. Both are
//! retried every 250 ms until `connect_retry` has passed, then the outcome is `unavailable` with
//! code `busy`. Anything later is a real failure and never re-run.
//!
//! # Differences from Node
//!
//! Proven identical by tests/replay.rs (recorded from the real modules by tools/gen-fixtures.cjs);
//! these are the named differences:
//!
//! 1. Types. `taskId`, `repo`, `headSha` are strings, the revision is a number ([`Request`]), the
//!    checkout's `source` is a string. Node's `String(repo || '')` of a non-string, a
//!    `verifyCheckout` that returns no source or throws something with a non-string `code`, and an
//!    outcome with an unknown status in the adapter cannot be written here; the adapter's
//!    `bad_outcome` is unreachable.
//! 2. An answer line `JSON.parse` reads but [`js_json::wtf16`] cannot hold (a character of
//!    U+10F800..U+10FFFF, a number past the `f64` range such as `1e999`, nesting deeper than about
//!    128 levels) is "not readable" (`bad_answer`); Node read it. A lone surrogate in the answer
//!    is held as a mapped character: it is written as U+FFFD in the tail and message (as
//!    `Buffer.from` does) and kept as the mapped character in an error `code`, which JSON writes
//!    as the `\udxxx` escape Node's `JSON.stringify` would.
//! 3. A task cancelled before the first attempt (or while waiting between attempts) never
//!    connects; Node connected and hung up at once. A cancel wakes the wait at once (Node let the
//!    250 ms pass, then cancelled). Reads, writes and connects look at the cancel and the
//!    deadline every 10 ms; the outcome is the same.
//! 4. A connect that is blocked (a unix socket whose backlog is full: Node's non-blocking connect
//!    answered `EAGAIN` and was retried) is abandoned at the deadline or cancel, not interrupted;
//!    its helper thread ends when the kernel finishes it. A unix path longer than `sun_path` is
//!    `ENOENT` (retried), as Node's connect finds it. Whether a peer that closed before the
//!    request went out shows as `write EPIPE` (an `error`, `unreachable`) or as a read that
//!    resets or ends (`not_listening`) depends on thread timing in both; the replay pins the
//!    deterministic forms (closed after reading the request, or with it unread).
//! 5. The answer limit counts the bytes of the decoded text as Node does, but chunk boundaries
//!    (64 KiB reads) follow the platform: a single read that pushes the total past the limit is
//!    refused even when it holds the newline, in both.
//! 6. System error texts: `connect ENOENT /path`, `connect ECONNREFUSED 127.0.0.1:9`,
//!    `read ECONNRESET`, `getaddrinfo ENOTFOUND host` are written as Node writes them for the
//!    Linux errnos in the table in net.rs; any other error shows Rust's text. An endpoint with
//!    more than one address that all fail shows the last one's (Node's `AggregateError` shows an
//!    empty message). A port past 65535 gives Node's `RangeError` text (code `config`).
//! 7. The default request id reads 16 bytes of `/dev/urandom` as lowercase hex (Node's
//!    `crypto.randomBytes(16).toString('hex')`); when that cannot be read the outcome is `error`
//!    with code `nonce` (Node cannot fail here).
//! 8. `durationMs` is a JS number: with a clock that goes backwards it is `0`.

#![forbid(unsafe_code)]

mod adapter;
mod net;
mod text;

pub use adapter::{Adapter, Verifier};
pub use code_acp::Cancel;
pub use net::MAX_ANSWER_BYTES;
pub use text::{clean_tail, tail_utf8};

use js_json::{wtf16, JValue};
use net::{after, ask, parse_target, pause, AskCode};
use std::time::{Duration, Instant, SystemTime};

/// The step id the events use.
pub const STEP_ID: &str = "tests";
/// The step title.
pub const STEP_TITLE: &str = "Run the tests";
/// The default output cap, in bytes.
pub const DEFAULT_TAIL_BYTES: usize = 16 * 1024;
/// The most the output cap may be set to.
pub const MAX_TAIL_BYTES: usize = 256 * 1024;
/// Longer than the verifier's own default wall limit (10 min) so its answer normally arrives first.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(12 * 60);
/// How long to keep trying a verifier that is restarting or busy.
pub const DEFAULT_CONNECT_RETRY: Duration = Duration::from_millis(15_000);
const RETRY_STEP: Duration = Duration::from_millis(250);
const MIN_TAIL_BYTES: f64 = 1024.0;

/// `/^(?:[0-9a-f]{40}|[0-9a-f]{64})$/`.
fn is_commit(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `/^[A-Za-z0-9_.-]{1,64}$/`.
fn is_repo_name(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// `Number.isInteger`.
fn is_integer(n: f64) -> bool {
    n.is_finite() && n.fract() == 0.0
}

/// `Number.isSafeInteger`.
fn is_safe_integer(n: f64) -> bool {
    is_integer(n) && n.abs() <= 9_007_199_254_740_991.0
}

/// What happened, as Node's outcome `status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Passed,
    Failed,
    Error,
    Unavailable,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Passed => "passed",
            Status::Failed => "failed",
            Status::Error => "error",
            Status::Unavailable => "unavailable",
        }
    }
}

/// Node's `{ code, message }`.
#[derive(Debug, Clone, PartialEq)]
pub struct VerifyError {
    pub code: String,
    pub message: String,
}

/// Node's `{ status, report, error }`: a report with `passed`/`failed`, an error with
/// `error`/`unavailable`.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub status: Status,
    pub report: Option<Report>,
    pub error: Option<VerifyError>,
}

impl Outcome {
    /// The outcome as the JSON Node's object stringifies to (a record only: see [`Report`]).
    pub fn to_json(&self) -> JValue {
        JValue::obj([
            ("status", JValue::Str(self.status.as_str().into())),
            (
                "report",
                self.report.as_ref().map_or(JValue::Null, Report::to_json),
            ),
            (
                "error",
                self.error.as_ref().map_or(JValue::Null, |e| {
                    JValue::obj([
                        ("code", JValue::Str(e.code.clone())),
                        ("message", JValue::Str(e.message.clone())),
                    ])
                }),
            ),
        ])
    }
}

fn num_or_null(n: Option<f64>) -> JValue {
    n.map_or(JValue::Null, JValue::Num)
}

/// The measurement. Only [`Verify::run`] makes one (the fields are private and there is no
/// constructor), so holding one is the proof Node's `WeakSet` gave; [`is_measured`] also binds it
/// to a task, commit and revision.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    passed: bool,
    exit_code: Option<f64>,
    signal: Option<String>,
    timed_out: bool,
    tail: String,
    total_bytes: Option<u64>,
    truncated: bool,
    task_id: String,
    head_sha: String,
    revision: f64,
    repo: String,
    duration_ms: f64,
}

impl Report {
    /// Exit 0, no signal, no timeout.
    pub fn passed(&self) -> bool {
        self.passed
    }
    /// The exit status, when the verifier reported an integer one.
    pub fn exit_code(&self) -> Option<f64> {
        self.exit_code
    }
    /// The signal that ended the run (`SIGKILL`, …).
    pub fn signal(&self) -> Option<&str> {
        self.signal.as_deref()
    }
    pub fn timed_out(&self) -> bool {
        self.timed_out
    }
    /// The output tail, cleaned and capped. Untrusted text; never parsed for a verdict.
    pub fn tail(&self) -> &str {
        &self.tail
    }
    pub fn tail_bytes(&self) -> usize {
        self.tail.len()
    }
    /// All the output the run produced, when the verifier said.
    pub fn total_bytes(&self) -> Option<u64> {
        self.total_bytes
    }
    pub fn truncated(&self) -> bool {
        self.truncated
    }
    pub fn task_id(&self) -> &str {
        &self.task_id
    }
    pub fn head_sha(&self) -> &str {
        &self.head_sha
    }
    pub fn revision(&self) -> f64 {
        self.revision
    }
    pub fn repo(&self) -> &str {
        &self.repo
    }
    pub fn duration_ms(&self) -> f64 {
        self.duration_ms
    }

    /// The report as the JSON object Node made (`Object.freeze({...})`, in this order): a record
    /// to store or show. Nothing turns it back into a `Report`.
    pub fn to_json(&self) -> JValue {
        JValue::obj([
            ("kind", JValue::Str("test-report".into())),
            ("passed", JValue::Bool(self.passed)),
            ("exitCode", num_or_null(self.exit_code)),
            (
                "signal",
                self.signal.clone().map_or(JValue::Null, JValue::Str),
            ),
            ("timedOut", JValue::Bool(self.timed_out)),
            ("tail", JValue::Str(self.tail.clone())),
            ("tailBytes", JValue::Num(self.tail.len() as f64)),
            (
                "totalBytes",
                num_or_null(self.total_bytes.map(|n| n as f64)),
            ),
            ("truncated", JValue::Bool(self.truncated)),
            ("taskId", JValue::Str(self.task_id.clone())),
            ("headSha", JValue::Str(self.head_sha.clone())),
            ("revision", JValue::Num(self.revision)),
            ("repo", JValue::Str(self.repo.clone())),
            ("durationMs", JValue::Num(self.duration_ms)),
            ("measuredBy", JValue::Str("verifier".into())),
        ])
    }
}

/// True only for a report this crate produced, for exactly this task, commit and revision. All
/// three are required: a measurement of another task or an older revision is not one.
pub fn is_measured(report: &Report, task_id: &str, head_sha: &str, revision: f64) -> bool {
    report.task_id == task_id && report.head_sha == head_sha && report.revision == revision
}

/// What `emit(type, data)` received in Node.
#[derive(Debug, Clone, PartialEq)]
pub enum Event<'a> {
    /// `step.started` `{ id, title }`.
    StepStarted,
    /// `step.completed`: `{ id, failed: true, reason }` when it stopped, `{ id, failed }` after a
    /// measurement.
    StepCompleted {
        failed: bool,
        reason: Option<&'a str>,
    },
    /// `artifact.created`: the report, as `{ ...report }`.
    ArtifactCreated(&'a Report),
}

impl Event<'_> {
    /// Node's `type`.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::StepStarted => "step.started",
            Event::StepCompleted { .. } => "step.completed",
            Event::ArtifactCreated(_) => "artifact.created",
        }
    }

    /// Node's `data`.
    pub fn data(&self) -> JValue {
        match self {
            Event::StepStarted => JValue::obj([
                ("id", JValue::Str(STEP_ID.into())),
                ("title", JValue::Str(STEP_TITLE.into())),
            ]),
            Event::StepCompleted { failed, reason } => {
                let mut pairs = vec![
                    ("id", JValue::Str(STEP_ID.into())),
                    ("failed", JValue::Bool(*failed)),
                ];
                if let Some(r) = reason {
                    pairs.push(("reason", JValue::Str((*r).to_string())));
                }
                JValue::obj(pairs)
            }
            Event::ArtifactCreated(r) => r.to_json(),
        }
    }
}

/// What `log(entry)` received in Node.
#[derive(Debug, Clone, PartialEq)]
pub enum LogEntry {
    /// `{ event: 'code.verify', taskId, revision, status, code }`: it stopped.
    Stopped {
        task_id: String,
        revision: f64,
        status: Status,
        code: String,
    },
    /// `{ event: 'code.verify', taskId, revision, status, exitCode, timedOut }`: it measured.
    Measured {
        task_id: String,
        revision: f64,
        status: Status,
        exit_code: Option<f64>,
        timed_out: bool,
    },
}

impl LogEntry {
    pub fn to_json(&self) -> JValue {
        match self {
            LogEntry::Stopped {
                task_id,
                revision,
                status,
                code,
            } => JValue::obj([
                ("event", JValue::Str("code.verify".into())),
                ("taskId", JValue::Str(task_id.clone())),
                ("revision", JValue::Num(*revision)),
                ("status", JValue::Str(status.as_str().into())),
                ("code", JValue::Str(code.clone())),
            ]),
            LogEntry::Measured {
                task_id,
                revision,
                status,
                exit_code,
                timed_out,
            } => JValue::obj([
                ("event", JValue::Str("code.verify".into())),
                ("taskId", JValue::Str(task_id.clone())),
                ("revision", JValue::Num(*revision)),
                ("status", JValue::Str(status.as_str().into())),
                ("exitCode", num_or_null(*exit_code)),
                ("timedOut", JValue::Bool(*timed_out)),
            ]),
        }
    }
}

/// What `workspaces.verifyCheckout` threw in Node: `e.code || 'checkout'`,
/// `e.message || 'Could not name the commit to verify.'` (an empty string counts as missing).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CheckoutError {
    pub code: Option<String>,
    pub message: Option<String>,
}

/// `workspaces.verifyCheckout(taskId, headSha)`: the source repository path to verify, or why not.
pub type VerifyCheckout<'a> = dyn Fn(&str, &str) -> Result<String, CheckoutError> + 'a;

/// One verification: `run`'s arguments.
pub struct Request<'a> {
    pub task_id: &'a str,
    pub repo: &'a str,
    /// The full commit id: 40 or 64 lowercase hex digits.
    pub head_sha: &'a str,
    /// A JS number: a non-negative integer, or the request is refused (`bad_request`).
    pub revision: f64,
    pub verify_checkout: &'a VerifyCheckout<'a>,
    /// The task's cancellation; a cancel hangs up on the verifier, which stops the run there.
    pub cancel: Option<&'a Cancel>,
}

/// `createCodeVerify`'s options.
pub struct Options {
    /// `CODE_VERIFY_ENDPOINT`: `unix:/absolute/path` or `host:port`. None (or empty): unavailable.
    pub endpoint: Option<String>,
    /// How long one attempt may take to answer.
    pub timeout: Duration,
    /// The output cap in bytes, as the JS number given (0 and NaN mean the default; the result is
    /// clamped to 1024..=262144).
    pub tail_bytes: f64,
    /// How long to retry a verifier that is restarting or busy.
    pub connect_retry: Duration,
    /// `Date.now()`: milliseconds, called at the start of a run and again to time a measurement.
    pub now: Box<dyn Fn() -> f64 + Send + Sync>,
    pub log: Box<dyn Fn(&LogEntry) + Send + Sync>,
    /// A fresh request id, echoed by the verifier. None: it cannot be made.
    pub nonce: Box<dyn Fn() -> Option<String> + Send + Sync>,
}

impl Options {
    /// The defaults, for this endpoint.
    pub fn new(endpoint: Option<String>) -> Options {
        Options {
            endpoint,
            timeout: DEFAULT_TIMEOUT,
            tail_bytes: DEFAULT_TAIL_BYTES as f64,
            connect_retry: DEFAULT_CONNECT_RETRY,
            now: Box::new(system_now),
            log: Box::new(|_| {}),
            nonce: Box::new(random_nonce),
        }
    }

    /// The defaults with `CODE_VERIFY_ENDPOINT` from the environment.
    pub fn from_env() -> Options {
        Options::new(std::env::var("CODE_VERIFY_ENDPOINT").ok())
    }
}

fn system_now() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

fn random_nonce() -> Option<String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The verifier client (`createCodeVerify`).
pub struct Verify {
    endpoint: Option<String>,
    timeout: Duration,
    cap: usize,
    connect_retry: Duration,
    now: Box<dyn Fn() -> f64 + Send + Sync>,
    log: Box<dyn Fn(&LogEntry) + Send + Sync>,
    nonce: Box<dyn Fn() -> Option<String> + Send + Sync>,
}

impl Verify {
    pub fn new(options: Options) -> Verify {
        let t = options.tail_bytes;
        let wanted = if t.is_nan() || t == 0.0 {
            DEFAULT_TAIL_BYTES as f64
        } else {
            t.floor()
        };
        let cap = wanted.max(MIN_TAIL_BYTES).min(MAX_TAIL_BYTES as f64) as usize;
        Verify {
            endpoint: options.endpoint.filter(|e| !e.is_empty()),
            timeout: options.timeout,
            cap,
            connect_retry: options.connect_retry,
            now: options.now,
            log: options.log,
            nonce: options.nonce,
        }
    }

    /// Whether there is a verifier to ask.
    pub fn available(&self) -> bool {
        self.endpoint.is_some()
    }

    /// The output cap in effect, in bytes.
    pub fn tail_cap(&self) -> usize {
        self.cap
    }

    /// [`is_measured`].
    pub fn is_measured(
        &self,
        report: &Report,
        task_id: &str,
        head_sha: &str,
        revision: f64,
    ) -> bool {
        is_measured(report, task_id, head_sha, revision)
    }

    fn stop(
        &self,
        request: &Request<'_>,
        emit: &mut dyn FnMut(&Event<'_>),
        status: Status,
        code: &str,
        message: &str,
    ) -> Outcome {
        (self.log)(&LogEntry::Stopped {
            task_id: request.task_id.to_string(),
            revision: request.revision,
            status,
            code: code.to_string(),
        });
        emit(&Event::StepCompleted {
            failed: true,
            reason: Some(message),
        });
        Outcome {
            status,
            report: None,
            error: Some(VerifyError {
                code: code.to_string(),
                message: message.to_string(),
            }),
        }
    }

    /// Verifies one revision. Never fails for an expected failure: the outcome says what
    /// happened, and `emit` heard `step.started` then either `step.completed` (stopped) or
    /// `artifact.created` and `step.completed` (measured).
    pub fn run(&self, request: &Request<'_>, emit: &mut dyn FnMut(&Event<'_>)) -> Outcome {
        let started = (self.now)();
        emit(&Event::StepStarted);
        let Some(endpoint) = self.endpoint.as_deref() else {
            return self.stop(
                request,
                emit,
                Status::Unavailable,
                "no_verifier",
                "Verification needs the verifier container (CODE_VERIFY_ENDPOINT).",
            );
        };
        if !is_repo_name(request.repo) {
            return self.stop(
                request,
                emit,
                Status::Error,
                "bad_request",
                "Verification needs the repository name.",
            );
        }
        if !is_commit(request.head_sha) {
            return self.stop(
                request,
                emit,
                Status::Error,
                "bad_sha",
                "Verification needs a full commit id.",
            );
        }
        if !(is_integer(request.revision) && request.revision >= 0.0) {
            return self.stop(
                request,
                emit,
                Status::Error,
                "bad_request",
                "Verification needs the revision number.",
            );
        }

        let source = match (request.verify_checkout)(request.task_id, request.head_sha) {
            Ok(s) => s,
            Err(e) => {
                let code = e
                    .code
                    .as_deref()
                    .filter(|c| !c.is_empty())
                    .unwrap_or("checkout");
                let message = e
                    .message
                    .as_deref()
                    .filter(|m| !m.is_empty())
                    .unwrap_or("Could not name the commit to verify.");
                return self.stop(request, emit, Status::Error, code, message);
            }
        };

        // A fresh nonce per request, echoed by the verifier: a stale or replayed answer does not
        // match. (It does not stop someone who can read the request; the verifier's isolation does.)
        let Some(nonce) = (self.nonce)() else {
            return self.stop(
                request,
                emit,
                Status::Error,
                "nonce",
                "Could not make a request id.",
            );
        };
        let line = JValue::obj([
            ("noevia", JValue::Str("verify".into())),
            ("repo", JValue::Str(request.repo.to_string())),
            ("source", JValue::Str(source)),
            ("headSha", JValue::Str(request.head_sha.to_string())),
            ("nonce", JValue::Str(nonce.clone())),
        ]);
        let Some(mut line) = wtf16::stringify(&line) else {
            return self.stop(
                request,
                emit,
                Status::Error,
                "bad_request",
                "Verification needs the repository name.",
            );
        };
        line.push('\n');

        let give_up = after(self.connect_retry);
        let before_give_up = || give_up.is_none_or(|g| Instant::now() < g);
        let cancelled = || request.cancel.is_some_and(Cancel::is_cancelled);
        let answer = loop {
            let target = match parse_target(endpoint) {
                Ok(t) => t,
                Err(message) => return self.stop(request, emit, Status::Error, "config", &message),
            };
            match ask(target, &line, self.timeout, request.cancel) {
                Err(e) => {
                    if e.code == AskCode::NotListening && before_give_up() && !cancelled() {
                        pause(RETRY_STEP, request.cancel);
                        continue;
                    }
                    if e.code == AskCode::NotListening {
                        return self.stop(
                            request,
                            emit,
                            Status::Unavailable,
                            "busy",
                            "The verifier is busy or restarting; try again shortly.",
                        );
                    }
                    return self.stop(request, emit, Status::Error, e.code.as_str(), &e.message);
                }
                Ok(answer) => {
                    let busy = answer.get("ok").as_bool() == Some(false)
                        && answer.get("error").as_str() == Some("busy");
                    if busy && before_give_up() && !cancelled() {
                        pause(RETRY_STEP, request.cancel);
                        continue;
                    }
                    break answer;
                }
            }
        };

        if answer.get("noevia").as_str() != Some("verify-result") {
            return self.stop(
                request,
                emit,
                Status::Error,
                "bad_answer",
                "The verifier\u{2019}s answer was not a verification result.",
            );
        }
        if answer.get("nonce").as_str() != Some(nonce.as_str()) {
            return self.stop(
                request,
                emit,
                Status::Error,
                "bad_answer",
                "The verifier\u{2019}s answer was not for this request.",
            );
        }
        if answer.get("ok").as_bool() != Some(true) {
            let code = match answer.get("error").as_str() {
                Some(e) => wtf16::slice(e, 40),
                None => "refused".to_string(),
            };
            let message = match answer.get("message").as_str() {
                Some(m) => clean_tail(m, 500),
                None => "The verifier refused the verification.".to_string(),
            };
            let status = if code == "not_configured" || code == "busy" {
                Status::Unavailable
            } else {
                Status::Error
            };
            return self.stop(request, emit, status, &code, &message);
        }
        if answer.get("headSha").as_str() != Some(request.head_sha) {
            return self.stop(
                request,
                emit,
                Status::Error,
                "bad_answer",
                "The verifier answered for a different commit.",
            );
        }

        let exit_code = answer.get("exitCode").as_num().filter(|n| is_integer(*n));
        let signal = answer
            .get("signal")
            .as_str()
            .filter(|s| is_signal_name(s))
            .map(str::to_string);
        let timed_out = answer.get("timedOut").as_bool() == Some(true);
        let tail = clean_tail(answer.get("tail").as_str().unwrap_or(""), self.cap);
        let total_bytes = answer
            .get("totalBytes")
            .as_num()
            .filter(|n| is_safe_integer(*n) && *n >= 0.0)
            .map(|n| n as u64);
        let truncated = answer.get("truncated").as_bool() == Some(true)
            || total_bytes.is_some_and(|t| t > tail.len() as u64);
        let elapsed = (self.now)() - started;
        let report = Report {
            passed: exit_code == Some(0.0) && signal.is_none() && !timed_out,
            exit_code,
            signal,
            timed_out,
            tail,
            total_bytes,
            truncated,
            task_id: request.task_id.to_string(),
            head_sha: request.head_sha.to_string(),
            revision: request.revision,
            repo: request.repo.to_string(),
            // Math.max(0, elapsed): NaN stays NaN.
            duration_ms: if elapsed > 0.0 || elapsed.is_nan() {
                elapsed
            } else {
                0.0
            },
        };
        let status = if report.passed {
            Status::Passed
        } else {
            Status::Failed
        };
        (self.log)(&LogEntry::Measured {
            task_id: request.task_id.to_string(),
            revision: request.revision,
            status,
            exit_code,
            timed_out,
        });
        emit(&Event::ArtifactCreated(&report));
        emit(&Event::StepCompleted {
            failed: !report.passed,
            reason: None,
        });
        Outcome {
            status,
            report: Some(report),
            error: None,
        }
    }
}

/// `/^SIG[A-Z0-9]{1,10}$/`.
fn is_signal_name(s: &str) -> bool {
    s.strip_prefix("SIG").is_some_and(|rest| {
        (1..=10).contains(&rest.len())
            && rest
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    })
}

impl Verifier for Verify {
    fn available(&self) -> bool {
        Verify::available(self)
    }
    fn run(&self, request: &Request<'_>, emit: &mut dyn FnMut(&Event<'_>)) -> Outcome {
        Verify::run(self, request, emit)
    }
    fn is_measured(&self, report: &Report, task_id: &str, head_sha: &str, revision: f64) -> bool {
        is_measured(report, task_id, head_sha, revision)
    }
}

#[cfg(test)]
mod tests;
