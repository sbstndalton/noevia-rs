#![allow(clippy::unwrap_used, clippy::expect_used)]

use code_sandbox_supervisor::{serve, Config};
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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
        Self::with_session_helper(
            script,
            cap,
            wall,
            grace,
            rust_bridge,
            env!("CARGO_BIN_EXE_code-sandbox-session-exec").into(),
        )
        .await
    }
    async fn with_session_helper(
        script: &str,
        cap: usize,
        wall: Duration,
        grace: Duration,
        rust_bridge: bool,
        session_helper: PathBuf,
    ) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let work = root.join("task");
        std::fs::create_dir_all(&work).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = Config {
            command: "/bin/sh".into(),
            session_helper,
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
// Only the synthetic escaped child carries this unique pid-file path in its
// command line. Kill that test-owned PID even if an assertion unwinds.
struct EscapedChildCleanup {
    pid_file: PathBuf,
}
impl Drop for EscapedChildCleanup {
    fn drop(&mut self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let marker = self.pid_file.to_string_lossy();
        loop {
            if let Ok(raw) = std::fs::read_to_string(&self.pid_file) {
                if let Ok(pid) = raw.trim().parse::<i32>() {
                    if let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) {
                        if cmdline
                            .windows(marker.len())
                            .any(|part| part == marker.as_bytes())
                        {
                            let _ = nix::sys::signal::kill(
                                nix::unistd::Pid::from_raw(pid),
                                nix::sys::signal::Signal::SIGKILL,
                            );
                        }
                    }
                }
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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
async fn disconnect_before_session_ready_still_tears_down_descendants() {
    let files = tempfile::tempdir().unwrap();
    let wrapper = files.path().join("delayed-helper");
    let entered = files.path().join("helper-entered");
    let marker = files.path().join("descendant-survived");
    let real_helper = env!("CARGO_BIN_EXE_code-sandbox-session-exec");
    std::fs::write(
        &wrapper,
        format!(
            "#!/usr/bin/python3\nimport os, pathlib, sys, time\npathlib.Path({:?}).touch()\ntime.sleep(0.25)\nos.execv({:?}, [{:?}, *sys.argv[1:]])\n",
            entered.display().to_string(),
            real_helper,
            real_helper
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let script = format!("(sleep 0.6; touch '{}') & cat", marker.display());
    let h = Harness::with_session_helper(
        &script,
        1,
        Duration::from_secs(3),
        Duration::from_millis(30),
        false,
        wrapper,
    )
    .await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !entered.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("delayed helper did not enter");
    drop(socket); // During the wrapper delay, the child is not yet a group leader.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(!marker.exists(), "descendant survived early disconnect");
    h.close().await;
}

#[tokio::test]
async fn helper_readiness_timeout_reaps_unready_pid_without_group_signal() {
    let files = tempfile::tempdir().unwrap();
    let wrapper = files.path().join("unready-helper");
    let pid_file = files.path().join("unready.pid");
    std::fs::write(
        &wrapper,
        format!(
            "#!/usr/bin/python3\nimport os, pathlib, time\npathlib.Path({:?}).write_text(str(os.getpid()))\ntime.sleep(5)\n",
            pid_file.display().to_string()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let h = Harness::with_session_helper(
        "printf NEVER_STARTED",
        1,
        Duration::from_secs(5),
        Duration::from_millis(30),
        false,
        wrapper,
    )
    .await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    let response = read_all(socket).await;
    assert!(response.contains("could not start"), "{response}");
    assert!(!response.contains("NEVER_STARTED"));
    let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "unready helper was not reaped"
    );
    h.close().await;
}

#[tokio::test]
async fn harness_exec_is_session_leader_and_cannot_rejoin_supervisor_group() {
    let files = tempfile::tempdir().unwrap();
    let script_path = files.path().join("session-probe.py");
    let pid_file = files.path().join("probe.pid");
    let marker = files.path().join("descendant-survived");
    std::fs::write(
        &script_path,
        r#"import errno, os, pathlib, sys, time
pid_file, marker, supervisor_group = sys.argv[1], sys.argv[2], int(sys.argv[3])
pid, group, session, parent = os.getpid(), os.getpgrp(), os.getsid(0), os.getppid()
pathlib.Path(pid_file).write_text(str(pid))
try:
    os.setpgid(0, supervisor_group)
    escape = 0
except OSError as error:
    escape = error.errno
if escape != errno.EPERM:
    print(f'{pid}|{group}|{session}|{parent}|{escape}', flush=True)
    time.sleep(5)
    sys.exit(0)
child = os.fork()
if child == 0:
    time.sleep(0.6)
    pathlib.Path(marker).touch()
    os._exit(0)
print(f'{pid}|{group}|{session}|{parent}|{escape}', flush=True)
time.sleep(5)
"#,
    )
    .unwrap();
    let cleanup = EscapedChildCleanup {
        pid_file: pid_file.clone(),
    };
    let script = format!(
        "exec /usr/bin/python3 {} {} {} {}",
        script_path.display(),
        pid_file.display(),
        marker.display(),
        nix::unistd::getpgrp().as_raw()
    );
    let h = Harness::new(&script, 1, Duration::from_secs(2)).await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    let mut reader = BufReader::new(socket);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let fields: Vec<i32> = line
        .trim()
        .split('|')
        .map(|part| part.parse().unwrap())
        .collect();
    let [pid, group, session, parent, escape] =
        <[i32; 5]>::try_from(fields).expect("unexpected session probe");
    assert_eq!(pid, group, "harness must lead its process group");
    assert_eq!(pid, session, "harness must lead a distinct session");
    assert_eq!(
        parent,
        std::process::id() as i32,
        "session helper must exec in place"
    );
    assert_eq!(
        escape,
        nix::errno::Errno::EPERM as i32,
        "harness rejoined supervisor group"
    );
    drop(reader);
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert!(
        !marker.exists(),
        "ordinary descendant survived group teardown"
    );
    h.close().await;
    drop(cleanup);
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

#[tokio::test]
async fn escaped_group_writer_cannot_extend_post_exit_drain_forever() {
    let marker = tempfile::tempdir().unwrap();
    let pid_file = marker.path().join("escaped-writer.pid");
    let cleanup = EscapedChildCleanup {
        pid_file: pid_file.clone(),
    };
    let script = format!(
        r#"setsid /bin/sh -c 'echo $$ > "{}"; exec yes "{}"' & exit 0"#,
        pid_file.display(),
        pid_file.display()
    );
    let h = Harness::with_options(
        &script,
        1,
        Duration::from_millis(700),
        Duration::from_millis(50),
        false,
    )
    .await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();

    let writer_pid = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(raw) = std::fs::read_to_string(&pid_file) {
                break raw.trim().parse::<i32>().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("escaped writer did not start");
    let pid = nix::unistd::Pid::from_raw(writer_pid);
    assert_eq!(nix::unistd::getsid(Some(pid)).unwrap(), pid);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut chunk = [0_u8; 16 * 1024];
    let mut bytes = 0_usize;
    loop {
        match tokio::time::timeout_at(deadline, socket.read(&mut chunk))
            .await
            .expect("continuous escaped writer held the connection indefinitely")
        {
            Ok(count) => {
                assert_ne!(count, 0, "truncated stream ended with clean EOF");
                bytes += count;
            }
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
    assert!(bytes > 0, "escaped writer produced no output");
    h.close().await;
    drop(cleanup);
}

#[tokio::test]
async fn quiet_escaped_stdout_pipe_resets_before_grace() {
    let marker = tempfile::tempdir().unwrap();
    let pid_file = marker.path().join("quiet-escaped.pid");
    let cleanup = EscapedChildCleanup {
        pid_file: pid_file.clone(),
    };
    let script = format!(
        r#"setsid /bin/sh -c 'echo $$ > "{}"; exec tail -f "{}"' & exit 0"#,
        pid_file.display(),
        pid_file.display()
    );
    let h = Harness::with_options(
        &script,
        1,
        Duration::from_secs(3),
        Duration::from_millis(600),
        false,
    )
    .await;
    let mut socket = h.connect().await;
    let started = tokio::time::Instant::now();
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    let mut chunk = [0_u8; 1024];
    loop {
        match tokio::time::timeout(Duration::from_secs(1), socket.read(&mut chunk))
            .await
            .expect("quiet escaped pipe did not close promptly")
        {
            Ok(count) => assert_ne!(count, 0, "open stdout pipe ended with clean EOF"),
            Err(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
                break;
            }
        }
    }
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "quiet escaped pipe waited for group grace"
    );
    h.close().await;
    drop(cleanup);
}

fn quiet_config(root: PathBuf) -> Config {
    Config {
        command: "/bin/sh".into(),
        session_helper: env!("CARGO_BIN_EXE_code-sandbox-session-exec").into(),
        args: Vec::new(),
        root,
        max_connections: 1,
        max_wall: Duration::from_secs(5),
        grace: Duration::ZERO,
        fallback_home: None,
        rust_bridge: false,
    }
}

#[tokio::test]
async fn already_stopped_server_does_not_start_accepting() {
    let root = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (_stop, rx) = watch::channel(true);
    tokio::time::timeout(
        Duration::from_secs(1),
        serve(listener, quiet_config(root.path().into()), rx),
    )
    .await
    .expect("already-stopped server ignored shutdown")
    .unwrap();
}

#[tokio::test]
async fn direct_invalid_deadlines_fail_before_accepting() {
    let root = tempfile::tempdir().unwrap();
    for (wall, grace) in [
        (Duration::ZERO, Duration::ZERO),
        (Duration::MAX, Duration::ZERO),
        (Duration::from_secs(1), Duration::MAX),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (_stop, rx) = watch::channel(false);
        let mut config = quiet_config(root.path().into());
        config.max_wall = wall;
        config.grace = grace;
        let error = tokio::time::timeout(Duration::from_secs(1), serve(listener, config, rx))
            .await
            .expect("invalid direct configuration entered accept loop")
            .expect_err("invalid deadline accepted");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}

#[tokio::test]
async fn aborting_server_closes_io_held_by_an_escaped_writer() {
    let files = tempfile::tempdir().unwrap();
    let pid_file = files.path().join("cancel-escaped.pid");
    let _cleanup = EscapedChildCleanup {
        pid_file: pid_file.clone(),
    };
    let script_path = files.path().join("cancel-escaped.py");
    std::fs::write(
        &script_path,
        r#"import os, pathlib, sys, time
if os.fork() == 0:
    os.setsid()
    pathlib.Path(sys.argv[1]).write_text(str(os.getpid()))
    time.sleep(5)
    os._exit(0)
print('ready', flush=True)
time.sleep(5)
"#,
    )
    .unwrap();
    let script = format!(
        "exec /usr/bin/python3 '{}' '{}'",
        script_path.display(),
        pid_file.display()
    );
    let mut h = Harness::new(&script, 1, Duration::from_secs(10)).await;
    let mut socket = h.connect().await;
    socket.write_all(h.start("{}").as_bytes()).await.unwrap();
    let mut ready = [0u8; 6];
    tokio::time::timeout(Duration::from_secs(2), socket.read_exact(&mut ready))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&ready, b"ready\n");
    let pid = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    break pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("escaped writer did not publish its test-owned PID");
    let pid = nix::unistd::Pid::from_raw(pid);
    assert_eq!(nix::unistd::getsid(Some(pid)).unwrap(), pid);
    h.server.abort();
    assert!((&mut h.server).await.unwrap_err().is_cancelled());
    let mut rest = Vec::new();
    let closed =
        match tokio::time::timeout(Duration::from_secs(1), socket.read_to_end(&mut rest)).await {
            Ok(Ok(_)) => true,
            Ok(Err(error)) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ),
            Err(_) => false,
        };
    assert!(
        closed,
        "cancelled connection retained its socket/escaped stdout"
    );
}
