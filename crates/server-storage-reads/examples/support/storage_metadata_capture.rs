//! Partial evidence survives faults. Cooperative group cleanup; no hard reap/RSS guarantee.
use std::{io::{Read, Write}, os::{fd::AsFd, unix::process::{CommandExt, ExitStatusExt}}, process::{Child, Command, Stdio}, time::{Duration, Instant}};
pub const INPUT: usize = 8 * 1024 * 1024;
pub const OUTPUT: usize = 8 * 1024 * 1024;
pub const STDERR: usize = 64 * 1024;
pub const PHASE: Duration = Duration::from_secs(30);

#[derive(Default, Debug)]
pub struct Evidence {
    pub stdout: Vec<u8>, pub stderr: Vec<u8>, pub stdout_eof: bool, pub stderr_eof: bool,
    pub code: Option<i32>, pub signal: Option<i32>, pub raw_status: Option<i32>,
    pub failure: Option<String>, pub cleanup: Vec<String>, pub spawned: bool,
    pub group_signal: Option<Result<(), String>>,
}
impl Evidence {
    pub fn completed(&self) -> bool {
        self.spawned && self.failure.is_none() && self.cleanup.is_empty()
            && self.stdout_eof && self.stderr_eof && self.raw_status.is_some()
            && matches!(self.group_signal.as_ref(), Some(Ok(())))
    }
}
pub fn retain(out: &mut Vec<u8>, bytes: &[u8], cap: usize) -> Result<(), String> {
    let count = bytes.len().min(cap.saturating_sub(out.len()));
    out.try_reserve(count).map_err(|_| "capture allocation")?;
    out.extend(bytes.iter().take(count));
    if count != bytes.len() { return Err("capture output cap".into()); }
    Ok(())
}
fn nonblocking(pipe: &impl AsFd) -> Result<(), String> {
    let flags = rustix::fs::fcntl_getfl(pipe).map_err(|e| e.to_string())?;
    rustix::fs::fcntl_setfl(pipe, flags | rustix::fs::OFlags::NONBLOCK).map_err(|e| e.to_string())
}
fn step(pipe: &mut impl Read, out: &mut Vec<u8>, cap: usize) -> Result<bool, String> {
    let mut bytes = [0; 4096];
    match pipe.read(&mut bytes) {
        Ok(0) => Ok(true),
        Ok(n) => { retain(out, bytes.get(..n).ok_or("read count")?, cap)?; Ok(false) }
        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => Ok(false),
        Err(e) => Err(format!("capture read: {e}")),
    }
}
struct Owned { child: Child, reaped: bool }
impl Owned {
    fn pid(&self) -> Result<rustix::process::Pid, String> {
        i32::try_from(self.child.id()).ok().and_then(rustix::process::Pid::from_raw).ok_or("child identity".into())
    }
    fn observe(&self) -> Result<bool, String> {
        use rustix::process::{waitid, WaitId, WaitIdOptions};
        waitid(WaitId::Pid(self.pid()?), WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT)
            .map(|s| s.is_some()).map_err(|e| format!("observe: {e}"))
    }
    fn finish(&mut self, evidence: &mut Evidence) {
        let signal = self.pid().and_then(|pid| rustix::process::kill_process_group(pid, rustix::process::Signal::KILL).map_err(|e| format!("group signal errno: {e}")));
        if let Err(error) = &signal { evidence.cleanup.push(error.clone()); }
        evidence.group_signal = Some(signal);
        // Keep the group/leader identity until after signaling, including an exited leader.
        match self.child.wait() {
            Ok(status) => { self.reaped = true; evidence.code = status.code(); evidence.signal = status.signal(); evidence.raw_status = Some(status.into_raw()); },
            Err(e) => evidence.cleanup.push(format!("reap errno: {e}")),
        }
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.reaped { let mut evidence = Evidence::default(); self.finish(&mut evidence); }
    }
}
pub fn bounded(mut command: Command, input: &[u8]) -> Evidence {
    let mut evidence = Evidence::default();
    if input.len() > INPUT { evidence.failure = Some("capture input cap".into()); return evidence; }
    let start = Instant::now();
    command.process_group(0).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = match command.spawn() { Ok(child) => child, Err(e) => { evidence.failure = Some(format!("spawn: {e}")); return evidence; } };
    evidence.spawned = true;
    let mut owned = Owned { child, reaped: false };
    let result = (|| {
        let stdin = owned.child.stdin.take().ok_or("stdin absent")?;
        let mut stdout = owned.child.stdout.take().ok_or("stdout absent")?;
        let mut stderr = owned.child.stderr.take().ok_or("stderr absent")?;
        nonblocking(&stdin)?; nonblocking(&stdout)?; nonblocking(&stderr)?;
        let mut written = 0; let mut stdin = Some(stdin);
        loop {
            if start.elapsed() >= PHASE { return Err("capture deadline".into()); }
            if let Some(pipe) = stdin.as_mut() {
                if written == input.len() { stdin = None; } else {
                    let bytes = input.get(written..).ok_or("input position")?;
                    match pipe.write(bytes.get(..bytes.len().min(4096)).ok_or("input chunk")?) {
                        Ok(0) => return Err("stdin zero progress".into()), Ok(n) => written += n,
                        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => {},
                        Err(e) => return Err(format!("stdin: {e}")),
                    }
                }
            }
            if !evidence.stdout_eof { evidence.stdout_eof = step(&mut stdout, &mut evidence.stdout, OUTPUT)?; }
            if !evidence.stderr_eof { evidence.stderr_eof = step(&mut stderr, &mut evidence.stderr, STDERR)?; }
            if owned.observe()? && stdin.is_none() && evidence.stdout_eof && evidence.stderr_eof { break; }
            std::thread::sleep(Duration::from_millis(2).min(PHASE.saturating_sub(start.elapsed())));
        }
        Ok::<(), String>(())
    })();
    evidence.failure = result.err();
    // Pipe locals close before signaling/reaping. Blocking wait remains a trust limitation.
    owned.finish(&mut evidence);
    evidence
}
