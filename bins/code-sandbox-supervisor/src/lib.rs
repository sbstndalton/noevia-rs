//! Native socket and process lifecycle for the isolated Code sandbox.
//! Source-only: no image or selector uses this binary yet.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{killpg, Signal};
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
        let max_connections = usize::try_from(connections)
            .map_err(|_| "CODE_SANDBOX_MAX_CONNECTIONS is too large")?;
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
            },
            port,
        ))
    }
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
    if !env.contains_key("HOME") {
        if let Some(home) = fallback_home {
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

struct GroupGuard(Pid);

impl GroupGuard {
    fn signal(&self, signal: Signal) {
        let _ = killpg(self.0, signal);
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.signal(Signal::SIGKILL);
    }
}

enum Stop {
    Child,
    Client,
    Wall,
    Shutdown,
}

async fn connection(
    mut stream: TcpStream,
    config: Config,
    mut shutdown: watch::Receiver<bool>,
    _permit: OwnedSemaphorePermit,
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
    let guard = GroupGuard(Pid::from_raw(pid));
    let Some(mut stdin) = child.stdin.take() else {
        return;
    };
    let Some(mut stdout) = child.stdout.take() else {
        return;
    };
    let Some(mut stderr) = child.stderr.take() else {
        return;
    };
    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let mut input = tokio::spawn(async move {
        if !rest.is_empty() {
            stdin.write_all(&rest).await?;
        }
        tokio::io::copy(&mut reader, &mut stdin).await?;
        stdin.shutdown().await
    });
    let output_writer = Arc::clone(&writer);
    let mut output = tokio::spawn(async move {
        let mut chunk = [0_u8; 8192];
        loop {
            let count = stdout.read(&mut chunk).await?;
            if count == 0 {
                return Ok::<(), std::io::Error>(());
            }
            let bytes = chunk
                .get(..count)
                .ok_or_else(|| std::io::Error::other("read count exceeded buffer"))?;
            output_writer.lock().await.write_all(bytes).await?;
        }
    });
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
        _ = child.wait() => Stop::Child,
        _ = &mut input => Stop::Client,
        _ = tokio::time::sleep(config.max_wall) => Stop::Wall,
        _ = shutdown.changed() => Stop::Shutdown,
    };
    guard.signal(Signal::SIGTERM);
    if matches!(reason, Stop::Wall) {
        output.abort();
        let message = json!({"jsonrpc":"2.0","id":null,"error":{"code":-32000,
            "message":"The agent ran past the sandbox time limit and was stopped."}})
        .to_string()
            + "\n";
        if let Ok(mut writer) = tokio::time::timeout(Duration::from_secs(1), writer.lock()).await {
            let _ =
                tokio::time::timeout(Duration::from_secs(1), writer.write_all(message.as_bytes()))
                    .await;
        }
    }
    tokio::time::sleep(config.grace).await;
    guard.signal(Signal::SIGKILL);
    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    if matches!(reason, Stop::Child) {
        let _ = tokio::time::timeout(Duration::from_millis(200), &mut output).await;
    }
    input.abort();
    output.abort();
    stderr_task.abort();
    drop(writer);
    // GroupGuard's drop repeats SIGKILL if a descendant survived a race.
}

/// Serve until the watch sender announces shutdown. The caller owns the listener.
pub async fn serve(
    listener: TcpListener,
    config: Config,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
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
    }
}
