//! Native socket and process lifecycle for the isolated Code sandbox.
//! Source-only: no image or selector uses this binary yet.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{killpg, Signal};
use nix::sys::wait::{waitid, Id, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::{watch, Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

const MAX_START_BYTES: usize = 64 * 1024;
const START_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CONNECTIONS: usize = 4;
const MAX_WALL_MS: u64 = 2 * 60 * 60 * 1000;

#[derive(Clone, Debug)]
pub struct Config {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub root: PathBuf,
    pub max_connections: usize,
    pub max_wall: Duration,
    pub grace: Duration,
    pub fallback_home: Option<String>,
    pub rust_bridge: bool,
}

impl Config {
    pub fn from_env() -> Result<(Self, u16), String> {
        let command = std::env::var("CODE_HARNESS_COMMAND")
            .map_err(|_| "CODE_HARNESS_COMMAND is required".to_owned())?;
        let root =
            std::env::var("WORKSPACE_ROOT").map_err(|_| "WORKSPACE_ROOT is required".to_owned())?;
        if command.is_empty() || root.is_empty() {
            return Err("CODE_HARNESS_COMMAND and WORKSPACE_ROOT are required".into());
        }
        let port = match std::env::var("PORT") {
            Ok(value) => value.parse::<u16>().map_err(|_| "PORT must be 0..65535")?,
            Err(_) => 8030,
        };
        let positive = |key: &str, fallback: u64| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(fallback)
        };
        let connections = positive("CODE_SANDBOX_MAX_CONNECTIONS", MAX_CONNECTIONS as u64);
        let max_connections = checked_connections(connections)?;
        Ok((
            Self {
                command: command.into(),
                args: std::env::var("CODE_HARNESS_ARGS")
                    .unwrap_or_default()
                    .split(' ')
                    .filter(|part| !part.is_empty())
                    .map(str::to_owned)
                    .collect(),
                root: root.into(),
                max_connections,
                max_wall: Duration::from_millis(positive("CODE_SANDBOX_MAX_WALL_MS", MAX_WALL_MS)),
                grace: Duration::from_secs(5),
                fallback_home: std::env::var("HOME").ok().filter(|v| !v.is_empty()),
                rust_bridge: std::env::var("SANDBOX_BRIDGE_IMPL").ok().as_deref() == Some("rust"),
            },
            port,
        ))
    }
}

fn checked_connections(value: u64) -> Result<usize, String> {
    let count = usize::try_from(value)
        .map_err(|_| "CODE_SANDBOX_MAX_CONNECTIONS is too large".to_owned())?;
    if count == 0 || count > Semaphore::MAX_PERMITS {
        return Err("CODE_SANDBOX_MAX_CONNECTIONS is outside the supported range".into());
    }
    Ok(count)
}

#[derive(Debug, PartialEq, Eq)]
struct Start {
    cwd: PathBuf,
    env: HashMap<String, String>,
}

fn parse_start(
    line: &str,
    root: &Path,
    fallback_home: Option<&str>,
) -> Result<Start, &'static str> {
    let parsed: Value = serde_json::from_str(&sandbox_bridge::start::start_json(line))
        .map_err(|_| "the sandbox bridge is unavailable")?;
    if parsed.get("start") != Some(&Value::Bool(true)) {
        return Err("the first line must be noevia’s start message");
    }
    let cwd = parsed
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or("that workspace is not inside this sandbox")?;
    let resolved_root = root
        .canonicalize()
        .map_err(|_| "that workspace is not inside this sandbox")?;
    let resolved = Path::new(cwd)
        .canonicalize()
        .map_err(|_| "that workspace is not inside this sandbox")?;
    if !resolved.starts_with(&resolved_root) {
        return Err("that workspace is not inside this sandbox");
    }
    let mut env = HashMap::new();
    if let Some(fields) = parsed.get("env").and_then(Value::as_object) {
        for (key, value) in fields {
            if let Some(value) = value.as_str() {
                env.insert(key.clone(), value.to_owned());
            }
        }
    }
    if env.get("HOME").is_none_or(String::is_empty) {
        env.remove("HOME");
        if let Some(home) = fallback_home.filter(|h| !h.is_empty()) {
            env.insert("HOME".into(), home.to_owned());
        }
    }
    Ok(Start { cwd: resolved, env })
}

