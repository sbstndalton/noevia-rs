//! crates/code-acp: the transcripts core's own code-acp.cjs produced against a scripted fake
//! sandbox (tests/fixtures/node.v1.json, tools/gen-fixtures.cjs), replayed against the Rust client
//! over a real TCP socket: every line written, every handler call, every log line, and how connect
//! and prompt ended must match.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use code_acp::{connect, Cancel, Error, Handlers, Options, Proxy, Refusal, Transport};
use js_json::{wtf16, JValue};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const PINNED_ACP: &str = "5521d82bca54984b40874865483447758cc7cb42ab5497ee3e42a592c874ffc2";
const PINNED_META: &str = "1fb1a895f1f2db33d9e075848935167dce774b9a6ea4f0d1c5ca10621831ba8b";
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/node.v1.json");

fn fixture() -> JValue {
    wtf16::parse(&fs::read_to_string(FIXTURE).unwrap()).unwrap()
}

fn text(v: &JValue) -> String {
    wtf16::stringify(v).unwrap()
}

#[derive(Default)]
struct Record {
    handlers: Vec<JValue>,
    logs: Vec<String>,
}

struct Scripted {
    answers: Mutex<BTreeMap<String, VecDeque<JValue>>>,
    record: Arc<Mutex<Record>>,
}

impl Scripted {
    fn answer(&self, handler: &str, params: &JValue) -> Result<JValue, Refusal> {
        self.record.lock().unwrap().handlers.push(JValue::obj([
            ("handler", JValue::Str(handler.into())),
            ("params", params.clone()),
        ]));
        let next = self
            .answers
            .lock()
            .unwrap()
            .get_mut(handler)
            .and_then(VecDeque::pop_front);
        let Some(next) = next else {
            return Err(Refusal {
                code: Some(-32603.0),
                message: "no QA answer".into(),
                data: None,
            });
        };
        match next.get("error") {
            JValue::Undefined => Ok(next.get("ok").clone()),
            e => Err(Refusal {
                code: e.get("code").as_num(),
                message: e.get("message").as_str().unwrap_or("").to_string(),
                data: match e.get("data") {
                    JValue::Undefined => None,
                    d => Some(d.clone()),
                },
            }),
        }
    }
}

impl Handlers for Scripted {
    fn request_permission(&self, p: &JValue) -> Result<JValue, Refusal> {
        self.answer("requestPermission", p)
    }
    fn read_text_file(&self, p: &JValue) -> Result<JValue, Refusal> {
        self.answer("readTextFile", p)
    }
    fn write_text_file(&self, p: &JValue) -> Result<JValue, Refusal> {
        self.answer("writeTextFile", p)
    }
    fn session_update(&self, u: &JValue) {
        self.record.lock().unwrap().handlers.push(JValue::obj([
            ("handler", JValue::Str("sessionUpdate".into())),
            ("params", u.clone()),
        ]));
    }
}

fn error_json(e: &Error) -> JValue {
    JValue::obj([
        ("message", JValue::Str(e.message.clone())),
        (
            "publicMessage",
            e.public_message.clone().map_or(JValue::Null, JValue::Str),
        ),
        (
            "status",
            e.status.map_or(JValue::Null, |s| JValue::Num(f64::from(s))),
        ),
        ("code", e.code.clone().unwrap_or(JValue::Null)),
    ])
}

fn ok_or_error(r: Result<JValue, Error>) -> JValue {
    match r {
        Ok(v) => JValue::obj([("ok", v)]),
        Err(e) => JValue::obj([("error", error_json(&e))]),
    }
}

