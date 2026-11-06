//! Server-measured tests through the existing isolated verifier engine. Only this module can
//! construct Measured; journal/model reports cannot become measured evidence by deserialization.
use crate::{
    code::Failure,
    context::{Clock, JournalContext},
    session::Context,
    workspace,
};
use js_json::{wtf16, JValue};
use std::{sync::Arc, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
const MAX_ANSWER: usize = 2 * 1024 * 1024;
fn text(s: impl Into<String>) -> JValue {
    JValue::Str(s.into())
}
fn object(fields: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
pub struct Measured {
    report: JValue,
}
impl Measured {
    pub fn report(&self) -> &JValue {
        &self.report
    }
    pub fn matches(&self, task: &str, head: &str, revision: f64) -> bool {
        self.report.get("taskId").as_str() == Some(task)
            && self.report.get("headSha").as_str() == Some(head)
            && self.report.get("revision").as_num() == Some(revision)
    }
}
pub struct Outcome {
    pub status: &'static str,
    pub report: Option<Measured>,
    pub error: Option<JValue>,
}
impl Outcome {
    pub fn value(&self) -> JValue {
        object(vec![
            ("status", text(self.status)),
            (
                "report",
                self.report
                    .as_ref()
                    .map_or(JValue::Null, |r| r.report.clone()),
            ),
            ("error", self.error.clone().unwrap_or(JValue::Null)),
        ])
    }
}
pub struct Verify {
    pub endpoint: Option<String>,
    pub timeout: Duration,
    pub retry: Duration,
    pub tail_bytes: usize,
    pub clock: Arc<dyn Clock>,
}
impl Verify {
    pub fn available(&self) -> bool {
        self.endpoint.as_ref().is_some_and(|s| !s.is_empty())
    }
}
#[derive(Clone, Copy)]
struct Refusal {
    code: &'static str,
    message: &'static str,
}
fn refuse(code: &'static str, message: &'static str) -> Refusal {
    Refusal { code, message }
}
fn cancelled() -> Refusal {
    refuse("cancelled", "Verification was cancelled.")
}
/// Byte-capped UTF8 tail; test output is data and never interpreted as a verdict.
pub fn clean_tail(input: &str, cap: usize) -> String {
    let mut clean = String::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        let c = if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            '\n'
        } else {
            c
        };
        if matches!(c,'\u{0}'..='\u{8}'|'\u{b}'..='\u{1f}'|'\u{7f}'..='\u{9f}') {
            continue;
        }
        if wtf16::lone_unit(c).is_some() {
            clean.push(char::REPLACEMENT_CHARACTER);
        } else {
            clean.push(c);
        }
    }
    if clean.len() <= cap {
        return clean;
    }
    let mut start = clean.len() - cap;
    while !clean.is_char_boundary(start) {
        start += 1;
    }
    clean.get(start..).unwrap_or("").into()
}
async fn ask(
    endpoint: &str,
    request: &JValue,
    context: &JournalContext,
    timeout: Duration,
) -> Result<JValue, Refusal> {
    let work =
        async {
            let (mut read, mut write): (
                Box<dyn AsyncRead + Unpin + Send>,
                Box<dyn AsyncWrite + Unpin + Send>,
            ) = if let Some(path) = endpoint.strip_prefix("unix:") {
                if !path.starts_with('/') {
                    return Err(refuse(
                        "config",
                        "The verifier endpoint is not configured correctly.",
                    ));
                }
                let stream = tokio::net::UnixStream::connect(path)
                    .await
                    .map_err(connect_error)?;
                let (r, w) = stream.into_split();
                (Box::new(r), Box::new(w))
            } else {
                let stream = tokio::net::TcpStream::connect(endpoint)
                    .await
                    .map_err(connect_error)?;
                let (r, w) = stream.into_split();
                (Box::new(r), Box::new(w))
            };
            let connected = tokio::time::Instant::now();
            let mut line = wtf16::stringify(request)
                .ok_or_else(|| refuse("bad_request", "Verification needs a valid request."))?;
            line.push('\n');
            write
                .write_all(line.as_bytes())
                .await
                .map_err(connect_error)?;
            let mut received = Vec::new();
            let mut chunk = [0; 8192];
            loop {
                let count = read.read(&mut chunk).await.map_err(|e| {
                    if received.is_empty() {
                        connect_error(e)
                    } else {
                        refuse("unreachable", "The verifier is unreachable.")
                    }
                })?;
                if count == 0 {
                    return Err(
                        if received.is_empty() && connected.elapsed() < Duration::from_secs(1) {
                            refuse(
                                "not_listening",
                                "The verifier closed the connection without an answer.",
                            )
                        } else {
                            refuse(
                                "unreachable",
                                "The verifier closed the connection without an answer.",
                            )
                        },
                    );
                }
                if received.len().saturating_add(count) > MAX_ANSWER {
                    return Err(refuse("bad_answer", "The verifier’s answer was too large."));
                }
                received.extend_from_slice(chunk.get(..count).ok_or_else(|| {
                    refuse("bad_answer", "The verifier’s answer was not readable.")
                })?);
                if let Some(end) = received.iter().position(|b| *b == b'\n') {
                    return wtf16::parse(&String::from_utf8_lossy(
                        received.get(..end).unwrap_or_default(),
                    ))
                    .map_err(|_| refuse("bad_answer", "The verifier’s answer was not readable."));
                }
            }
        };
    let mut cancel = context.cancellation();
    if *cancel.borrow() {
        return Err(cancelled());
    }
    tokio::select! {biased;_=cancel.changed()=>Err(cancelled()),result=tokio::time::timeout(timeout,work)=>result.unwrap_or_else(|_|Err(refuse("timeout","The verifier did not answer in time.")))}
}
fn connect_error(error: std::io::Error) -> Refusal {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::ConnectionReset
    ) {
        refuse("not_listening", "The verifier is unreachable.")
    } else {
        refuse("unreachable", "The verifier is unreachable.")
    }
}
impl Verify {
    fn stop(
        &self,
        context: &JournalContext,
        status: &'static str,
        error: Refusal,
    ) -> Result<Outcome, Failure> {
        context
            .event(
                "step.completed",
                object(vec![
                    ("id", text("tests")),
                    ("failed", JValue::Bool(true)),
                    ("reason", text(error.message)),
                ]),
            )
            .map_err(|_| Failure::unavailable())?;
        Ok(Outcome {
            status,
            report: None,
            error: Some(object(vec![
                ("code", text(error.code)),
                ("message", text(error.message)),
            ])),
        })
    }
    pub async fn run(
        &self,
        context: Arc<JournalContext>,
        repo: &str,
        head: &str,
        revision: f64,
    ) -> Result<Outcome, Failure> {
        let started = self.clock.now();
        context
            .event(
                "step.started",
                object(vec![
                    ("id", text("tests")),
                    ("title", text("Run the tests")),
                ]),
            )
            .map_err(|_| Failure::unavailable())?;
        let Some(endpoint) = self.endpoint.as_deref().filter(|s| !s.is_empty()) else {
            return self.stop(
                &context,
                "unavailable",
                refuse(
                    "no_verifier",
                    "Verification needs the verifier container (CODE_VERIFY_ENDPOINT).",
                ),
            );
        };
        if repo.is_empty()
            || repo.len() > 64
            || !repo
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return self.stop(
                &context,
                "error",
                refuse("bad_request", "Verification needs the repository name."),
            );
        }
        if !workspace::full_sha(head) {
            return self.stop(
                &context,
                "error",
                refuse("bad_sha", "Verification needs a full commit id."),
            );
        }
        if !revision.is_finite() || revision.fract() != 0.0 || revision < 0.0 {
            return self.stop(
                &context,
                "error",
                refuse("bad_request", "Verification needs the revision number."),
            );
        }
        context.active().map_err(|_| Failure::unavailable())?;
        let flag = context.blocking_cancellation();
        let target =
            match context
                .account()
                .workspaces
                .verify_checkout(context.task(), head, flag.flag())
            {
                Ok(target) => target,
                Err(workspace::Error::Coded { code, message }) => {
                    context
                        .event(
                            "step.completed",
                            object(vec![
                                ("id", text("tests")),
                                ("failed", JValue::Bool(true)),
                                ("reason", text(&message)),
                            ]),
                        )
                        .map_err(|_| Failure::unavailable())?;
                    return Ok(Outcome {
                        status: "error",
                        report: None,
                        error: Some(object(vec![
                            ("code", text(code)),
                            ("message", text(message)),
                        ])),
                    });
                }
                Err(_) => {
                    return self.stop(
                        &context,
                        "error",
                        refuse("checkout", "Could not name the commit to verify."),
                    )
                }
            };
        drop(flag);
        let nonce = server_account::util::random_hex(16);
        let request = object(vec![
            ("noevia", text("verify")),
            ("repo", text(repo)),
            ("source", target.get("source").clone()),
            ("headSha", text(head)),
            ("nonce", text(&nonce)),
        ]);
        let give_up = tokio::time::Instant::now()
            .checked_add(self.retry)
            .unwrap_or_else(tokio::time::Instant::now);
        let answer = loop {
            context.active().map_err(|_| Failure::unavailable())?;
            let result = ask(endpoint, &request, &context, self.timeout).await;
            let retry = match &result {
                Err(error) => error.code == "not_listening",
                Ok(answer) => {
                    answer.get("ok") == &JValue::Bool(false)
                        && answer.get("error").as_str() == Some("busy")
                }
            };
            if retry && tokio::time::Instant::now() < give_up {
                let mut cancel = context.cancellation();
                if *cancel.borrow() {
                    return self.stop(&context, "error", cancelled());
                }
                tokio::select! {biased;_=cancel.changed()=>return self.stop(&context,"error",cancelled()),_=tokio::time::sleep(Duration::from_millis(250))=>{}}
                continue;
            }
            match result {
                Ok(answer) => break answer,
                Err(error) if error.code == "not_listening" => {
                    return self.stop(
                        &context,
                        "unavailable",
                        refuse(
                            "busy",
                            "The verifier is busy or restarting; try again shortly.",
                        ),
                    )
                }
                Err(error) => return self.stop(&context, "error", error),
            }
        };
        if answer.get("noevia").as_str() != Some("verify-result") {
            return self.stop(
                &context,
                "error",
                refuse(
                    "bad_answer",
                    "The verifier’s answer was not a verification result.",
                ),
            );
        }
        if answer.get("nonce").as_str() != Some(nonce.as_str()) {
            return self.stop(
                &context,
                "error",
                refuse(
                    "bad_answer",
                    "The verifier’s answer was not for this request.",
                ),
            );
        }
        if answer.get("ok") != &JValue::Bool(true) {
            // Engine failures are classified; reflected remote text never enters the journal/model context.
            let (status,error)=match answer.get("error").as_str(){Some("not_configured")=>("unavailable",refuse("not_configured","No verification command is configured for this repository.")),Some("busy")=>("unavailable",refuse("busy","The verifier is already running a verification; try again when it finishes.")),_=>("error",refuse("refused","The verifier refused the verification."))};
            return self.stop(&context, status, error);
        }
        if answer.get("headSha").as_str() != Some(head) {
            return self.stop(
                &context,
                "error",
                refuse(
                    "bad_answer",
                    "The verifier answered for a different commit.",
                ),
            );
        }
        context.active().map_err(|_| Failure::unavailable())?;
        let exit = answer
            .get("exitCode")
            .as_num()
            .filter(|n| n.is_finite() && n.fract() == 0.0)
            .map_or(JValue::Null, JValue::Num);
        let signal = answer
            .get("signal")
            .as_str()
            .filter(|s| {
                s.starts_with("SIG")
                    && (4..=13).contains(&s.len())
                    && s.get(3..).is_some_and(|s| {
                        s.bytes()
                            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                    })
            })
            .map_or(JValue::Null, text);
        let timed_out = answer.get("timedOut") == &JValue::Bool(true);
        let passed = exit.as_num() == Some(0.0) && signal == JValue::Null && !timed_out;
        let cap = self.tail_bytes.clamp(1024, 256 * 1024);
        let tail = clean_tail(answer.get("tail").as_str().unwrap_or(""), cap);
        let length = tail.len();
        let total = answer
            .get("totalBytes")
            .as_num()
            .filter(|n| n.is_finite() && n.fract() == 0.0 && *n >= 0.0 && *n <= 9007199254740991.0);
        let report = object(vec![
            ("kind", text("test-report")),
            ("passed", JValue::Bool(passed)),
            ("exitCode", exit),
            ("signal", signal),
            ("timedOut", JValue::Bool(timed_out)),
            ("tail", text(tail)),
            ("tailBytes", JValue::Num(length as f64)),
            ("totalBytes", total.map_or(JValue::Null, JValue::Num)),
            (
                "truncated",
                JValue::Bool(
                    answer.get("truncated") == &JValue::Bool(true)
                        || total.is_some_and(|n| n > length as f64),
                ),
            ),
            ("taskId", text(context.task())),
            ("headSha", text(head)),
            ("revision", JValue::Num(revision)),
            ("repo", text(repo)),
            (
                "durationMs",
                JValue::Num(self.clock.now().saturating_sub(started).max(0) as f64),
            ),
            ("measuredBy", text("verifier")),
        ]);
        context
            .event("artifact.created", report.clone())
            .map_err(|_| Failure::unavailable())?;
        context
            .event(
                "step.completed",
                object(vec![
                    ("id", text("tests")),
                    ("failed", JValue::Bool(!passed)),
                ]),
            )
            .map_err(|_| Failure::unavailable())?;
        Ok(Outcome {
            status: if passed { "passed" } else { "failed" },
            report: Some(Measured { report }),
            error: None,
        })
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{
        account::Account,
        facts::{CodeConfig, Repository},
        Scope,
    };
    use server_projects::task_journals::Surface;
    use std::{path::Path, sync::atomic::AtomicBool};
    use tokio::io::AsyncBufReadExt;
    const USER: &str = "11111111-1111-4111-8111-111111111111";
    const TASK: &str = "22222222-2222-4222-8222-222222222222";
    struct Fixed;
    impl Clock for Fixed {
        fn now(&self) -> i64 {
            1234
        }
    }
    fn git(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=QA",
                "-c",
                "user.email=qa@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    fn context() -> (tempfile::TempDir, Arc<JournalContext>, String) {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("file"), "synthetic").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "base"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        std::fs::create_dir_all(root.path().join("users").join(USER)).unwrap();
        let mut config = CodeConfig::from_env(|_| None);
        config.repositories = vec![Repository {
            id: "repo".into(),
            path: repo.clone(),
        }];
        let account =
            Arc::new(Account::open(root.path(), USER, &config, "epoch".into(), 1234).unwrap());
        account
            .create(Surface::Code, TASK, "project", vec![], 1234)
            .unwrap();
        let guard = account
            .control
            .begin(
                Scope {
                    user: USER.into(),
                    project: "project".into(),
                    surface: Surface::Code,
                },
                TASK.into(),
            )
            .unwrap();
        let context = Arc::new(
            JournalContext::new(
                account.clone(),
                guard,
                Arc::new(Fixed),
                Duration::from_secs(60),
            )
            .unwrap(),
        );
        account
            .workspaces
            .claim(
                TASK,
                &repo,
                None,
                &[],
                &[],
                &[],
                1234,
                &AtomicBool::new(false),
            )
            .unwrap();
        account.workspaces.release(TASK, false, 1234).unwrap();
        (root, context, head)
    }
    fn events(root: &Path, head: &str) -> JValue {
        let raw = std::fs::read_to_string(
            root.join("users")
                .join(USER)
                .join("jobs")
                .join(format!("{TASK}.jsonl")),
        )
        .unwrap();
        JValue::Arr(
            raw.lines()
                .map(|s| wtf16::parse(s).unwrap())
                .filter(|e| {
                    matches!(
                        e.get("type").as_str(),
                        Some("step.started" | "step.completed" | "artifact.created")
                    )
                })
                .map(|e| {
                    let data = wtf16::stringify(e.get("data"))
                        .unwrap()
                        .replace(head, &"a".repeat(40));
                    object(vec![
                        ("type", e.get("type").clone()),
                        ("data", wtf16::parse(&data).unwrap()),
                    ])
                })
                .collect(),
        )
    }
    fn answer(request: &JValue, input: &JValue, head: &str) -> JValue {
        let mut result = object(vec![
            ("noevia", text("verify-result")),
            ("nonce", request.get("nonce").clone()),
            ("ok", JValue::Bool(true)),
            ("headSha", text(head)),
            ("exitCode", JValue::Num(0.0)),
            ("signal", JValue::Null),
            ("timedOut", JValue::Bool(false)),
            ("tail", text("ok\n")),
            ("totalBytes", JValue::Num(3.0)),
            ("truncated", JValue::Bool(false)),
        ]);
        let JValue::Obj(fields) = &mut result else {
            panic!()
        };
        if let JValue::Obj(input) = input {
            for (k, v) in input {
                wtf16::set(fields, k, v.clone());
            }
        }
        if input.get("badShape") == &JValue::Bool(true) {
            wtf16::set(fields, "noevia", text("other"));
        }
        if input.get("badNonce") == &JValue::Bool(true) {
            wtf16::set(fields, "nonce", text("stale"));
        }
        if input.get("badHead") == &JValue::Bool(true) {
            wtf16::set(fields, "headSha", text("b".repeat(40)));
        }
        result
    }
    #[tokio::test]
    async fn exact_node_verifier_results_events_and_tail_bounds() {
        let fixtures =
            wtf16::parse(include_str!("../tests/fixtures/verify-client-node.json")).unwrap();
        let JValue::Arr(fixtures) = fixtures else {
            panic!()
        };
        for fixture in fixtures {
            let (root, context, head) = context();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = listener.local_addr().unwrap().to_string();
            let input = fixture.get("input").clone();
            let served = head.clone();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (mut read, mut write) = stream.into_split();
                let mut line = String::new();
                tokio::io::BufReader::new(&mut read)
                    .read_line(&mut line)
                    .await
                    .unwrap();
                let request = wtf16::parse(&line).unwrap();
                assert_eq!(request.get("noevia").as_str(), Some("verify"));
                assert_eq!(request.get("repo").as_str(), Some("repo"));
                assert_eq!(request.get("headSha").as_str(), Some(served.as_str()));
                assert_eq!(request.get("nonce").as_str().unwrap().len(), 32);
                let mut encoded = wtf16::stringify(&answer(&request, &input, &served)).unwrap();
                encoded.push('\n');
                write.write_all(encoded.as_bytes()).await.unwrap();
                let mut byte = [0];
                assert_eq!(
                    read.read(&mut byte).await.unwrap(),
                    0,
                    "client closes each one-shot request"
                );
            });
            let verifier = Verify {
                endpoint: Some(endpoint),
                timeout: Duration::from_secs(5),
                retry: Duration::ZERO,
                tail_bytes: 1024,
                clock: Arc::new(Fixed),
            };
            let outcome = verifier
                .run(context.clone(), "repo", &head, 1.0)
                .await
                .unwrap();
            if let Some(report) = &outcome.report {
                assert!(report.matches(TASK, &head, 1.0));
                assert!(!report.matches(TASK, &head, 2.0));
            }
            let result = wtf16::parse(
                &wtf16::stringify(&outcome.value())
                    .unwrap()
                    .replace(&head, &"a".repeat(40)),
            )
            .unwrap();
            assert_eq!(result, *fixture.get("outcome"));
            assert_eq!(events(root.path(), &head), *fixture.get("events"));
            server.await.unwrap();
        }
    }
    #[tokio::test]
    async fn unix_private_verifier_refusal_never_reflects_engine_text() {
        let (root, context, head) = context();
        let socket = root.path().join("v.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut read, mut write) = stream.into_split();
            let mut line = String::new();
            tokio::io::BufReader::new(&mut read)
                .read_line(&mut line)
                .await
                .unwrap();
            let request = wtf16::parse(&line).unwrap();
            let result = object(vec![
                ("noevia", text("verify-result")),
                ("nonce", request.get("nonce").clone()),
                ("ok", JValue::Bool(false)),
                ("error", text("synthetic-reflected-token")),
                ("message", text("synthetic-reflected-token")),
            ]);
            write
                .write_all(format!("{}\n", wtf16::stringify(&result).unwrap()).as_bytes())
                .await
                .unwrap();
        });
        let verifier = Verify {
            endpoint: Some(format!("unix:{}", socket.to_string_lossy())),
            timeout: Duration::from_secs(5),
            retry: Duration::ZERO,
            tail_bytes: 1024,
            clock: Arc::new(Fixed),
        };
        let outcome = verifier
            .run(context.clone(), "repo", &head, 1.0)
            .await
            .unwrap();
        assert_eq!(outcome.status, "error");
        assert!(!wtf16::stringify(&outcome.value())
            .unwrap()
            .contains("synthetic-reflected-token"));
        assert!(!wtf16::stringify(&events(root.path(), &head))
            .unwrap()
            .contains("synthetic-reflected-token"));
        server.await.unwrap();
    }
    #[tokio::test]
    async fn cancellation_at_real_verifier_read_hangs_up_without_measured_report() {
        let (_root, context, head) = context();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();
        let (entered, wait) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut line = String::new();
            tokio::io::BufReader::new(&mut stream)
                .read_line(&mut line)
                .await
                .unwrap();
            let _ = entered.send(());
            let mut bytes = [0];
            assert_eq!(stream.read(&mut bytes).await.unwrap(), 0);
        });
        let verifier = Verify {
            endpoint: Some(endpoint),
            timeout: Duration::from_secs(5),
            retry: Duration::ZERO,
            tail_bytes: 1024,
            clock: Arc::new(Fixed),
        };
        let cancelling = context.clone();
        let work = verifier.run(context, "repo", &head, 1.0);
        tokio::pin!(work);
        tokio::select! {_=wait=>{},_= &mut work=>panic!()};
        cancelling
            .account()
            .control
            .cancel(cancelling.scope(), TASK)
            .unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(2), work)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.status, "error");
        assert!(outcome.report.is_none());
        assert_eq!(
            outcome.error.unwrap().get("code").as_str(),
            Some("cancelled")
        );
        server.await.unwrap();
    }
    #[test]
    fn tail_cleaning_reuses_node_control_and_utf8_rules() {
        assert_eq!(clean_tail("a\r\nb\rc\u{1b}\u{85}", 1024), "a\nb\nc");
        assert_eq!(clean_tail("x😀", 3), "");
    }
    #[tokio::test]
    async fn stage_cancel_reaches_git_while_the_synchronous_effect_is_executing() {
        let (root, context, _head) = context();
        let (stage, stop) = context.stage();
        let flag = stage.blocking_cancellation();
        let heartbeat = root.path().join("git-heartbeat");
        let path = heartbeat.clone();
        let repo = root.path().join("repo");
        let worker = tokio::task::spawn_blocking(move || {
            crate::git::run(&["-c".into(),"alias.synthetic=!while :; do printf x >> \"$SYNTHETIC_HEARTBEAT\"; sleep .01; done".into(),"synthetic".into()],&repo,&std::collections::BTreeMap::from([("GIT_CONFIG_GLOBAL".into(),"/dev/null".into()),("GIT_CONFIG_SYSTEM".into(),"/dev/null".into()),("SYNTHETIC_HEARTBEAT".into(),path.to_string_lossy().into_owned())]),1024,false,flag.flag())
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !heartbeat.exists() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        drop(stop);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), worker)
                .await
                .unwrap()
                .unwrap(),
            Err(crate::git::Error::Cancelled)
        ));
        assert!(context.active().is_ok());
        let length = std::fs::metadata(&heartbeat).unwrap().len();
        assert!(length > 0);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(std::fs::metadata(&heartbeat).unwrap().len(), length);
    }
}
