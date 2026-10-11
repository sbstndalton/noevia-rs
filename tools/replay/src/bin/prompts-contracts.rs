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
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const TOKEN: &str = "synthetic-prompts-publicfront-token-0123456789abcdef";
const RUNTIME: &str = "6c233f4a05730ed5dccd7997ae601e9999eaa6d7";
#[path = "support/prompts_dependencies.rs"]
mod dependencies;
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
const SHIM: &str = r#"#!/usr/bin/env python3
import os,sys
from pathlib import Path
from app import main,bench,services
main.hw.start_sampler=lambda:None
bench.reap_orphans=lambda:None
services._docker_client=lambda:None
Path(os.environ['PROMPTS_CHILD_MARKER']).write_text(os.environ.get('NOEVIA_NATIVE_MODEL_MANAGER_PROMPTS_CONFIRMED','absent'))
import uvicorn
uvicorn.run(main.app,host=sys.argv[3],port=int(sys.argv[5]),log_level='error')
"#;
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
    program: String,
}
impl Owned {
    fn spawn(mut command: Command) -> Result<Self> {
        let program = command.get_program().to_string_lossy().into_owned();
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
            program,
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
            let diagnostic = String::from_utf8_lossy(&self.diagnostic);
            let reason = diagnostic.lines().find(|line| {
                line.starts_with("ModuleNotFoundError:")
                    || line.starts_with("Error:")
                    || line.starts_with("corpus-launcher:")
            });
            let fallback: String = diagnostic.chars().take(512).collect();
            return Err(format!(
                "owned synthetic server {} exited before qualification: {}",
                self.program,
                reason.unwrap_or(&fallback)
            )
            .into());
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
    let mut body = serde_json::from_slice::<Value>(&response.body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&response.body).into_owned()));
    if let Some(prompts) = body.get_mut("prompts") {
        let rows = prompts.as_array_mut().ok_or("prompt rows shape")?;
        require(rows.len() <= 7, "synthetic prompt row count")?;
        for row in rows {
            let fields = row.as_object_mut().ok_or("prompt row shape")?;
            require(
                fields
                    .get("created_at")
                    .and_then(Value::as_f64)
                    .is_some_and(f64::is_finite),
                "finite runtime timestamp",
            )?;
            fields.insert("created_at".into(), json!("RUNTIME_TIMESTAMP"));
        }
    }
    Ok(
        json!({"status":response.status,"body":body,"cache_control":response.header("cache-control")}),
    )
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
fn app_command(app: &Path, root: &Path) -> Result<Command> {
    let mut command = Command::new(std::env::var_os("PYTHON").ok_or("PYTHON required")?);
    command.env_clear();
    command.current_dir(app.parent().ok_or("app parent")?);
    let path = format!("{}:{}", root.join("bin").display(), std::env::var("PATH")?);
    command
        .env("PATH", path)
        .env("PYTHONPATH", app.parent().ok_or("app parent")?)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("DATA_DIR", root.join("mm-data"))
        .env("MODELS_DIR", root.join("models"))
        .env("MODELS_INI_PATH", root.join("models/models.ini"))
        .env("MODEL_LOADER_TOKEN", TOKEN)
        .env("MODEL_MANAGER_HOST_IMPL", "python")
        .env("MODEL_MANAGER_SECTIONS_IMPL", "python")
        .env("MIGRATE_CACHE_RAM_ON_START", "false")
        .env("DOCKER_HOST", "unix:///absent-synthetic.sock")
        .env("LLAMA_CONTAINERS", "synthetic-only")
        .env("PROMPTS_CHILD_MARKER", root.join("child-confirmation"));
    for key in [
        "NOEVIA_NATIVE_MODEL_MANAGER_CONFIRMED",
        "NOEVIA_NATIVE_MODEL_MANAGER_HOST_CONFIRMED",
        "NOEVIA_NATIVE_MODEL_MANAGER_SECTIONS_CONFIRMED",
        "NOEVIA_NATIVE_MODEL_MANAGER_PROMPTS_CONFIRMED",
    ] {
        command.env_remove(key);
    }
    Ok(command)
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
fn run_mode(
    core: &Path,
    app: &Path,
    bins: &Path,
    mode: &str,
    dependencies: &Path,
) -> Result<Vec<Value>> {
    let root = tempfile::tempdir()?;
    let root = root.path();
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    for dir in ["bin", "models", "mm-data", "ui-data"] {
        fs::create_dir(root.join(dir))?;
    }
    fs::write(root.join("models/models.ini"), "version = 1\n")?;
    fs::write(root.join("bin/uvicorn"), SHIM)?;
    fs::set_permissions(root.join("bin/uvicorn"), fs::Permissions::from_mode(0o700))?;
    let guards = [
        TcpListener::bind("127.0.0.1:8090")?,
        TcpListener::bind("127.0.0.1:8091")?,
    ];
    let mut manager_command = app_command(app, root)?;
    if mode == "rust" {
        manager_command = {
            let prepared = manager_command;
            let mut command = Command::new(bins.join("noevia-model-manager"));
            command.env_clear();
            for (key, value) in prepared.get_envs() {
                match value {
                    Some(value) => {
                        command.env(key, value);
                    }
                    None => {
                        command.env_remove(key);
                    }
                }
            }
            command
                .current_dir(app.parent().ok_or("app parent")?)
                .env("MODEL_MANAGER_IMPL", "rust")
                .env("MODEL_MANAGER_PROMPTS_IMPL", "rust");
            command
        };
    } else {
        manager_command
            .arg("-B")
            .arg(root.join("bin/uvicorn"))
            .args(["app.main:app", "--host", "127.0.0.1", "--port", "8090"]);
    }
    drop(guards);
    let mut manager = Owned::spawn(manager_command)?;
    ready(
        &Base {
            host: "127.0.0.1".into(),
            port: 8090,
        },
        &mut [&mut manager],
        Readiness::Manager,
    )?;
    let core_listener = TcpListener::bind("127.0.0.1:0")?;
    let front_listener = TcpListener::bind("127.0.0.1:0")?;
    let core_port = core_listener.local_addr()?.port();
    let front_port = front_listener.local_addr()?.port();
    drop(core_listener);
    let base = Base {
        host: "127.0.0.1".into(),
        port: front_port,
    };
    let mut node = Command::new(bins.join("corpus-launcher"));
    node.env_clear()
        .env("PATH", std::env::var_os("PATH").ok_or("PATH required")?)
        .env(
            "NOEVIA_PROMPTS_DEPENDENCIES",
            dependencies.parent().ok_or("dependency parent")?,
        );
    node.args([if mode.starts_with("prompts-") {
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
    front.env_clear();
    front
        .env("UI_HOST", "127.0.0.1")
        .env("UI_PORT", front_port.to_string())
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
    if mode.starts_with("prompts-") {
        if mode == "prompts-wrong-token" {
            let upstream = request(
                &Base {
                    host: "127.0.0.1".into(),
                    port: 8090,
                },
                "GET",
                "/api/v1/prompts",
                &[(
                    "X-Model-Loader-Token".into(),
                    "synthetic-prompts-wrong-token-0123456789abcdef".into(),
                )],
                &[],
            )?;
            require(
                upstream.status == 401,
                &format!(
                    "synthetic upstream wrong-token expected 401, observed {}",
                    upstream.status
                ),
            )?;
        }
        let member_response = member.call(&base, "GET", "/api/model-manager/prompts", &[], true)?;
        require(
            member_response.status == 403,
            "negative proxy control preserves role-first gate",
        )?;
        let response = admin.call(&base, "GET", "/api/model-manager/prompts", &[], true)?;
        let status = match mode {
            "prompts-absent" => 404,
            "prompts-wrong-token" => 401,
            "prompts-port-omitted" => 502,
            _ => return Err("unknown synthetic negative control".into()),
        };
        require(
            response.status == status,
            &format!(
                "synthetic proxy control {mode}: expected {status}, observed {}",
                response.status
            ),
        )?;
        return Ok(vec![
            json!({"control":mode,"member":normalized(&member_response)?,"admin":normalized(&response)?}),
        ]);
    }
    let mut anon = Client::default();
    let mut rows = Vec::new();
    for (client, method, path, body, csrf, status) in [
        (
            &mut anon,
            "GET",
            "/api/model-manager/prompts",
            "",
            true,
            401,
        ),
        (
            &mut member,
            "GET",
            "/api/model-manager/prompts",
            "",
            true,
            403,
        ),
        (
            &mut admin,
            "POST",
            "/api/model-manager/prompts",
            "{",
            false,
            403,
        ),
    ] {
        let response = client.call(&base, method, path, body.as_bytes(), csrf)?;
        require(response.status == status, "public session/admin/CSRF gate")?;
        rows.push(record(&response, method, path, body.as_bytes())?);
    }
    let oversized = vec![b'x'; 1_048_577];
    for (client, status) in [(&mut member, 403), (&mut admin, 413)] {
        let response = client.call(
            &base,
            "POST",
            "/api/model-manager/prompts",
            &oversized,
            true,
        )?;
        require(response.status == status, "public role before request cap")?;
        rows.push(record(
            &response,
            "POST",
            "/api/model-manager/prompts",
            &oversized,
        )?);
    }
    for (method, path, body, status) in [
        ("GET", "/api/model-manager/prompts", "", 200),
        (
            "POST",
            "/api/model-manager/prompts",
            r#"{"name":" 日本語 😀 ","body":[true,null,"it's"]}"#,
            200,
        ),
        (
            "POST",
            "/api/model-manager/prompts",
            r#"{"name":{"z":1,"a":2,"z":3},"body":"body"}"#,
            200,
        ),
        (
            "POST",
            "/api/model-manager/prompts",
            r#"{"name":true,"body":1e16}"#,
            200,
        ),
        ("POST", "/api/model-manager/prompts", "{", 422),
        (
            "POST",
            "/api/model-manager/prompts",
            r#"{"name":0,"body":"x"}"#,
            400,
        ),
        ("PUT", "/api/model-manager/prompts", "{}", 405),
        ("DELETE", "/api/model-manager/prompts/5", "", 200),
        ("DELETE", "/api/model-manager/prompts/5", "", 404),
        ("DELETE", "/api/model-manager/prompts/+0006.00", "", 200),
        ("DELETE", "/api/model-manager/prompts/nope", "", 422),
        ("GET", "/api/model-manager/prompts", "", 200),
        ("GET", "/api/model-manager/prompts/bad!", "", 400),
    ] {
        for child in [&mut manager, &mut node, &mut front] {
            child.pump()?;
        }
        let response = admin.call(&base, method, path, body.as_bytes(), true)?;
        require(response.status == status, "actual public prompt status")?;
        require(
            response
                .header("cache-control")
                .is_some_and(|value| value.contains("no-store")),
            "public no-store preserved",
        )?;
        rows.push(record(&response, method, path, body.as_bytes())?);
    }
    require(
        fs::read_to_string(root.join("child-confirmation"))?
            == if mode == "rust" { "1" } else { "absent" },
        "actual child confirmation",
    )?;
    Ok(rows)
}
fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let core = PathBuf::from(args.next().ok_or("actual core checkout required")?).canonicalize()?;
    let app = PathBuf::from(args.next().ok_or("actual runtime app required")?).canonicalize()?;
    let output = PathBuf::from(args.next().ok_or("output artifact required")?);
    let core_revision = include_str!("../../../../contracts/http/core.ref").trim();
    let core_sources = sources(
        &core,
        core_revision,
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
    let runtime_root = app
        .parent()
        .and_then(Path::parent)
        .ok_or("runtime repository root")?;
    let runtime_sources = sources(
        runtime_root,
        RUNTIME,
        &[
            "model-manager/app/main.py",
            "model-manager/app/db.py",
            "model-manager/app/api.py",
            "model-manager/app/config.py",
            "model-manager/requirements.txt",
        ],
    )?;
    let expected = [
        git(
            &core,
            &["show", &format!("{core_revision}:server/package.json")],
        )?,
        git(
            &core,
            &["show", &format!("{core_revision}:server/package-lock.json")],
        )?,
    ];
    let parameter = std::env::var_os("NOEVIA_PROMPTS_DEPENDENCIES").map(PathBuf::from);
    let (dependencies, dependency_provenance) =
        dependencies::verify(&core, runtime_root, parameter.as_deref(), &expected)?;
    let provenance = json!({"core":core_revision,"core_sources":core_sources,"runtime":RUNTIME,"runtime_sources":runtime_sources,"dependencies":dependency_provenance,"dependency_seam_sha256":format!("{:x}",Sha256::digest(include_bytes!("support/prompts_dependencies.rs"))),"generator_sha256":format!("{:x}",Sha256::digest(include_bytes!("prompts-contracts.rs"))),"launcher_sha256":format!("{:x}",Sha256::digest(include_bytes!("corpus-launcher.rs"))),"launcher_seam_sha256":format!("{:x}",Sha256::digest(include_bytes!("../synthetic-host-launch.cjs"))),"process_sha256":format!("{:x}",Sha256::digest(include_bytes!("../../../qualification/process.rs")))});
    let mode = args.next();
    require(args.next().is_none(), "unexpected arguments")?;
    let python_only = mode.as_deref() == Some(std::ffi::OsStr::new("--python-only"));
    let negative_only = mode.as_deref() == Some(std::ffi::OsStr::new("--negative-only"));
    let wrong_token_only = mode.as_deref() == Some(std::ffi::OsStr::new("--wrong-token-only"));
    require(
        mode.is_none() || python_only || negative_only || wrong_token_only,
        "unknown qualification mode",
    )?;
    require(
        python_only || negative_only || wrong_token_only || cfg!(target_os = "linux"),
        "actual native binary qualification requires Linux",
    )?;
    let exe = std::env::current_exe()?;
    let bins = exe.parent().ok_or("binary parent")?;
    let mut controls = Vec::new();
    if negative_only || !python_only {
        for control in [
            "prompts-absent",
            "prompts-wrong-token",
            "prompts-port-omitted",
        ] {
            if wrong_token_only && control != "prompts-wrong-token" {
                continue;
            }
            controls.extend(run_mode(&core, &app, bins, control, &dependencies)?);
        }
    }
    if negative_only || wrong_token_only {
        artifact(
            &output,
            &json!({"provenance":provenance,"native_binary_qualified":false,"negative_controls":controls}),
        )?;
        println!(
            "actual publicfront negative controls={} native_binary_qualified=false",
            controls.len()
        );
        return Ok(());
    }
    let baseline = run_mode(&core, &app, bins, "python", &dependencies)?;
    let candidate = if python_only {
        None
    } else {
        require(
            cfg!(target_os = "linux"),
            "actual native binary qualification requires Linux",
        )?;
        Some(run_mode(&core, &app, bins, "rust", &dependencies)?)
    };
    if let Some(candidate) = &candidate {
        require(
            candidate == &baseline,
            "actual publicfront prompt corpus difference",
        )?;
    }
    artifact(
        &output,
        &json!({"provenance":provenance,"native_binary_qualified":candidate.is_some(),"runtime_timestamps":"created_at finite and compared structurally; exact synthetic clocks in SQLite oracle","baseline":baseline,"candidate":candidate,"negative_controls":controls}),
    )?;
    println!(
        "actual publicfront prompt cases={} native_binary_qualified={} comparison={}",
        baseline.len(),
        candidate.is_some(),
        if candidate.is_some() {
            "zero-differences"
        } else {
            "baseline-only"
        }
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;
    #[test]
    fn fixed_health_predicate_and_expired_budget() {
        let response = |status, body: &[u8]| Response {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
        };
        assert_eq!(Readiness::Manager.path(), "/api/v1/health");
        assert_eq!(Readiness::CoreOrFront.path(), "/api/ready");
        assert!(Readiness::Manager.accepts(&response(200, b"{\"ok\":true}")));
        for (status, body) in [
            (401, &b"{\"ok\":true}"[..]),
            (200, &b"{\"ok\":false}"[..]),
            (200, &b"{}"[..]),
            (200, &b"broken"[..]),
            (200, &b"{\"ok\":true,\"extra\":1}"[..]),
        ] {
            assert!(!Readiness::Manager.accepts(&response(status, body)));
        }
        assert!(Readiness::CoreOrFront.accepts(&response(200, b"arbitrary")));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = Base {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
        };
        assert!(ready_until(&base, &mut [], Readiness::Manager, Instant::now()).is_err());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    #[test]
    fn owned_synthetic_manager_health_ready_before_request() {
        const MARKER: &str = "NOEVIA_SYNTHETIC_READY_PORT";
        if let Ok(port) = std::env::var(MARKER) {
            let listener = TcpListener::bind(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                assert!(header.len() < 4096);
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            assert!(header.starts_with(b"GET /api/v1/health HTTP/1.1\r\n"));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}").unwrap();
            return;
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.env_clear().env(MARKER, port.to_string()).args([
            "--exact",
            "tests::owned_synthetic_manager_health_ready_before_request",
        ]);
        let mut child = Owned::spawn(command).unwrap();
        ready(
            &Base {
                host: "127.0.0.1".into(),
                port,
            },
            &mut [&mut child],
            Readiness::Manager,
        )
        .unwrap();
    }
    #[test]
    fn timestamp_mask_is_limited_to_finite_prompt_rows() {
        let response = |body: Value| Response {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&body).unwrap(),
        };
        let ordinary = response(json!({"input":{"created_at":"synthetic user field"}}));
        assert_eq!(
            normalized(&ordinary).unwrap()["body"]["input"]["created_at"],
            "synthetic user field"
        );
        assert!(normalized(&response(json!({"prompts":[{"created_at":"bad"}]}))).is_err());
        assert!(normalized(&response(json!({"prompts":[{}]}))).is_err());
        assert_eq!(
            normalized(&response(
                json!({"prompts":[{"created_at":1.0,"name":"same"}]})
            ))
            .unwrap()["body"]["prompts"][0]["name"],
            "same"
        );
    }
    #[test]
    fn artifact_overflow_refuses_before_output_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("artifact.json");
        assert!(artifact(&path, &json!("x".repeat(1_048_577))).is_err());
        assert!(!path.exists());
    }
    #[test]
    fn source_binding_rejects_modified_and_untracked_source() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]).unwrap();
        fs::write(root.path().join("source.txt"), "synthetic source").unwrap();
        git(root.path(), &["add", "source.txt"]).unwrap();
        git(
            root.path(),
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "user.name=Synthetic",
                "-c",
                "user.email=synthetic@example.invalid",
                "commit",
                "-qm",
                "synthetic source",
            ],
        )
        .unwrap();
        let revision =
            String::from_utf8(git(root.path(), &["rev-parse", "HEAD"]).unwrap()).unwrap();
        assert!(sources(root.path(), revision.trim(), &["source.txt"]).is_ok());
        fs::write(root.path().join("source.txt"), "changed").unwrap();
        assert!(sources(root.path(), revision.trim(), &["source.txt"]).is_err());
        fs::write(root.path().join("source.txt"), "synthetic source").unwrap();
        fs::write(root.path().join("shadow.txt"), "untracked").unwrap();
        assert!(sources(root.path(), revision.trim(), &["source.txt"]).is_err());
    }
    #[test]
    fn each_server_pipe_has_an_independent_cap() {
        for pipe in ["stdout", "stderr"] {
            let mut command =
                Command::new(std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into()));
            command
                .env_clear()
                .args(["-c", &format!("import sys;sys.{pipe}.write('x'*65537)")]);
            let mut child = Owned::spawn(command).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match child.pump() {
                    Err(error) => {
                        assert!(error.to_string().contains("child output cap"), "{error}");
                        break;
                    }
                    Ok(()) => {
                        assert!(Instant::now() < deadline);
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        }
    }
    #[test]
    fn slow_progress_cannot_reset_send_deadline_and_zero_progress_refuses() {
        struct Slow {
            calls: usize,
            zero: bool,
            remaining: Vec<Duration>,
        }
        impl SendStep for Slow {
            fn send_step(&mut self, bytes: &[u8], remaining: Duration) -> std::io::Result<usize> {
                assert!(bytes.len() <= 4096);
                self.calls += 1;
                self.remaining.push(remaining);
                if self.zero {
                    return Ok(0);
                }
                thread::sleep(Duration::from_millis(3));
                Ok(1)
            }
        }
        let mut slow = Slow {
            calls: 0,
            zero: false,
            remaining: Vec::new(),
        };
        let error = send(
            &mut slow,
            &[0; 8192],
            Instant::now() + Duration::from_millis(12),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "HTTP send deadline");
        assert!(slow.calls < 8192);
        assert!(slow.remaining.windows(2).all(|pair| pair[1] < pair[0]));
        let mut zero = Slow {
            calls: 0,
            zero: true,
            remaining: Vec::new(),
        };
        assert_eq!(
            send(
                &mut zero,
                &[0; 8192],
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap_err()
            .to_string(),
            "HTTP send made no progress"
        );
        assert_eq!(zero.calls, 1);
    }
}
