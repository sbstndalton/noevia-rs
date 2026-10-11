#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
#[path = "support/oracle.rs"]
mod oracle;
use base64::{engine::general_purpose::STANDARD, Engine};
use laya_service::{Config, Failure, Pool};
use serde_json::{json, Value};
use std::time::Duration;
fn config(directory: &std::path::Path, size: usize) -> Config {
    let mut config = Config::production();
    config.python = "python3".into();
    config.script = directory.join("server.py");
    config.size = size;
    config.startup = Duration::from_secs(3);
    config.decision = Duration::from_millis(100);
    config.queue = Duration::from_millis(50);
    config.late_grace = Duration::from_millis(500);
    config.retries = [
        Duration::ZERO,
        Duration::from_millis(20),
        Duration::from_millis(40),
    ];
    config
}
#[test]
fn actual_python_environment_parser_parity() {
    let directory = prepare();
    // Fixed actual-source function loader. Cases and expected/native evaluation
    // are Rust-owned; no model imports or duplicated Python configuration logic.
    let loader="import json,sys;sys.path.insert(0,sys.argv[1]);import server;e=json.loads(sys.argv[2]);print(json.dumps([server.max_concurrency_from_env(e,lambda _:None),server.decision_timeout_from_env(e,lambda _:None),server.queue_timeout_from_env(e,lambda _:None)]))";
    for value in [
        "",
        " ",
        "-0",
        "+2",
        "2",
        "2.0",
        "1.3",
        "1.0_0",
        "١",
        "١.٣",
        "２",
        "２_０",
        "0.49",
        "2.01",
        "10000",
        "10001",
        "inf",
        "nan",
        "-1",
        "1__0",
        "x",
        "\u{1c}2\u{1f}",
    ] {
        let env: std::collections::HashMap<String, String> = [
            "LAYA_MAX_CONCURRENCY",
            "LAYA_DECISION_TIMEOUT_S",
            "LAYA_QUEUE_TIMEOUT_MS",
        ]
        .into_iter()
        .map(|key| (key.into(), value.into()))
        .collect();
        let output = std::process::Command::new("python3")
            .args(["-c", loader])
            .arg(directory.path())
            .arg(serde_json::to_string(&env).unwrap())
            .output()
            .unwrap();
        assert!(output.status.success());
        let expected: Value = serde_json::from_slice(&output.stdout).unwrap();
        let native = Config::from_env(&env);
        assert_eq!(
            json!([
                native.size,
                native.decision.as_secs_f64(),
                native.queue.as_secs_f64()
            ]),
            expected,
            "environment numeric parity: {value:?}"
        );
    }
}
fn prepare() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    oracle::prepare(&oracle::reference(), dir.path()).unwrap();
    dir
}
async fn wait_file(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
async fn http(port: u16, method: &str, state: Option<&str>) -> Value {
    let method = method.to_owned();
    let body = state.map(oracle::body).unwrap_or_default();
    let length = state.map(|_| body.len().to_string());
    tokio::task::spawn_blocking(move || {
        oracle::exchange(
            port,
            &method,
            if method == "GET" {
                "/health"
            } else {
                "/v1/decisions"
            },
            length.as_deref(),
            &body,
        )
    })
    .await
    .unwrap()
    .unwrap()
}
#[tokio::test]
async fn real_http_health_busy_timeout_recovery_and_disconnect() {
    use tokio::io::AsyncWriteExt;
    let dir = prepare();
    let pool = Pool::start(config(dir.path(), 1)).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, done) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(laya_service::serve_with_shutdown(
        listener,
        pool.clone(),
        async {
            let _ = done.await;
        },
    ));
    let busy = tokio::spawn(http(port, "POST", Some("__sleep:0.3")));
    wait_file(&dir.path().join("events")).await;
    assert_eq!(http(port, "GET", None).await["status"], 200);
    let over = http(port, "POST", Some("ok")).await;
    assert_eq!(over["status"], 503);
    assert_eq!(over["retry"], "1");
    assert_eq!(over["body"]["error"], "Decision service busy");
    assert_eq!(busy.await.unwrap()["status"], 503);
    tokio::time::sleep(Duration::from_millis(220)).await;
    assert_eq!(
        http(port, "POST", Some("ok")).await["body"]["selected"],
        "continue"
    );
    std::fs::write(dir.path().join("delay-startup"), b"0.3").unwrap();
    assert_eq!(http(port, "POST", Some("__crash")).await["status"], 503);
    assert_eq!(
        http(port, "GET", None).await,
        json!({"status":503,"type":"application/json","retry":null,"body":{"ready":false}})
    );
    assert_eq!(http(port, "POST", Some("ok")).await["status"], 503);
    wait_ready(&pool).await;
    std::fs::remove_file(dir.path().join("delay-startup")).unwrap();
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let body = oracle::body("__sleep:0.3");
    socket
        .write_all(
            format!(
                "POST /v1/decisions HTTP/1.0\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    socket.write_all(&body).await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    drop(socket);
    assert_eq!(http(port, "POST", Some("ok")).await["status"], 503);
    tokio::time::sleep(Duration::from_millis(220)).await;
    assert_eq!(
        http(port, "POST", Some("ok")).await["body"]["selected"],
        "continue"
    );
    stop.send(()).unwrap();
    serving.await.unwrap().unwrap();
}
#[tokio::test]
async fn startup_deadline_and_stop_during_replacement() {
    let dir = prepare();
    std::fs::write(dir.path().join("delay-startup"), b"0.3").unwrap();
    let mut short = config(dir.path(), 1);
    short.startup = Duration::from_millis(30);
    assert!(Pool::start(short).await.is_err());
    std::fs::remove_file(dir.path().join("delay-startup")).unwrap();
    let pool = Pool::start(config(dir.path(), 1)).await.unwrap();
    std::fs::write(dir.path().join("delay-startup"), b"2").unwrap();
    assert_eq!(
        pool.decide(oracle::body("__crash")).await,
        Err(Failure::Unavailable)
    );
    pool.stop().await;
    assert!(!pool.ready());
    assert!(!pool.failed());
}
async fn wait_ready(pool: &Pool) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while !pool.ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn actual_python_goldens_and_live_oracle_through_native_http() {
    let directory = prepare();
    let python = oracle::Server::start(directory.path()).unwrap();
    let pool = Pool::start(config(directory.path(), 1)).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, done) = tokio::sync::oneshot::channel();
    let job = tokio::spawn(laya_service::serve_with_shutdown(
        listener,
        pool.clone(),
        async {
            let _ = done.await;
        },
    ));
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/python-http.v1.json")).unwrap();
    assert_eq!(fixture["services"], oracle::REF);
    assert_eq!(fixture["server_sha256"], oracle::SERVER_HASH);
    let mut cases = fixture["cases"].as_array().unwrap().clone();
    assert_eq!(cases.len(), oracle::cases().len());
    // Production3.12 default unsupported-method HTML is deliberately qualified
    // on Linux CI; local3.14 has a changed template, not a fabricated parity pass.
    if python.production {
        for method in ["PUT", "HEAD", "PATCH"] {
            cases.push(json!({"method":method,"path":"/health","length":null,"data":""}));
        }
    }
    for row in cases {
        let data = STANDARD.decode(row["data"].as_str().unwrap()).unwrap();
        let method = row["method"].as_str().unwrap().to_owned();
        let path = row["path"].as_str().unwrap().to_owned();
        let length = row["length"].as_str().map(str::to_owned);
        let expected =
            oracle::exchange(python.port, &method, &path, length.as_deref(), &data).unwrap();
        if row.get("expected").is_some() {
            assert_eq!(
                expected, row["expected"],
                "actual source oracle drift: {row}"
            );
        }
        let result = tokio::task::spawn_blocking(move || {
            oracle::exchange(port, &method, &path, length.as_deref(), &data)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result, expected, "native HTTP parity mismatch: {row}");
    }
    assert!(
        !laya_service::validate(b"{\"state\":\"x\",\"question\":\"?\",\"options\":[]}"),
        "negative control accepted invalid request"
    );
    stop.send(()).unwrap();
    job.await.unwrap().unwrap();
    assert!(!pool.ready());
}
#[tokio::test]
async fn deadlines_stale_reply_discard_no_queue_behind_abandoned_work() {
    let dir = prepare();
    let pool = Pool::start(config(dir.path(), 1)).await.unwrap();
    let starts = std::fs::read_to_string(dir.path().join("starts")).unwrap();
    assert_eq!(
        pool.decide(oracle::body("__sleep:0.3")).await,
        Err(Failure::Timeout)
    );
    assert!(pool.ready());
    assert_eq!(pool.decide(oracle::body("ok")).await, Err(Failure::Timeout));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("events"))
            .unwrap()
            .lines()
            .count(),
        1,
        "request was sent behind unfinished work"
    );
    tokio::time::sleep(Duration::from_millis(180)).await;
    assert_eq!(
        pool.decide(oracle::body("ok")).await.unwrap()["selected"],
        "continue",
        "stale verify answer leaked"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("starts")).unwrap(),
        starts,
        "late worker unnecessarily replaced"
    );
    pool.stop().await;
}
#[tokio::test]
async fn busy_admission_and_independent_workers() {
    let dir = prepare();
    let pool = Pool::start(config(dir.path(), 2)).await.unwrap();
    let p = pool.clone();
    let first = tokio::spawn(async move { p.decide(oracle::body("__sleep:0.08")).await });
    let p = pool.clone();
    let second = tokio::spawn(async move { p.decide(oracle::body("__sleep:0.08")).await });
    tokio::time::sleep(Duration::from_millis(15)).await;
    assert!(pool.ready());
    assert_eq!(pool.decide(oracle::body("ok")).await, Err(Failure::Busy));
    assert!(first.await.unwrap().is_ok());
    assert!(second.await.unwrap().is_ok());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("starts"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    pool.stop().await;
}
#[tokio::test]
async fn cancellation_releases_slot_but_discards_unfinished_engine_work() {
    let dir = prepare();
    let pool = Pool::start(config(dir.path(), 1)).await.unwrap();
    let p = pool.clone();
    let request = tokio::spawn(async move { p.decide(oracle::body("__sleep:0.3")).await });
    tokio::time::sleep(Duration::from_millis(25)).await;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert_eq!(pool.decide(oracle::body("ok")).await, Err(Failure::Timeout));
    tokio::time::sleep(Duration::from_millis(220)).await;
    assert_eq!(
        pool.decide(oracle::body("ok")).await.unwrap()["selected"],
        "continue"
    );
    pool.stop().await;
}
#[tokio::test]
async fn cancelled_waiter_is_not_sent_after_stale_work_finishes() {
    let dir = prepare();
    let pool = Pool::start(config(dir.path(), 1)).await.unwrap();
    assert_eq!(
        pool.decide(oracle::body("__sleep:0.15")).await,
        Err(Failure::Timeout)
    );
    let p = pool.clone();
    let waiting = tokio::spawn(async move { p.decide(oracle::body("cancelled-projection")).await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    waiting.abort();
    let _ = waiting.await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("events"))
            .unwrap()
            .lines()
            .count(),
        1,
        "cancelled projection sent after discarded stale answer"
    );
    assert_eq!(
        pool.decide(oracle::body("ok")).await.unwrap()["selected"],
        "continue"
    );
    pool.stop().await;
}
#[tokio::test]
async fn crashed_or_stuck_workers_replaced_and_exhaustion_is_fatal() {
    let dir = prepare();
    let pool = Pool::start(config(dir.path(), 1)).await.unwrap();
    assert_eq!(
        pool.decide(oracle::body("__crash")).await,
        Err(Failure::Unavailable)
    );
    wait_ready(&pool).await;
    assert_eq!(
        pool.decide(oracle::body("ok")).await.unwrap()["selected"],
        "continue"
    );
    assert_eq!(
        pool.decide(oracle::body("__hang")).await,
        Err(Failure::Timeout)
    );
    tokio::time::sleep(Duration::from_millis(650)).await;
    wait_ready(&pool).await;
    assert!(
        std::fs::read_to_string(dir.path().join("starts"))
            .unwrap()
            .lines()
            .count()
            >= 3
    );
    assert_eq!(
        pool.decide(oracle::body("__fail_recovery")).await,
        Err(Failure::Unavailable)
    );
    tokio::time::timeout(Duration::from_secs(4), async {
        while !pool.failed() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!pool.ready());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("starts"))
            .unwrap()
            .lines()
            .count(),
        6
    );
    pool.stop().await;
}
#[tokio::test]
async fn startup_refusal_and_shutdown_cleanup() {
    let dir = prepare();
    std::fs::write(dir.path().join("fail-startup"), b"").unwrap();
    assert!(Pool::start(config(dir.path(), 2)).await.is_err());
    std::fs::remove_file(dir.path().join("fail-startup")).unwrap();
    let pool = Pool::start(config(dir.path(), 1)).await.unwrap();
    assert!(pool.decide(oracle::body("__child")).await.is_ok());
    let pid: i32 = std::fs::read_to_string(dir.path().join("child-pid"))
        .unwrap()
        .parse()
        .unwrap();
    pool.stop().await;
    let starts = std::fs::read_to_string(dir.path().join("starts")).unwrap();
    for leader in starts.lines() {
        let pid: i32 = leader.parse().unwrap();
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err(),
            "owned engine process survived"
        );
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string(format!("/proc/{pid}/stat"));
        assert!(
            status.is_err() || status.unwrap().split_whitespace().nth(2) == Some("Z"),
            "owned grandchild survived"
        );
    }
    #[cfg(not(target_os = "linux"))]
    assert!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err(),
        "owned grandchild survived"
    );
}
proptest::proptest! {
    #[test]
    fn unicode_character_caps_are_python_scalars(state in ".{0,100}", question in ".{1,100}") {
        let value=json!({"state":state,"question":question,"options":[{"id":"a","label":"A"},{"id":"b","label":"B"}]});let bytes=serde_json::to_vec(&value).unwrap();proptest::prop_assert!(laya_service::validate(&bytes));
    }
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn idle_leader_exit_with_descendant_retaining_framed_pipe_is_recovered() {
    let dir = prepare();
    let pool = Pool::start(config(dir.path(), 1)).await.unwrap();
    assert!(pool.decide(oracle::body("__pipe_child")).await.is_ok());
    let leader: i32 = std::fs::read_to_string(dir.path().join("starts"))
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let descendant: i32 = std::fs::read_to_string(dir.path().join("child-pid"))
        .unwrap()
        .parse()
        .unwrap();
    // Independently prove descendant holds the same framed output pipe.
    let leader_pipes: Vec<_> = std::fs::read_dir(format!("/proc/{leader}/fd"))
        .unwrap()
        .filter_map(|fd| {
            let fd = fd.ok()?;
            let info = std::fs::read_to_string(format!(
                "/proc/{leader}/fdinfo/{}",
                fd.file_name().to_string_lossy()
            ))
            .ok()?;
            let flags = info
                .lines()
                .find_map(|line| line.strip_prefix("flags:\t"))?;
            let flags = u32::from_str_radix(flags, 8).ok()?;
            if flags & 3 != 1 {
                return None;
            }
            std::fs::read_link(fd.path()).ok()
        })
        .collect();
    let child_pipes: Vec<_> = std::fs::read_dir(format!("/proc/{descendant}/fd"))
        .unwrap()
        .filter_map(|fd| std::fs::read_link(fd.ok()?.path()).ok())
        .collect();
    assert!(
        leader_pipes
            .iter()
            .any(|pipe| pipe.to_string_lossy().starts_with("pipe:") && child_pipes.contains(pipe)),
        "descendant did not retain owned pipe"
    );
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(leader),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    tokio::time::timeout(Duration::from_millis(600), async {
        while std::fs::read_to_string(dir.path().join("starts"))
            .unwrap()
            .lines()
            .count()
            < 2
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("idle leader death hidden by retained pipe");
    wait_ready(&pool).await;
    assert_eq!(
        pool.decide(oracle::body("healthy-after-idle-exit"))
            .await
            .unwrap()["selected"],
        "continue"
    );
    let status = std::fs::read_to_string(format!("/proc/{descendant}/stat"));
    assert!(
        status.is_err() || status.unwrap().split_whitespace().nth(2) == Some("Z"),
        "owned pipe-retaining descendant survived replacement"
    );
    assert!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(leader), None).is_err(),
        "leader not reaped"
    );
    pool.stop().await;
}
