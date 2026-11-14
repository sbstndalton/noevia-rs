//! `code-harness-bridge`: the tokio shell around [`code_harness_bridge::Bridge`].
//!
//! Environment, as the JS reference reads it: `HOME` (the task's private home, required for a
//! session), `PI_COMMAND` (the harness command, default `pi`), `PI_BRIDGE_ASK_TIMEOUT_MS`.
//! `SANDBOX_BRIDGE_IMPL` (set by the supervisor) is never passed on to the harness.
//!
//! Lines the harness wrote before it exited are handled before its exit is (the reference ends
//! the session on the exit event and may drop them); the drain is bounded by [`EXIT_DRAIN`].

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use code_harness_bridge::{ask_timeout_ms, Bridge, Harness, KILL_GRACE_MS};
use nix::sys::signal::{kill, killpg, Signal};
use nix::unistd::Pid;
use sandbox_bridge::frame::{Framer, Pushed, DEFAULT_LIMIT};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::time::{sleep_until, timeout, Instant};

/// How long the harness's remaining output is read after it exited.
const EXIT_DRAIN: Duration = Duration::from_millis(200);
const READ_CHUNK: usize = 64 * 1024;
const LOG_LIMIT: usize = 2000;

/// Node's `setEncoding('utf8')`: a UTF-8 decoder across chunk boundaries, U+FFFD for bad bytes.
#[derive(Default)]
struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        let mut rest: &[u8] = &self.pending;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let (good, bad) = rest.split_at(e.valid_up_to());
                    out.push_str(std::str::from_utf8(good).unwrap_or(""));
                    match e.error_len() {
                        Some(n) => {
                            out.push('\u{fffd}');
                            rest = bad.get(n..).unwrap_or(&[]);
                        }
                        None => {
                            rest = bad;
                            break;
                        }
                    }
                }
            }
        }
        let tail = rest.to_vec();
        self.pending = tail;
        out
    }
}

/// The harness process: one per session.
struct Process {
    command: String,
    child: Option<Child>,
    stdout: Option<ChildStdout>,
    stdin: Option<UnboundedSender<Vec<u8>>>,
    pid: Option<i32>,
    kill_at: Option<Instant>,
}

fn errno_name(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(n) => format!("{:?}", nix::errno::Errno::from_raw(n)),
        None => e.kind().to_string(),
    }
}

impl Harness for Process {
    fn spawn(&mut self, cwd: Option<&str>, args: &[String]) -> Result<(), String> {
        let mut command = Command::new(&self.command);
        command
            .args(args)
            .env_remove("SANDBOX_BRIDGE_IMPL")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // Its own process group, so whatever it spawns dies with the session.
            .process_group(0);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("spawn {} {}", self.command, errno_name(&e)))?;
        self.pid = child.id().and_then(|p| i32::try_from(p).ok());
        self.stdout = child.stdout.take();
        if let Some(mut stdin) = child.stdin.take() {
            let (tx, mut rx) = unbounded_channel::<Vec<u8>>();
            tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    // EPIPE once the harness died: its exit ends the session.
                    if stdin.write_all(&line).await.is_err() {
                        break;
                    }
                }
            });
            self.stdin = Some(tx);
        }
        self.child = Some(child);
        Ok(())
    }

    fn write(&mut self, command: &Value) {
        if let Some(tx) = &self.stdin {
            let mut line = command.to_string();
            line.push('\n');
            let _ = tx.send(line.into_bytes());
        }
    }

    fn kill(&mut self) {
        self.signal(Signal::SIGTERM);
        if self.child.is_some() {
            self.kill_at = Some(Instant::now() + Duration::from_millis(KILL_GRACE_MS));
        }
    }
}

impl Process {
    fn signal(&self, signal: Signal) {
        let Some(pid) = self.pid.map(Pid::from_raw) else {
            return;
        };
        if killpg(pid, signal).is_err() {
            let _ = kill(pid, signal);
        }
    }
}

fn describe(status: Option<ExitStatus>) -> String {
    use std::os::unix::process::ExitStatusExt;
    let Some(status) = status else {
        return "exited (null)".to_owned();
    };
    if let Some(sig) = status.signal() {
        let name =
            Signal::try_from(sig).map_or_else(|_| sig.to_string(), |s| s.as_str().to_owned());
        return format!("exited ({name})");
    }
    format!("exited ({})", status.code().unwrap_or(0))
}

async fn read_some<R: AsyncRead + Unpin>(
    r: &mut Option<R>,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    match r {
        Some(r) => r.read(buf).await,
        None => std::future::pending().await,
    }
}

async fn wait_child(child: &mut Option<Child>) -> Option<ExitStatus> {
    match child {
        Some(c) => c.wait().await.ok(),
        None => std::future::pending().await,
    }
}

