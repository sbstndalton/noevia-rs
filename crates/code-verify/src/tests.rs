#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use super::*;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::Mutex;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn no_checkout(_: &str, _: &str) -> Result<String, CheckoutError> {
    Ok("/workspaces/qa-repo".to_string())
}

fn request<'a>(
    repo: &'a str,
    sha: &'a str,
    revision: f64,
    checkout: &'a VerifyCheckout<'a>,
) -> Request<'a> {
    Request {
        task_id: "qa-task",
        repo,
        head_sha: sha,
        revision,
        verify_checkout: checkout,
        cancel: None,
    }
}

fn options(endpoint: Option<String>) -> Options {
    let mut o = Options::new(endpoint);
    o.timeout = Duration::from_secs(5);
    o.connect_retry = Duration::from_millis(0);
    o.nonce = Box::new(|| Some("n-1".to_string()));
    o
}

/// A one-shot unix verifier answering with `reply`, `{NONCE}` filled in.
fn serve(dir: &std::path::Path, reply: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let path = dir.join("v.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let handle = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(s.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        s.write_all(reply.replace("{SHA}", SHA).as_bytes()).unwrap();
        line
    });
    (format!("unix:{}", path.display()), handle)
}

fn run(v: &Verify, repo: &str, sha: &str, revision: f64) -> (Outcome, Vec<String>) {
    let events = Mutex::new(Vec::new());
    let out = v.run(&request(repo, sha, revision, &no_checkout), &mut |e| {
        events.lock().unwrap().push(e.kind().to_string());
    });
    (out, events.into_inner().unwrap())
}

#[test]
fn tail_cap_clamps_like_node() {
    let cap = |t: f64| {
        let mut o = Options::new(None);
        o.tail_bytes = t;
        Verify::new(o).tail_cap()
    };
    assert_eq!(cap(0.0), 16384);
    assert_eq!(cap(f64::NAN), 16384);
    assert_eq!(cap(10.0), 1024);
    assert_eq!(cap(-5.0), 1024);
    assert_eq!(cap(1e9), 262_144);
    assert_eq!(cap(f64::INFINITY), 262_144);
    assert_eq!(cap(5000.9), 5000);
}

#[test]
fn input_checks_come_in_node_order() {
    let v = Verify::new(options(Some("unix:/nonexistent/v.sock".into())));
    let code = |r: &str, s: &str, n: f64| run(&v, r, s, n).0.error.unwrap().code;
    assert_eq!(code("", SHA, 1.0), "bad_request");
    assert_eq!(code("a b", SHA, 1.0), "bad_request");
    assert_eq!(code(&"a".repeat(65), SHA, 1.0), "bad_request");
    assert_eq!(code("repo\n", SHA, 1.0), "bad_request");
    assert_eq!(code("repo", &SHA[1..], 1.0), "bad_sha");
    assert_eq!(code("repo", &SHA.to_uppercase(), 1.0), "bad_sha");
    assert_eq!(code("repo", SHA, 1.5), "bad_request");
    assert_eq!(code("repo", SHA, -1.0), "bad_request");
    assert_eq!(code("repo", SHA, f64::NAN), "bad_request");
    let none = Verify::new(options(Some(String::new())));
    assert!(!none.available());
    assert_eq!(run(&none, "", "", 0.0).0.error.unwrap().code, "no_verifier");
}

#[test]
fn a_measurement_is_a_report_bound_to_its_request() {
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, server) = serve(
        dir.path(),
        "{\"noevia\":\"verify-result\",\"ok\":true,\"nonce\":\"n-1\",\"headSha\":\"{SHA}\",\"exitCode\":0,\"tail\":\"ok\\r\\n\",\"totalBytes\":9}\n",
    );
    let v = Verify::new(options(Some(endpoint)));
    let (out, events) = run(&v, "qa-repo", SHA, 3.0);
    assert_eq!(out.status, Status::Passed);
    assert_eq!(
        events,
        ["step.started", "artifact.created", "step.completed"]
    );
    let r = out.report.unwrap();
    assert!(r.passed() && r.truncated() && r.tail() == "ok\n");
    assert!(is_measured(&r, "qa-task", SHA, 3.0));
    assert!(!is_measured(&r, "other-task", SHA, 3.0));
    assert!(!is_measured(&r, "qa-task", SHA, 4.0));
    assert!(!is_measured(&r, "qa-task", &"f".repeat(40), 3.0));
    assert_eq!(
        server.join().unwrap(),
        format!("{{\"noevia\":\"verify\",\"repo\":\"qa-repo\",\"source\":\"/workspaces/qa-repo\",\"headSha\":\"{SHA}\",\"nonce\":\"n-1\"}}\n")
    );
}

#[test]
fn report_json_keeps_nodes_field_order() {
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, server) = serve(
        dir.path(),
        "{\"noevia\":\"verify-result\",\"ok\":true,\"nonce\":\"n-1\",\"headSha\":\"{SHA}\",\"exitCode\":1,\"signal\":\"SIGSEGV\"}\n",
    );
    let v = Verify::new(options(Some(endpoint)));
    let (out, _) = run(&v, "qa-repo", SHA, 0.0);
    server.join().unwrap();
    let keys: Vec<String> = match out.report.unwrap().to_json() {
        JValue::Obj(items) => items.into_iter().map(|(k, _)| k).collect(),
        _ => panic!(),
    };
    assert_eq!(
        keys.join(","),
        "kind,passed,exitCode,signal,timedOut,tail,tailBytes,totalBytes,truncated,taskId,headSha,revision,repo,durationMs,measuredBy"
    );
}

