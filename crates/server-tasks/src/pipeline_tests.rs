#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    code::Settings,
    execution::{Engine, EngineLease, Open},
    facts::Repository,
    role_pin::{Authority, Endpoint, Fault},
    role_stream::{Body, Error, Response, Wait},
    start::Request,
};
use std::{
    collections::VecDeque,
    path::Path,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::watch,
};
struct Fixed;
impl crate::context::Clock for Fixed {
    fn now(&self) -> i64 {
        1234
    }
}
struct Stream(VecDeque<Vec<u8>>);
impl Body for Stream {
    fn next(&mut self) -> Wait<'_, Result<Option<Vec<u8>>, Error>> {
        Box::pin(async { Ok(self.0.pop_front()) })
    }
}
struct Host {
    input: JValue,
    plan: JValue,
    reviews: AtomicUsize,
}
impl Authority for Host {
    fn credentials(&self) -> Vec<Zeroizing<String>> {
        vec![]
    }
    fn endpoint(&self) -> Result<Endpoint, Fault> {
        Ok(Endpoint {
            configured: !yes(self.input.get("pinUnavailable")),
            external: false,
            model: Some("synthetic-coder".into()),
            machine: None,
            schema_supported: false,
            reasoning_model: false,
        })
    }
    fn loaded(
        &self,
        _: &Endpoint,
        _: watch::Receiver<bool>,
    ) -> Wait<'_, Result<Vec<String>, Fault>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn ready(&self, _: &Endpoint, _: watch::Receiver<bool>) -> Wait<'_, Result<(), Fault>> {
        Box::pin(async { Ok(()) })
    }
    fn budget(&self, _: &str, _: watch::Receiver<bool>) -> Wait<'_, Result<Option<String>, Fault>> {
        Box::pin(async { Ok(None) })
    }
    fn send(
        &self,
        _: &Endpoint,
        payload: JValue,
        _: watch::Receiver<bool>,
    ) -> Wait<'_, Result<Response, Error>> {
        Box::pin(async move {
            let JValue::Arr(messages) = payload.get("messages") else {
                panic!()
            };
            let user = messages[1].get("content").as_str().unwrap();
            let answer = if user.contains("Write a bounded execution plan") {
                self.plan.clone()
            } else if user.contains("reviewing a finished code change") {
                let n = self.reviews.fetch_add(1, Ordering::SeqCst) + 1;
                if n <= self.input.get("changes").as_num().unwrap_or(0.0) as usize {
                    object(vec![
                        ("verdict", text("request_changes")),
                        ("summary", text("Needs a test.")),
                        (
                            "findings",
                            JValue::Arr(vec![object(vec![
                                ("severity", text("major")),
                                ("file", text("widget.js")),
                                ("message", text(format!("Add a test (round {n})."))),
                            ])]),
                        ),
                    ])
                } else {
                    object(vec![
                        ("verdict", text("approve")),
                        ("summary", text("Looks right.")),
                        ("findings", JValue::Arr(vec![])),
                    ])
                }
            } else if user.contains("You are the Auditor.") {
                if yes(self.input.get("badAudit")) {
                    object(vec![("invalid", JValue::Bool(true))])
                } else {
                    object(vec![
                        ("completeness", text("complete")),
                        ("summary", text("All plan steps have evidence.")),
                        (
                            "evidence",
                            JValue::Arr(vec![object(vec![
                                ("source", text("tests")),
                                ("note", text("Measured at head.")),
                            ])]),
                        ),
                        ("gaps", JValue::Arr(vec![])),
                    ])
                }
            } else {
                panic!("unknown role")
            };
            let event = object(vec![(
                "choices",
                JValue::Arr(vec![object(vec![(
                    "delta",
                    object(vec![("content", text(wtf16::stringify(&answer).unwrap()))]),
                )])]),
            )]);
            let raw = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                wtf16::stringify(&event).unwrap()
            );
            Ok(Response {
                status: 200,
                body: Some(Box::new(Stream(VecDeque::from([raw.into_bytes()])))),
            })
        })
    }
}
struct Binding(Arc<Host>);
impl Models for Binding {
    fn bind(&self, ctx: Arc<JournalContext>) -> Result<Arc<dyn Authority>, Failure> {
        assert_eq!(ctx.scope().user, "11111111-1111-4111-8111-111111111111");
        Ok(self.0.clone())
    }
}
struct CodingEngine;
impl Engine for CodingEngine {
    fn open<'a>(
        &'a self,
        ctx: Arc<JournalContext>,
        model: Option<&'a str>,
        machine: Option<&'a JValue>,
    ) -> Open<'a> {
        Box::pin(async move {
            assert_eq!(ctx.scope().user, "11111111-1111-4111-8111-111111111111");
            assert_eq!(model, Some("synthetic-coder"));
            assert_eq!(
                machine,
                Some(&JValue::Null),
                "a pinned local task is not an unpinned task"
            );
            Ok(EngineLease::new(
                Some("http://engine.test/v1".into()),
                None,
                Some("synthetic-coder".into()),
                Some(8192.0),
                String::new(),
                None,
            ))
        })
    }
}
async fn line(read: &mut (impl tokio::io::AsyncBufRead + Unpin)) -> Option<JValue> {
    let mut line = String::new();
    if read.read_line(&mut line).await.unwrap() == 0 {
        None
    } else {
        Some(wtf16::parse(&line).unwrap())
    }
}
async fn send(write: &mut (impl tokio::io::AsyncWrite + Unpin), value: JValue) {
    write
        .write_all(format!("{}\n", wtf16::stringify(&value).unwrap()).as_bytes())
        .await
        .unwrap();
}
fn reply(request: &JValue, value: JValue) -> JValue {
    object(vec![
        ("jsonrpc", text("2.0")),
        ("id", request.get("id").clone()),
        ("result", value),
    ])
}
async fn coding(
    listener: TcpListener,
    input: JValue,
    mut stopped: watch::Receiver<bool>,
) -> Vec<String> {
    let mut prompts = Vec::new();
    let mut round = 0;
    loop {
        let stream = tokio::select! {biased;_=stopped.changed()=>break,stream=listener.accept()=>stream.unwrap().0};
        let (read, mut write) = stream.into_split();
        let mut read = tokio::io::BufReader::new(read);
        let start = line(&mut read).await.unwrap();
        assert_eq!(start.get("noevia").as_str(), Some("start"));
        let cwd = start.get("cwd").as_str().unwrap();
        let init = line(&mut read).await.unwrap();
        send(&mut write, reply(&init, object(vec![]))).await;
        let session = line(&mut read).await.unwrap();
        send(
            &mut write,
            reply(&session, object(vec![("sessionId", text("session"))])),
        )
        .await;
        let prompt = line(&mut read).await.unwrap();
        let JValue::Arr(parts) = prompt.get("params").get("prompt") else {
            panic!()
        };
        prompts.push(parts[0].get("text").as_str().unwrap().into());
        round += 1;
        if !yes(input.get("unchanged")) {
            std::fs::write(
                Path::new(cwd).join("widget.js"),
                format!("module.exports = () => 2; // round {round}\n"),
            )
            .unwrap();
            crate::verify::tests::git(Path::new(cwd), &["add", "widget.js"]);
            crate::verify::tests::git(
                Path::new(cwd),
                &["commit", "-qm", &format!("round {round}")],
            );
        }
        send(
            &mut write,
            reply(&prompt, object(vec![("stopReason", text("end_turn"))])),
        )
        .await;
        assert!(line(&mut read).await.is_none());
    }
    prompts
}
async fn verifier(listener: TcpListener, input: JValue, mut stopped: watch::Receiver<bool>) {
    let mut round = 0;
    loop {
        let mut stream = tokio::select! {biased;_=stopped.changed()=>break,stream=listener.accept()=>stream.unwrap().0};
        let mut raw = String::new();
        tokio::io::BufReader::new(&mut stream)
            .read_line(&mut raw)
            .await
            .unwrap();
        let request = wtf16::parse(&raw).unwrap();
        round += 1;
        let passed = round > input.get("failedTests").as_num().unwrap_or(0.0) as usize;
        let tail = if passed {
            "ok 1 widget\n"
        } else {
            "not ok 1 widget returns two\n"
        };
        send(
            &mut stream,
            object(vec![
                ("noevia", text("verify-result")),
                ("nonce", request.get("nonce").clone()),
                ("ok", JValue::Bool(true)),
                ("headSha", request.get("headSha").clone()),
                ("exitCode", JValue::Num(if passed { 0.0 } else { 1.0 })),
                ("signal", JValue::Null),
                ("timedOut", JValue::Bool(false)),
                ("tail", text(tail)),
                ("totalBytes", JValue::Num(tail.len() as f64)),
                ("truncated", JValue::Bool(false)),
            ]),
        )
        .await;
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
    }
}
fn normalize(value: &JValue, task: &str, repo: &str) -> JValue {
    fn walk(value: &JValue, task: &str, repo: &str) -> JValue {
        match value {
            JValue::Obj(fields) => JValue::Obj(
                fields
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            if k == "at" {
                                JValue::Num(0.0)
                            } else {
                                walk(v, task, repo)
                            },
                        )
                    })
                    .collect(),
            ),
            JValue::Arr(values) => {
                JValue::Arr(values.iter().map(|v| walk(v, task, repo)).collect())
            }
            JValue::Str(s) => {
                let s = s.replace(task, "TASK").replace(repo, "<REPO>");
                let chars = s.chars().collect::<Vec<_>>();
                let mut out = String::new();
                let mut i = 0;
                while i < chars.len() {
                    if chars[i].is_ascii_hexdigit() && chars[i].is_ascii_lowercase()
                        || chars[i].is_ascii_digit()
                    {
                        let start = i;
                        while i < chars.len()
                            && (chars[i].is_ascii_digit() || matches!(chars[i], 'a'..='f'))
                        {
                            i += 1;
                        }
                        match i - start {
                            64 => out.push_str("HASH"),
                            40 => out.push_str("SHA"),
                            12 if s.starts_with("Revision ") && s.contains(" is bound to ") => {
                                out.push_str("SHORT")
                            }
                            _ => out.extend(chars[start..i].iter()),
                        }
                    } else {
                        out.push(chars[i]);
                        i += 1;
                    }
                }
                text(out)
            }
            _ => value.clone(),
        }
    }
    walk(value, task, repo)
}
#[tokio::test]
async fn complete_native_pipeline_matches_node_across_rounds_cards_and_failures() {
    let fixtures =
        wtf16::parse(include_str!("../tests/fixtures/pipeline-runtime-node.json")).unwrap();
    let JValue::Arr(fixtures) = fixtures else {
        panic!()
    };
    for fixture in fixtures {
        let input = fixture.get("input").clone();
        let (root, ctx, _base) = crate::verify::tests::context();
        let repo = root.path().join("repo");
        let task_id = ctx.task().to_owned();
        let request = Request {
            repository: Repository {
                id: "fixture".into(),
                path: repo.clone(),
            },
            prompt: "Make the widget return two.".into(),
            capabilities: vec!["read".into(), "edit".into(), "execute".into()],
            domains: vec![],
            harness: "opencode".into(),
            preparation: "planner".into(),
            model: None,
            approve_plan: yes(input.get("approvePlan")),
        };
        let mut resources =
            Resources::claim(ctx.clone(), &request, None, true, &AtomicBool::new(false))
                .await
                .unwrap();
        crate::verify::tests::git(&repo, &["checkout", "-q", "--detach"]);
        emit(&ctx, "job.started", object(vec![])).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let code_endpoint = listener.local_addr().unwrap().to_string();
        let verify_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let verify_endpoint = verify_listener.local_addr().unwrap().to_string();
        let (stop, stopped) = watch::channel(false);
        let coding = tokio::spawn(coding(listener, input.clone(), stopped.clone()));
        let verifying = tokio::spawn(verifier(verify_listener, input.clone(), stopped));
        let host = Arc::new(Host {
            input: input.clone(),
            plan: fixture.get("plan").clone(),
            reviews: AtomicUsize::new(0),
        });
        let merge = input.get("merge") != &JValue::Bool(false);
        let pipeline = Pipeline {
            executor: Arc::new(Executor {
                engine: Arc::new(CodingEngine),
                endpoint: code_endpoint,
                spawn: None,
                owner: None,
                sandboxed: false,
                path: None,
                lang: None,
                review: None,
            }),
            models: Some(Arc::new(Binding(host))),
            verify: Arc::new(Verify {
                endpoint: if yes(input.get("verifyUnavailable")) {
                    None
                } else {
                    Some(verify_endpoint)
                },
                timeout: Duration::from_secs(5),
                retry: Duration::ZERO,
                tail_bytes: 16384,
                clock: Arc::new(Fixed),
            }),
            egress: None,
            merge: Arc::new(move || merge),
            constrain: Arc::new(|| false),
            audit_writeup: true,
            max_loops: 2,
            limits: Limits::default(),
        };
        let task = Task {
            request,
            context: String::new(),
            shared_context: false,
            settings: Settings {
                merge,
                ..Settings::default()
            },
        };
        assert_eq!(
            hash(fixture.get("plan")).unwrap(),
            fixture.get("planHash").as_str().unwrap()
        );
        let (decide_done, mut deciding_done) = watch::channel(false);
        let account = ctx.account().clone();
        let scope = ctx.scope().clone();
        let decisions = input.clone();
        let deciding = tokio::spawn(async move {
            loop {
                let cards = account.control.cards(&scope).unwrap();
                for (id, task, card) in cards {
                    let decision = if card.get("action").as_str() == Some("approve_plan") {
                        decisions.get("planDecision").as_str().unwrap_or("approve")
                    } else {
                        decisions.get("decision").as_str().unwrap_or("approve")
                    };
                    let _ = account.control.decide(&scope, &task, &id, decision);
                }
                tokio::select! {biased;_=deciding_done.changed()=>break,_=tokio::time::sleep(Duration::from_millis(2))=>{}}
            }
        });
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            pipeline.run(ctx.clone(), &mut resources, &task),
        )
        .await
        .unwrap();
        match result {
            Ok(result) => emit(&ctx, "job.completed", object(vec![("result", result)])).unwrap(),
            Err(error) => {
                let mut data = object(vec![("error", text(error.recorded()))]);
                if let Some(result) = error.result {
                    set(&mut data, "result", result);
                }
                emit(&ctx, "job.failed", data).unwrap();
            }
        }
        decide_done.send_replace(true);
        deciding.await.unwrap();
        stop.send_replace(true);
        let prompts = coding.await.unwrap();
        verifying.await.unwrap();
        let job = ctx.job().unwrap();
        let norm = |v: &JValue| normalize(v, &task_id, &repo.to_string_lossy());
        assert_eq!(
            job.status,
            *fixture.get("status"),
            "input {}",
            wtf16::stringify(&input).unwrap()
        );
        assert_eq!(job.error, *fixture.get("error"));
        assert_eq!(norm(&job.result), *fixture.get("result"));
        let authority = job.authoritative.as_ref().unwrap();
        assert_eq!(
            norm(&JValue::Arr(authority.stages.clone())),
            *fixture.get("stages")
        );
        assert_eq!(norm(&authority.revision), *fixture.get("revision"));
        assert_eq!(norm(&JValue::Arr(job.artifacts)), *fixture.get("artifacts"));
        assert_eq!(
            JValue::Arr(prompts.into_iter().map(text).collect()),
            *fixture.get("executorPrompts")
        );
        let raw = std::fs::read_to_string(
            root.path()
                .join("users")
                .join(&ctx.scope().user)
                .join("jobs")
                .join(format!("{task_id}.jsonl")),
        )
        .unwrap();
        let rows = raw
            .lines()
            .map(|line| wtf16::parse(line).unwrap())
            .collect::<Vec<_>>();
        let cards = rows
            .iter()
            .filter(|r| r.get("type").as_str() == Some("approval.requested"))
            .map(|r| r.get("data").clone())
            .collect();
        assert_eq!(norm(&JValue::Arr(cards)), *fixture.get("cards"));
        let events = rows
            .iter()
            .filter(|r| {
                matches!(
                    r.get("type").as_str(),
                    Some(
                        "task.stage"
                            | "task.revision"
                            | "review.requested"
                            | "review.completed"
                            | "review.failed"
                            | "approval.requested"
                            | "approval.decided"
                    )
                )
            })
            .map(|r| {
                object(vec![
                    ("type", r.get("type").clone()),
                    ("data", r.get("data").clone()),
                ])
            })
            .collect();
        assert_eq!(norm(&JValue::Arr(events)), *fixture.get("events"));
        assert_eq!(
            resources
                .released()
                .unwrap()
                .workspace
                .as_ref()
                .unwrap()
                .get("status")
                .as_str(),
            Some("released")
        );
        if yes(job.result.get("merged")) {
            assert_eq!(
                crate::verify::tests::git(&repo, &["rev-parse", "main"]),
                job.result.get("headSha").as_str().unwrap()
            );
        }
    }
}
