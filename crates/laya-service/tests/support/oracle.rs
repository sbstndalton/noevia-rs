//! Rust-owned actual-source oracle; Python strings below are fixed engine
//! collaborators and a lifetime/readiness loader, never case or HTTP tooling.
#![allow(dead_code)]
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{TcpStream, UdpSocket},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub const REF: &str = "4f3cf816225d4ed12db33fd8e4a7e2d7ae946a4b";
pub const SERVER_HASH: &str = "fa9965aa8d2606699e0cb1a4c16a71106e7fd0762b54e30656dfd224a17383e3";
const TORCH: &str = "def set_num_threads(n):pass\ndef set_num_interop_threads(n):pass\n";
const ENGINE: &str = r#"import os,time,json,subprocess
class Tok:
 def encode(self,s):return list(s)*(400 if s.startswith('__budget') else 1)
class Agent:
 cfg={'max_len':8192,'head_max_len':4096}
 tok=Tok()
 def predict(self,state,questions):
  with open(os.path.join(os.path.dirname(__file__),'events'),'a') as f:f.write(json.dumps(state)+'\n')
  if state=='__crash':os._exit(2)
  if state=='__error':raise ValueError('synthetic private exception')
  if state=='__fail_recovery':
   open(os.path.join(os.path.dirname(__file__),'fail-startup'),'w').close()
   raise ValueError('synthetic private exception')
  if state=='__hang':time.sleep(30)
  if state.startswith('__sleep:'):time.sleep(float(state.split(':')[1]))
  if state=='__child':
   child=subprocess.Popen(['python3','-c','import time;time.sleep(30)'])
   with open(os.path.join(os.path.dirname(__file__),'child-pid'),'w') as f:f.write(str(child.pid))
  if state=='__pipe_child':
   fds=tuple(int(fd) for fd in os.listdir('/proc/self/fd') if int(fd)>2 and os.path.exists('/proc/self/fd/'+fd))
   child=subprocess.Popen(['python3','-c','import time;time.sleep(30)'],pass_fds=fds)
   with open(os.path.join(os.path.dirname(__file__),'child-pid'),'w') as f:f.write(str(child.pid))
  keys=list(questions['decision']['criteria'])
  choice=keys[-1] if state.startswith('__sleep:') else keys[0]
  return {'answers':{'decision':{'choice':choice,'probabilities':{k:int(k==choice) for k in keys}}}}
def load(path,device):
 assert (path,device)==('/model','cpu')
 os.environ['LAYA_KEEP_WARM_S']='0'
 with open(os.path.join(os.path.dirname(__file__),'starts'),'a') as f:f.write(str(os.getpid())+'\n')
 delay=os.path.join(os.path.dirname(__file__),'delay-startup')
 if os.path.exists(delay):time.sleep(float(open(delay).read()))
 if os.path.exists(os.path.join(os.path.dirname(__file__),'fail-startup')):raise ValueError('synthetic startup fault')
 print('synthetic library stdout diagnostic',flush=True)
 os.write(1,b'synthetic native stdout diagnostic\n')
 return Agent()
