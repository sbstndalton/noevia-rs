#![allow(clippy::unwrap_used, clippy::expect_used)]

use code_sandbox_supervisor::{serve, Config};
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

struct Harness {
    _tmp: TempDir,
    work: PathBuf,
    port: u16,
    stop: watch::Sender<bool>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}
impl Harness {
    async fn new(script: &str, cap: usize, wall: Duration) -> Self {
        Self::with_options(script, cap, wall, Duration::from_millis(30), false).await
    }
    async fn with_options(
        script: &str,
        cap: usize,
        wall: Duration,
        grace: Duration,
        rust_bridge: bool,
    ) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let work = root.join("task");
        std::fs::create_dir_all(&work).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Config {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            root,
            max_connections: cap,
            max_wall: wall,
            grace,
            fallback_home: Some("/sandbox-home".into()),
            rust_bridge,
        };
        let (stop, rx) = watch::channel(false);
        let server = tokio::spawn(serve(listener, config, rx));
        Self {
            _tmp: tmp,
            work,
            port,
            stop,
            server,
        }
    }
    async fn connect(&self) -> TcpStream {
        TcpStream::connect(("127.0.0.1", self.port)).await.unwrap()
    }
    fn start(&self, env: &str) -> String {
        format!(
            r#"{{"noevia":"start","cwd":"{}","env":{env}}}"#,
            self.work.display()
        ) + "\n"
    }
    async fn close(self) {
        self.stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(3), self.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
async fn read_all(mut socket: TcpStream) -> String {
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn forwards_start_remainder_and_later_acp_bytes() {
    let h = Harness::new("cat", 2, Duration::from_secs(2)).await;
    let mut socket = h.connect().await;
    socket
        .write_all((h.start("{}") + "first\n").as_bytes())
        .await
        .unwrap();
    let mut first = [0_u8; 6];
    socket.read_exact(&mut first).await.unwrap();
    assert_eq!(&first, b"first\n");
    socket.write_all(b"second\n").await.unwrap();
    let mut second = [0_u8; 7];
    socket.read_exact(&mut second).await.unwrap();
    assert_eq!(&second, b"second\n");
    drop(socket);
    h.close().await;
}

#[tokio::test]
async fn rejects_bad_start_escape_and_oversize() {
    let h = Harness::new("printf AGENT_STARTED", 2, Duration::from_secs(2)).await;
    for payload in [
        "not json\n".to_owned(),
        r#"{"method":"initialize"}"#.to_owned() + "\n",
        r#"{"noevia":"start","cwd":"/etc"}"#.to_owned() + "\n",
        "x".repeat(70 * 1024),
    ] {
        let mut socket = h.connect().await;
        socket.write_all(payload.as_bytes()).await.unwrap();
        let response = read_all(socket).await;
        assert!(response.contains("\"code\":-32000"), "{response}");
        assert!(!response.contains("AGENT_STARTED"));
    }
    h.close().await;
}

#[tokio::test]
async fn filters_environment_and_falls_back_home() {
    let h = Harness::new(
        "printf '%s|%s|%s' \"$HOME\" \"$PATH\" \"$(printenv LD_PRELOAD || printf unset)\"",
        1,
        Duration::from_secs(2),
    )
    .await;
    let mut socket = h.connect().await;
    socket
        .write_all(
            h.start(r#"{"PATH":"/bin","LD_PRELOAD":"/evil","HOME":5}"#)
                .as_bytes(),
        )
        .await
        .unwrap();
    assert_eq!(read_all(socket).await, "/sandbox-home|/bin|unset");
    h.close().await;
}

#[tokio::test]
async fn cap_releases_after_disconnect() {
    let h = Harness::new("cat", 1, Duration::from_secs(2)).await;
    let mut first = h.connect().await;
    first.write_all(h.start("{}").as_bytes()).await.unwrap();
    first.write_all(b"ready\n").await.unwrap();
    let mut echoed = [0_u8; 6];
    first.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"ready\n");
    let response = read_all(h.connect().await).await;
    assert!(response.contains("already running 1 agents"));
    drop(first);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut third = h.connect().await;
    third.write_all(h.start("{}").as_bytes()).await.unwrap();
    third.write_all(b"again\n").await.unwrap();
    third.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"again\n");
    drop(third);
    h.close().await;
}

#[tokio::test]
async fn timeout_stops_descendant_and_returns_error() {
    let marker = tempfile::tempdir().unwrap();
    let path = marker.path().join("survived");
    let script = format!("(sleep 0.5; touch '{}') & wait", path.display());
    let h = Harness::new(&script, 1, Duration::from_millis(40)).await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    let response = read_all(socket).await;
    assert!(response.contains("time limit"), "{response}");
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(!path.exists(), "descendant survived group stop");
    h.close().await;
}

#[tokio::test]
async fn shutdown_stops_active_connection() {
    let h = Harness::new("cat", 1, Duration::from_secs(5)).await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    socket.write_all(b"ready\n").await.unwrap();
    let mut echoed = [0_u8; 6];
    socket.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"ready\n");
    h.close().await;
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), socket.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn leader_exit_terminates_its_background_process_group() {
    let marker = tempfile::tempdir().unwrap();
    let path = marker.path().join("survived");
    let script = format!("(sleep 0.5; touch '{}') & exit 0", path.display());
    let h = Harness::new(&script, 1, Duration::from_secs(2)).await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    assert_eq!(read_all(socket).await, "");
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(!path.exists(), "descendant survived leader exit");
    h.close().await;
}