/// Runs one scenario against the Rust client; the transcript in the generator's shape.
fn run(f: &JValue, sc: &JValue) -> JValue {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let home = match sc.get("home") {
        JValue::Obj(files) => {
            let dir = tempfile::tempdir().unwrap();
            for (rel, body) in files {
                let p = dir.path().join(rel);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(p, body.as_str().unwrap()).unwrap();
            }
            Some(dir)
        }
        _ => None,
    };
    let home_path = home
        .as_ref()
        .map(|d| d.path().to_str().unwrap().to_string());
    let o = sc.get("options");
    let record = Arc::new(Mutex::new(Record::default()));
    let mut options = Options::new(
        Transport::Socket(format!("127.0.0.1:{port}")),
        o.get("cwd").as_str().unwrap(),
    );
    options.home = home_path
        .clone()
        .or_else(|| o.get("home").as_str().map(str::to_string));
    if let JValue::Obj(env) = o.get("env") {
        options.env = env
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
    }
    options.permission = match o.get("permission") {
        JValue::Undefined | JValue::Null => None,
        p => Some(p.clone()),
    };
    options.harness = o.get("harness").as_str().map(str::to_string);
    options.model = o.get("model").as_str().map(str::to_string);
    if let JValue::Obj(_) = o.get("proxy") {
        options.proxy = Some(Proxy {
            url: o.get("proxy").get("url").as_str().unwrap().to_string(),
            no_proxy: o.get("proxy").get("noProxy").as_str().map(str::to_string),
        });
    }
    options.base_path = f.get("env").get("PATH").as_str().map(str::to_string);
    options.base_lang = f.get("env").get("LANG").as_str().map(str::to_string);
    if let Some(ms) = o.get("handshakeMs").as_num() {
        options.handshake = Duration::from_millis(ms as u64);
    }
    let logs = record.clone();
    options.log = Box::new(move |t| logs.lock().unwrap().logs.push(t.to_string()));
    let mut answers = BTreeMap::new();
    if let JValue::Obj(a) = sc.get("answers") {
        for (k, list) in a {
            let JValue::Arr(list) = list else { panic!() };
            answers.insert(k.clone(), list.iter().cloned().collect());
        }
    }
    let handlers = Arc::new(Scripted {
        answers: Mutex::new(answers),
        record: record.clone(),
    });
    let cancel = Cancel::new();
    let (prompt_tx, prompt_rx) = mpsc::channel::<String>();
    let client = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            let connected = connect(options, handlers, cancel);
            let (conn, connect_json) = match connected {
                Ok(c) => {
                    let j = JValue::obj([(
                        "ok",
                        JValue::obj([
                            ("sessionId", c.session().clone()),
                            ("agent", c.agent().clone()),
                        ]),
                    )]);
                    (Some(c), j)
                }
                Err(e) => (None, JValue::obj([("error", error_json(&e))])),
            };
            let mut prompt = JValue::Null;
            if let (Some(conn), Ok(text)) = (
                conn.as_ref(),
                prompt_rx.recv_timeout(Duration::from_secs(10)),
            ) {
                prompt = ok_or_error(conn.prompt(&text));
            }
            (connect_json, prompt, conn.map(|_| ()))
        })
    };
    let JValue::Arr(script) = sc.get("script") else {
        panic!()
    };
    let mut lines: Vec<JValue> = Vec::new();
    let mut closed = false;
    if !script.is_empty() {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => panic!("no connection: {e}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut writer: TcpStream = stream.try_clone().unwrap();
        for step in script {
            if step.get("expect").as_bool() == Some(true) {
                let mut l = String::new();
                reader.read_line(&mut l).unwrap();
                let l = l.strip_suffix('\n').unwrap_or(&l).to_string();
                let l = match &home_path {
                    Some(h) => l.replace(h.as_str(), "<HOME>"),
                    None => l,
                };
                lines.push(JValue::Str(l));
            } else if let Some(chunk) = step.get("send").as_str() {
                std::thread::sleep(Duration::from_millis(5));
                writer.write_all(chunk.as_bytes()).unwrap();
                writer.flush().unwrap();
                std::thread::sleep(Duration::from_millis(5));
            } else if let Some(t) = step.get("prompt").as_str() {
                prompt_tx.send(t.to_string()).unwrap();
            } else if step.get("abort").as_bool() == Some(true) {
                cancel.cancel();
            } else if step.get("close").as_bool() == Some(true) {
                stream.shutdown(Shutdown::Both).unwrap();
            } else if step.get("expectClose").as_bool() == Some(true) {
                let mut rest = String::new();
                let n = reader.read_line(&mut rest).unwrap_or(0);
                assert_eq!(n, 0, "more after the expected close: {rest}");
                closed = true;
            }
        }
    }
    drop(prompt_tx);
    let (connect_json, prompt, _) = client.join().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let r = record.lock().unwrap();
    JValue::obj([
        ("lines", JValue::Arr(lines)),
        ("handlers", JValue::Arr(r.handlers.clone())),
        (
            "logs",
            JValue::Arr(r.logs.iter().map(|l| JValue::Str(l.clone())).collect()),
        ),
        ("connect", connect_json),
        ("prompt", prompt),
        ("closed", JValue::Bool(closed)),
    ])
}