async fn read_start(stream: &mut TcpStream) -> Result<(String, Vec<u8>), &'static str> {
    tokio::time::timeout(START_TIMEOUT, async {
        let mut buffer = Vec::new();
        loop {
            if let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                if end > MAX_START_BYTES {
                    return Err("start line too long");
                }
                let line = std::str::from_utf8(buffer.get(..end).ok_or("start line too long")?)
                    .map_err(|_| "the first line must be noevia’s start message")?
                    .to_owned();
                return Ok((line, buffer.iter().skip(end + 1).copied().collect()));
            }
            if buffer.len() >= MAX_START_BYTES {
                return Err("start line too long");
            }
            let mut chunk = [0_u8; 4096];
            let count = stream
                .read(&mut chunk)
                .await
                .map_err(|_| "the first line must be noevia’s start message")?;
            if count == 0 {
                return Err("the first line must be noevia’s start message");
            }
            buffer.extend(chunk.iter().take(count).copied());
        }
    })
    .await
    .map_err(|_| "start line timed out")?
}

async fn refuse(stream: &mut TcpStream, message: &str) {
    let line = json!({"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":message}})
        .to_string()
        + "\n";
    let _ = tokio::time::timeout(Duration::from_secs(1), stream.write_all(line.as_bytes())).await;
    let _ = stream.shutdown().await;
}

struct GroupGuard {
    pid: Pid,
    armed: bool,
}

impl GroupGuard {
    fn new(pid: Pid) -> Self {
        Self { pid, armed: true }
    }

    fn signal(&self, signal: Signal) -> nix::Result<()> {
        killpg(self.pid, signal)
    }

    // Only call after SIGKILL while the leader is still waitable. Reaping
    // releases the numeric PGID for reuse; Drop must not signal after that.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.signal(Signal::SIGKILL);
        }
    }
}

enum Stop {
    Child,
    Client,
    Wall,
    Shutdown,
    LostIdentity,
}

// WNOWAIT observes exit without reaping. The leader (including a zombie)
// holds its PID/PGID reservation until group teardown signals are complete.
fn leader_state(pid: Pid) -> nix::Result<bool> {
    match waitid(
        Id::Pid(pid),
        WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
    )? {
        WaitStatus::StillAlive => Ok(false),
        _ => Ok(true),
    }
}

async fn observe_leader(pid: Pid) -> Stop {
    loop {
        match leader_state(pid) {
            Ok(true) => return Stop::Child,
            Ok(false) => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => return Stop::LostIdentity,
        }
    }
}