#[tokio::test]
async fn disconnect_terminates_its_background_process_group() {
    let marker = tempfile::tempdir().unwrap();
    let path = marker.path().join("survived");
    let script = format!("(sleep 0.5; touch '{}') & cat", path.display());
    let h = Harness::new(&script, 1, Duration::from_secs(2)).await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    socket.write_all(b"ready\n").await.unwrap();
    let mut echoed = [0_u8; 6];
    socket.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"ready\n");
    drop(socket);
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(!path.exists(), "descendant survived disconnect");
    h.close().await;
}

#[tokio::test]
async fn empty_home_uses_supervisor_fallback() {
    let h = Harness::new("printf %s \"$HOME\"", 1, Duration::from_secs(2)).await;
    let mut socket = h.connect().await;
    socket
        .write_all(h.start(r#"{"HOME":""}"#).as_bytes())
        .await
        .unwrap();
    assert_eq!(read_all(socket).await, "/sandbox-home");
    h.close().await;
}

#[tokio::test]
async fn normal_exit_closes_socket_before_group_kill_grace() {
    let marker = tempfile::tempdir().unwrap();
    let path = marker.path().join("survived");
    let script = format!("(sleep 0.8; touch '{}') & printf done", path.display());
    let h = Harness::with_options(
        &script,
        1,
        Duration::from_secs(3),
        Duration::from_millis(600),
        false,
    )
    .await;
    let mut socket = h.connect().await;
    let start = tokio::time::Instant::now();
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    assert_eq!(read_all(socket).await, "done");
    assert!(
        start.elapsed() < Duration::from_millis(350),
        "EOF waited for group grace"
    );
    tokio::time::sleep(Duration::from_millis(850)).await;
    assert!(!path.exists(), "descendant survived grace cleanup");
    h.close().await;
}

#[tokio::test]
async fn closed_child_stdin_does_not_cut_off_later_stdout() {
    let h = Harness::new(
        "exec 0<&-; printf ready; sleep 0.1; printf done",
        1,
        Duration::from_secs(2),
    )
    .await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    let mut first = [0_u8; 5];
    socket.read_exact(&mut first).await.unwrap();
    assert_eq!(&first, b"ready");
    socket.write_all(b"ignored input\n").await.unwrap();
    assert_eq!(read_all(socket).await, "done");
    h.close().await;
}

#[tokio::test]
async fn rust_bridge_mode_reaches_downstream_agent() {
    let h = Harness::with_options(
        "printf %s \"$SANDBOX_BRIDGE_IMPL\"",
        1,
        Duration::from_secs(2),
        Duration::from_millis(30),
        true,
    )
    .await;
    let mut socket = h.connect().await;
    socket
        .write_all(h.start(r#"{"SANDBOX_BRIDGE_IMPL":"fake"}"#).as_bytes())
        .await
        .unwrap();
    assert_eq!(read_all(socket).await, "rust");
    h.close().await;
}

#[tokio::test]
async fn many_short_connections_leave_listener_responsive() {
    let h = Harness::new("printf ok", 4, Duration::from_secs(2)).await;
    for _ in 0..120 {
        let mut socket = h.connect().await;
        socket.write_all(h.start("{}").as_bytes()).await.unwrap();
        assert_eq!(read_all(socket).await, "ok");
    }
    h.close().await;
}

#[tokio::test]
async fn large_final_output_survives_slow_reader_after_leader_exit() {
    const BYTES: usize = 8 * 1024 * 1024;
    let h = Harness::with_options(
        "/usr/bin/head -c 8388608 /dev/zero",
        1,
        Duration::from_secs(10),
        Duration::from_millis(600),
        false,
    )
    .await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut total = 0_usize;
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let count = tokio::time::timeout_at(deadline, socket.read(&mut chunk))
            .await
            .unwrap()
            .unwrap();
        if count == 0 {
            break;
        }
        assert!(chunk.iter().take(count).all(|byte| *byte == 0));
        total += count;
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(total, BYTES, "final ACP bytes were truncated");
    h.close().await;
}

#[tokio::test]
async fn stalled_final_output_resets_instead_of_clean_eof() {
    // The leader exits immediately; an uncooperative descendant keeps the
    // pipe busy. A client that never reads must get a reset on the bounded
    // post-exit write stall, not a successful EOF on a partial ACP stream.
    let h = Harness::with_options(
        "(trap '' TERM; head -c 67108864 /dev/zero) & exit 0",
        1,
        Duration::from_secs(10),
        Duration::from_millis(600),
        false,
    )
    .await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        match tokio::time::timeout_at(deadline, socket.read(&mut chunk))
            .await
            .expect("stalled connection did not close")
        {
            Ok(count) => assert_ne!(count, 0, "partial ACP output ended with clean EOF"),
            Err(error) => {
                assert!(
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ),
                    "unexpected read error: {error}"
                );
                break;
            }
        }
    }
    h.close().await;
}
