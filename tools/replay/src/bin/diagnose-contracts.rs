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
const RUNTIME: &str = "6c233f4a05730ed5dccd7997ae601e9999eaa6d7";
fn qualification_path(key: &str, default: &str) -> PathBuf {
    std::env::var_os(key)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}
// AST-only immutable selected functions; no production application startup.
const SHIM: &str = r#"import ast,dataclasses,os,sys,types,importlib.metadata,json
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
api={'__package__':'synthetic_app','router':router,'services':services,'HTTPException':HTTPException,'dataclasses':dataclasses,'Path':Path,'Any':Any}
selected(p['api'],['_plain','_logs_since_start','backend_diagnose','health'],api)
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
struct Owned {
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
    fn spawn(mut command: Command) -> Result<Self> {
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut leader = Leader(command.spawn()?);
        let stdout = leader.0.stdout.take().ok_or("child stdout absent")?;
        let stderr = leader.0.stderr.take().ok_or("child stderr absent")?;
        let owned = Self {
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
    fn pid(&self) -> Result<rustix::process::Pid> {
        i32::try_from(self.leader.0.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .ok_or("child identity".into())
    }
    fn pump(&mut self) -> Result<()> {
        fn step(
            pipe: &mut impl Read,
            count: &mut usize,
            diagnostic: Option<&mut Vec<u8>>,
        ) -> Result<()> {
            let mut chunk = [0; 4096];
            match pipe.read(&mut chunk) {
                Ok(n) => {
                    *count += n;
                    require(*count <= 65_536, "child output cap")?;
                    if let Some(diagnostic) = diagnostic {
                        diagnostic.try_reserve(n)?;
                        diagnostic.extend_from_slice(chunk.get(..n).ok_or("diagnostic count")?);
                    }
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
        step(&mut self.stdout, &mut self.out, None)?;
        step(&mut self.stderr, &mut self.err, Some(&mut self.diagnostic))?;
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
                step(&mut self.stdout, &mut self.out, None)?;
                step(&mut self.stderr, &mut self.err, Some(&mut self.diagnostic))?;
            }
            retain_private_diagnostic(&self.diagnostic)?;
            return Err(diagnostic_summary(&self.diagnostic).into());
        }
        Ok(())
    }
}
struct Leader(Child);
impl Drop for Leader {
    fn drop(&mut self) {
        if let Some(pid) = i32::try_from(self.0.id())
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
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
fn run_mode(core: &Path, bins: &Path, mode: &str, oracle: &Value) -> Result<Vec<Value>> {
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
    let guard_a = TcpListener::bind("127.0.0.1:8090")?;
    let guard_b = TcpListener::bind("127.0.0.1:8091")?;
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
    let mut manager = Owned::spawn(manager)?;
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
    if mode == "rust-startup" || mode == "rust-off" || mode == "rust-confirmation-check" {
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
        if mode == "rust-confirmation-check" {
            fake.finish()?;
            return Ok(rows);
        }
        require(
            marker == if selected { b"1" } else { b"0" },
            "explicit child diagnosis confirmation",
        )?;
        require(
            response.status == if selected { 503 } else { 200 },
            "owned child diagnosis selection/refusal",
        )?;
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
    let mut node = Owned::spawn(node)?;
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
    let mut front = Owned::spawn(front)?;
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
    let core = qualification_path(
        "NOEVIA_DIAGNOSE_CORE_SOURCE",
        "/private/tmp/sol-routing-core010735-oracle",
    );
    let runtime = qualification_path(
        "NOEVIA_DIAGNOSE_SERVICES_SOURCE",
        "/Users/sebastiandalton/noevia-work/s1-prompts-services",
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
        let secret = "synthetic-public-secret-0000000001";
        let log=format!("spawning server instance with name=synthetic-model on port 18001\n[18001] failed to allocate kv Authorization: Bearer {secret}\ninstance name=synthetic-model exited with status 1");
        oracle.insert("logs".into(), json!({"synthetic-security":log}));
        let input = Value::Object(oracle);
        let baseline = run_mode(core, bins, "python", &input)?;
        let candidate = run_mode(core, bins, "rust", &input)?;
        artifact(
            Path::new("/tmp/s1-diagnose-public-security.json"),
            &json!({"baseline":baseline,"candidate":candidate,"named_difference":"Native evidence uses existing Redactor; fixed synthetic bearer removed. Python unchanged.","security_difference_rows":1,"zero_difference_claim":false}),
        )?;
        let baseline_body = baseline
            .first()
            .and_then(|row| row.get("response"))
            .and_then(|row| row.get("body"))
            .ok_or("security baseline body")?;
        let candidate_body = candidate
            .first()
            .and_then(|row| row.get("response"))
            .and_then(|row| row.get("body"))
            .ok_or("security candidate body")?;
        let baseline_text = serde_json::to_string(baseline_body)?;
        let candidate_text = serde_json::to_string(candidate_body)?;
        require(
            baseline_text.contains(secret)
                && !candidate_text.contains(secret)
                && candidate_text.contains("[redacted]"),
            "named native redaction security difference",
        )?;
        require(
            baseline
                .first()
                .and_then(|row| row.get("response"))
                .and_then(|row| row.get("status"))
                == Some(&json!(200))
                && candidate
                    .first()
                    .and_then(|row| row.get("response"))
                    .and_then(|row| row.get("status"))
                    == Some(&json!(200)),
            "security responses are complete success",
        )?;
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
    fn native_child_confirmation_semantics() -> Result<()> {
        let runtime = qualification_path(
            "NOEVIA_DIAGNOSE_SERVICES_SOURCE",
            "/Users/sebastiandalton/noevia-work/s1-prompts-services",
        );
        let mut oracle = serde_json::Map::new();
        for (key, path) in [
            ("api", "model-manager/app/api.py"),
            ("main", "model-manager/app/main.py"),
            ("services", "model-manager/app/services.py"),
            ("diagnose", "model-manager/app/diagnose.py"),
        ] {
            oracle.insert(
                key.into(),
                Value::String(String::from_utf8(git(
                    &runtime,
                    &["show", &format!("{RUNTIME}:{path}")],
                )?)?),
            );
        }
        oracle.insert("logs".into(), json!({"synthetic-only":""}));
        let exe = std::env::current_exe()?;
        let bins = exe
            .parent()
            .and_then(Path::parent)
            .ok_or("owned test binary parent")?;
        let rows = run_mode(
            Path::new("/private/tmp/sol-routing-core010735-oracle"),
            bins,
            "rust-confirmation-check",
            &Value::Object(oracle),
        )?;
        let marker = rows
            .first()
            .and_then(|row| row.get("child_confirmation"))
            .ok_or("observed child confirmation")?;
        assert_eq!(marker, &json!("1"));
        Ok(())
    }
}