async fn connection(
    mut stream: TcpStream,
    config: Config,
    mut shutdown: watch::Receiver<bool>,
    permit: OwnedSemaphorePermit,
) {
    let start = tokio::select! {
        value = read_start(&mut stream) => value,
        _ = shutdown.changed() => return,
    };
    let (line, rest) = match start {
        Ok(v) => v,
        Err(reason) => {
            refuse(&mut stream, reason).await;
            return;
        }
    };
    let start = match parse_start(&line, &config.root, config.fallback_home.as_deref()) {
        Ok(v) => v,
        Err(reason) => {
            refuse(&mut stream, reason).await;
            return;
        }
    };
    let mut command = Command::new(&config.command);
    command
        .args(&config.args)
        .current_dir(&start.cwd)
        .env_clear()
        .envs(&start.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if config.rust_bridge {
        // The downstream pi bridge must keep its pinned Rust/WASM framing.
        command.env("SANDBOX_BRIDGE_IMPL", "rust");
    }
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            refuse(&mut stream, "the sandbox agent could not start").await;
            return;
        }
    };
    let Some(pid) = child.id().and_then(|id| i32::try_from(id).ok()) else {
        let _ = child.kill().await;
        refuse(&mut stream, "the sandbox agent could not start").await;
        return;
    };
    let mut guard = GroupGuard::new(Pid::from_raw(pid));
    let Some(mut stdin) = child.stdin.take() else {
        guard.signal(Signal::SIGKILL).ok();
        guard.disarm();
        let _ = child.wait().await;
        return;
    };
    let Some(mut stdout) = child.stdout.take() else {
        guard.signal(Signal::SIGKILL).ok();
        guard.disarm();
        let _ = child.wait().await;
        return;
    };
    let Some(mut stderr) = child.stderr.take() else {
        guard.signal(Signal::SIGKILL).ok();
        guard.disarm();
        let _ = child.wait().await;
        return;
    };
    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let mut input = tokio::spawn(async move {
        let mut stdin_open = rest.is_empty() || stdin.write_all(&rest).await.is_ok();
        let mut chunk = [0_u8; 8192];
        loop {
            let count = reader.read(&mut chunk).await?;
            if count == 0 {
                break;
            }
            // Node ignores child.stdin errors. Continue draining the socket so
            // a child that closed stdin can still finish and send its stdout.
            if stdin_open {
                let bytes = chunk
                    .get(..count)
                    .ok_or_else(|| std::io::Error::other("read count exceeded buffer"))?;
                if stdin.write_all(bytes).await.is_err() {
                    stdin_open = false;
                }
            }
        }
        let _ = stdin.shutdown().await;
        Ok::<(), std::io::Error>(())
    });
    let (output_failure_tx, output_failure_rx) = tokio::sync::oneshot::channel();
    let output_writer = Arc::clone(&writer);
    let mut output = tokio::spawn(async move {
        let mut chunk = [0_u8; 8192];
        loop {
            let count = match stdout.read(&mut chunk).await {
                Ok(count) => count,
                Err(error) => {
                    let _ = output_failure_tx.send(());
                    return Err(error);
                }
            };
            if count == 0 {
                return Ok::<(), std::io::Error>(());
            }
            let bytes = chunk
                .get(..count)
                .ok_or_else(|| std::io::Error::other("read count exceeded buffer"))?;
            if let Err(error) = output_writer.lock().await.write_all(bytes).await {
                let _ = output_failure_tx.send(());
                return Err(error);
            }
        }
    });
    let output_failed = async {
        match output_failure_rx.await {
            Ok(()) => Stop::Client,
            Err(_) => std::future::pending().await,
        }
    };
    let stderr_task = tokio::spawn(async move {
        let mut chunk = [0_u8; 2048];
        while let Ok(count) = stderr.read(&mut chunk).await {
            if count == 0 {
                break;
            }
            // No untrusted stderr is logged: it may contain tokens.
        }
    });
    let reason = tokio::select! {
        state = observe_leader(guard.pid) => state,
        state = output_failed => state,
        _ = &mut input => Stop::Client,
        _ = tokio::time::sleep(config.max_wall) => Stop::Wall,
        _ = shutdown.changed() => Stop::Shutdown,
    };
    // No child.wait()/try_wait() has reaped the group leader. WNOWAIT keeps its
    // PID reserved even after exit, so the following group signals cannot
    // address a different process group.
    let identity_ok = !matches!(reason, Stop::LostIdentity) && leader_state(guard.pid).is_ok();
    let term_sent = if identity_ok {
        guard.signal(Signal::SIGTERM).is_ok()
    } else {
        // If some external reaper stole the child, the numeric PGID is no
        // longer safe to address. Fail closed on this connection.
        eprintln!("code-sandbox: child identity was lost before group teardown");
        guard.disarm();
        false
    };
    input.abort();
    if matches!(reason, Stop::Child) {
        // Preserve buffered final output, but never wait for a descendant
        // holding stdout open through the five-second group grace.
        let _ = tokio::time::timeout(Duration::from_millis(100), &mut output).await;
    }
    output.abort();
    stderr_task.abort();
    if matches!(reason, Stop::Wall) {
        let message = json!({"jsonrpc":"2.0","id":null,"error":{"code":-32000,
            "message":"The agent ran past the sandbox time limit and was stopped."}})
        .to_string()
            + "\n";
        if let Ok(mut sink) = tokio::time::timeout(Duration::from_millis(200), writer.lock()).await
        {
            let _ = tokio::time::timeout(
                Duration::from_millis(200),
                sink.write_all(message.as_bytes()),
            )
            .await;
        }
    }
    if let Ok(mut sink) = tokio::time::timeout(Duration::from_millis(200), writer.lock()).await {
        let _ = tokio::time::timeout(Duration::from_millis(200), sink.shutdown()).await;
    }
    drop(writer);
    // Node releases its connection slot on socket close, before SIGKILL's
    // grace timer. The cleanup task still owns the waitable leader.
    drop(permit);
    if term_sent {
        tokio::time::sleep(config.grace).await;
    }
    if identity_ok {
        let _ = guard.signal(Signal::SIGKILL);
        guard.disarm();
    }
    // Reap only after the last group signal. A failed wait cannot revive the
    // disarmed numeric PGID.
    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
}

