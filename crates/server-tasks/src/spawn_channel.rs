//! Private stdio engine channel. One worker owns the unreaped process-group leader until every
//! descendant has been signalled. No listener, inherited environment, or public channel exists.
use crate::acp::Handlers;
use js_json::JValue;
use rustix::{
    fs::{fcntl_getfl, fcntl_setfl, OFlags},
    process::{kill_process_group, waitid, Pid, Signal, WaitId, WaitIdOptions},
};
use std::{
    io::{self, Read, Write},
    os::unix::{net::UnixStream, process::CommandExt},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
pub struct Channel {
    pub stream: tokio::net::UnixStream,
    pub settled: tokio::sync::oneshot::Receiver<()>,
}
struct Process {
    child: Child,
    pid: Pid,
    reaped: bool,
}
impl Process {
    fn exited(&self) -> bool {
        waitid(
            WaitId::Pid(self.pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .map(|s| s.is_some())
        .unwrap_or(true)
    }
    fn signal(&self, signal: Signal) {
        let _ = kill_process_group(self.pid, signal);
    }
    fn reap(&mut self) {
        self.signal(Signal::KILL);
        let _ = self.child.wait();
        self.reaped = true;
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if !self.reaped {
            self.signal(Signal::KILL);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
fn nonblock(fd: &impl std::os::fd::AsFd) -> io::Result<()> {
    let flags = fcntl_getfl(fd).map_err(io::Error::from)?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK).map_err(io::Error::from)
}
fn fill(reader: &mut impl Read, buffer: &mut Vec<u8>) -> io::Result<bool> {
    let mut chunk = [0; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(false),
            Ok(n) => {
                if buffer.len().saturating_add(n) > 64 * 1024 * 1024 {
                    return Err(io::Error::other("engine channel limit"));
                }
                buffer.extend_from_slice(
                    chunk
                        .get(..n)
                        .ok_or_else(|| io::Error::other("engine channel"))?,
                );
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}
fn flush(writer: &mut impl Write, buffer: &mut Vec<u8>) -> io::Result<()> {
    while !buffer.is_empty() {
        match writer.write(buffer) {
            Ok(0) => return Err(io::Error::other("engine closed")),
            Ok(n) => {
                buffer.drain(..n);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
/// Call only for an operator-configured engine command. The worker retains callbacks and their
/// account/inference/reservation guards through process teardown, even if the caller disappears.
pub fn open(
    command: &str,
    args: &[String],
    cwd: &str,
    environment: &JValue,
    handlers: Arc<dyn Handlers>,
    grace: Duration,
) -> io::Result<Channel> {
    let mut command = Command::new(command);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let JValue::Obj(fields) = environment {
        for (k, v) in fields {
            if let Some(v) = v.as_str() {
                command.env(k, v);
            }
        }
    }
    let mut child = command.spawn()?;
    let Some(pid) = i32::try_from(child.id()).ok().and_then(Pid::from_raw) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other("engine process unavailable"));
    };
    let mut process = Process {
        child,
        pid,
        reaped: false,
    };
    let mut input = process
        .child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("engine input unavailable"))?;
    let mut output = process
        .child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("engine output unavailable"))?;
    let mut errors = process
        .child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("engine diagnostics unavailable"))?;
    nonblock(&input)?;
    nonblock(&output)?;
    nonblock(&errors)?;
    let (native, mut bridge) = UnixStream::pair()?;
    native.set_nonblocking(true)?;
    bridge.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(native)?;
    let (done, settled) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("task-engine".into())
        .spawn(move || {
            let _handlers = handlers;
            let mut incoming = Vec::new();
            let mut outgoing = Vec::new();
            let mut diagnostics = Vec::new();
            loop {
                let live = (|| -> io::Result<bool> {
                    let input_live = fill(&mut bridge, &mut incoming)?;
                    flush(&mut input, &mut incoming)?;
                    let output_live = fill(&mut output, &mut outgoing)?;
                    flush(&mut bridge, &mut outgoing)?;
                    let _ = fill(&mut errors, &mut diagnostics)?;
                    diagnostics.clear();
                    Ok(input_live && (output_live || !outgoing.is_empty()))
                })();
                if process.exited() {
                    process.signal(Signal::KILL);
                }
                if !matches!(live, Ok(true)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            process.signal(Signal::TERM);
            let started = Instant::now();
            while !process.exited() && started.elapsed() < grace {
                std::thread::sleep(Duration::from_millis(2));
            }
            process.reap();
            drop(bridge);
            let _ = done.send(());
        })?;
    Ok(Channel { stream, settled })
}
