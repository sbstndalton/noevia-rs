#![cfg(all(feature = "native-routing", unix))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// Actual shipping binary, private synthetic stores, an empty executable PATH and fake Docker.
// One sequential process contract owns fixed API port8090; no live Docker/model operations.
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const TOKEN: &str = "qa-native-service-token-0123456789abcdef";
struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn binary() -> std::ffi::OsString {
    std::env::var_os("MODEL_MANAGER_PRODUCTION_BIN")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_noevia-model-manager").into())
}
fn signal(child: &Child) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
}

fn wait(child: &mut Child, budget: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "native process did not terminate"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn request(method: &str, path: &str, body: &str, auth: bool) -> String {
    let mut socket = TcpStream::connect("127.0.0.1:8090").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(7)))
        .unwrap();
    let token = if auth {
        format!(
            "x-model-loader-token: {TOKEN}
"
        )
    } else {
        String::new()
    };
    write!(
        socket,
        "{method} {path} HTTP/1.1
Host: qa.invalid
Connection: close
{token}Content-Type: application/json
Content-Length: {}

{body}",
        body.len()
    )
    .unwrap();
    let mut answer = String::new();
    socket.read_to_string(&mut answer).unwrap();
    answer
}
fn start(root: &std::path::Path, socket: &std::path::Path) -> Running {
    start_config(root, socket, None)
}
fn start_config(
    root: &std::path::Path,
    socket: &std::path::Path,
    migration: Option<&str>,
) -> Running {
    let logs = fs::File::create(root.join("stderr.log")).unwrap();
    let mut command = Command::new(binary());
    command
        .env_clear()
        .env("PATH", root.join("no-executables"))
        .env("TZ", "UTC")
        .env("DATA_DIR", root.join("data"))
        .env("MODELS_DIR", root.join("models"))
        .env("MODELS_INI_PATH", root.join("models/models.ini"))
        .env("SEARCH_TRUSTED_PATH", root.join("trusted.json"))
        .env("DOCKER_HOST", format!("unix://{}", socket.display()))
        .env("MODEL_LOADER_TOKEN", TOKEN)
        .env("MODEL_MANAGER_ROUTING_IMPL", "invalid-retired-value")
        .env("MODEL_MANAGER_PROMPTS_IMPL", "python")
        .env("MODEL_MANAGER_DB_INIT_IMPL", "python")
        .stdout(Stdio::null())
        .stderr(logs);
    if let Some(value) = migration {
        command.env("MIGRATE_CACHE_RAM_ON_START", value);
    }
    let mut child = Running(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            panic!(
                "startup exited {status}: {}",
                fs::read_to_string(root.join("stderr.log")).unwrap()
            );
        }
        if TcpStream::connect("127.0.0.1:8090").is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "native startup timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    child
}
fn assert_lock_held(root: &std::path::Path) {
    let lock = fs::File::options()
        .read(true)
        .write(true)
        .open(root.join("data/model_loader.db.prompts.lock"))
        .unwrap();
    assert!(
        lock.try_lock().is_err(),
        "old process released writer lease early"
    );
}
#[test]
fn shipping_process_has_no_child_and_retains_leases_through_graceful_and_deadline_exit() {
    // Fail clearly rather than interacting with any pre-existing API listener.
    let reservation = std::net::TcpListener::bind("127.0.0.1:8090")
        .expect("synthetic native service test requires free port8090");
    drop(reservation);
    let root = tempfile::Builder::new()
        .prefix("noevia-native-service-")
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["data", "models", "no-executables"] {
        fs::create_dir(root.path().join(name)).unwrap();
        fs::set_permissions(root.path().join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::write(
        root.path().join("trusted.json"),
        r#"{"quantisers":["qa"],"labs":["qa"]}"#,
    )
    .unwrap();
    let socket = root.path().join("docker.sock");
    let docker = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    docker.set_nonblocking(true).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let stopped = done.clone();
    let daemon = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !stopped.load(Ordering::Relaxed) && Instant::now() < deadline {
            match docker.accept() {
                Ok((mut peer, _)) => {
                    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    let mut buf = [0u8; 8192];
                    let _ = peer.read(&mut buf);
                    let _ = peer.write_all(
                        b"HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: 2
Connection: close

[]",
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(e) => panic!("{e}"),
            }
        }
    });
    let mut child = start(root.path(), &socket);
    assert!(request("GET", "/api/v1/health", "", false).starts_with("HTTP/1.1 200"));
    assert!(Command::new(binary())
        .env_clear()
        .arg("--healthcheck")
        .status()
        .unwrap()
        .success());
    assert!(request("GET", "/api/v1/settings", "", false).starts_with("HTTP/1.1 401"));
    assert!(request("GET", "/api/v1/prompts", "", true).starts_with("HTTP/1.1 200"));
    assert!(request("GET", "/api/v1/%70rompts/", "", true).starts_with("HTTP/1.1 307"));
    assert!(request("PATCH", "/api/v1/prompts", "", true).contains("allow: GET"));
    let children = fs::read_to_string(format!(
        "/proc/{}/task/{}/children",
        child.0.id(),
        child.0.id()
    ))
    .unwrap();
    assert!(
        children.trim().is_empty(),
        "native service spawned a subprocess"
    );
    assert!(
        !root.path().join("models/models.ini").exists(),
        "bootstrap fabricated a preset"
    );
    assert_lock_held(root.path());
    // Hold SQLite's write lock while an actual admitted prompt writer is queued.
    let db = rusqlite::Connection::open(root.path().join("data/model_loader.db")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let writer = std::thread::spawn(|| {
        request(
            "POST",
            "/api/v1/prompts",
            r#"{"name":"qa-shutdown","body":"qa"}"#,
            true,
        )
    });
    std::thread::sleep(Duration::from_millis(40));
    signal(&child.0);
    std::thread::sleep(Duration::from_millis(40));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "shutdown did not drain admitted writer"
    );
    assert_lock_held(root.path());
    db.execute_batch("ROLLBACK").unwrap();
    let answer = writer.join().unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(wait(&mut child.0, Duration::from_secs(4)).success());
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM prompts WHERE name='qa-shutdown'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    drop(db);
    // Preserve the predecessor startup byte-order cases directly at native listener readiness,
    // rather than having a fake uvicorn executable inspect the file.
    for original in [
        "[chat]
model = /models/qa.gguf
",
        "version = 1
[*] ; shared
cache-ram = -1

[chat]
model = m

[qa-embed]
",
        "[*]trailing
sleep-idle-seconds = 60
cache-ram = 99999
[chat]
",
        "[*]
[chat]
cache-ram = 512
",
        "[a] trailing
model = m
[b];c
k : v
",
    ] {
        for setting in [None, Some("false")] {
            let reference = root.path().join("reference.ini");
            fs::write(&reference, original).unwrap();
            noevia_model_manager::idle_unload::reconcile_file(&reference, Some(1800)).unwrap();
            if setting.is_none() {
                noevia_model_manager::section_writes::SectionWrites::new(&reference, (1024, 2048))
                    .migrate_cache_ram()
                    .unwrap();
            }
            fs::write(root.path().join("models/models.ini"), original).unwrap();
            let mut child = start_config(root.path(), &socket, setting);
            assert_eq!(
                fs::read(root.path().join("models/models.ini")).unwrap(),
                fs::read(reference).unwrap(),
                "startup INI order {original:?}/{setting:?}"
            );
            signal(&child.0);
            assert!(wait(&mut child.0, Duration::from_secs(3)).success());
        }
    }
    // Stored administrator choice wins over the deployment default on the next native start.
    fs::write(
        root.path().join("data/chat-idle-unload.json"),
        r#"{"seconds":7200}"#,
    )
    .unwrap();
    let mut child = start(root.path(), &socket);
    assert!(fs::read_to_string(root.path().join("models/models.ini"))
        .unwrap()
        .contains("sleep-idle-seconds = 7200"));
    signal(&child.0);
    assert!(wait(&mut child.0, Duration::from_secs(3)).success());
    let db = rusqlite::Connection::open(root.path().join("data/model_loader.db")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM prompts WHERE name='qa-shutdown'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    drop(db);
    // An unrelated private-port trap is never consulted; it is not a child listener.
    let trap = std::net::TcpListener::bind("127.0.0.1:8091").unwrap();
    trap.set_nonblocking(true).unwrap();
    let mut child = start(root.path(), &socket);
    assert!(request("GET", "/api/v1/models", "", true).starts_with("HTTP/1.1 200"));
    assert!(request("GET", "/api/v1/overview", "", true).starts_with("HTTP/1.1 200"));
    assert!(
        trap.accept().is_err(),
        "native service attempted private RPC"
    );
    // A partial request body cannot drain; process death is bounded, with the writer lease
    // retained throughout the entire deadline. No state destructor runs before OS termination.
    let mut pending = TcpStream::connect("127.0.0.1:8090").unwrap();
    write!(
        pending,
        "POST /api/v1/downloads HTTP/1.1
Host: qa.invalid
x-model-loader-token: {TOKEN}
Content-Length: 1000

{{"
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let started = Instant::now();
    signal(&child.0);
    std::thread::sleep(Duration::from_millis(300));
    assert!(child.0.try_wait().unwrap().is_none());
    assert_lock_held(root.path());
    assert_eq!(
        wait(&mut child.0, Duration::from_secs(10)).code(),
        Some(124)
    );
    assert!(
        started.elapsed() >= Duration::from_secs(7),
        "shutdown skipped admission drain"
    );
    assert!(trap.accept().is_err(), "shutdown attempted private RPC");
    let lock = fs::File::options()
        .read(true)
        .write(true)
        .open(root.path().join("data/model_loader.db.prompts.lock"))
        .unwrap();
    lock.try_lock()
        .expect("writer lease was not released after process death");
    let db = rusqlite::Connection::open(root.path().join("data/model_loader.db")).unwrap();
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    done.store(true, Ordering::Relaxed);
    daemon.join().unwrap();
}