async fn until(at: Option<Instant>) {
    match at {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn feed(framer: &mut Framer, text: &str) -> Result<Vec<String>, usize> {
    match framer.push(text) {
        Pushed::Lines(lines) => Ok(lines),
        Pushed::Overflow(size) => Err(size),
    }
}

fn harness_chunk(bridge: &mut Bridge, process: &mut Process, framer: &mut Framer, text: &str) {
    match feed(framer, text) {
        Ok(lines) => {
            for line in lines {
                bridge.harness_line(process, &line);
            }
        }
        Err(size) => bridge.harness_overflow(process, size),
    }
    bridge.end_chunk(process);
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let home = std::env::var("HOME").ok();
    let ask = ask_timeout_ms(std::env::var("PI_BRIDGE_ASK_TIMEOUT_MS").ok().as_deref());
    let command = std::env::var("PI_COMMAND")
        .ok()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "pi".to_owned());
    let mut bridge = Bridge::new(home, ask);
    let mut process = Process {
        command,
        child: None,
        stdout: None,
        stdin: None,
        pid: None,
        kill_at: None,
    };

    let (out_tx, mut out_rx) = unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(&line).await.is_err() || stdout.flush().await.is_err() {
                break; // client gone
            }
        }
        let _ = stdout.shutdown().await;
    });
    let mut out_tx = Some(out_tx);

    let mut stdin = Some(tokio::io::stdin());
    let (mut client_text, mut client_frame) = (Utf8Stream::default(), Framer::new(DEFAULT_LIMIT));
    let (mut harness_text, mut harness_frame) = (Utf8Stream::default(), Framer::new(DEFAULT_LIMIT));
    let mut timers: Vec<(String, Instant)> = Vec::new();
    let mut client_buf = vec![0u8; READ_CHUNK];
    let mut harness_buf = vec![0u8; READ_CHUNK];

    loop {
        for line in bridge.take_logs() {
            let line: String = line.chars().take(LOG_LIMIT).collect();
            eprintln!("code-harness-bridge: {line}");
        }
        for message in bridge.take_output() {
            if let Some(tx) = &out_tx {
                let mut line = message.to_string();
                line.push('\n');
                let _ = tx.send(line.into_bytes());
            }
        }
        let now = Instant::now();
        for (id, ms) in bridge.take_timers() {
            timers.push((id, now + Duration::from_millis(ms)));
        }
        if bridge.output_closed() {
            out_tx = None;
            stdin = None;
        }
        if process.child.is_none() && (stdin.is_none() || bridge.closed()) {
            break;
        }
        let next_timer = timers.iter().map(|(_, at)| *at).min();
        let kill_at = process.kill_at.filter(|_| process.child.is_some());
        tokio::select! {
            read = read_some(&mut stdin, &mut client_buf) => {
                match read {
                    Ok(n) if n > 0 => {
                        let text = client_text.push(client_buf.get(..n).unwrap_or(&[]));
                        match feed(&mut client_frame, &text) {
                            Ok(lines) => {
                                for line in lines {
                                    bridge.client_line(&mut process, &line);
                                }
                            }
                            Err(size) => bridge.client_overflow(&mut process, size),
                        }
                    }
                    _ => {
                        stdin = None;
                        if !bridge.closed() {
                            bridge.client_end(&mut process);
                        }
                    }
                }
                bridge.end_chunk(&mut process);
            }
            read = read_some(&mut process.stdout, &mut harness_buf) => {
                match read {
                    Ok(n) if n > 0 => {
                        let text = harness_text.push(harness_buf.get(..n).unwrap_or(&[]));
                        harness_chunk(&mut bridge, &mut process, &mut harness_frame, &text);
                    }
                    _ => process.stdout = None,
                }
            }
            status = wait_child(&mut process.child) => {
                // Read what it wrote before it went, then report it gone.
                let deadline = Instant::now() + EXIT_DRAIN;
                while process.stdout.is_some() {
                    let left = deadline.saturating_duration_since(Instant::now());
                    match timeout(left, read_some(&mut process.stdout, &mut harness_buf)).await {
                        Ok(Ok(n)) if n > 0 => {
                            let text = harness_text.push(harness_buf.get(..n).unwrap_or(&[]));
                            harness_chunk(&mut bridge, &mut process, &mut harness_frame, &text);
                        }
                        _ => process.stdout = None,
                    }
                }
                process.child = None;
                process.stdin = None;
                process.pid = None;
                bridge.harness_gone(&mut process, &describe(status));
                bridge.end_chunk(&mut process);
            }
            () = until(next_timer) => {
                let now = Instant::now();
                let (due, keep): (Vec<_>, Vec<_>) = timers.drain(..).partition(|(_, at)| *at <= now);
                timers = keep;
                for (id, _) in due {
                    bridge.ask_timed_out(&mut process, &id);
                }
                bridge.end_chunk(&mut process);
            }
            () = until(kill_at) => {
                process.kill_at = None;
                process.signal(Signal::SIGKILL);
            }
        }
    }
    for line in bridge.take_logs() {
        let line: String = line.chars().take(LOG_LIMIT).collect();
        eprintln!("code-harness-bridge: {line}");
    }
    drop(out_tx);
    let _ = writer.await;
    // Not a return: tokio's stdin reader is a blocking thread that would keep the process alive
    // until the client writes or hangs up, and the bridge must exit on its own (#852).
    std::process::exit(0);
}
