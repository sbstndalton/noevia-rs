//! Bounded nonblocking pipe IO; signal the owned group before reaping its leader.
//! Descendants that leave that group are outside cleanup authority. Their inherited
//! pipe descriptors cannot delay deadline return because no IO worker is joined.
use std::{
    io::{Read, Write},
    os::fd::AsFd,
    os::unix::process::CommandExt,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};
pub const INPUT: usize = 8 * 1024 * 1024;
const OUTPUT: usize = 8 * 1024 * 1024;
const STDERR: usize = 64 * 1024;
struct Owned(Child, bool);
impl Owned {
    fn pid(&self) -> Result<rustix::process::Pid, String> {
        i32::try_from(self.0.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .ok_or_else(|| "invalid child identity".into())
    }
    fn exited(&self) -> Result<bool, String> {
        use rustix::process::{waitid, WaitId, WaitIdOptions};
        waitid(
            WaitId::Pid(self.pid()?),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .map(|status| status.is_some())
        .map_err(|e| format!("child observation failed: {e}"))
    }
    fn stop(&mut self, before_signal: impl FnOnce(u32)) -> Result<ExitStatus, String> {
        if self.1 {
            return Err("child already reaped".into());
        }
        // No try_wait/wait has consumed the leader. Even an exited leader retains
        // this numeric identity until the wait below, after the owned group signal.
        before_signal(self.0.id());
        if let Ok(pid) = self.pid() {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.0.kill();
        let status = self.0.wait().map_err(|e| format!("child reap failed: {e}"));
        self.1 = status.is_ok();
        status
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.1 {
            let _ = self.stop(|_| {});
        }
    }
}
fn nonblocking(pipe: &impl AsFd) -> Result<(), String> {
    let flags = rustix::fs::fcntl_getfl(pipe).map_err(|e| e.to_string())?;
    rustix::fs::fcntl_setfl(pipe, flags | rustix::fs::OFlags::NONBLOCK).map_err(|e| e.to_string())
}
// One bounded syscall per stream per iteration prevents a continuously producing
// child from starving another stream or the monotonic deadline check.
fn read_step(pipe: &mut impl Read, bytes: &mut Vec<u8>, cap: usize) -> Result<bool, String> {
    let mut buf = [0; 4096];
    match pipe.read(&mut buf) {
        Ok(0) => Ok(true),
        Ok(n) => {
            if bytes.len() + n > cap {
                return Err("child output limit exceeded".into());
            }
            bytes.try_reserve(n).map_err(|e| e.to_string())?;
            bytes.extend(buf.get(..n).ok_or("invalid child read count")?);
            Ok(false)
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(format!("child output failed: {e}")),
    }
}
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
pub fn bounded(mut command: Command, input: Vec<u8>, deadline: Duration) -> Result<Output, String> {
    bounded_with(&mut command, input, deadline, |_| {})
}
pub fn bounded_with(
    command: &mut Command,
    input: Vec<u8>,
    deadline: Duration,
    spawned: impl FnOnce(u32),
) -> Result<Output, String> {
    bounded_observed(command, input, deadline, spawned, |_| {})
}
pub fn bounded_observed(
    command: &mut Command,
    input: Vec<u8>,
    deadline: Duration,
    spawned: impl FnOnce(u32),
    before_signal: impl FnOnce(u32),
) -> Result<Output, String> {
    if input.len() > INPUT {
        return Err("child input limit exceeded".into());
    }
    let start = Instant::now();
    command
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = Owned(
        command
            .spawn()
            .map_err(|e| format!("child spawn failed: {e}"))?,
        false,
    );
    spawned(child.0.id());
    let mut stdin = Some(child.0.stdin.take().ok_or("child stdin unavailable")?);
    let mut stdout_pipe = child.0.stdout.take().ok_or("child stdout unavailable")?;
    let mut stderr_pipe = child.0.stderr.take().ok_or("child stderr unavailable")?;
    nonblocking(stdin.as_ref().ok_or("child stdin absent")?)?;
    nonblocking(&stdout_pipe)?;
    nonblocking(&stderr_pipe)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut written = 0;
    let mut out_done = false;
    let mut err_done = false;
    let result = (|| loop {
        if start.elapsed() >= deadline {
            return Err("child deadline exceeded".into());
        }
        let previous = (
            written,
            stdout.len(),
            stderr.len(),
            stdin.is_none(),
            out_done,
            err_done,
        );
        if let Some(pipe) = stdin.as_mut() {
            if written == input.len() {
                stdin = None;
            } else {
                let remaining = input.get(written..).ok_or("invalid child write count")?;
                let chunk = remaining
                    .get(..remaining.len().min(4096))
                    .ok_or("invalid child input slice")?;
                match pipe.write(chunk) {
                    Ok(0) => return Err("child stdin failed: write zero".into()),
                    Ok(n) => written += n,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(e) => return Err(format!("child stdin failed: {e}")),
                }
            }
        }
        if !out_done {
            out_done = read_step(&mut stdout_pipe, &mut stdout, OUTPUT)?;
        }
        if !err_done {
            err_done = read_step(&mut stderr_pipe, &mut stderr, STDERR)?;
        }
        if child.exited()? && stdin.is_none() && out_done && err_done {
            return Ok(());
        }
        if previous
            == (
                written,
                stdout.len(),
                stderr.len(),
                stdin.is_none(),
                out_done,
                err_done,
            )
        {
            thread::sleep(Duration::from_millis(2).min(deadline.saturating_sub(start.elapsed())));
        }
    })();
    // Close local pipes before cleanup; remote holders have no bearing on this.
    drop(stdin);
    drop(stdout_pipe);
    drop(stderr_pipe);
    let status = child.stop(before_signal)?;
    result?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
pub fn json_input(value: &serde_json::Value) -> Result<Vec<u8>, String> {
    struct Limited(Vec<u8>);
    impl Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0.len() + bytes.len() > INPUT {
                return Err(std::io::Error::other("child input limit exceeded"));
            }
            self.0
                .try_reserve(bytes.len())
                .map_err(std::io::Error::other)?;
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Limited(Vec::new());
    serde_json::to_writer(&mut writer, value).map_err(|e| e.to_string())?;
    Ok(writer.0)
}
