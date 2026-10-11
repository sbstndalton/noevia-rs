//! Rust-owned finite qualification of immutable Python acquisition and Docker SDK wire options.
#[path = "support/diagnose_process.rs"]
mod process;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read, Write},
    os::unix::net::UnixListener,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const REF: &str = "6c233f4a05730ed5dccd7997ae601e9999eaa6d7";
fn python() -> String {
    std::env::var("PYTHON").unwrap_or_else(|_| "/tmp/s1-prompts-native-venv/bin/python".into())
}
fn repository() -> String {
    std::env::var("NOEVIA_DIAGNOSE_SERVICES_SOURCE")
        .unwrap_or_else(|_| "/Users/sebastiandalton/noevia-work/s1-prompts-services".into())
}
fn sdk() -> String {
    std::env::var("NOEVIA_DIAGNOSE_DOCKER_SDK").unwrap_or_else(|_| {
        "/tmp/s1-prompts-native-venv/lib/python3.12/site-packages/docker".into()
    })
}
fn command(path: &str) -> Command {
    let mut command = Command::new(path);
    command
        .env_clear()
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        )
        .env("LANG", "C.UTF-8");
    command
}
fn source() -> Result<Vec<u8>, String> {
    let mut cmd = command("/usr/bin/git");
    cmd.args([
        "-C",
        &repository(),
        "show",
        &format!("{REF}:model-manager/app/api.py"),
    ]);
    let output = process::bounded(cmd, Vec::new(), Duration::from_secs(10))?;
    if !output.status.success() || !output.stderr.is_empty() || output.stdout.len() > 128 * 1024 {
        return Err("immutable API source refused".into());
    }
    if format!("{:x}", Sha256::digest(&output.stdout))
        != "cbd633a8f9e150ff8f93fa3191bd7b4dd9278c43d093b19b8d1686ce1f979a19"
    {
        return Err("immutable API source hash mismatch".into());
    }
    Ok(output.stdout)
}
fn checked_sdk(relative: &str, hash: &str) -> Result<(), String> {
    let path = format!("{}/{relative}", sdk());
    let metadata = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if metadata.len() > 128 * 1024 {
        return Err("SDK source cap".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() > 128 * 1024 || format!("{:x}", Sha256::digest(bytes)) != hash {
        return Err("SDK source hash mismatch".into());
    }
    Ok(())
}
fn immutable(path: &str, expected: &str) -> Result<String, String> {
    let mut command = command("/usr/bin/git");
    command.args(["-C", &repository(), "show", &format!("{REF}:{path}")]);
    let output = process::bounded(command, Vec::new(), Duration::from_secs(10))?;
    if !output.status.success()
        || !output.stderr.is_empty()
        || output.stdout.len() > 256 * 1024
        || format!("{:x}", Sha256::digest(&output.stdout)) != expected
    {
        return Err("immutable route source refused".into());
    }
    String::from_utf8(output.stdout).map_err(|e| e.to_string())
}
fn route_oracle() -> Result<(), String> {
    let api = String::from_utf8(source()?).map_err(|e| e.to_string())?;
    let main = immutable(
        "model-manager/app/main.py",
        "28f0acd83bffa88c781373a7f568ef5e73def8333da1221f0dbac49055242cbb",
    )?;
    let services = immutable(
        "model-manager/app/services.py",
        "eb11d5357f4e309d2dadf2c01346d404df6d568d5eb579d01eb89c45a7d92a18",
    )?;
    let diagnose = immutable(
        "model-manager/app/diagnose.py",
        "87257c86816978b3c4f82eff2765cd5247d13a5340462eb2995180e9a5790520",
    )?;
    let mut cases = Vec::new();
    for (id, path) in [
        ("canonical", "/api/v1/backends/synthetic-backend/diagnose"),
        ("slash", "/api/v1/backends/synthetic-backend/diagnose/"),
        (
            "double-slash",
            "/api/v1/backends//synthetic-backend/diagnose",
        ),
        ("empty", "/api/v1/backends//diagnose"),
        ("extra", "/api/v1/backends/synthetic-backend/extra/diagnose"),
        (
            "encoded-letter",
            "/api/v1/backends/%73ynthetic-backend/diagnose",
        ),
        (
            "encoded-slash",
            "/api/v1/backends/synthetic%2Fbackend/diagnose",
        ),
        (
            "encoded-backslash",
            "/api/v1/backends/synthetic%5Cbackend/diagnose",
        ),
        (
            "encoded-percent",
            "/api/v1/backends/synthetic%25backend/diagnose",
        ),
        ("unicode", "/api/v1/backends/%E6%A8%A1%E5%9E%8B/diagnose"),
        ("invalid-utf8", "/api/v1/backends/%FF/diagnose"),
        ("malformed-percent", "/api/v1/backends/%Q0/diagnose"),
        ("unknown", "/api/v1/backends/unknown/diagnose"),
        (
            "queries",
            "/api/v1/backends/synthetic-backend/diagnose?tail=1&tail=9&since=0&unknown=x",
        ),
        (
            "encoded-route",
            "/api/v1/backends/synthetic-backend/%64iagnose",
        ),
    ] {
        for method in ["GET", "HEAD", "POST", "PUT", "DELETE", "OPTIONS"] {
            cases.push(json!({"id":format!("{id}-{method}"),"method":method,"path":path,"token":"synthetic-shared-token-00000000000","configured":["synthetic-backend"],"log":"spawning server instance with name=synthetic-model on port 18001\n[18001] failed to allocate kv\ninstance name=synthetic-model exited with status 1"}));
        }
    }
    cases.push(json!({"id":"wrong-token-auth-first","method":"GET","path":"/api/v1/backends/synthetic-backend/diagnose","token":"wrong","configured":["synthetic-backend"],"log":""}));
    let input = serde_json::to_vec(
        &json!({"api":api,"main":main,"services":services,"diagnose":diagnose,"cases":cases}),
    )
    .map_err(|e| e.to_string())?;
    let script = r#"import ast,asyncio,dataclasses,json,sys,types,os,importlib.metadata
from pathlib import Path
from typing import Any
from fastapi import FastAPI,APIRouter,HTTPException,Request
from fastapi.responses import Response
from docker.errors import DockerException
import httpx
assert importlib.metadata.version('fastapi')=='0.115.6'
assert importlib.metadata.version('starlette')=='0.41.3'
assert importlib.metadata.version('pydantic')=='2.14.0'
assert importlib.metadata.version('httpx')=='0.28.1'
p=json.load(sys.stdin)
def selected(source,names,scope):
    nodes=[node for node in ast.parse(source).body if isinstance(node,(ast.FunctionDef,ast.AsyncFunctionDef)) and node.name in names]
    assert {node.name for node in nodes}==set(names)
    exec(compile(ast.Module(body=nodes,type_ignores=[]),'immutable-selected.py','exec'),scope)
package=types.ModuleType('synthetic_app');package.__path__=[];sys.modules['synthetic_app']=package
diagnose=types.ModuleType('synthetic_app.diagnose');sys.modules[diagnose.__name__]=diagnose
exec(compile(p['diagnose'],'immutable-diagnose.py','exec'),diagnose.__dict__)
async def run():
    rows=[]
    for case in p['cases']:
        calls=[]
        settings=types.SimpleNamespace(model_loader_token='synthetic-shared-token-00000000000',llama_container_names=case['configured'])
        def get(name):
            calls.append(['get',name])
            def logs(**kwargs):calls.append(['logs',{key:value.isoformat() if hasattr(value,'isoformat') else value for key,value in kwargs.items()}]);return case['log'].encode()
            return types.SimpleNamespace(attrs={'State':{'StartedAt':'2026-10-11T00:00:00.123456789Z'}},logs=logs)
        def listing(**kwargs):calls.append(['list',kwargs]);return []
        scope={'settings':settings,'DockerException':DockerException,'_docker_client':lambda:types.SimpleNamespace(containers=types.SimpleNamespace(list=listing,get=get))}
        selected(p['services'],['discover_llama_containers','_effective_container_names'],scope)
        services=types.SimpleNamespace(**scope)
        app=FastAPI(title='Model Loader');router=APIRouter(prefix='/api/v1')
        api={'__package__':'synthetic_app','router':router,'services':services,'HTTPException':HTTPException,'dataclasses':dataclasses,'Path':Path,'Any':Any}
        selected(p['api'],['_plain','_logs_since_start','backend_diagnose'],api)
        middleware={'app':app,'settings':settings,'os':os,'Request':Request,'Response':Response}
        selected(p['main'],['_require_token'],middleware)
        app.include_router(router)
        async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app),base_url='http://synthetic.invalid',follow_redirects=False) as client:
            response=await client.request(case['method'],case['path'],headers={'x-model-loader-token':case['token']})
            rows.append(dict(case,status=response.status_code,body=response.text,headers={key:value for key,value in response.headers.items() if key in ['content-type','allow','location']},calls=calls))
    return rows
json.dump({'schema':1,'python':sys.version,'versions':{name:importlib.metadata.version(name) for name in ['fastapi','starlette','pydantic','httpx']},'cases':asyncio.run(run())},sys.stdout)
"#;
    let mut command = command(&python());
    command
        .env("TZ", "UTC")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .args(["-I", "-c", script]);
    let output = process::bounded(command, input, Duration::from_secs(10))?;
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(format!(
            "actual route oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let mut fixture: Value = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    let object = fixture
        .as_object_mut()
        .ok_or("route oracle object missing")?;
    object.insert(
        "generator_sha256".into(),
        json!(format!(
            "{:x}",
            Sha256::digest(include_bytes!("generate-diagnose-transport-oracle.rs"))
        )),
    );
    object.insert("oracle_ref".into(), json!(REF));
    io::stdout()
        .lock()
        .write_all(&serde_json::to_vec_pretty(&fixture).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}
fn fake_wire(listener: UnixListener, stop: Arc<AtomicBool>) -> Result<Vec<String>, String> {
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut requests = Vec::new();
    while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
        let (mut socket, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        socket
            .set_read_timeout(Some(Duration::from_millis(300)))
            .map_err(|e| e.to_string())?;
        socket
            .set_write_timeout(Some(Duration::from_millis(300)))
            .map_err(|e| e.to_string())?;
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            socket.read_exact(&mut byte).map_err(|e| e.to_string())?;
            if header.len() == 8192 {
                return Err("fake wire header cap".into());
            }
            header.extend_from_slice(&byte);
        }
        let header = String::from_utf8(header).map_err(|e| e.to_string())?;
        let first = header.lines().next().ok_or("fake wire request missing")?;
        if requests.len() == 10 || !first.starts_with("GET /v1.45/containers/synthetic-id/logs?stderr=1&stdout=1&timestamps=0&follow=0&tail=40000") || !first.ends_with(" HTTP/1.1") {
            return Err("fake wire operation refused".into());
        }
        requests.push(header);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nsynthetic-log",
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(requests)
}
fn main() -> Result<(), String> {
    if std::env::args().any(|argument| argument == "--routes") {
        return route_oracle();
    }
    let source = String::from_utf8(source()?).map_err(|e| e.to_string())?;
    checked_sdk(
        "api/container.py",
        "3429dc4760d4c7a30e071824d6fd36ff8d79e4c0ebcd138d591dcdcca67e5b85",
    )?;
    checked_sdk(
        "utils/utils.py",
        "3feaa36c089c264e157092ba8f29384e735b7ad3f3c8e2e66abe40ac698a6786",
    )?;
    let cases = [
        ("nanoseconds", "2026-10-11T12:34:56.987654321Z"),
        ("seconds", "2026-10-11T12:34:56Z"),
        ("epoch", "1970-01-01T00:00:00Z"),
        ("pre-epoch", "1969-12-31T23:59:59Z"),
        ("leap-year", "2000-02-29T23:59:59.999999999Z"),
        ("year-one", "0001-01-01T00:00:00Z"),
        ("zero-date", "0000-01-01T00:00:00Z"),
        ("missing", ""),
        ("offset", "2026-10-11T12:34:56+02:00"),
        ("malformed", "synthetic-invalid"),
    ]
    .into_iter()
    .map(|(id, started)| json!({"id":id,"started":started}))
    .collect::<Vec<_>>();
    // Only selected immutable acquisition executes; the SDK connects exclusively to
    // the fresh Rust-owned Unix fixture, with auto-version detection suppressed.
    let script = r#"import ast,json,sys,types,time
import docker,requests
assert docker.__version__ == '7.1.0'
time.tzset()
p=json.load(sys.stdin)
nodes=[n for n in ast.parse(p['source']).body if isinstance(n,ast.FunctionDef) and n.name=='_logs_since_start']
assert len(nodes)==1
rows=[]
for case in p['cases']:
    captured=[]
    sdk=docker.APIClient(base_url=p['socket'],version='1.45',timeout=1)
    actual_get=sdk._get
    def fixed_get(url,**kwargs):
        assert url=='http+docker://localhost/v1.45/containers/synthetic-id/logs'
        assert set(kwargs)=={'params','stream'} and kwargs['stream'] is False
        prepared=requests.Request('GET',url,params=kwargs['params']).prepare()
        captured.append({'method':prepared.method,'path':prepared.path_url,'params':kwargs['params']})
        return actual_get(url,**kwargs)
    sdk._get=fixed_get
    sdk._get_result=lambda container,stream,response: response.content
    container=types.SimpleNamespace(attrs={'State':{'StartedAt':case['started']}},logs=lambda **kwargs:sdk.logs('synthetic-id',**kwargs))
    get=lambda name: container if name=='synthetic-backend' else (_ for _ in ()).throw(AssertionError('unexpected name'))
    scope={'services':types.SimpleNamespace(_docker_client=lambda:types.SimpleNamespace(containers=types.SimpleNamespace(get=get)))}
    exec(compile(ast.Module(body=nodes,type_ignores=[]),'immutable-acquisition.py','exec'),scope)
    output=scope['_logs_since_start']('synthetic-backend')
    rows.append(dict(case,output=output,wire=captured))
    sdk.close()
json.dump({'python':sys.version,'sdk':docker.__version__,'requests':requests.__version__,'tz':time.tzname,'cases':rows},sys.stdout)
"#;
    let mut runs = Vec::new();
    for zone in ["UTC", "America/New_York"] {
        let directory = tempfile::Builder::new()
            .prefix("s1-diag-wire-")
            .tempdir_in("/tmp")
            .map_err(|e| e.to_string())?;
        let socket_path = directory.path().join("docker.sock");
        let listener = UnixListener::bind(&socket_path).map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let input = serde_json::to_vec(&json!({"source":source,"cases":cases,"socket":format!("unix://{}",socket_path.display())})).map_err(|e| e.to_string())?;
        let worker = std::thread::spawn(move || fake_wire(listener, worker_stop));
        let mut cmd = command(&python());
        cmd.env("TZ", zone)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .args(["-I", "-c", script]);
        let output = process::bounded(cmd, input, Duration::from_secs(10));
        stop.store(true, Ordering::Release);
        let wire = worker.join().map_err(|_| "fake wire worker failed")??;
        let output = output?;
        if !output.status.success() || !output.stderr.is_empty() {
            return Err(format!("SDK wire oracle failed for {zone}"));
        }
        let result: Value = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
        if wire.len() != 7 {
            return Err("fake wire request count mismatch".into());
        }
        runs.push(json!({"zone":zone,"result":result,"actual_unix_requests":wire}));
    }
    let value = json!({"schema":1,"oracle_ref":REF,"api_sha256":"cbd633a8f9e150ff8f93fa3191bd7b4dd9278c43d093b19b8d1686ce1f979a19","generator_sha256":format!("{:x}",Sha256::digest(include_bytes!("generate-diagnose-transport-oracle.rs"))),"process_sha256":format!("{:x}",Sha256::digest(include_bytes!("support/diagnose_process.rs"))),"wire_boundary":"actual SDK logs/HTTP over Rust-owned fresh private Unix socket; fixed synthetic result collaborator suppresses inspect","runs":runs});
    let bytes = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
    if bytes.len() > 1024 * 1024 {
        return Err("SDK fixture cap".into());
    }
    io::stdout()
        .lock()
        .write_all(&bytes)
        .map_err(|e| e.to_string())
}