/// Serve until the watch sender announces shutdown. The caller owns the listener.
pub async fn serve(
    listener: TcpListener,
    config: Config,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    if config.max_connections == 0 || config.max_connections > Semaphore::MAX_PERMITS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "CODE_SANDBOX_MAX_CONNECTIONS is outside the supported range",
        ));
    }
    let permits = Arc::new(Semaphore::new(config.max_connections));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, _) = accepted?;
                match Arc::clone(&permits).try_acquire_owned() {
                    Ok(permit) => {
                        tasks.spawn(connection(stream, config.clone(), shutdown.clone(), permit));
                    }
                    Err(_) => {
                        refuse(&mut stream, &format!("The sandbox is already running {} agents; try again when one finishes.", config.max_connections)).await;
                    }
                }
            }
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = joined {
                    eprintln!("code-sandbox: connection task failed: {error}");
                }
            }
            _ = shutdown.changed() => break,
        }
    }
    while tasks.join_next().await.is_some() {}
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_start_filters_environment_and_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let work = root.join("task");
        std::fs::create_dir_all(&work).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let line = format!(
            r#"{{"noevia":"start","cwd":"{}","env":{{"PATH":"/bin","LD_PRELOAD":"bad","HOME":10}}}}"#,
            work.display()
        );
        let start = parse_start(&line, &root, Some("/fallback")).unwrap();
        assert_eq!(start.cwd, work);
        assert_eq!(start.env.get("PATH").map(String::as_str), Some("/bin"));
        assert_eq!(start.env.get("HOME").map(String::as_str), Some("/fallback"));
        assert!(!start.env.contains_key("LD_PRELOAD"));
        let bad = format!(
            r#"{{"noevia":"start","cwd":"{}"}}"#,
            root.join("escape").display()
        );
        assert!(parse_start(&bad, &root, None).is_err());
        assert!(parse_start(r#"{"method":"initialize"}"#, &root, None).is_err());
        let empty_home = format!(
            r#"{{"noevia":"start","cwd":"{}","env":{{"HOME":""}}}}"#,
            work.display()
        );
        let start = parse_start(&empty_home, &root, Some("/fallback")).unwrap();
        assert_eq!(start.env.get("HOME").map(String::as_str), Some("/fallback"));
    }

    #[test]
    fn connection_cap_rejects_semaphore_overflow() {
        assert!(checked_connections(u64::MAX).is_err());
        assert!(checked_connections((Semaphore::MAX_PERMITS as u64) + 1).is_err());
        assert_eq!(checked_connections(4).unwrap(), 4);
    }
}