#[test]
fn a_nonce_that_cannot_be_made_is_an_error() {
    let mut o = options(Some("unix:/nonexistent/v.sock".into()));
    o.nonce = Box::new(|| None);
    let (out, _) = run(&Verify::new(o), "qa-repo", SHA, 0.0);
    assert_eq!(out.error.unwrap().code, "nonce");
    assert!(
        random_nonce().is_some_and(|n| n.len() == 32 && n.bytes().all(|b| b.is_ascii_hexdigit()))
    );
}

struct Stub {
    available: bool,
    outcome: Mutex<Option<Outcome>>,
    measured: bool,
    panics: bool,
}

impl Verifier for Stub {
    fn available(&self) -> bool {
        self.available
    }
    fn run(&self, _: &Request<'_>, _: &mut dyn FnMut(&Event<'_>)) -> Outcome {
        assert!(!self.panics, "boom");
        self.outcome.lock().unwrap().take().unwrap()
    }
    fn is_measured(&self, _: &Report, _: &str, _: &str, _: f64) -> bool {
        self.measured
    }
}

fn stub(outcome: Outcome, measured: bool) -> Adapter {
    Adapter::new(Some(Box::new(Stub {
        available: true,
        outcome: Mutex::new(Some(outcome)),
        measured,
        panics: false,
    })))
}

fn adapted(a: &Adapter) -> Outcome {
    a.run(&request("qa-repo", SHA, 1.0, &no_checkout), &mut |_| {})
}

fn err_outcome(status: Status, code: &str, message: &str) -> Outcome {
    Outcome {
        status,
        report: None,
        error: Some(VerifyError {
            code: code.into(),
            message: message.into(),
        }),
    }
}

#[test]
fn adapter_maps_every_class() {
    let none = Adapter::new(None);
    assert!(!none.available());
    assert_eq!(adapted(&none).error.unwrap().code, "no_verifier");

    let a = stub(err_outcome(Status::Unavailable, "busy", ""), true);
    let o = adapted(&a);
    assert_eq!(
        (o.status, o.error.unwrap().message.as_str()),
        (Status::Unavailable, "Verification is unavailable.")
    );

    let long = "m".repeat(400);
    let o = adapted(&stub(
        err_outcome(Status::Error, &"c".repeat(50), &long),
        true,
    ));
    let e = o.error.unwrap();
    assert_eq!((e.code.len(), e.message.len()), (40, 300));

    let o = adapted(&stub(err_outcome(Status::Error, "", ""), true));
    let e = o.error.unwrap();
    assert_eq!(
        (e.code.as_str(), e.message.as_str()),
        ("error", "Verification could not run.")
    );

    // A pass with no report is not measured.
    let o = adapted(&stub(
        Outcome {
            status: Status::Passed,
            report: None,
            error: None,
        },
        true,
    ));
    assert_eq!(o.error.unwrap().code, "unmeasured");
}

#[test]
fn adapter_treats_a_panicking_verifier_as_one_that_threw() {
    let a = Adapter::new(Some(Box::new(Stub {
        available: true,
        outcome: Mutex::new(None),
        measured: true,
        panics: true,
    })));
    let o = adapted(&a);
    assert_eq!(o.error.unwrap().code, "verifier_threw");
    let off = Adapter::new(Some(Box::new(Stub {
        available: false,
        outcome: Mutex::new(None),
        measured: true,
        panics: true,
    })));
    assert!(!off.available());
}

#[test]
fn adapter_rederives_passed_and_checks_the_binding() {
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, server) = serve(
        dir.path(),
        "{\"noevia\":\"verify-result\",\"ok\":true,\"nonce\":\"n-1\",\"headSha\":\"{SHA}\",\"exitCode\":2}\n",
    );
    let v = Verify::new(options(Some(endpoint)));
    let (real, _) = run(&v, "qa-repo", SHA, 1.0);
    server.join().unwrap();
    let report = real.report.clone().unwrap();
    // A "passed" status over a failing report is a failure; the same report for another task is
    // not measured.
    let lying = Outcome {
        status: Status::Passed,
        ..real
    };
    let a = stub(lying.clone(), true);
    assert_eq!(adapted(&a).status, Status::Failed);
    assert!(!is_measured(&report, "qa-task", SHA, 2.0));
    let a = stub(lying, false);
    assert_eq!(adapted(&a).error.unwrap().code, "unmeasured");
}

#[test]
fn log_and_event_json_match_nodes_shapes() {
    let stopped = LogEntry::Stopped {
        task_id: "t".into(),
        revision: 2.0,
        status: Status::Error,
        code: "x".into(),
    };
    assert_eq!(
        wtf16::stringify(&stopped.to_json()).unwrap(),
        r#"{"event":"code.verify","taskId":"t","revision":2,"status":"error","code":"x"}"#
    );
    let done = Event::StepCompleted {
        failed: true,
        reason: Some("why"),
    };
    assert_eq!(
        wtf16::stringify(&done.data()).unwrap(),
        r#"{"id":"tests","failed":true,"reason":"why"}"#
    );
    assert_eq!(
        wtf16::stringify(&Event::StepStarted.data()).unwrap(),
        r#"{"id":"tests","title":"Run the tests"}"#
    );
}
