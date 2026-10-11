use crate::BODY_CAP;
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{mpsc, oneshot, Mutex, Semaphore},
    time::{timeout, timeout_at, Instant},
};

#[derive(Clone, Debug)]
pub struct Config {
    pub python: PathBuf,
    pub script: PathBuf,
    pub size: usize,
    pub startup: Duration,
    pub decision: Duration,
    pub queue: Duration,
    pub late_grace: Duration,
    pub retries: [Duration; 3],
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
    use super::*;
    #[tokio::test]
    async fn admitted_recovery_race_bounds_send_and_reply_without_expired_dispatch() {
        let directory = tempfile::tempdir().unwrap();
        crate::oracle::prepare(&crate::oracle::reference(), directory.path()).unwrap();
        let mut config = Config::production();
        config.python = "python3".into();
        config.script = directory.path().join("server.py");
        config.startup = Duration::from_secs(3);
        config.decision = Duration::from_millis(80);
        config.queue = Duration::from_millis(20);
        let pool = Pool::start(config).await.unwrap();
        std::fs::write(directory.path().join("delay-startup"), b"0.5").unwrap();
        assert_eq!(
            pool.decide(crate::oracle::body("__crash")).await,
            Err(Failure::Unavailable)
        );
        timeout(Duration::from_secs(2), async {
            while std::fs::read_to_string(directory.path().join("starts"))
                .unwrap()
                .lines()
                .count()
                < 2
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        // Deterministically model the ready load immediately preceding EOF:
        // the actor is now in delayed recovery, with its request channel open.
        pool.slots[0].ready.store(true, Ordering::Release);
        for state in ["expired-reply", "expired-full-channel"] {
            let begin = Instant::now();
            assert_eq!(
                timeout(
                    Duration::from_millis(200),
                    pool.decide(crate::oracle::body(state))
                )
                .await
                .unwrap(),
                Err(Failure::Timeout)
            );
            assert!(begin.elapsed() < Duration::from_millis(200));
        }
        tokio::time::sleep(Duration::from_millis(450)).await;
        let (reply, answer) = oneshot::channel();
        pool.slots[0]
            .tx
            .send(Request {
                body: crate::oracle::body("expired-open-reply"),
                end: Instant::now() - Duration::from_millis(1),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(
            answer.await.unwrap(),
            Err(Failure::Timeout),
            "actor restarted expired admission budget"
        );
        assert_eq!(
            pool.decide(crate::oracle::body("healthy")).await.unwrap()["selected"],
            "continue"
        );
        let events = std::fs::read_to_string(directory.path().join("events")).unwrap();
        assert!(
            !events.contains("expired"),
            "expired admitted request reached replacement"
        );
        assert_eq!(events.lines().count(), 2);
        pool.stop().await;
    }
    #[tokio::test]
    async fn frame_reader_preserves_partial_prefix_and_payload_on_cancellation() {
        let payload = br#"{"ready":true}"#;
        let header = (payload.len() as u32).to_be_bytes();
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let mut frame = Reader::default();
        writer.write_all(&header[..2]).await.unwrap();
        assert!(timeout(Duration::from_millis(10), frame.read(&mut reader))
            .await
            .is_err());
        writer.write_all(&header[2..]).await.unwrap();
        writer.write_all(&payload[..5]).await.unwrap();
        assert!(timeout(Duration::from_millis(10), frame.read(&mut reader))
            .await
            .is_err());
        writer.write_all(&payload[5..]).await.unwrap();
        assert_eq!(
            frame.read(&mut reader).await.unwrap(),
            json!({"ready":true})
        );
        writer.write_all(&header).await.unwrap();
        writer.write_all(payload).await.unwrap();
        assert_eq!(
            frame.read(&mut reader).await.unwrap(),
            json!({"ready":true})
        );
    }
    #[tokio::test]
    async fn malformed_and_oversized_frames_are_bounded() {
        for data in [
            vec![0, 0],
            vec![0, 0, 0, 0],
            vec![255, 255, 255, 255],
            vec![0, 0, 0, 1, b'x'],
            vec![0, 0, 0, 10, b'{'],
        ] {
            let mut reader = data.as_slice();
            assert_eq!(
                Reader::default().read(&mut reader).await,
                Err(Failure::Unavailable)
            );
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    Busy,
    Unavailable,
    Invalid,
    Timeout,
    Cancelled,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Laya unavailable")
    }
}
impl std::error::Error for Failure {}

// Frame state lives in the process, so cancelling a timeout/select future cannot
// discard a partially read prefix/payload and misattribute the next reply.
#[derive(Default)]
struct Reader {
    header: [u8; 4],
    at: usize,
    body: Vec<u8>,
    used: usize,
}
impl Reader {
    async fn read(
        &mut self,
        output: &mut (impl tokio::io::AsyncRead + Unpin),
    ) -> Result<Value, Failure> {
        while self.at < 4 {
            let count = output
                .read(self.header.get_mut(self.at..).ok_or(Failure::Unavailable)?)
                .await
                .map_err(|_| Failure::Unavailable)?;
            if count == 0 {
                return Err(Failure::Unavailable);
            }
            self.at += count;
        }
        let size =
            usize::try_from(u32::from_be_bytes(self.header)).map_err(|_| Failure::Unavailable)?;
        if size == 0 || size > BODY_CAP {
            return Err(Failure::Unavailable);
        }
        if self.body.is_empty() {
            self.body
                .try_reserve_exact(size)
                .map_err(|_| Failure::Unavailable)?;
            self.body.resize(size, 0);
        }
        while self.used < size {
            let count = output
                .read(self.body.get_mut(self.used..).ok_or(Failure::Unavailable)?)
                .await
                .map_err(|_| Failure::Unavailable)?;
            if count == 0 {
                return Err(Failure::Unavailable);
            }
            self.used += count;
        }
        let result = serde_json::from_slice(&self.body).map_err(|_| Failure::Unavailable);
        self.at = 0;
        self.used = 0;
        self.body.clear();
        result
    }
}
struct Process {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    reader: Reader,
    group: nix::unistd::Pid,
    killed: bool,
}
impl Process {
    #[cfg(target_os = "linux")]
    fn exited(&self) -> bool {
        use nix::sys::wait::{waitid, Id, WaitPidFlag, WaitStatus};
        // Observe exit without reaping: the process-group ID remains reserved
        // until owned descendants are signalled and child.wait() reaps it.
        !matches!(
            waitid(
                Id::Pid(self.group),
                WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT
            ),
            Ok(WaitStatus::StillAlive)
        )
    }
    #[cfg(not(target_os = "linux"))]
    fn exited(&self) -> bool {
        false
    }
    fn signal(&mut self) {
        if !self.killed {
            let _ = nix::sys::signal::killpg(self.group, nix::sys::signal::Signal::SIGKILL);
            self.killed = true;
        }
    }
    async fn stop(mut self) {
        self.signal();
        let _ = timeout(Duration::from_secs(4), self.child.wait()).await;
    }
    async fn start(config: &Config) -> Result<Self, Failure> {
        let mut child = Command::new(&config.python)
            .arg(&config.script)
            .arg("--engine-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| Failure::Unavailable)?;
        let group = nix::unistd::Pid::from_raw(
            child
                .id()
                .and_then(|n| i32::try_from(n).ok())
                .ok_or(Failure::Unavailable)?,
        );
        let input = child.stdin.take().ok_or(Failure::Unavailable)?;
        let output = child.stdout.take().ok_or(Failure::Unavailable)?;
        let mut process = Self {
            child,
            input,
            output,
            reader: Reader::default(),
            group,
            killed: false,
        };
        if timeout(config.startup, process.reader.read(&mut process.output)).await
            != Ok(Ok(json!({"ready":true})))
        {
            process.stop().await;
            return Err(Failure::Unavailable);
        }
        Ok(process)
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.signal();
    }
}
struct Request {
    body: Vec<u8>,
    end: Instant,
    reply: oneshot::Sender<Result<Value, Failure>>,
}
struct Slot {
    tx: mpsc::Sender<Request>,
    ready: Arc<AtomicBool>,
    late: Arc<AtomicBool>,
}
pub struct Pool {
    slots: Vec<Slot>,
    idle: std::sync::Mutex<Vec<usize>>,
    permits: Arc<Semaphore>,
    config: Config,
    stop: tokio::sync::watch::Sender<bool>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    fatal: Arc<AtomicBool>,
}
impl Pool {
    pub async fn start(config: Config) -> Result<Arc<Self>, Failure> {
        if !(1..=4).contains(&config.size) {
            return Err(Failure::Unavailable);
        }
        let (stop, _) = tokio::sync::watch::channel(false);
        let fatal = Arc::new(AtomicBool::new(false));
        let mut starters = tokio::task::JoinSet::new();
        for _ in 0..config.size {
            let c = config.clone();
            starters.spawn(async move { Process::start(&c).await });
        }
        let mut processes = Vec::new();
        while let Some(result) = starters.join_next().await {
            match result {
                Ok(Ok(p)) => processes.push(p),
                _ => {
                    starters.abort_all();
                    while starters.join_next().await.is_some() {}
                    for p in processes {
                        p.stop().await;
                    }
                    return Err(Failure::Unavailable);
                }
            }
        }
        let mut slots = Vec::new();
        let mut tasks = Vec::new();
        for process in processes {
            let (tx, rx) = mpsc::channel(1);
            let ready = Arc::new(AtomicBool::new(true));
            let late = Arc::new(AtomicBool::new(false));
            tasks.push(tokio::spawn(actor(
                process,
                rx,
                ready.clone(),
                late.clone(),
                stop.subscribe(),
                fatal.clone(),
                config.clone(),
            )));
            slots.push(Slot { tx, ready, late });
        }
        Ok(Arc::new(Self {
            idle: std::sync::Mutex::new((0..config.size).collect()),
            permits: Arc::new(Semaphore::new(config.size)),
            slots,
            config,
            stop,
            tasks: Mutex::new(tasks),
            fatal,
        }))
    }
    pub fn ready(&self) -> bool {
        !*self.stop.borrow() && self.slots.iter().any(|s| s.ready.load(Ordering::Acquire))
    }
    pub fn failed(&self) -> bool {
        self.fatal.load(Ordering::Acquire)
    }
    pub async fn stop(&self) {
        let _ = self.stop.send(true);
        self.permits.close();
        for task in self.tasks.lock().await.drain(..) {
            let _ = task.await;
        }
    }
    pub async fn decide(&self, body: Vec<u8>) -> Result<Value, Failure> {
        if !crate::validate(&body) {
            return Err(Failure::Invalid);
        }
        let permit = timeout(self.config.queue, self.permits.clone().acquire_owned())
            .await
            .map_err(|_| Failure::Busy)?
            .map_err(|_| Failure::Unavailable)?;
        // Admission and actor execution share one budget, including recovery
        // races and a full channel. Recovery cannot restart this deadline.
        let end = Instant::now() + self.config.decision;
        let index = {
            let mut idle = self.idle.lock().map_err(|_| Failure::Unavailable)?;
            let selected = idle
                .iter()
                .position(|n| {
                    self.slots.get(*n).is_some_and(|s| {
                        s.ready.load(Ordering::Acquire) && !s.late.load(Ordering::Acquire)
                    })
                })
                .or_else(|| {
                    idle.iter().position(|n| {
                        self.slots
                            .get(*n)
                            .is_some_and(|s| s.ready.load(Ordering::Acquire))
                    })
                })
                .unwrap_or(0);
            if selected >= idle.len() {
                return Err(Failure::Unavailable);
            }
            idle.remove(selected)
        };
        // Return the slot even if the HTTP caller drops this future. A cancelled
        // oneshot tells the actor to retain/discard unfinished work under grace.
        struct Lease<'a> {
            pool: &'a Pool,
            index: usize,
            _permit: tokio::sync::OwnedSemaphorePermit,
        }
        impl Drop for Lease<'_> {
            fn drop(&mut self) {
                if let Ok(mut idle) = self.pool.idle.lock() {
                    idle.push(self.index);
                } else {
                    self.pool.permits.close();
                }
            }
        }
        let _lease = Lease {
            pool: self,
            index,
            _permit: permit,
        };
        let slot = self.slots.get(index).ok_or(Failure::Unavailable)?;
        if !slot.ready.load(Ordering::Acquire) {
            return Err(Failure::Unavailable);
        }
        let (reply, answer) = oneshot::channel();
        timeout_at(end, slot.tx.send(Request { body, end, reply }))
            .await
            .map_err(|_| Failure::Timeout)?
            .map_err(|_| Failure::Unavailable)?;
        timeout_at(end, answer)
            .await
            .map_err(|_| Failure::Timeout)?
            .map_err(|_| Failure::Unavailable)?
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

async fn actor(
    mut process: Process,
    mut requests: mpsc::Receiver<Request>,
    ready: Arc<AtomicBool>,
    late: Arc<AtomicBool>,
    mut stop: tokio::sync::watch::Receiver<bool>,
    fatal: Arc<AtomicBool>,
    config: Config,
) {
    let mut abandoned: Option<Instant> = None;
    let mut health = tokio::time::interval(Duration::from_millis(50));
    loop {
        let deadline = abandoned.map(|at| at + config.late_grace);
        let next = tokio::select! {
            biased;
            _=stop.changed()=>break,
            _=health.tick()=> {if process.exited() {None} else {continue;}},
            response=process.reader.read(&mut process.output)=> {
                if abandoned.is_some() && response.is_ok(){abandoned=None;late.store(false,Ordering::Release);continue;}
                None
            },
            _=async {if let Some(at)=deadline {tokio::time::sleep_until(at).await;}else{std::future::pending::<()>().await;}}=>None,
            request=requests.recv()=>request,
        };
        let mut replace = false;
        if let Some(mut request) = next {
            if request.reply.is_closed() {
                continue;
            }
            let end = request.end;
            if Instant::now() >= end {
                let _ = request.reply.send(Err(Failure::Timeout));
                continue;
            }
            let result = async {
                if abandoned.is_some() {
                    if deadline.is_some_and(|d| Instant::now() > d) {
                        replace = true;
                        return Err(Failure::Unavailable);
                    }
                    match timeout_at(end, process.reader.read(&mut process.output)).await {
                        Ok(Ok(_)) => {
                            abandoned = None;
                            late.store(false, Ordering::Release);
                        }
                        Err(_) => return Err(Failure::Timeout),
                        _ => {
                            replace = true;
                            return Err(Failure::Unavailable);
                        }
                    }
                }
                if request.reply.is_closed() {
                    return Err(Failure::Cancelled);
                }
                if Instant::now() >= end {
                    return Err(Failure::Timeout);
                }
                let size = u32::try_from(request.body.len()).map_err(|_| Failure::Invalid)?;
                let write = async {
                    process.input.write_all(&size.to_be_bytes()).await?;
                    process.input.write_all(&request.body).await?;
                    process.input.flush().await
                };
                if !matches!(timeout_at(end, write).await, Ok(Ok(()))) {
                    replace = true;
                    return Err(Failure::Unavailable);
                }
                let reply = tokio::select! {
                    _=request.reply.closed()=>Err(Failure::Cancelled),
                    value=timeout_at(end,process.reader.read(&mut process.output))=>match value {Ok(v)=>v,Err(_)=>Err(Failure::Timeout)},
                };
                match reply {
                    Err(Failure::Timeout | Failure::Cancelled) => {
                        if config.late_grace.is_zero() {
                            replace = true;
                        } else {
                            abandoned = Some(Instant::now());
                            late.store(true, Ordering::Release);
                        }
                        reply
                    }
                    Ok(Value::Object(object))
                        if object.get("error")
                            == Some(&Value::String(
                                "Input exceeds model context budget".into(),
                            )) =>
                    {
                        Err(Failure::Invalid)
                    }
                    Ok(Value::Object(object)) if !object.contains_key("error") => {
                        Ok(Value::Object(object))
                    }
                    _ => {
                        replace = true;
                        Err(Failure::Unavailable)
                    }
                }
            };
            let result = tokio::select! {_ = stop.changed()=>break,result=result=>result};
            let _ = request.reply.send(result);
        } else {
            replace = true;
        }
        if replace {
            ready.store(false, Ordering::Release);
            late.store(false, Ordering::Release);
            abandoned = None;
            process.stop().await;
            let mut recovered = None;
            for delay in config.retries {
                tokio::select! {_=stop.changed()=>return,_=tokio::time::sleep(delay)=>()}
                let started = tokio::select! {_=stop.changed()=>return,started=Process::start(&config)=>started};
                if let Ok(worker) = started {
                    recovered = Some(worker);
                    break;
                }
            }
            if let Some(worker) = recovered {
                process = worker;
                ready.store(true, Ordering::Release);
            } else {
                fatal.store(true, Ordering::Release);
                requests.close();
                return;
            }
        }
    }
    ready.store(false, Ordering::Release);
    process.stop().await;
}
