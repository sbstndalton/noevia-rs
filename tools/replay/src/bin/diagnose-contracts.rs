//! Actual core/public-front/native service HTTP proof on owned synthetic roots only.
#![forbid(unsafe_code)]
use replay::http::{parse_response, Base, Response};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::{
        fd::AsFd,
        unix::{
            fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const TOKEN: &str = "synthetic-prompts-publicfront-token-0123456789abcdef";
const RUNTIME: &str = "d890ab476d1f1a8b6de799d6428e45b6af42a473";
fn qualification_path(key: &str, default: &str) -> PathBuf {
    std::env::var_os(key)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}
// AST-only immutable selected functions; no production application startup.
const SHIM: &str = r#"import ast,dataclasses,os,re,sys,types,importlib.metadata,json
from pathlib import Path
from typing import Any
from fastapi import FastAPI,APIRouter,HTTPException,Request
from fastapi.responses import Response
from docker.errors import DockerException
assert importlib.metadata.version('fastapi')=='0.115.6'
assert importlib.metadata.version('starlette')=='0.41.3'
assert importlib.metadata.version('pydantic')=='2.14.0'
p=json.loads(Path(os.environ['DIAGNOSE_ORACLE_SOURCE']).read_text())
def selected(source,names,scope):
    nodes=[n for n in ast.parse(source).body if isinstance(n,(ast.FunctionDef,ast.AsyncFunctionDef)) and n.name in names]
    assert {n.name for n in nodes}==set(names)
    exec(compile(ast.Module(body=nodes,type_ignores=[]),'immutable-selected.py','exec'),scope)
package=types.ModuleType('synthetic_app');package.__path__=[];sys.modules['synthetic_app']=package
diagnose=types.ModuleType('synthetic_app.diagnose');sys.modules[diagnose.__name__]=diagnose
exec(compile(p['diagnose'],'immutable-diagnose.py','exec'),diagnose.__dict__)
settings=types.SimpleNamespace(model_loader_token=os.environ['MODEL_LOADER_TOKEN'],llama_container_names=list(p['logs']))
def get(name):
    assert name in p['logs']
    def logs(**kwargs):
        assert set(kwargs)=={'since','tail'} and kwargs['tail']==40000 and kwargs['since'].isoformat()=='2026-10-11T00:00:00'
        return p['logs'][name].encode()
    return types.SimpleNamespace(attrs={'State':{'StartedAt':'2026-10-11T00:00:00.123456789Z'}},logs=logs)
def listing(**kwargs):
    assert kwargs=={'all':True}
    return []
scope={'settings':settings,'DockerException':DockerException,'_docker_client':lambda:types.SimpleNamespace(containers=types.SimpleNamespace(list=listing,get=get))}
selected(p['services'],['discover_llama_containers','_effective_container_names'],scope)
services=types.SimpleNamespace(**scope)
app=FastAPI(title='Model Loader');router=APIRouter(prefix='/api/v1')
api={'__package__':'synthetic_app','router':router,'services':services,'HTTPException':HTTPException,'dataclasses':dataclasses,'Path':Path,'Any':Any,'re':re}
secret=[n for n in ast.parse(p['api']).body if isinstance(n,ast.Assign) and any(isinstance(t,ast.Name) and t.id=='_SECRET_PATTERNS' for t in n.targets)]
assert len(secret)==1
exec(compile(ast.Module(body=secret,type_ignores=[]),'immutable-secret-patterns.py','exec'),api)
selected(p['api'],['_plain','_logs_since_start','redact_log_line','backend_diagnose','health'],api)
middleware={'app':app,'settings':settings,'os':os,'Request':Request,'Response':Response}
selected(p['main'],['_require_token'],middleware)
# Qualification-only refusal until the separately authorized services follow-on.
@app.middleware('http')
async def synthetic_native_diagnose_guard(request,call_next):
    token=settings.model_loader_token
    import hmac
    if request.url.path!='/api/v1/health' and not hmac.compare_digest(request.headers.get('x-model-loader-token','').encode(),token.encode()):
        return Response('{"error":"unauthorized"}',status_code=401,media_type='application/json')
    if os.environ.get('NOEVIA_NATIVE_MODEL_MANAGER_DIAGNOSE_CONFIRMED')=='1' and request.url.path.startswith('/api/v1/backends/') and request.url.path.endswith('/diagnose'):
        return Response('{"detail":"Native model manager owns this endpoint"}',status_code=503,media_type='application/json')
    return await call_next(request)
app.include_router(router)
Path(os.environ['DIAGNOSE_CHILD_MARKER']).write_text(os.environ.get('NOEVIA_NATIVE_MODEL_MANAGER_DIAGNOSE_CONFIRMED','absent'))
import uvicorn
uvicorn.run(app,host=sys.argv[3],port=int(sys.argv[5]),log_level='error')
"#;
struct FakeDocker {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<thread::JoinHandle<std::result::Result<(), String>>>,
}
impl FakeDocker {
    fn new(socket: &Path, logs: BTreeMap<String, Vec<u8>>) -> Result<Self> {
        let listener = std::os::unix::net::UnixListener::bind(socket)?;
        listener.set_nonblocking(true)?;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(90);
            let mut count = 0;
            while !stopping.load(std::sync::atomic::Ordering::Acquire) && Instant::now() < deadline
            {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => return Err(e.to_string()),
                };
                count += 1;
                if count > 256 {
                    return Err("fake Docker request cap".into());
                }
                stream
                    .set_read_timeout(Some(Duration::from_millis(500)))
                    .map_err(|e| e.to_string())?;
                stream
                    .set_write_timeout(Some(Duration::from_millis(500)))
                    .map_err(|e| e.to_string())?;
                let mut raw = Vec::new();
                while !raw.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read(&mut byte).map_err(|e| e.to_string())? == 0 {
                        return Err("fake Docker request EOF".into());
                    }
                    if raw.len() >= 8192 {
                        return Err("fake Docker request header cap".into());
                    }
                    raw.push(byte[0]);
                }
                let request = std::str::from_utf8(&raw)
                    .map_err(|e| e.to_string())?
                    .lines()
                    .next()
                    .ok_or("request line")?;
                let body = if request
                    == "GET /containers/json?all=true&limit=1025&size=false HTTP/1.1"
                {
                    b"[]".to_vec()
                } else {
                    let mut body = None;
                    for (name, log) in &logs {
                        let id = format!("synthetic-id-{name}");
                        if request == format!("GET /containers/{name}/json HTTP/1.1")
                            || request == format!("GET /containers/{id}/json HTTP/1.1")
                        {
                            body=Some(serde_json::to_vec(&json!({"Id":id,"Name":format!("/{name}"),"Image":"synthetic-image","State":{"StartedAt":"2026-10-11T00:00:00.123456789Z"},"Config":{"Tty":true}})).map_err(|e|e.to_string())?);
                            break;
                        }
                        if request==format!("GET /containers/{id}/logs?stdout=true&stderr=true&timestamps=false&follow=false&tail=40000&since=1791676800 HTTP/1.1") {body=Some(log.clone());break;}
                    }
                    body.ok_or_else(|| format!("unexpected fixed fake Docker request: {request}"))?
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .map_err(|e| e.to_string())?;
                stream.write_all(&body).map_err(|e| e.to_string())?;
            }
            Ok(())
        });
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| "fake Docker worker panic")?
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        }
        Ok(())
    }
}
impl Drop for FakeDocker {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}
#[path = "support/prompts_dependencies.rs"]
mod dependencies;
#[path = "support/diagnose_dependencies.rs"]
mod owned_dependencies;
#[allow(dead_code)]
#[path = "../../../qualification/process.rs"]
mod process;
fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").ok_or("PATH required")?)
        .arg("-C")
        .arg(root)
        .args(args);
    let result = process::bounded(command, Vec::new(), Duration::from_secs(5))?;
    require(result.status.success(), "source provenance git failure")?;
    Ok(result.stdout)
}
fn sources(root: &Path, revision: &str, paths: &[&str]) -> Result<Value> {
    require(
        String::from_utf8(git(root, &["rev-parse", "HEAD"])?)?.trim() == revision,
        "actual source revision mismatch",
    )?;
    git(root, &["diff", "--quiet", revision, "--"])?;
    require(
        git(root, &["ls-files", "--others", "--exclude-standard"])?.is_empty(),
        "untracked source refused",
    )?;
    let mut values = serde_json::Map::new();
    for path in paths {
        let mut actual = Vec::new();
        fs::File::open(root.join(path))?
            .take(1_048_577)
            .read_to_end(&mut actual)?;
        require(actual.len() <= 1_048_576, "source file cap")?;
        let spec = format!("{revision}:{path}");
        require(
            actual == git(root, &["show", &spec])?,
            "source file differs from immutable revision",
        )?;
        values.insert((*path).into(), json!({"blob":String::from_utf8(git(root, &["rev-parse", &spec])?)?.trim(),"sha256":format!("{:x}",Sha256::digest(&actual))}));
    }
    Ok(
        json!({"tree":String::from_utf8(git(root, &["rev-parse", &format!("{revision}^{{tree}}")])?)?.trim(),"tracked_source_clean":true,"files":values}),
    )
}
fn require(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn artifact(path: &Path, value: &Value) -> Result<()> {
    struct Bounded(Vec<u8>);
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > 1_048_576_usize.saturating_sub(self.0.len()) {
                return Err(std::io::Error::other("qualification artifact cap"));
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
    let mut output = Bounded(Vec::new());
    serde_json::to_writer_pretty(&mut output, value)?;
    fs::write(path, output.0)?;
    Ok(())
}
fn record(response: &Response, method: &str, path: &str, body: &[u8]) -> Result<Value> {
    Ok(
        json!({"request":{"method":method,"path":path,"body_bytes":body.len(),"body_sha256":format!("{:x}",Sha256::digest(body))},"response":normalized(response)?}),
    )
}
fn nonblocking(pipe: &impl AsFd) -> Result<()> {
    let flags = rustix::fs::fcntl_getfl(pipe)?;
    rustix::fs::fcntl_setfl(pipe, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}
#[derive(Clone, Copy)]
enum OwnedRole {
    RustManager,
    PythonFixture,
    NodeLauncher,
    RustFront,
    #[cfg(test)]
    ClosedControl,
}
impl OwnedRole {
    fn name(self) -> &'static str {
        match self {
            Self::RustManager => "rust-manager",
            Self::PythonFixture => "python-fixture",
            Self::NodeLauncher => "node-launcher",
            Self::RustFront => "rust-front",
            #[cfg(test)]
            Self::ClosedControl => "closed-control",
        }
    }
}
#[derive(Clone, Copy)]
enum RunMode {
    Python,
    Rust,
    RustStartup,
    RustOff,
    PromptsAbsent,
    PromptsWrongToken,
    PromptsPortOmitted,
    PublicInvalid,
    #[cfg(test)]
    ClosedControl,
}
impl RunMode {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "python" => Ok(Self::Python),
            "rust" => Ok(Self::Rust),
            "rust-startup" => Ok(Self::RustStartup),
            "rust-off" => Ok(Self::RustOff),
            "prompts-absent" => Ok(Self::PromptsAbsent),
            "prompts-wrong-token" => Ok(Self::PromptsWrongToken),
            "prompts-port-omitted" => Ok(Self::PromptsPortOmitted),
            "public-invalid" => Ok(Self::PublicInvalid),
            _ => Err("unknown fixed qualification mode".into()),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::Rust => "rust",
            Self::RustStartup => "rust-startup",
            Self::RustOff => "rust-off",
            Self::PromptsAbsent => "prompts-absent",
            Self::PromptsWrongToken => "prompts-wrong-token",
            Self::PromptsPortOmitted => "prompts-port-omitted",
            Self::PublicInvalid => "public-invalid",
            #[cfg(test)]
            Self::ClosedControl => "closed-control",
        }
    }
}
#[derive(Clone, Copy, Default)]
struct ExitObservation {
    code: Option<i32>,
    signal: Option<i32>,
    exited: bool,
    killed: bool,
    dumped: bool,
}
impl ExitObservation {
    fn from_status(status: &rustix::process::WaitIdStatus) -> Self {
        Self {
            code: status.exit_status(),
            signal: status.terminating_signal(),
            exited: status.exited(),
            killed: status.killed(),
            dumped: status.dumped(),
        }
    }
    fn accepts(self) -> bool {
        self.exited && self.code == Some(0) && self.signal.is_none() && !self.killed && !self.dumped
    }
    fn accepts_for(self, role: OwnedRole, mode: RunMode, owned_term_sent: bool) -> bool {
        if self.accepts() {
            return true;
        }
        // Only the direct qualification shim re-raises our successfully sent TERM.
        let direct_fixture = matches!(role, OwnedRole::PythonFixture)
            && matches!(
                mode,
                RunMode::Python
                    | RunMode::PromptsAbsent
                    | RunMode::PromptsWrongToken
                    | RunMode::PromptsPortOmitted
                    | RunMode::PublicInvalid
            );
        direct_fixture
            && owned_term_sent
            && !self.exited
            && self.code.is_none()
            && self.signal == Some(15)
            && self.killed
            && !self.dumped
    }
    fn matches_reaped(self, status: std::process::ExitStatus) -> bool {
        use std::os::unix::process::ExitStatusExt;
        if self.accepts() {
            status.success()
                && status.code() == self.code
                && status.signal().is_none()
                && !status.core_dumped()
        } else {
            !self.exited
                && self.code.is_none()
                && self.killed
                && !self.dumped
                && status.code().is_none()
                && status.signal() == self.signal
                && !status.core_dumped()
        }
    }
}
#[derive(Clone, Copy)]
enum ShutdownStage {
    Signal,
    Observe,
    ExitStatus,
    Deadline,
    Output,
    Wait,
    Retention,
}
impl ShutdownStage {
    fn name(self) -> &'static str {
        match self {
            Self::Signal => "signal-request",
            Self::Observe => "status-observation",
            Self::ExitStatus => "exit-status",
            Self::Deadline => "deadline",
            Self::Output => "output",
            Self::Wait => "actual-wait",
            Self::Retention => "private-retention",
        }
    }
}
fn shutdown_summary(
    role: OwnedRole,
    mode: RunMode,
    stage: ShutdownStage,
    observation: ExitObservation,
    out: usize,
    stdout_digest: &Sha256,
    stderr: &[u8],
) -> String {
    let code = observation
        .code
        .map(|n| n.to_string())
        .unwrap_or_else(|| "none".into());
    let signal = observation
        .signal
        .map(|n| n.to_string())
        .unwrap_or_else(|| "none".into());
    format!("owned shutdown refused: role={} mode={} stage={} code={code} signal={signal} exited={} killed={} dumped={} stdout_bytes={out} stdout_sha256={:x} stderr_bytes={} stderr_sha256={:x}", role.name(),mode.name(),stage.name(),observation.exited,observation.killed,observation.dumped,stdout_digest.clone().finalize(),stderr.len(),Sha256::digest(stderr))
}
struct Owned {
    role: OwnedRole,
    mode: RunMode,
    stdout_digest: Sha256,
    leader: Leader,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    out: usize,
    err: usize,
    diagnostic: Vec<u8>,
}
fn diagnostic_summary(raw: &[u8]) -> String {
    let diagnostic = String::from_utf8_lossy(raw);
    let kind = if diagnostic
        .lines()
        .any(|line| line.starts_with("ModuleNotFoundError:"))
    {
        "module-unavailable"
    } else if diagnostic.lines().any(|line| line.starts_with("Error:")) {
        "child-error"
    } else if diagnostic
        .lines()
        .any(|line| line.starts_with("corpus-launcher:"))
    {
        "launcher-error"
    } else {
        "child-exited"
    };
    format!("owned synthetic server exited before qualification: kind={kind} stderr_bytes={} stderr_sha256={:x}", raw.len(), Sha256::digest(raw))
}
fn retain_private_diagnostic(raw: &[u8]) -> Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ordinal = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "s1-diagnose-child-diagnostic-{}-{ordinal}",
        std::process::id()
    ));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .map_err(|_| "private child diagnostic directory unavailable")?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join("stderr.raw"))
        .map_err(|_| "private child diagnostic file unavailable")?;
    file.write_all(raw)
        .map_err(|_| "private child diagnostic retention failed")?;
    Ok(())
}
impl Owned {
    fn spawn(mut command: Command, role: OwnedRole, mode: RunMode) -> Result<Self> {
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut leader = Leader {
            child: command.spawn()?,
            reaped: false,
        };
        let stdout = leader.child.stdout.take().ok_or("child stdout absent")?;
        let stderr = leader.child.stderr.take().ok_or("child stderr absent")?;
        let owned = Self {
            role,
            mode,
            stdout_digest: Sha256::new(),
            leader,
            stdout,
            stderr,
            out: 0,
            err: 0,
            diagnostic: Vec::new(),
        };
        nonblocking(&owned.stdout)?;
        nonblocking(&owned.stderr)?;
        Ok(owned)
    }
    fn drain_pair(&mut self) -> Result<()> {
        fn step(
            pipe: &mut impl Read,
            count: &mut usize,
            bytes: Option<&mut Vec<u8>>,
            digest: Option<&mut Sha256>,
        ) -> Result<()> {
            let mut chunk = [0; 4096];
            match pipe.read(&mut chunk) {
                Ok(n) => {
                    require(n <= 65_536usize.saturating_sub(*count), "child output cap")?;
                    let value = chunk.get(..n).ok_or("diagnostic count")?;
                    if let Some(bytes) = bytes {
                        require(
                            n <= 65_536usize.saturating_sub(bytes.len()),
                            "child diagnostic cap",
                        )?;
                        bytes.try_reserve(n)?;
                        bytes.extend_from_slice(value);
                    }
                    if let Some(digest) = digest {
                        digest.update(value);
                    }
                    *count += n;
                    Ok(())
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    Ok(())
                }
                Err(e) => Err(e.into()),
            }
        }
        // One bounded read per pipe per round; stderr is never starved by stdout.
        let stdout = step(
            &mut self.stdout,
            &mut self.out,
            None,
            Some(&mut self.stdout_digest),
        );
        let stderr = step(
            &mut self.stderr,
            &mut self.err,
            Some(&mut self.diagnostic),
            None,
        );
        stdout?;
        stderr?;
        Ok(())
    }
    fn shutdown_refusal(
        &mut self,
        mut stage: ShutdownStage,
        observation: ExitObservation,
        deadline: Instant,
    ) -> Box<dyn std::error::Error> {
        // Drain only currently available bytes; remote EOF and kernel waits are not deadlines.
        for _ in 0..16 {
            if Instant::now() >= deadline {
                break;
            }
            if self.drain_pair().is_err() {
                stage = ShutdownStage::Output;
                break;
            }
        }
        if retain_private_diagnostic(&self.diagnostic).is_err() {
            stage = ShutdownStage::Retention;
        }
        shutdown_summary(
            self.role,
            self.mode,
            stage,
            observation,
            self.out,
            &self.stdout_digest,
            &self.diagnostic,
        )
        .into()
    }
    fn finish(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let pending = ExitObservation::default();
        let pid = self.pid()?;
        if rustix::process::kill_process(pid, rustix::process::Signal::TERM).is_err() {
            return Err(self.shutdown_refusal(ShutdownStage::Signal, pending, deadline));
        }
        loop {
            use rustix::process::{waitid, WaitId, WaitIdOptions};
            let observed = match waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            ) {
                Ok(value) => value,
                Err(_) => {
                    return Err(self.shutdown_refusal(ShutdownStage::Observe, pending, deadline))
                }
            };
            if let Some(status) = observed {
                let observation = ExitObservation::from_status(&status);
                // Failure retains numeric identity until fallback group signal precedes wait.
                if !observation.accepts_for(self.role, self.mode, true) {
                    return Err(self.shutdown_refusal(
                        ShutdownStage::ExitStatus,
                        observation,
                        deadline,
                    ));
                }
                let status = match self.leader.child.wait() {
                    Ok(status) => status,
                    Err(_) => {
                        return Err(self.shutdown_refusal(
                            ShutdownStage::Wait,
                            observation,
                            deadline,
                        ))
                    }
                };
                self.leader.reaped = true;
                if !observation.matches_reaped(status) {
                    return Err(self.shutdown_refusal(ShutdownStage::Wait, observation, deadline));
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(self.shutdown_refusal(ShutdownStage::Deadline, pending, deadline));
            }
            if self.drain_pair().is_err() {
                return Err(self.shutdown_refusal(ShutdownStage::Output, pending, deadline));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn pid(&self) -> Result<rustix::process::Pid> {
        i32::try_from(self.leader.child.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .ok_or("child identity".into())
    }
    fn pump(&mut self) -> Result<()> {
        self.drain_pair()?;
        use rustix::process::{waitid, WaitId, WaitIdOptions};
        if waitid(
            WaitId::Pid(self.pid()?),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )?
        .is_some()
        {
            // Fair bounded drain after exit: one read before NOWAIT may precede
            // the final diagnostic. Each stream still has its independent cap.
            for _ in 0..16 {
                self.drain_pair()?;
            }
            retain_private_diagnostic(&self.diagnostic)?;
            return Err(diagnostic_summary(&self.diagnostic).into());
        }
        Ok(())
    }
}
struct Leader {
    child: Child,
    reaped: bool,
}
impl Leader {
    fn stop(&mut self) -> std::io::Result<()> {
        if self.reaped {
            return Ok(());
        }
        if let Some(pid) = i32::try_from(self.child.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.child.kill();
        self.child.wait()?;
        self.reaped = true;
        Ok(())
    }
}
impl Drop for Leader {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn request(
    base: &Base,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<Response> {
    require(
        base.host == "127.0.0.1"
            && method.len() <= 16
            && method.bytes().all(|byte| byte.is_ascii_uppercase())
            && path.len() <= 8192
            && body.len() <= 1_048_577
            && headers.len() <= 32
            && headers
                .iter()
                .all(|(key, value)| key.len() <= 256 && value.len() <= 8192)
            && headers
                .iter()
                .map(|(key, value)| key.len() + value.len())
                .sum::<usize>()
                <= 16_384
            && !path.contains(['\r', '\n', ' ']),
        "fixed loopback HTTP target",
    )?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut message = Vec::new();
    message.try_reserve(1_081_345)?;
    write!(message,"{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Length: {}\r\n",base.port,body.len())?;
    for (key, value) in headers {
        require(
            !key.contains(['\r', '\n', ':']) && !value.contains(['\r', '\n']),
            "HTTP header shape",
        )?;
        write!(message, "{key}: {value}\r\n")?;
    }
    message.extend_from_slice(b"\r\n");
    message.extend_from_slice(body);
    require(message.len() <= 1_081_345, "complete HTTP request cap")?;
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], base.port)),
        Duration::from_millis(500),
    )?;
    send(&mut stream, &message, deadline)?;
    let mut raw = Vec::new();
    loop {
        stream.set_read_timeout(Some(
            deadline
                .checked_duration_since(Instant::now())
                .ok_or("HTTP deadline")?,
        ))?;
        let mut chunk = [0; 4096];
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                require(raw.len() + n <= 131_072, "synthetic HTTP response cap")?;
                raw.try_reserve(n)?;
                raw.extend_from_slice(chunk.get(..n).ok_or("HTTP count")?);
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(e) => return Err(e.into()),
        }
    }
    parse_response(&raw, method == "HEAD").map_err(Into::into)
}
trait SendStep {
    fn send_step(&mut self, bytes: &[u8], remaining: Duration) -> std::io::Result<usize>;
}
impl SendStep for TcpStream {
    fn send_step(&mut self, bytes: &[u8], remaining: Duration) -> std::io::Result<usize> {
        self.set_write_timeout(Some(remaining))?;
        self.write(bytes)
    }
}
fn send(writer: &mut impl SendStep, bytes: &[u8], deadline: Instant) -> Result<()> {
    let mut sent = 0;
    while sent < bytes.len() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or("HTTP send deadline")?;
        let end = sent.saturating_add(4096).min(bytes.len());
        match writer.send_step(bytes.get(sent..end).ok_or("HTTP send range")?, remaining) {
            Ok(0) => return Err("HTTP send made no progress".into()),
            Ok(count) if count <= end - sent => sent += count,
            Ok(_) => return Err("HTTP send count".into()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ) =>
            {
                return Ok(())
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
#[derive(Default)]
struct Client {
    cookies: BTreeMap<String, String>,
}
impl Client {
    fn call(
        &mut self,
        base: &Base,
        method: &str,
        path: &str,
        body: &[u8],
        csrf: bool,
    ) -> Result<Response> {
        let mut headers = vec![
            ("Origin".into(), base.origin()),
            ("Content-Type".into(), "application/json".into()),
        ];
        if !self.cookies.is_empty() {
            headers.push((
                "Cookie".into(),
                self.cookies
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        if csrf {
            if let Some(value) = self.cookies.get("cowork_csrf") {
                headers.push(("X-CSRF-Token".into(), value.clone()));
            }
        }
        let response = request(base, method, path, &headers, body)?;
        for (_, value) in response
            .headers
            .iter()
            .filter(|(key, _)| key == "set-cookie")
        {
            let pair = value.split(';').next().ok_or("cookie pair")?;
            let (key, value) = pair.split_once('=').ok_or("cookie separator")?;
            require(
                key.len() <= 256
                    && value.len() <= 4096
                    && (self.cookies.contains_key(key) || self.cookies.len() < 16),
                "cookie cap",
            )?;
            self.cookies.insert(key.to_owned(), value.to_owned());
        }
        Ok(response)
    }
}
fn normalized(response: &Response) -> Result<Value> {
    let body = serde_json::from_slice::<Value>(&response.body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&response.body).into_owned()));
    let mut headers = serde_json::Map::new();
    for (name, value) in &response.headers {
        if replay::diff::VOLATILE_HEADERS.contains(&name.as_str()) {
            continue;
        }
        match headers.get_mut(name) {
            None => {
                headers.insert(name.clone(), json!(value));
            }
            Some(Value::String(previous)) => {
                let first = previous.clone();
                *headers.get_mut(name).ok_or("header entry")? = json!([first, value]);
            }
            Some(Value::Array(values)) => values.push(json!(value)),
            Some(_) => return Err("header value shape".into()),
        }
    }
    Ok(json!({"status":response.status,"body":body,"headers":headers}))
}
#[derive(Clone, Copy)]
enum Readiness {
    CoreOrFront,
    Manager,
}
impl Readiness {
    fn path(self) -> &'static str {
        match self {
            Self::CoreOrFront => "/api/ready",
            Self::Manager => "/api/v1/health",
        }
    }
    fn accepts(self, response: &Response) -> bool {
        response.status == 200
            && match self {
                Self::CoreOrFront => true,
                Self::Manager => serde_json::from_slice::<Value>(&response.body)
                    .is_ok_and(|body| body == json!({"ok":true})),
            }
    }
}
fn ready(base: &Base, children: &mut [&mut Owned], readiness: Readiness) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    ready_until(base, children, readiness, deadline)
}
fn ready_until(
    base: &Base,
    children: &mut [&mut Owned],
    readiness: Readiness,
    deadline: Instant,
) -> Result<()> {
    loop {
        require(
            Instant::now() < deadline,
            "actual startup readiness deadline",
        )?;
        for child in children.iter_mut() {
            child.pump()?;
        }
        require(
            Instant::now() < deadline,
            "actual startup readiness deadline",
        )?;
        let response = request(base, "GET", readiness.path(), &[], &[]);
        require(
            Instant::now() < deadline,
            "actual startup readiness deadline",
        )?;
        if response.is_ok_and(|response| readiness.accepts(&response)) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
}
fn session(base: &Base, data: &Path) -> Result<(Client, Client)> {
    let mut bytes = Vec::new();
    fs::File::open(data.join("first-run-setup-code"))?
        .take(257)
        .read_to_end(&mut bytes)?;
    require(bytes.len() <= 256, "synthetic setup code cap")?;
    let setup = String::from_utf8(bytes)?;
    let mut admin = Client::default();
    let body = serde_json::to_vec(
        &json!({"setupCode":setup.trim(),"publicOrigin":base.origin(),"username":"synthetic-prompts-admin","displayName":"Synthetic Admin","password":"synthetic-prompts-password-0000001","diaryEnabled":false}),
    )?;
    require(
        admin
            .call(base, "POST", "/api/setup/complete", &body, true)?
            .status
            == 201,
        "actual admin setup",
    )?;
    let invitation = admin.call(
        base,
        "POST",
        "/api/admin/invitations",
        br#"{"role":"member"}"#,
        true,
    )?;
    require(invitation.status == 201, "actual member invitation")?;
    let invite: Value = serde_json::from_slice(&invitation.body)?;
    let token = invite
        .get("token")
        .and_then(Value::as_str)
        .ok_or("synthetic invitation token")?;
    let mut member = Client::default();
    let body = serde_json::to_vec(
        &json!({"token":token,"username":"synthetic-prompts-member","displayName":"Synthetic Member","password":"synthetic-prompts-password-0000002","diaryEnabled":false}),
    )?;
    require(
        member
            .call(base, "POST", "/api/auth/invitations/accept", &body, true)?
            .status
            == 201,
        "actual member acceptance",
    )?;
    Ok((admin, member))
}
fn copy_dependencies() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    require(
        args.len() == 1 && args.first().is_some_and(|v| v == "--copy-dependencies"),
        "qualification mode required",
    )?;
    let core = Path::new("/private/tmp/sol-routing-core010735-oracle");
    let revision = "010735c7d86d30269ab3f4a351b2135faea1c8e7";
    let actual_revision = String::from_utf8(git(core, &["rev-parse", "HEAD"])?)?;
    require(
        actual_revision.trim() == revision,
        "immutable core revision mismatch",
    )?;
    let expected = [
        git(core, &["show", &format!("{revision}:server/package.json")])?,
        git(
            core,
            &["show", &format!("{revision}:server/package-lock.json")],
        )?,
    ];
    let proof = owned_dependencies::copy(
        Path::new("/private/tmp/m6-reasoning-node22-deps"),
        Path::new("/tmp/s1-diagnose-node-dependencies"),
        &expected,
    )?;
    artifact(Path::new("/tmp/s1-diagnose-dependency-copy.json"), &proof)?;
    Ok(())
}
fn probe_error(stage: &'static str, error: rustix::io::Errno) -> std::io::Error {
    let kind = std::io::Error::from(error).kind();
    std::io::Error::new(
        kind,
        format!(
            "numeric loopback probe stage={stage} errno={}",
            error.raw_os_error()
        ),
    )
}
fn reusable_probe(port: u16) -> std::io::Result<TcpListener> {
    use rustix::net::{ipproto, socket, AddressFamily, SocketType};
    let fd = socket(AddressFamily::INET, SocketType::STREAM, Some(ipproto::TCP))
        .map_err(|e| probe_error("socket", e))?;
    rustix::net::sockopt::set_socket_reuseaddr(&fd, true)
        .map_err(|e| probe_error("reuseaddr", e))?;
    rustix::net::bind(
        &fd,
        &std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, port),
    )
    .map_err(|e| probe_error("bind", e))?;
    rustix::net::listen(&fd, 1).map_err(|e| probe_error("listen", e))?;
    Ok(TcpListener::from(fd))
}
fn run_mode(core: &Path, bins: &Path, mode: &str, oracle: &Value) -> Result<Vec<Value>> {
    let fixed_mode = RunMode::parse(mode)?;
    let owned = tempfile::Builder::new()
        .prefix("s1-diagnose-public-")
        .tempdir_in("/tmp")?;
    let root = owned.path();
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    for dir in ["bin", "models", "mm-data", "ui-data"] {
        fs::create_dir(root.join(dir))?;
    }
    fs::write(root.join("models/models.ini"), "version = 1\n")?;
    artifact(&root.join("oracle.json"), oracle)?;
    let python = qualification_path("PYTHON", "/tmp/s1-prompts-native-venv/bin/python");
    require(
        python.is_absolute()
            && python
                .to_str()
                .is_some_and(|s| !s.contains(['\n', '\r', ' '])),
        "qualification Python must be absolute shebang-safe path",
    )?;
    let shim = format!("#!{}\n{SHIM}", python.display());
    fs::write(root.join("bin/uvicorn"), shim)?;
    fs::set_permissions(root.join("bin/uvicorn"), fs::Permissions::from_mode(0o700))?;
    let logs = oracle
        .get("logs")
        .and_then(Value::as_object)
        .ok_or("fixed synthetic logs required")?;
    require(
        !logs.is_empty() && logs.len() <= 8,
        "fixed synthetic log inventory cap",
    )?;
    let mut fixed = BTreeMap::new();
    for (name, log) in logs {
        require(
            name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "fixed fixture name",
        )?;
        let text = log.as_str().ok_or("fixed log text")?;
        require(text.len() <= 65536, "fixed fixture log cap")?;
        fixed.insert(name.clone(), text.as_bytes().to_vec());
    }
    let mut fake = FakeDocker::new(&root.join("docker.sock"), fixed)?;
    let guard_a = reusable_probe(8090).map_err(|_| "fixed native port 8090 unavailable")?;
    let guard_b = reusable_probe(8091).map_err(|_| "fixed legacy port 8091 unavailable")?;
    let mut manager = if mode.starts_with("rust") {
        Command::new(bins.join("noevia-model-manager"))
    } else {
        Command::new(&python)
    };
    manager
        .env_clear()
        .current_dir(root)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .env("TZ", "UTC")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("DATA_DIR", root.join("mm-data"))
        .env("MODELS_DIR", root.join("models"))
        .env("MODELS_INI_PATH", root.join("models/models.ini"))
        .env("MODEL_LOADER_TOKEN", TOKEN)
        .env(
            "LLAMA_CONTAINERS",
            logs.keys().cloned().collect::<Vec<_>>().join(","),
        )
        .env("DIAGNOSE_ORACLE_SOURCE", root.join("oracle.json"))
        .env("DIAGNOSE_CHILD_MARKER", root.join("child-confirmation"))
        .env(
            "DOCKER_HOST",
            format!("unix://{}", root.join("docker.sock").display()),
        );
    if mode.starts_with("rust") {
        manager.env("MODEL_MANAGER_IMPL", "rust").env(
            "MODEL_MANAGER_DIAGNOSE_IMPL",
            if mode == "rust-off" { "python" } else { "rust" },
        );
    } else {
        manager.arg("-B").arg(root.join("bin/uvicorn")).args([
            "app.main:app",
            "--host",
            "127.0.0.1",
            "--port",
            "8090",
        ]);
    }
    drop(guard_a);
    drop(guard_b);
    let mut manager = Owned::spawn(
        manager,
        if mode.starts_with("rust") {
            OwnedRole::RustManager
        } else {
            OwnedRole::PythonFixture
        },
        fixed_mode,
    )?;
    let manager_base = Base {
        host: "127.0.0.1".into(),
        port: 8090,
    };
    ready(&manager_base, &mut [&mut manager], Readiness::Manager)?;
    require(
        request(
            &manager_base,
            "GET",
            "/api/v1/backends/synthetic-only/diagnose",
            &[("X-Model-Loader-Token".into(), "wrong".into())],
            &[],
        )?
        .status
            == 401,
        "direct wrong-token readiness gate",
    )?;
    if mode == "rust-startup" || mode == "rust-off" {
        let child = Base {
            host: "127.0.0.1".into(),
            port: 8091,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            manager.pump()?;
            require(Instant::now() < deadline, "owned child startup deadline")?;
            if request(
                &child,
                "GET",
                "/api/v1/backends/synthetic-only/diagnose",
                &[("X-Model-Loader-Token".into(), "wrong".into())],
                &[],
            )
            .is_ok_and(|response| response.status == 401)
            {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let response = request(
            &child,
            "GET",
            "/api/v1/backends/synthetic-only/diagnose",
            &[("X-Model-Loader-Token".into(), TOKEN.into())],
            &[],
        )?;
        let marker = dependencies::manifest(&root.join("child-confirmation"))?;
        let selected = mode != "rust-off";
        let rows = vec![
            json!({"mode":mode,"child_confirmation":String::from_utf8(marker.clone())?,"direct_child":normalized(&response)?}),
        ];
        artifact(
            &Path::new("/tmp").join(format!("s1-diagnose-startup-{mode}.json")),
            &json!({"rows":rows,"test_only_guard":true}),
        )?;

        require(
            marker == if selected { b"1" } else { b"0" },
            "explicit child diagnosis confirmation",
        )?;
        require(
            response.status == if selected { 503 } else { 200 },
            "owned child diagnosis selection/refusal",
        )?;
        manager.finish()?;
        fake.finish()?;
        return Ok(rows);
    }
    let core_listener = TcpListener::bind("127.0.0.1:0")?;
    let front_listener = TcpListener::bind("127.0.0.1:0")?;
    let core_port = core_listener.local_addr()?.port();
    let base = Base {
        host: "127.0.0.1".into(),
        port: front_listener.local_addr()?.port(),
    };
    drop(core_listener);
    let mut node = Command::new(bins.join("corpus-launcher"));
    node.env_clear()
        .env("PATH", std::env::var_os("PATH").ok_or("PATH required")?)
        .env(
            "NOEVIA_PROMPTS_DEPENDENCIES",
            qualification_path(
                "NOEVIA_DIAGNOSE_DEPENDENCIES",
                "/tmp/s1-diagnose-node-dependencies",
            ),
        )
        .args([if mode.starts_with("prompts-") {
            mode
        } else {
            "prompts-serve"
        }])
        .arg(core)
        .arg("8090")
        .env("UI_PORT", core_port.to_string())
        .env("UI_DATA_DIR", root.join("ui-data"))
        .env("PUBLIC_ORIGIN", base.origin())
        .env("NOEVIA_RUST_AUTH", "1")
        .env("NOEVIA_RUST_AUTH_CONFIRMED", "1");
    let mut node = Owned::spawn(node, OwnedRole::NodeLauncher, fixed_mode)?;
    ready(
        &Base {
            host: "127.0.0.1".into(),
            port: core_port,
        },
        &mut [&mut manager, &mut node],
        Readiness::CoreOrFront,
    )?;
    drop(front_listener);
    let mut front = Command::new(bins.join("noevia-server"));
    front
        .env_clear()
        .env("UI_HOST", "127.0.0.1")
        .env("UI_PORT", base.port.to_string())
        .env("UI_DATA_DIR", root.join("ui-data"))
        .env("PUBLIC_ORIGIN", base.origin())
        .env("NOEVIA_RUST_AUTH", "1")
        .env(
            "NOEVIA_LEGACY_UPSTREAM",
            format!("http://127.0.0.1:{core_port}"),
        );
    let mut front = Owned::spawn(front, OwnedRole::RustFront, fixed_mode)?;
    ready(
        &base,
        &mut [&mut manager, &mut node, &mut front],
        Readiness::CoreOrFront,
    )?;
    let (mut admin, mut member) = session(&base, &root.join("ui-data"))?;
    let mut rows = Vec::new();
    let path = format!(
        "/api/model-manager/backends/{}/diagnose",
        logs.keys().next().ok_or("fixture name")?
    );
    require(
        member.call(&base, "GET", &path, &[], false)?.status == 403,
        "member role-first refusal",
    )?;
    if mode.starts_with("prompts-") || mode == "public-invalid" {
        let path = if mode == "public-invalid" {
            "/api/model-manager/backends/a..b/diagnose"
        } else {
            &path
        };
        let response = admin.call(&base, "GET", path, &[], false)?;
        let expected = match mode {
            "prompts-absent" => 404,
            "prompts-wrong-token" => 401,
            "prompts-port-omitted" => 502,
            "public-invalid" => 400,
            _ => return Err("negative mode refused".into()),
        };
        rows.push(record(&response, "GET", path, &[])?);
        artifact(
            &Path::new("/tmp").join(format!("s1-diagnose-public-control-{mode}.json")),
            &json!({"mode":mode,"expected_status":expected,"rows":rows}),
        )?;
        require(
            response.status == expected,
            "new public negative control status",
        )?;
        front.finish()?;
        node.finish()?;
        manager.finish()?;
        fake.finish()?;
        return Ok(rows);
    }
    for name in logs.keys() {
        let path = format!("/api/model-manager/backends/{name}/diagnose");
        let response = admin.call(&base, "GET", &path, &[], false)?;
        rows.push(record(&response, "GET", &path, &[])?);
    }
    let path = "/api/model-manager/backends/unknown/diagnose";
    rows.push(record(
        &admin.call(&base, "GET", path, &[], false)?,
        "GET",
        path,
        &[],
    )?);
    front.finish()?;
    node.finish()?;
    manager.finish()?;
    fake.finish()?;
    Ok(rows)
}
fn featureless_control() -> Result<()> {
    let root = tempfile::tempdir()?;
    let binary = Path::new("/tmp/s1-diagnose-featureless-manager");
    let data = root.path().join("unopened-data");
    let marker = root.path().join("child-marker");
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("MODEL_MANAGER_IMPL", "rust")
        .env("MODEL_MANAGER_DIAGNOSE_IMPL", "rust")
        .env("TZ", "UTC")
        .env("DATA_DIR", &data)
        .env("DIAGNOSE_CHILD_MARKER", &marker);
    let output = process::bounded(command, Vec::new(), Duration::from_secs(2))?;
    require(output.status.code()==Some(1) && output.stdout.is_empty() && output.stderr==b"Error: \"native-diagnose requires rust model manager and compiled native-diagnose feature\"\n","featureless diagnosis startup refusal")?;
    require(
        !data.exists() && !marker.exists(),
        "featureless startup touched child/store",
    )?;
    let mut capability = Command::new(binary);
    capability.env_clear().arg("--features");
    let capability = process::bounded(capability, Vec::new(), Duration::from_secs(2))?;
    require(
        capability.status.code() == Some(1)
            && capability.stdout.is_empty()
            && capability.stderr == b"Error: \"native-logs feature is not enabled\"\n",
        "featureless capability omission",
    )?;
    artifact(
        Path::new("/tmp/s1-diagnose-featureless-control.json"),
        &json!({"binary":binary,"status":output.status.code(),"capability_status":capability.status.code(),"child_marker_absent":!marker.exists(),"store_absent":!data.exists()}),
    )?;
    Ok(())
}
fn write_corpus(rows: &[Value], provenance: &Value, input: &Value) -> Result<()> {
    let root = Path::new("contracts/http/corpus/model-manager-diagnose.v1");
    require(!root.exists(), "fresh isolated diagnosis corpus required")?;
    fs::create_dir(root)?;
    fs::create_dir(root.join("exchanges"))?;
    let mut covered = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let request = row.get("request").ok_or("corpus request")?;
        let path = request
            .get("path")
            .and_then(Value::as_str)
            .ok_or("corpus path")?;
        let response = row.get("response").ok_or("corpus response")?;
        let seq = index + 1;
        covered.push(format!("GET {path}"));
        let exchange = json!({"v":1,"seq":seq,"request":{"method":"GET","path":path,"query":[],"headers":{"accept":"application/json","cookie":"<diagnose-admin-cookie>","origin":"<origin>"},"body":{"kind":"empty"}},"response":{"status":response.get("status").ok_or("corpus status")?,"headers":response.get("headers").ok_or("corpus headers")?,"body":{"kind":"json","json":response.get("body").ok_or("corpus body")?}}});
        artifact(
            &root
                .join("exchanges")
                .join(format!("{seq:06}-GET-diagnose-{seq:02}.json")),
            &exchange,
        )?;
    }
    artifact(
        &root.join("manifest.json"),
        &json!({"v":1,"captureDomain":"model-manager-diagnose","covered":covered,"inputs":{},"note":"Owned synthetic fixed-log fixture; supply --bind '<diagnose-admin-cookie>=cookie-header' from its fresh actual administrator session. SOURCE_ORACLE fixes backend logs. No stored session token. Nonvolatile response headers compare completely."}),
    )?;
    artifact(
        &root.join("SOURCE_ORACLE.json"),
        &json!({"provenance":provenance,"fixed_logs":input.get("logs").ok_or("fixed logs")?,"scope":"actual Node auth/admin proxy and existing Rust public front; immutable selected AST functions and fixed fakes; test-only native child refusal guard","sdk_timezone_finding":"Actual SDK naive datetime conversion differs in UTC and America/New_York; native selector requires exact UTC. Historical actual wire gate source c4e92ed40ffd3aac17a491237d46030c7bee9212d40fba320e393b152584898e; production Python remains unchanged."}),
    )?;
    let loaded = replay::corpus::load(root)?;
    require(
        loaded.exchanges.len() == rows.len(),
        "isolated v1 corpus loader count",
    )?;
    Ok(())
}
fn main() -> Result<()> {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|v| v == "--featureless")
    {
        return featureless_control();
    }
    if std::env::args_os()
        .nth(1)
        .is_some_and(|v| v == "--copy-dependencies")
    {
        return copy_dependencies();
    }
    let controls_only = std::env::args_os()
        .nth(1)
        .is_some_and(|v| v == "--controls");
    let startup_only = std::env::args_os().nth(1).is_some_and(|v| v == "--startup");
    let corpus_write = std::env::args_os()
        .nth(1)
        .is_some_and(|v| v == "--write-corpus");
    let security_only = std::env::args_os()
        .nth(1)
        .is_some_and(|v| v == "--security");
    require(
        std::env::args_os().count()
            == if controls_only || startup_only || corpus_write || security_only {
                2
            } else {
                1
            },
        "unexpected qualification arguments",
    )?;
    let core = PathBuf::from(
        std::env::var_os("NOEVIA_DIAGNOSE_CORE_SOURCE")
            .ok_or("explicit immutable core source required")?,
    );
    let runtime = PathBuf::from(
        std::env::var_os("NOEVIA_DIAGNOSE_SERVICES_SOURCE")
            .ok_or("explicit immutable services source required")?,
    );
    let core = core.as_path();
    let runtime = runtime.as_path();
    let revision = include_str!("../../../../contracts/http/core.ref").trim();
    let core_sources = sources(
        core,
        revision,
        &[
            "server/index.cjs",
            "server/routes/models.cjs",
            "server/http.cjs",
            "server/auth.cjs",
            "server/rust-auth.cjs",
            "server/dav-parse.lock",
            "server/package.json",
            "server/package-lock.json",
            "tools/contract-corpus/serve.cjs",
            "tools/contract-corpus/mocks.cjs",
            "tools/contract-corpus/net-guard.cjs",
        ],
    )?;
    let expected = [
        git(core, &["show", &format!("{revision}:server/package.json")])?,
        git(
            core,
            &["show", &format!("{revision}:server/package-lock.json")],
        )?,
    ];
    let dependency_root = qualification_path(
        "NOEVIA_DIAGNOSE_DEPENDENCIES",
        "/tmp/s1-diagnose-node-dependencies",
    );
    let (_, mut dependency_provenance) =
        dependencies::verify(core, runtime, Some(&dependency_root), &expected)?;
    if dependency_root == Path::new("/tmp/s1-diagnose-node-dependencies") {
        *dependency_provenance.get_mut("installation").ok_or("dependency provenance field")? = json!("Rust-owned copy of cached locked package closure; see separately retained copy proof and source-boundary caveats");
    }
    let mut runtime_sources = serde_json::Map::new();
    let mut oracle = serde_json::Map::new();
    for (key, path) in [
        ("api", "model-manager/app/api.py"),
        ("main", "model-manager/app/main.py"),
        ("services", "model-manager/app/services.py"),
        ("diagnose", "model-manager/app/diagnose.py"),
    ] {
        let spec = format!("{RUNTIME}:{path}");
        let bytes = git(runtime, &["show", &spec])?;
        runtime_sources.insert(path.into(),json!({"blob":String::from_utf8(git(runtime,&["rev-parse",&spec])?)?.trim(),"sha256":format!("{:x}",Sha256::digest(&bytes))}));
        oracle.insert(key.into(), Value::String(String::from_utf8(bytes)?));
    }
    let exe = std::env::current_exe()?;
    let bins = exe.parent().ok_or("binary parent")?;
    if controls_only || startup_only {
        oracle.insert("logs".into(), json!({"synthetic-only":""}));
        let input = Value::Object(oracle);
        if startup_only {
            for mode in ["rust-startup", "rust-off"] {
                run_mode(core, bins, mode, &input)?;
            }
        } else {
            for mode in [
                "prompts-absent",
                "prompts-wrong-token",
                "prompts-port-omitted",
                "public-invalid",
            ] {
                run_mode(core, bins, mode, &input)?;
            }
        }
        return Ok(());
    }
    if security_only {
        // services #1379: Python backend_diagnose redacts the model name and evidence too, so
        // both public paths must now return the same redacted body; neither may carry the secret.
        let secret = "synthetic-public-secret-0000000001";
        let log=format!("spawning server instance with name=synthetic-model on port 18001\n[18001] failed to allocate kv Authorization: Bearer {secret}\ninstance name=synthetic-model exited with status 1");
        let model_log=format!("spawning server instance with name=api_key={secret} on port 18001\n[18001] failed to allocate kv api_key={secret}\ninstance name=api_key={secret} exited with status 1");
        oracle.insert(
            "logs".into(),
            json!({"synthetic-security":log,"synthetic-security-model":model_log}),
        );
        let input = Value::Object(oracle);
        let baseline = run_mode(core, bins, "python", &input)?;
        let candidate = run_mode(core, bins, "rust", &input)?;
        artifact(
            Path::new("/tmp/s1-diagnose-public-security.json"),
            &json!({"baseline":baseline,"candidate":candidate,"named_difference":"None for these probes: Python (services #1379) and native both redact model and evidence with the same output. Remaining stronger native differences (numeric credentials, cross-line suppression) are asserted in the pure differential.","security_difference_rows":0,"zero_difference_claim":false}),
        )?;
        require(
            baseline.len() >= 2 && candidate.len() >= 2,
            "security rows present",
        )?;
        for index in 0..2 {
            let baseline_body = baseline
                .get(index)
                .and_then(|row| row.get("response"))
                .and_then(|row| row.get("body"))
                .ok_or("security baseline body")?;
            let candidate_body = candidate
                .get(index)
                .and_then(|row| row.get("response"))
                .and_then(|row| row.get("body"))
                .ok_or("security candidate body")?;
            let baseline_text = serde_json::to_string(baseline_body)?;
            let candidate_text = serde_json::to_string(candidate_body)?;
            require(
                !baseline_text.contains(secret)
                    && !candidate_text.contains(secret)
                    && baseline_text.contains("[redacted]")
                    && candidate_text.contains("[redacted]")
                    && baseline_body == candidate_body,
                "python and native redact model and evidence identically",
            )?;
            require(
                baseline
                    .get(index)
                    .and_then(|row| row.get("response"))
                    .and_then(|row| row.get("status"))
                    == Some(&json!(200))
                    && candidate
                        .get(index)
                        .and_then(|row| row.get("response"))
                        .and_then(|row| row.get("status"))
                        == Some(&json!(200)),
                "security responses are complete success",
            )?;
        }
        return Ok(());
    }
    let fixture: Value = serde_json::from_slice(include_bytes!(
        "../../../../bins/noevia-model-manager/tests/fixtures/diagnose-python.v1.json"
    ))?;
    let mut logs = serde_json::Map::new();
    for id in [
        "cause-0", "cause-1", "cause-2", "cause-3", "cause-4", "cause-5", "cause-6", "cause-7",
    ] {
        let case = fixture
            .get("cases")
            .and_then(Value::as_array)
            .ok_or("fixture cases")?
            .iter()
            .find(|case| case.get("id").and_then(Value::as_str) == Some(id))
            .ok_or("new public case")?;
        require(
            case.get("narrowing").and_then(Value::as_str) == Some(""),
            "admitted nonsecret public case required",
        )?;
        logs.insert(
            format!("synthetic-{id}"),
            case.get("text").ok_or("case text")?.clone(),
        );
    }
    oracle.insert("logs".into(), Value::Object(logs));
    let input = Value::Object(oracle);
    let baseline = run_mode(core, bins, "python", &input)?;
    let candidate = run_mode(core, bins, "rust", &input)?;
    let provenance = json!({"core":revision,"core_sources":core_sources,"runtime":RUNTIME,"runtime_selected_sources":runtime_sources,"dependencies":dependency_provenance,"generator_sha256":format!("{:x}",Sha256::digest(include_bytes!("diagnose-contracts.rs"))),"owned_dependency_seam_sha256":format!("{:x}",Sha256::digest(include_bytes!("support/diagnose_dependencies.rs")))});
    artifact(
        Path::new("/tmp/s1-diagnose-public-current.json"),
        &json!({"baseline":baseline,"candidate":candidate,"provenance":provenance,"oracle_scope":"immutable selected AST functions, fixed fakes; native child guard is test-only"}),
    )?;
    if corpus_write {
        write_corpus(&baseline, &provenance, &input)?;
    }
    require(
        baseline == candidate,
        "public diagnosis semantic difference",
    )?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    /// The checked-in SOURCE_ORACLE records the runtime and the sources of this harness; it
    /// goes stale silently unless it is bound to them. Refresh by editing the three values.
    #[test]
    fn source_oracle_provenance_matches_this_harness() -> Result<()> {
        let oracle: Value = serde_json::from_str(include_str!(
            "../../../../contracts/http/corpus/model-manager-diagnose.v1/SOURCE_ORACLE.json"
        ))?;
        let provenance = oracle.get("provenance").ok_or("provenance")?;
        assert_eq!(
            provenance.get("runtime").and_then(Value::as_str),
            Some(RUNTIME)
        );
        assert_eq!(
            provenance.get("generator_sha256").and_then(Value::as_str),
            Some(
                format!(
                    "{:x}",
                    Sha256::digest(include_bytes!("diagnose-contracts.rs"))
                )
                .as_str()
            ),
            "generator_sha256 must hash diagnose-contracts.rs"
        );
        assert_eq!(
            provenance
                .get("owned_dependency_seam_sha256")
                .and_then(Value::as_str),
            Some(
                format!(
                    "{:x}",
                    Sha256::digest(include_bytes!("support/diagnose_dependencies.rs"))
                )
                .as_str()
            ),
            "owned_dependency_seam_sha256 must hash support/diagnose_dependencies.rs"
        );
        Ok(())
    }
    #[test]
    fn exited_child_diagnostics_never_reflect_synthetic_credentials() {
        let sentinel = "synthetic-qualification-credential-0123456789";
        for (prefix, kind) in [
            ("ModuleNotFoundError:", "module-unavailable"),
            ("Error:", "child-error"),
            ("corpus-launcher:", "launcher-error"),
            ("arbitrary stderr", "child-exited"),
        ] {
            let raw = format!("{prefix} password={sentinel}\n");
            let summary = diagnostic_summary(raw.as_bytes());
            assert_eq!(summary, format!("owned synthetic server exited before qualification: kind={kind} stderr_bytes={} stderr_sha256={:x}", raw.len(), Sha256::digest(raw.as_bytes())));
            assert!(!summary.contains(sentinel));
            assert!(!summary.contains("password="));
        }
    }
    #[test]
    fn reusable_listener_retains_live_conflict_and_accepts_closed_connection() -> Result<()> {
        fn phase(phase: &'static str, error: std::io::Error) -> std::io::Error {
            let raw = error.raw_os_error().or_else(|| {
                // Only reusable_probe's fixed schema may supply its wrapped errno.
                let fixed = error.to_string();
                if fixed.len() > 128 {
                    return None;
                }
                let fields = fixed.strip_prefix("numeric loopback probe stage=")?;
                let (stage, number) = fields.split_once(" errno=")?;
                if !matches!(stage, "socket" | "reuseaddr" | "bind" | "listen") {
                    return None;
                }
                let digits = number.strip_prefix('-').unwrap_or(number);
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                number.parse::<i32>().ok()
            });
            let errno = raw.map(|n| n.to_string()).unwrap_or_else(|| "none".into());
            std::io::Error::new(
                error.kind(),
                format!("closed listener phase={phase} errno={errno}"),
            )
        }
        let listener = reusable_probe(0).map_err(|e| phase("initial-bind", e))?;
        let port = listener
            .local_addr()
            .map_err(|e| phase("initial-address", e))?
            .port();
        assert!(reusable_probe(port).is_err());
        let listener_reuse = rustix::net::sockopt::socket_reuseaddr(&listener)
            .map_err(|e| phase("listener-reuse-check", e.into()))?;
        assert!(listener_reuse);
        let client_fd = rustix::net::socket(
            rustix::net::AddressFamily::INET,
            rustix::net::SocketType::STREAM,
            Some(rustix::net::ipproto::TCP),
        )
        .map_err(|e| phase("client-socket", e.into()))?;
        rustix::net::sockopt::set_socket_reuseaddr(&client_fd, true)
            .map_err(|e| phase("client-reuse", e.into()))?;
        rustix::net::bind(
            &client_fd,
            &std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, 0),
        )
        .map_err(|e| phase("client-bind", e.into()))?;
        rustix::net::connect(
            &client_fd,
            &std::net::SocketAddrV4::new(std::net::Ipv4Addr::LOCALHOST, port),
        )
        .map_err(|e| phase("client-connect", e.into()))?;
        let mut client = TcpStream::from(client_fd);
        let client_reuse = rustix::net::sockopt::socket_reuseaddr(&client)
            .map_err(|e| phase("client-reuse-check", e.into()))?;
        assert!(client_reuse);
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .map_err(|e| phase("client-timeout", e))?;
        let (mut server, _) = listener.accept().map_err(|e| phase("server-accept", e))?;
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .map_err(|e| phase("server-timeout", e))?;
        rustix::net::sockopt::set_socket_reuseaddr(&server, true)
            .map_err(|e| phase("server-reuse", e.into()))?;
        let server_reuse = rustix::net::sockopt::socket_reuseaddr(&server)
            .map_err(|e| phase("server-reuse-check", e.into()))?;
        assert!(server_reuse);
        let client_local = client
            .local_addr()
            .map_err(|e| phase("client-local", e))?
            .port();
        let client_peer = client
            .peer_addr()
            .map_err(|e| phase("client-peer", e))?
            .port();
        let server_local = server
            .local_addr()
            .map_err(|e| phase("server-local", e))?
            .port();
        let server_peer = server
            .peer_addr()
            .map_err(|e| phase("server-peer", e))?
            .port();
        // The client initiates this closed fixture's graceful shutdown.
        client
            .shutdown(std::net::Shutdown::Write)
            .map_err(|e| phase("client-half-close", e))?;
        let mut byte = [0];
        assert_eq!(
            server
                .read(&mut byte)
                .map_err(|e| phase("server-read-eof", e))?,
            0
        );
        server
            .shutdown(std::net::Shutdown::Write)
            .map_err(|e| phase("server-half-close", e))?;
        assert_eq!(
            client
                .read(&mut byte)
                .map_err(|e| phase("client-read-eof", e))?,
            0
        );
        eprintln!("closed-listener-policy-v1 phase=before-drop listener_port={port} client_local={client_local} client_peer={client_peer} server_local={server_local} server_peer={server_peer} listener_reuse={} client_reuse={} server_reuse={}", u8::from(listener_reuse), u8::from(client_reuse), u8::from(server_reuse));
        drop(client);
        drop(server);
        drop(listener);
        eprintln!("closed-listener-policy-v1 phase=before-replacement listener_port={port}");
        let replacement = rebind_after_teardown(port, Duration::from_secs(5))
            .map_err(|e| phase("replacement-bind", e))?;
        assert!(reusable_probe(port).is_err());
        drop(replacement);
        Ok(())
    }
    /// The closed connection's kernel teardown is asynchronous on Linux: a rebind can still see
    /// EADDRINUSE for a short while after every fd is closed (CI errno=98, passed on rerun). Retry
    /// only that refusal, within a fixed budget; any other error, or a conflict that outlives the
    /// budget, is still returned to the caller.
    fn rebind_after_teardown(port: u16, budget: Duration) -> std::io::Result<TcpListener> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            match reusable_probe(port) {
                Ok(listener) => return Ok(listener),
                Err(error)
                    if error.kind() == std::io::ErrorKind::AddrInUse
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
    }
    #[test]
    fn rebind_after_teardown_refuses_a_live_conflict_within_its_budget() -> Result<()> {
        let live = reusable_probe(0)?;
        let port = live.local_addr()?.port();
        let started = std::time::Instant::now();
        let error = match rebind_after_teardown(port, Duration::from_millis(60)) {
            Ok(_) => return Err("live listener must keep its port".into()),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(live);
        let freed = rebind_after_teardown(port, Duration::from_secs(1))?;
        drop(freed);
        Ok(())
    }
    #[test]
    fn owned_foreground_cleanup_observes_child_wait() -> Result<()> {
        let mut command = Command::new("/bin/sleep");
        command.arg("60").env_clear();
        let mut owned = Owned::spawn(command, OwnedRole::ClosedControl, RunMode::ClosedControl)?;
        let pid = owned.leader.child.id();
        // This closed fixture is the directly owned child; its retained handle binds PID.
        let result = owned.finish();
        assert!(result.is_err()); // sleep is terminated by TERM, not successful graceful exit.
        assert!(!owned.leader.reaped);
        owned.leader.stop()?;
        assert!(owned.leader.reaped);
        assert_eq!(owned.leader.child.id(), pid);
        assert!(owned.leader.child.try_wait()?.is_some());
        Ok(())
    }
    #[test]
    fn shutdown_metadata_is_fixed_and_never_reflects_credentials() -> Result<()> {
        let secret = b"synthetic-qualification-secret password=not-public";
        let digest = Sha256::digest(secret);
        let observations = [
            ExitObservation {
                code: Some(0),
                exited: true,
                ..ExitObservation::default()
            },
            ExitObservation {
                code: Some(1),
                exited: true,
                ..ExitObservation::default()
            },
            ExitObservation {
                signal: Some(15),
                killed: true,
                ..ExitObservation::default()
            },
            ExitObservation {
                signal: Some(9),
                killed: true,
                ..ExitObservation::default()
            },
            ExitObservation::default(),
        ];
        for (index, observation) in observations.into_iter().enumerate() {
            assert_eq!(observation.accepts(), index == 0);
            let result = shutdown_summary(
                OwnedRole::RustManager,
                RunMode::RustStartup,
                ShutdownStage::ExitStatus,
                observation,
                0,
                &Sha256::new(),
                secret,
            );
            let code = match index {
                0 => "0",
                1 => "1",
                _ => "none",
            };
            let signal = match index {
                2 => "15",
                3 => "9",
                _ => "none",
            };
            let exited = index < 2;
            let killed = matches!(index, 2 | 3);
            assert_eq!(result,format!("owned shutdown refused: role=rust-manager mode=rust-startup stage=exit-status code={code} signal={signal} exited={exited} killed={killed} dumped=false stdout_bytes=0 stdout_sha256={:x} stderr_bytes={} stderr_sha256={digest:x}",Sha256::digest([]),secret.len()));
            assert!(!result.contains("password="));
            assert!(!result.contains("not-public"));
        }
        for name in [
            "python",
            "rust",
            "rust-startup",
            "rust-off",
            "prompts-absent",
            "prompts-wrong-token",
            "prompts-port-omitted",
            "public-invalid",
        ] {
            assert_eq!(RunMode::parse(name)?.name(), name);
        }
        for unknown in ["future-mode", "rust-future", "closed-control", ""] {
            assert!(RunMode::parse(unknown).is_err());
        }
        for (role, name) in [
            (OwnedRole::PythonFixture, "python-fixture"),
            (OwnedRole::NodeLauncher, "node-launcher"),
            (OwnedRole::RustFront, "rust-front"),
            (OwnedRole::ClosedControl, "closed-control"),
        ] {
            assert_eq!(role.name(), name);
        }
        Ok(())
    }

    #[test]
    fn role_scoped_term_requires_owned_signal_and_matching_reap() {
        use std::os::unix::process::ExitStatusExt;
        let normal = ExitObservation {
            code: Some(0),
            exited: true,
            ..Default::default()
        };
        let term = ExitObservation {
            signal: Some(15),
            killed: true,
            ..Default::default()
        };
        let roles = [
            OwnedRole::RustManager,
            OwnedRole::RustFront,
            OwnedRole::NodeLauncher,
            OwnedRole::PythonFixture,
            OwnedRole::ClosedControl,
        ];
        for role in roles {
            assert!(normal.accepts_for(role, RunMode::Python, true));
            assert!(normal.matches_reaped(std::process::ExitStatus::from_raw(0)));
            assert!(!normal.matches_reaped(std::process::ExitStatus::from_raw(15)));
            assert_eq!(
                term.accepts_for(role, RunMode::Python, true),
                matches!(role, OwnedRole::PythonFixture)
            );
            assert!(!term.accepts_for(role, RunMode::Python, false));
        }
        for mode in [
            RunMode::Python,
            RunMode::PromptsAbsent,
            RunMode::PromptsWrongToken,
            RunMode::PromptsPortOmitted,
            RunMode::PublicInvalid,
        ] {
            assert!(term.accepts_for(OwnedRole::PythonFixture, mode, true));
        }
        for mode in [
            RunMode::Rust,
            RunMode::RustStartup,
            RunMode::RustOff,
            RunMode::ClosedControl,
        ] {
            assert!(!term.accepts_for(OwnedRole::PythonFixture, mode, true));
        }
        assert!(term.matches_reaped(std::process::ExitStatus::from_raw(15)));
        for raw in [0, 1 << 8, 2, 9, 15 | 128] {
            assert!(!term.matches_reaped(std::process::ExitStatus::from_raw(raw)));
        }
        for invalid in [
            ExitObservation {
                signal: Some(2),
                ..term
            },
            ExitObservation {
                signal: Some(9),
                ..term
            },
            ExitObservation {
                code: Some(0),
                ..term
            },
            ExitObservation {
                dumped: true,
                ..term
            },
            ExitObservation {
                exited: true,
                ..term
            },
            ExitObservation {
                killed: false,
                ..term
            },
            ExitObservation {
                code: Some(1),
                exited: true,
                ..Default::default()
            },
            ExitObservation::default(),
        ] {
            assert!(!invalid.accepts_for(OwnedRole::PythonFixture, RunMode::Python, true));
        }
        assert!(RunMode::parse("future-role-or-mode").is_err());
    }
}