"#;
const LOADER:&str="import sys,threading,socket\nsys.path.insert(0,sys.argv[1])\nimport server\np=server.Pool.start(1,queue_timeout=0.05,decision_timeout=0.1,late_grace=0.5)\nout=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)\ndef ready(h):out.sendto((str(h.server_address[1])+':'+str(sys.version_info.major)+'.'+str(sys.version_info.minor)).encode(),('127.0.0.1',int(sys.argv[2])))\nt=threading.Thread(target=server.serve,args=(p,('127.0.0.1',0),ready),daemon=True);t.start()\ntry:sys.stdin.buffer.read()\nfinally:p.stop()\n";
pub fn reference() -> PathBuf {
    PathBuf::from(
        std::env::var("NOEVIA_LAYA_REFERENCE")
            .unwrap_or_else(|_| "/tmp/s4-laya-actual-source/laya".into()),
    )
}
pub fn prepare(source: &Path, target: &Path) -> Result<(), Error> {
    let bytes = std::fs::read(source.join("server.py"))?;
    if format!("{:x}", Sha256::digest(&bytes)) != SERVER_HASH {
        return Err("actual merged worker hash mismatch".into());
    }
    std::fs::write(target.join("server.py"), bytes)?;
    std::fs::write(target.join("torch.py"), TORCH)?;
    std::fs::write(target.join("laya.py"), ENGINE)?;
    Ok(())
}
pub struct Server {
    child: Child,
    pub port: u16,
    pub production: bool,
}
impl Server {
    pub fn start(directory: &Path) -> Result<Self, Error> {
        let udp = UdpSocket::bind("127.0.0.1:0")?;
        udp.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut child = Command::new("python3")
            .args(["-c", LOADER])
            .arg(directory)
            .arg(udp.local_addr()?.port().to_string())
            .env("LAYA_KEEP_WARM_S", "0")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let result = (|| {
            let mut bytes = [0u8; 64];
            let n = udp.recv(&mut bytes)?;
            let text = std::str::from_utf8(bytes.get(..n).ok_or("read count")?)?;
            let (port, version) = text.split_once(':').ok_or("readiness")?;
            Ok::<_, Error>((port.parse()?, version == "3.12"))
        })();
        match result {
            Ok((port, production)) => Ok(Self {
                child,
                port,
                production,
            }),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(e)
            }
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}
pub fn body(state: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"state":state,"question":"Next?","options":[{"id":"continue","label":"Continue"},{"id":"verify","label":"Check"}]})).unwrap_or_default()
}
pub fn exchange(
    port: u16,
    method: &str,
    path: &str,
    length: Option<&str>,
    body: &[u8],
) -> Result<Value, Error> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut headers = format!("{method} {path} HTTP/1.0\r\nHost: localhost\r\n");
    if let Some(length) = length {
        headers.push_str(&format!("Content-Length: {length}\r\n"));
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes())?;
    stream.write_all(body)?;
    let mut response = Vec::new();
    stream.take(65537).read_to_end(&mut response)?;
    if response.len() > 65536 {
        return Err("response bound".into());
    }
    let at = response
        .windows(4)
        .position(|b| b == b"\r\n\r\n")
        .ok_or("HTTP delimiter")?;
    let headers = std::str::from_utf8(response.get(..at).ok_or("headers")?)?;
    let payload = response.get(at + 4..).ok_or("payload")?;
    let status = headers
        .split_whitespace()
        .nth(1)
        .ok_or("status")?
        .parse::<u16>()?;
    let field = |name: &str| {
        headers.lines().find_map(|l| {
            l.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.trim())
        })
    };
    let kind = field("content-type").ok_or("content type")?;
    let size = field("content-length").ok_or("length")?.parse::<usize>()?;
    if payload.len() != if method == "HEAD" { 0 } else { size } {
        return Err("incomplete response".into());
    }
    let value = if kind == "application/json" {
        serde_json::from_slice(payload)?
    } else {
        Value::String(std::str::from_utf8(payload)?.into())
    };
    Ok(json!({"status":status,"type":kind,"retry":field("retry-after"),"body":value}))
}
pub fn cases() -> Vec<Value> {
    let mut rows = Vec::new();
    let mut add = |method: &str, path: &str, length: Option<String>, bytes: Vec<u8>| {
        rows.push(
            json!({"method":method,"path":path,"length":length,"data":STANDARD.encode(bytes)}),
        );
    };
    add("GET", "/health", None, vec![]);
    add("GET", "//health", None, vec![]);
    add("GET", "/health?x=1", None, vec![]);
    add("POST", "/unknown", None, vec![]);
    for length in [
        None,
        Some("0"),
        Some("16385"),
        Some("-1"),
        Some("bad"),
        Some("1_"),
        Some("+0"),
    ] {
        add("POST", "/v1/decisions", length.map(str::to_owned), vec![]);
    }
    for data in [b"null".as_slice(), b"{}", b"{", b"[]", b"true", b"\xff"] {
        add(
            "POST",
            "/v1/decisions",
            Some(data.len().to_string()),
            data.to_vec(),
        );
    }
    let mut bodies = vec![body("ok"), body(""), body("__budget_state")];
    let mut budget: Value = serde_json::from_slice(&body("ok")).unwrap_or(Value::Null);
    if let Some(o) = budget.as_object_mut() {
        o.insert("question".into(), Value::String("__budget_question".into()));
    }
    bodies.push(serde_json::to_vec(&budget).unwrap_or_default());
    for (key, count) in [
        ("state", 4000),
        ("state", 4001),
        ("question", 0),
        ("question", 500),
        ("question", 501),
    ] {
        for character in ["x", "\u{1f680}"] {
            let mut value: Value = serde_json::from_slice(&body("ok")).unwrap_or(Value::Null);
            if let Some(o) = value.as_object_mut() {
                o.insert(key.into(), Value::String(character.repeat(count)));
            }
            bodies.push(serde_json::to_vec(&value).unwrap_or_default());
        }
    }
    for options in [
        json!([]),
        json!([{"id":"x","label":"X"}]),
        json!([{"id":"x","label":"X"},{"id":"x","label":"Y"}]),
        json!([{"id":"X","label":"X"},{"id":"y","label":"Y"}]),
        json!([{"id":"x","label":""},{"id":"y","label":"Y"}]),
        json!([{"id":"x","label":"X","extra":true},{"id":"y","label":"Y"}]),
    ] {
        let mut v: Value = serde_json::from_slice(&body("ok")).unwrap_or(Value::Null);
        if let Some(o) = v.as_object_mut() {
            o.insert("options".into(), options);
        }
        bodies.push(serde_json::to_vec(&v).unwrap_or_default());
    }
    for count in [2, 8, 9] {
        let options: Vec<_> = (0..count)
            .map(|n| json!({"id":format!("o{n}"),"label":"Option"}))
            .collect();
        let mut v: Value = serde_json::from_slice(&body("ok")).unwrap_or(Value::Null);
        if let Some(o) = v.as_object_mut() {
            o.insert("options".into(), Value::Array(options));
        }
        bodies.push(serde_json::to_vec(&v).unwrap_or_default());
    }
    for id in [
        "a".repeat(32),
        "a".repeat(33),
        "å".into(),
        "a_1".into(),
        "a-b".into(),
    ] {
        let v = json!({"state":"ok","question":"?","options":[{"id":id,"label":"A"},{"id":"b","label":"B"}]});
        bodies.push(serde_json::to_vec(&v).unwrap_or_default());
    }
    for count in [120, 121] {
        for c in ["x", "🚀"] {
            let v = json!({"state":"ok","question":"?","options":[{"id":"a","label":c.repeat(count)},{"id":"b","label":"B"}]});
            bodies.push(serde_json::to_vec(&v).unwrap_or_default());
        }
    }
    let valid = String::from_utf8(body("ok")).unwrap_or_default();
    bodies.push(
        valid
            .replacen("\"state\":\"ok\"", "\"state\":NaN,\"state\":\"ok\"", 1)
            .into_bytes(),
    );
    for constant in ["Infinity", "-Infinity", "NaN.0", "-NaN", "1eNaN"] {
        bodies.push(
            valid
                .replacen(
                    "\"state\":\"ok\"",
                    &format!("\"state\":{constant},\"state\":\"ok\""),
                    1,
                )
                .into_bytes(),
        );
    }
    bodies.push(
        valid
            .replacen("\"state\":\"ok\"", "\"state\":\"\\ud800\"", 1)
            .into_bytes(),
    );
    bodies.push(
        valid
            .replacen("\"state\":\"ok\"", "\"state\":\"\\ud83d\\ude80\"", 1)
            .into_bytes(),
    );
    for count in [250, 251] {
        let prefix = valid.replacen("\"question\":\"Next?\"", "\"question\":\"RAW\"", 1);
        let (left, right) = prefix.split_once("RAW").unwrap_or(("", ""));
        let mut raw = left.as_bytes().to_vec();
        for _ in 0..count {
            raw.extend_from_slice(&[0xed, 0xa0, 0xbd, 0xed, 0xba, 0x80]);
        }
        raw.extend_from_slice(right.as_bytes());
        bodies.push(raw);
    }
    let mut utf16 = vec![0xff, 0xfe];
    for c in valid.encode_utf16() {
        utf16.extend_from_slice(&c.to_le_bytes());
    }
    bodies.push(utf16);
    let mut utf32 = vec![0, 0, 0xfe, 0xff];
    for c in valid.chars() {
        utf32.extend_from_slice(&u32::from(c).to_be_bytes());
    }
    bodies.push(utf32);
    for bytes in bodies {
        add(
            "POST",
            "/v1/decisions",
            Some(bytes.len().to_string()),
            bytes,
        );
    }
    rows
}
pub fn generate(source: &Path, output: &Path) -> Result<(), Error> {
    let directory = tempfile::tempdir()?;
    prepare(source, directory.path())?;
    let server = Server::start(directory.path())?;
    if std::env::var("CI").is_ok() && !server.production {
        return Err("mandatory Python3.12 oracle unavailable".into());
    }
    let mut rows = cases();
    for row in &mut rows {
        let bytes = STANDARD.decode(row.get("data").and_then(Value::as_str).ok_or("data")?)?;
        let expected = exchange(
            server.port,
            row.get("method").and_then(Value::as_str).ok_or("method")?,
            row.get("path").and_then(Value::as_str).ok_or("path")?,
            row.get("length").and_then(Value::as_str),
            &bytes,
        )?;
        row.as_object_mut()
            .ok_or("row")?
            .insert("expected".into(), expected);
    }
    std::fs::write(
        output,
        format!(
            "{}\n",
            serde_json::to_string_pretty(
                &json!({"services":REF,"server_sha256":SERVER_HASH,"cases":rows})
            )?
        ),
    )?;
    Ok(())
}