#[test]
fn fixture_records_its_sources() {
    let f = fixture();
    let sha = |n: &str| f.get("sources").get(n).as_str().unwrap().to_string();
    assert_eq!(sha("code-acp.cjs").len(), 64);
    assert_eq!(sha("code-meta.cjs").len(), 64);
}

#[test]
fn every_node_transcript_replays() {
    let f = fixture();
    let JValue::Obj(all) = f.get("scenarios") else {
        panic!()
    };
    for (name, sc) in all {
        // The spawn transport's transcript has no socket to replay over: see the spawn tests.
        if sc.get("transport").as_str() == Some("spawn") {
            continue;
        }
        let node = sc.get("transcript");
        let got = run(&f, sc);
        for key in ["lines", "handlers", "logs", "connect", "prompt"] {
            assert_eq!(text(got.get(key)), text(node.get(key)), "{name}: {key}");
        }
        let JValue::Arr(script) = sc.get("script") else {
            panic!()
        };
        if script
            .iter()
            .any(|s| s.get("expectClose").as_bool() == Some(true))
        {
            assert_eq!(got.get("closed"), node.get("closed"), "{name}: closed");
        }
    }
}

#[test]
fn fixture_sources_are_the_pinned_ones() {
    // core a72ae94: a fixture regenerated from other sources fails here until updated on purpose.
    let f = fixture();
    assert_eq!(
        f.get("sources").get("code-acp.cjs").as_str(),
        Some(PINNED_ACP)
    );
    assert_eq!(
        f.get("sources").get("code-meta.cjs").as_str(),
        Some(PINNED_META)
    );
}

struct Quiet;
impl Handlers for Quiet {
    fn request_permission(&self, _: &JValue) -> Result<JValue, Refusal> {
        Ok(JValue::Null)
    }
    fn read_text_file(&self, _: &JValue) -> Result<JValue, Refusal> {
        Ok(JValue::Null)
    }
    fn write_text_file(&self, _: &JValue) -> Result<JValue, Refusal> {
        Ok(JValue::Null)
    }
    fn session_update(&self, _: &JValue) {}
}

/// A sandbox stand-in on a socket: accepts once, answers the handshake, then runs `then`.
fn sandbox(then: impl FnOnce(BufReader<TcpStream>, TcpStream) + Send + 'static) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut w = s;
        let mut l = String::new();
        for answer in [
            None,
            Some(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
            Some(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"qa"}}"#),
        ] {
            l.clear();
            r.read_line(&mut l).unwrap();
            if let Some(a) = answer {
                writeln!(w, "{a}").unwrap();
            }
        }
        then(r, w);
    });
    port
}

#[test]
fn a_waiting_permission_does_not_hold_up_session_updates() {
    struct Waits {
        seen: Mutex<Vec<String>>,
        go: Mutex<Option<mpsc::Receiver<()>>>,
        updated: Mutex<Option<mpsc::Sender<()>>>,
    }
    impl Handlers for Waits {
        fn request_permission(&self, _: &JValue) -> Result<JValue, Refusal> {
            // Answered only once the update sent after this request has been handled.
            let go = self.go.lock().unwrap().take().unwrap();
            go.recv_timeout(Duration::from_secs(10)).unwrap();
            self.seen.lock().unwrap().push("permission answered".into());
            Ok(JValue::obj([("outcome", JValue::Str("cancelled".into()))]))
        }
        fn read_text_file(&self, _: &JValue) -> Result<JValue, Refusal> {
            Ok(JValue::Null)
        }
        fn write_text_file(&self, _: &JValue) -> Result<JValue, Refusal> {
            Ok(JValue::Null)
        }
        fn session_update(&self, _: &JValue) {
            self.seen.lock().unwrap().push("update".into());
            if let Some(tx) = self.updated.lock().unwrap().take() {
                tx.send(()).unwrap();
            }
        }
    }
    let (tx, rx) = mpsc::channel();
    let handlers = Arc::new(Waits {
        seen: Mutex::new(Vec::new()),
        go: Mutex::new(Some(rx)),
        updated: Mutex::new(Some(tx)),
    });
    let port = sandbox(|mut r, mut w| {
        let mut l = String::new();
        r.read_line(&mut l).unwrap(); // the prompt
        writeln!(
            w,
            r#"{{"jsonrpc":"2.0","id":"p","method":"session/request_permission","params":{{}}}}"#
        )
        .unwrap();
        writeln!(w, r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"plan"}}}}}}"#).unwrap();
        l.clear();
        r.read_line(&mut l).unwrap();
        assert!(
            l.contains(r#""id":"p","result":{"outcome":{"outcome":"cancelled"}}"#),
            "{l}"
        );
        writeln!(
            w,
            r#"{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}"#
        )
        .unwrap();
    });
    let conn = connect(
        Options::new(Transport::Socket(format!("127.0.0.1:{port}")), "/qa"),
        handlers.clone(),
        Cancel::new(),
    )
    .unwrap();
    let done = conn.prompt("QA").unwrap();
    assert_eq!(done.get("stopReason").as_str(), Some("end_turn"));
    assert_eq!(
        *handlers.seen.lock().unwrap(),
        vec!["update".to_string(), "permission answered".to_string()]
    );
}

#[test]
fn a_spawned_agent_gets_only_its_environment_and_its_group_is_stopped() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("env.out");
    let pids = dir.path().join("pids");
    let agent = dir.path().join("qa-agent");
    fs::write(
        &agent,
        format!(
            "#!/bin/sh\nread a\necho '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}'\nread b\necho '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"sessionId\":7}}}}'\nenv | sort > '{}'\nread c\nsleep 30 &\necho $! > '{}'\nwait\n",
            out.display(),
            pids.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
    let mut options = Options::new(
        Transport::Spawn {
            command: agent.to_str().unwrap().to_string(),
            args: Vec::new(),
        },
        dir.path().to_str().unwrap(),
    );
    options.env = vec![("QA_ONLY".into(), "1".into())];
    options.base_path = Some("/usr/bin:/bin".into());
    options.base_lang = None;
    options.grace = Duration::from_millis(200);
    let cancel = Cancel::new();
    let conn = connect(options, Arc::new(Quiet), cancel.clone()).unwrap();
    assert_eq!(conn.session_id(), "7");
    let prompting = std::thread::spawn(move || conn.prompt("QA"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !pids.exists() || fs::read_to_string(&pids).unwrap().trim().is_empty() {
        assert!(Instant::now() < deadline, "the agent never got the prompt");
        std::thread::sleep(Duration::from_millis(10));
    }
    let sleeper: u32 = fs::read_to_string(&pids).unwrap().trim().parse().unwrap();
    cancel.cancel();
    let e = prompting.join().unwrap().unwrap_err();
    assert_eq!(e.message, "Cancelled");
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::path::Path::new(&format!("/proc/{sleeper}")).exists() {
        let state = fs::read_to_string(format!("/proc/{sleeper}/stat")).unwrap_or_default();
        if state.contains(") Z ") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the agent's own child outlived the stop"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let env = fs::read_to_string(&out).unwrap();
    let keys: Vec<&str> = env
        .lines()
        .map(|l| l.split('=').next().unwrap())
        .filter(|k| !k.is_empty())
        .collect();
    for k in &keys {
        assert!(
            [
                "CURL_HOME",
                "PATH",
                "PWD",
                "QA_ONLY",
                "SHLVL",
                "WGETRC",
                "_",
                "OLDPWD"
            ]
            .contains(k),
            "unexpected variable {k} in {env}"
        );
    }
    assert!(keys.contains(&"QA_ONLY") && keys.contains(&"CURL_HOME"));
}

#[test]
fn difference_an_oversized_or_unholdable_line_is_logged_and_dropped() {
    let logs = Arc::new(Mutex::new(Vec::<String>::new()));
    let port = sandbox(|mut r, mut w| {
        let mut l = String::new();
        r.read_line(&mut l).unwrap();
        // 17 MiB without a newline, then a line with a character this port cannot hold.
        let big = vec![b'x'; 17 * 1024 * 1024];
        w.write_all(&big).unwrap();
        w.write_all(b"\n").unwrap();
        writeln!(
            w,
            "{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"t\":\"\u{10F800}\"}}}}"
        )
        .unwrap();
        writeln!(w, r#"{{"jsonrpc":"2.0","id":3,"result":{{"ok":true}}}}"#).unwrap();
    });
    let mut options = Options::new(Transport::Socket(format!("127.0.0.1:{port}")), "/qa");
    let sink = logs.clone();
    options.log = Box::new(move |t| sink.lock().unwrap().push(t.to_string()));
    let conn = connect(options, Arc::new(Quiet), Cancel::new()).unwrap();
    let done = conn.prompt("QA").unwrap();
    assert_eq!(done.get("ok"), &JValue::Bool(true));
    let logs = logs.lock().unwrap().clone();
    assert!(
        logs.contains(&"The harness sent an oversized message.".to_string()),
        "{logs:?}"
    );
    assert!(
        logs.contains(&"The harness sent something that is not JSON-RPC.".to_string()),
        "{logs:?}"
    );
}

#[test]
fn difference_claude_settings_are_never_read_through_a_link() {
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("settings.json"), r#"{"leaked":true}"#).unwrap();
    std::os::unix::fs::symlink(outside.path(), home.path().join(".claude")).unwrap();
    let e = code_acp::read_claude_settings(home.path().to_str()).unwrap_err();
    assert_eq!(e.status, Some(409));
    fs::remove_file(home.path().join(".claude")).unwrap();
    fs::create_dir(home.path().join(".claude")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("settings.json"),
        home.path().join(".claude/settings.json"),
    )
    .unwrap();
    assert!(code_acp::read_claude_settings(home.path().to_str()).is_err());
    fs::remove_file(home.path().join(".claude/settings.json")).unwrap();
    fs::write(home.path().join(".claude/settings.json"), r#"{"own":1}"#).unwrap();
    assert_eq!(
        text(&code_acp::read_claude_settings(home.path().to_str()).unwrap()),
        r#"{"own":1}"#
    );
}

#[test]
fn endpoints_are_host_and_port() {
    assert_eq!(
        code_acp::split_endpoint("sandbox:9000").unwrap(),
        ("sandbox".to_string(), 9000)
    );
    assert_eq!(
        code_acp::split_endpoint("[::1]:9000").unwrap(),
        ("::1".to_string(), 9000)
    );
    for bad in ["sandbox", "a:b:1", ":1", "x:", "x:70000", "[::1]9000"] {
        let e = code_acp::split_endpoint(bad).unwrap_err();
        assert_eq!(e.status, Some(500), "{bad}");
        assert_eq!(
            e.message,
            format!("CODE_HARNESS_ENDPOINT should be host:port, not \"{bad}\"")
        );
    }
}

#[test]
fn a_spawned_agent_that_exits_ends_the_prompt_as_node_says_and_its_group_goes_too() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    let node = f.get("scenarios").get("spawn-exit").get("transcript");
    let dir = tempfile::tempdir().unwrap();
    let got_lines = dir.path().join("lines");
    let got_env = dir.path().join("env");
    let pids = dir.path().join("pids");
    let agent = dir.path().join("qa-agent");
    // Records what it is sent and its environment, answers the handshake, takes the prompt,
    // leaves a child behind and exits 3.
    fs::write(
        &agent,
        format!(
            "#!/bin/sh\nenv > '{env}'\nread a\necho \"$a\" >> '{lines}'\necho '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}'\nread b\necho \"$b\" >> '{lines}'\necho '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"sessionId\":\"qa-spawned\"}}}}'\nread c\necho \"$c\" >> '{lines}'\nsleep 30 &\necho $! > '{pids}'\nexit 3\n",
            env = got_env.display(),
            lines = got_lines.display(),
            pids = pids.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
    let mut options = Options::new(
        Transport::Spawn {
            command: agent.to_str().unwrap().to_string(),
            args: vec!["--acp".into()],
        },
        "/qa/tree",
    );
    options.cwd = dir.path().to_str().unwrap().to_string();
    options.base_path = f.get("env").get("PATH").as_str().map(str::to_string);
    options.base_lang = f.get("env").get("LANG").as_str().map(str::to_string);
    options.grace = Duration::from_millis(300);
    let conn = connect(options, Arc::new(Quiet), Cancel::new()).unwrap();
    let t = Instant::now();
    let e = conn.prompt("QA").unwrap_err();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert_eq!(text(&error_json(&e)), text(node.get("prompt").get("error")));
    // What the agent was sent is what Node sent.
    let sent: Vec<JValue> = fs::read_to_string(&got_lines)
        .unwrap()
        .lines()
        // A real child needs a real working directory; Node's fake one was /qa/tree.
        .map(|l| JValue::Str(l.replace(dir.path().to_str().unwrap(), "/qa/tree")))
        .collect();
    assert_eq!(text(&JValue::Arr(sent)), text(node.get("lines")));
    // Its environment is Node's (the shell adds PWD, SHLVL and _).
    let env: BTreeMap<String, String> = fs::read_to_string(&got_env)
        .unwrap()
        .lines()
        .filter_map(|l| {
            l.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .filter(|(k, _)| !["PWD", "SHLVL", "_", "OLDPWD"].contains(&k.as_str()))
        .collect();
    let JValue::Obj(want) = node.get("spawned").get("env") else {
        panic!()
    };
    let want: BTreeMap<String, String> = want
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    assert_eq!(env, want);
    // And the child it left behind is stopped with its group, as Node's `-pid` signal does.
    let sleeper: u32 = fs::read_to_string(&pids).unwrap().trim().parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stat = fs::read_to_string(format!("/proc/{sleeper}/stat")).unwrap_or_default();
        if stat.is_empty() || stat.contains(") Z ") {
            break;
        }
        assert!(Instant::now() < deadline, "the agent's child outlived it");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_cancel_is_not_held_up_by_an_agent_that_stops_reading() {
    let port = sandbox(|_r, _w| {
        // Never reads again; keeps the socket open.
        std::thread::sleep(Duration::from_secs(20));
    });
    let cancel = Cancel::new();
    let conn = connect(
        Options::new(Transport::Socket(format!("127.0.0.1:{port}")), "/qa"),
        Arc::new(Quiet),
        cancel.clone(),
    )
    .unwrap();
    let big = "x".repeat(64 * 1024 * 1024);
    let prompting = std::thread::spawn(move || conn.prompt(&big));
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    cancel.cancel();
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "cancel took {:?}",
        t.elapsed()
    );
    let e = prompting.join().unwrap().unwrap_err();
    assert_eq!(e.message, "Cancelled");
}

#[test]
fn an_agent_cannot_make_noevia_start_threads_without_limit() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Holds(Arc<AtomicBool>);
    impl Handlers for Holds {
        fn request_permission(&self, _: &JValue) -> Result<JValue, Refusal> {
            // Every permission waits until the sandbox has counted the refusals.
            let deadline = Instant::now() + Duration::from_secs(20);
            while !self.0.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(JValue::Null)
        }
        fn read_text_file(&self, _: &JValue) -> Result<JValue, Refusal> {
            Ok(JValue::Null)
        }
        fn write_text_file(&self, _: &JValue) -> Result<JValue, Refusal> {
            Ok(JValue::Null)
        }
        fn session_update(&self, _: &JValue) {}
    }
    let release = Arc::new(AtomicBool::new(false));
    let (seen_tx, seen_rx) = mpsc::channel::<usize>();
    let go = release.clone();
    let port = sandbox(move |mut r, mut w| {
        r.get_ref()
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut l = String::new();
        r.read_line(&mut l).unwrap(); // the prompt
        for i in 0..1000 {
            writeln!(w, r#"{{"jsonrpc":"2.0","id":{i},"method":"session/request_permission","params":{{}}}}"#).unwrap();
        }
        let mut refused = 0;
        while refused < 1000 - code_acp::MAX_IN_FLIGHT {
            l.clear();
            if r.read_line(&mut l).unwrap_or(0) == 0 {
                break;
            }
            if l.contains("Too many requests at once.") {
                refused += 1;
            }
        }
        seen_tx.send(refused).unwrap();
        go.store(true, Ordering::SeqCst);
        writeln!(w, r#"{{"jsonrpc":"2.0","id":3,"result":{{}}}}"#).unwrap();
    });
    let conn = connect(
        Options::new(Transport::Socket(format!("127.0.0.1:{port}")), "/qa"),
        Arc::new(Holds(release)),
        Cancel::new(),
    )
    .unwrap();
    let prompting = std::thread::spawn(move || conn.prompt("QA"));
    let refused = seen_rx.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(refused, 1000 - code_acp::MAX_IN_FLIGHT);
    prompting.join().unwrap().unwrap();
}

#[test]
fn connecting_is_bounded_by_the_handshake_and_a_cancel() {
    // An address nothing answers on: whatever the network does with it, connect gives up in time.
    let mut options = Options::new(Transport::Socket("10.255.255.1:9".into()), "/qa");
    options.handshake = Duration::from_millis(300);
    let t = Instant::now();
    assert!(connect(options, Arc::new(Quiet), Cancel::new()).is_err());
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    let options = Options::new(Transport::Socket("10.255.255.1:9".into()), "/qa");
    let cancel = Cancel::new();
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        c2.cancel();
    });
    let t = Instant::now();
    let e = connect(options, Arc::new(Quiet), cancel).err().unwrap();
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert!(
        e.message == "Cancelled" || e.message.starts_with("The coding sandbox is unreachable"),
        "{e:?}"
    );
}
