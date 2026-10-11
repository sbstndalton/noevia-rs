//! Rust owns synthetic cases and provenance; Python executes immutable predecessor functions.
#![forbid(unsafe_code)]
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::Command,
    time::Duration,
};
#[path = "support/prompts_process.rs"]
mod process;
type Error = Box<dyn std::error::Error>;
const SOURCE: &str = "2360cb51f6475122ba0aa3d32272849a63028d66";
const LOADER: &str = r#"
import ast,hashlib,importlib.util,json,sqlite3,sys,types,unicodedata,tempfile
from pathlib import Path
import fastapi,pydantic,pydantic_core,starlette
from fastapi import Body,FastAPI,HTTPException
from fastapi.testclient import TestClient
source=Path(sys.argv[1]);root=Path(sys.argv[2])
app=types.ModuleType('app');app.__path__=[str(source)];sys.modules['app']=app
config=types.ModuleType('app.config');config.settings=types.SimpleNamespace(data_dir=root);sys.modules['app.config']=config
spec=importlib.util.spec_from_file_location('app.db',source/'db.py');db=importlib.util.module_from_spec(spec);sys.modules['app.db']=db;spec.loader.exec_module(db)
db.time.time=lambda:1700000000.125
tree=ast.parse((source/'api.py').read_text(encoding='utf-8'))
tree.body=[n for n in tree.body if isinstance(n,ast.FunctionDef) and n.name in ('prompts','add_prompt','delete_prompt')]
for n in tree.body:n.decorator_list=[]
api=types.ModuleType('api');api.__dict__.update(db=db,Body=Body,HTTPException=HTTPException);exec(compile(tree,str(source/'api.py'),'exec'),api.__dict__)
http=FastAPI();http.get('/api/v1/prompts')(api.prompts);http.post('/api/v1/prompts')(api.add_prompt);http.delete('/api/v1/prompts/{pid}')(api.delete_prompt)
client=TestClient(http,raise_server_exceptions=False)
def call(q):
 raw=bytes.fromhex(q['body_hex']) if 'body_hex' in q else q.get('body','')
 content_type=q.get('content_type','application/json');headers={} if content_type is None else {'Content-Type':content_type}
 response=client.request(q['method'],q['path'],content=raw,headers=headers,follow_redirects=False)
 try:body=response.json()
 except ValueError:body=response.text
 return {'status':response.status_code,'body':body,'location':response.headers.get('location')}
db.init()
print(json.dumps({'python':sys.version,'sqlite':sqlite3.sqlite_version,'fastapi':fastapi.__version__,'pydantic':pydantic.__version__,'pydantic_core':pydantic_core.__version__,'starlette':starlette.__version__,'unicode':unicodedata.unidata_version,'seed':db._BENCH_PROMPTS}),flush=True)
for line in sys.stdin:
 q=json.loads(line)
 if q.get('op')=='printability':
  ranges=[];start=None
  for cp in range(0x110000):
   nonprint=not chr(cp).isprintable()
   if nonprint and start is None:start=cp
   if not nonprint and start is not None:ranges.append([start,cp-1]);start=None
  if start is not None:ranges.append([start,0x10ffff])
  answer={'nonprintable':ranges}
 elif q.get('op') in ('ordering','seedcase'):
  previous=db._DB_PATH
  with tempfile.TemporaryDirectory(dir=root) as temp:
   db._DB_PATH=Path(temp)/'model_loader.db';db.init()
   for name in q.get('names',[]):db.add_prompt(name,'synthetic')
   if q.get('indexed'):
    with db._conn() as c:c.execute('CREATE INDEX nocase_desc ON prompts(name COLLATE NOCASE,id DESC)')
   if q.get('op')=='seedcase':
    if q.get('flag') is not None:db.set_setting(db._BENCH_SEED_KEY,q['flag'])
    added=db.seed_bench_prompts();answer={'added':added,'prompts':db.list_prompts(),'flag':db.get_setting(db._BENCH_SEED_KEY,'')}
   else:answer={'prompts':db.list_prompts()}
  db._DB_PATH=previous
 elif q.get('op')=='coercion':
  captured=[];add=db.add_prompt;listing=db.list_prompts
  db.add_prompt=lambda name,body:captured.append([name,body]) or 1;db.list_prompts=lambda:[]
  try:
   result=api.add_prompt(json.loads(q['body']));answer={'status':200,'fields':captured[0]}
  except HTTPException as e:answer={'status':e.status_code,'detail':e.detail}
  finally:db.add_prompt=add;db.list_prompts=listing
 elif q.get('op')=='seed':
  answer={'added':db.seed_bench_prompts(),'prompts':db.list_prompts(),'flag':db.get_setting(db._BENCH_SEED_KEY,'')}
 elif q.get('op')=='restriction':
  previous=db._DB_PATH
  with tempfile.TemporaryDirectory(dir=root) as temp:
   db._DB_PATH=Path(temp)/'model_loader.db';db.init();answer=call(q['request'])
  db._DB_PATH=previous
 else:answer=call(q)
 print(json.dumps(answer,ensure_ascii=True,allow_nan=False),flush=True)
"#;

fn answer(reader: &mut impl BufRead) -> Result<Value, Error> {
    let mut bytes = Vec::new();
    bytes.try_reserve(1024)?;
    reader.take(16_777_217).read_until(b'\n', &mut bytes)?;
    if bytes.is_empty() || bytes.len() > 16_777_216 || bytes.last() != Some(&b'\n') {
        return Err("oracle response missing or exceeded cap".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn main() -> Result<(), Error> {
    let mut args = std::env::args_os().skip(1);
    let source = PathBuf::from(args.next().ok_or("immutable app path required")?);
    let output = PathBuf::from(args.next().ok_or("output path required")?);
    let literals = args.next().map(PathBuf::from);
    if args.next().is_some() {
        return Err("unexpected arguments".into());
    }
    let mut hashes = serde_json::Map::new();
    for (name, expected) in [
        ("db.py", "b6fdb19a39a76355aced71288b004e9982a86815"),
        ("api.py", "c624647e3e23870f9abfad41d78797056ac21417"),
        ("main.py", "b206d1919a9e8e6aed7247e45a60eec30bfe3e63"),
        ("config.py", "914c6c17275e8418568e662c4d570cdb19dc3c7e"),
    ] {
        // Verify Git's actual blob identity before importing any oracle source.
        let path = source.join(name);
        let mut command = Command::new("git");
        command.args(["hash-object"]).arg(&path);
        let actual = process::bounded(command, Vec::new(), Duration::from_secs(5))?;
        if !actual.status.success() || std::str::from_utf8(&actual.stdout)?.trim() != expected {
            return Err(format!("oracle source identity mismatch: {name}").into());
        }
        hashes.insert(name.into(), json!({"git_blob":expected,"sha256":format!("{:x}",Sha256::digest(std::fs::read(path)?))}));
    }
    let mut requests = vec![json!({"op":"printability"}), json!({"op":"seed"})];
    for body in [
        "",
        "null",
        "[]",
        "true",
        "12",
        "\"text\"",
        "{",
        "{}",
        r#"{"name":"\u001c 日本語 😀 \u3000","body":"\u001f text \u001e"}"#,
        r#"{"name":true,"body":["it's","both'\"",null,true,false,0,-0.0,1e16,1e-5]}"#,
        r#"{"name":{"z":1,"a":2,"z":3},"body":{"β":["\u200b","\u2028","😀"]}}"#,
        r#"{"name":1234567890123456789012345678901234567890,"body":1.2345e100}"#,
        r#"{"name":0,"body":"x"}"#,
        r#"{"name":[],"body":"x"}"#,
        r#"{"name":{},"body":"x"}"#,
        r#"{"name":false,"body":"x"}"#,
        r#"{"name":"first","name":"last","body":"kept","unused":"ignored"}"#,
    ] {
        requests.push(json!({"method":"POST","path":"/api/v1/prompts","body":body}));
    }
    for body in [
        "{",
        "[",
        "{\"a\":}",
        "{\"a\":1,}",
        "[1,]",
        "[1 2]",
        "{\"a\" 1}",
        "{\"a\":01}",
        "01",
        "1.",
        "1e+",
        "-",
        "\"unterminated",
        "\"日本語",
        "\"\\q\"",
        "\"\\u12zz\"",
        "\"\\u12\"",
        "{\"日本語\": ",
        "\"line\nfeed\"",
        "true extra",
    ] {
        requests.push(
            json!({"name":"json-diagnostic","method":"POST","path":"/api/v1/prompts","body":body}),
        );
    }
    let raw = r#"{"name":"encoding 日本語 😀","body":"plain"}"#;
    let mut encoded = vec![
        raw.as_bytes().to_vec(),
        [&[0xef, 0xbb, 0xbf][..], raw.as_bytes()].concat(),
    ];
    for little in [true, false] {
        let mut utf16 = Vec::new();
        let mut utf32 = Vec::new();
        for unit in raw.encode_utf16() {
            utf16.extend_from_slice(&if little {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        for character in raw.chars() {
            utf32.extend_from_slice(&if little {
                (character as u32).to_le_bytes()
            } else {
                (character as u32).to_be_bytes()
            });
        }
        encoded.push(utf16.clone());
        encoded.push(utf32.clone());
        let mut with_bom = if little {
            vec![0xff, 0xfe]
        } else {
            vec![0xfe, 0xff]
        };
        with_bom.extend(utf16);
        encoded.push(with_bom);
        let mut with_bom = if little {
            vec![0xff, 0xfe, 0, 0]
        } else {
            vec![0, 0, 0xfe, 0xff]
        };
        with_bom.extend(utf32);
        encoded.push(with_bom);
    }
    encoded.extend([
        vec![0xef, 0xbb, 0xbf],
        vec![0xff, 0xfe],
        vec![0xff, 0xfe, 0, 0],
        vec![0, 0, 0xfe, 0xff],
        vec![0, b'{', 0],
        vec![0xff],
        vec![0xc0, 0xaf],
        vec![0xff, 0xfe, b'{'],
        b"{\"name\":NaN,\"body\":\"x\"}".to_vec(),
        b"{\"name\":Infinity,\"body\":\"x\"}".to_vec(),
        b"{\"name\":-Infinity,\"body\":\"x\"}".to_vec(),
    ]);
    encoded.push([&[0xef, 0xbb, 0xbf, 0xef, 0xbb, 0xbf][..], raw.as_bytes()].concat());
    for bytes in encoded {
        let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        requests.push(
            json!({"name":"raw-encoding","method":"POST","path":"/api/v1/prompts","body_hex":hex}),
        );
    }
    for content_type in [
        None,
        Some("application/json"),
        Some("application/json; charset=utf-16"),
        Some("application/problem+json"),
        Some("application/JSON"),
        Some("text/plain"),
        Some("application/octet-stream"),
        Some("application/x-www-form-urlencoded"),
    ] {
        requests.push(json!({"method":"POST","path":"/api/v1/prompts","body":raw,"content_type":content_type}));
    }
    for body in [
        " ",
        "{\"x\":}",
        "[1,]",
        "{\"x\":1,}",
        "{\"x\" 1}",
        "{\"x\":1 2}",
        "null extra",
        "\"unfinished",
        "\"\\q\"",
        "\"\\u00xx\"",
        "\"raw\nline\"",
        "{\"β\":}",
    ] {
        requests.push(json!({"method":"POST","path":"/api/v1/prompts","body":body}));
    }
    for id in [
        "1",
        "01",
        "+2",
        "-1",
        "0",
        " 3 ",
        "4.0",
        "4.00",
        "4e0",
        "1.",
        "1.0_0",
        "1_0.0",
        "01.00",
        "-0.0",
        "1__0",
        "+_1",
        "\u{a0}99\u{a0}",
        "\u{2003}99\u{3000}",
        "99%1C",
        "1_0",
        "１２",
        "١",
        "9223372036854775808",
        "%FF",
        "%2F",
        "%",
        "4/",
    ] {
        requests.push(json!({"method":"DELETE","path":format!("/api/v1/prompts/{id}")}));
    }
    for prefix in ["", "+", "-", " ", "\u{85}", "\u{a0}"] {
        for number in [
            "999", "000999", "9_9_9", "9__99", "999.0", "999.00", "999.01", "999e0", "999.",
        ] {
            for suffix in ["", " ", "\u{a0}", "%1F"] {
                requests.push(json!({"method":"DELETE","path":format!("/api/v1/prompts/{prefix}{number}{suffix}")}));
            }
        }
    }
    for method in ["GET", "POST", "DELETE", "PUT", "HEAD", "OPTIONS"] {
        for path in [
            "/api/v1/prompts",
            "/api/v1/prompts/",
            "/api/v1/prompts/99",
            "/api/v1/prompts/99/",
            "/api/v1/%70rompts",
        ] {
            requests.push(json!({"method":method,"path":path,"body":"{}"}));
        }
    }
    // Stateless calls to the actual API function preserve coercion policy without bloating
    // the live SQLite sequence. Rust generates deterministic broad finite-float cases.
    let mut bits = 0x9e3779b97f4a7c15u64;
    for index in 0..512usize {
        bits ^= bits << 13;
        bits ^= bits >> 7;
        bits ^= bits << 17;
        let number = f64::from_bits(bits);
        if !number.is_finite() {
            continue;
        }
        let character = char::from_u32((bits % 0x110000) as u32).unwrap_or('\u{fffd}');
        let body = serde_json::to_string(
            &json!({"name":[number,format!("{character}'\"\\")],"body":{"z":[number,true,null],"a":index}}),
        )?;
        requests.push(json!({"op":"coercion","body":body}));
    }
    for indexed in [false, true] {
        requests.push(json!({"op":"ordering","indexed":indexed,"names":["a","A","a","b","B","Ä","ä","é","É"]}));
    }
    for (flag, names) in [
        (None, vec![]),
        (Some(""), vec![]),
        (Some("0"), vec![]),
        (Some("false"), vec![]),
        (None, vec!["bench: coding"]),
        (None, vec!["Bench: coding"]),
        (
            None,
            vec![
                "Bench: creative writing",
                "Bench: coding",
                "Bench: reasoning",
                "Bench: summarise long input",
            ],
        ),
    ] {
        requests.push(json!({"op":"seedcase","flag":flag,"names":names}));
    }
    for body in [
        "NaN",
        "Infinity",
        "-Infinity",
        "[NaN]",
        r#"{"name":"\ud800","body":"x"}"#,
        r#"{"name":["\ud800"],"body":"x"}"#,
    ] {
        requests.push(json!({"op":"restriction","name":"nonrepresentable-or-nonfinite-validation","request":{"method":"POST","path":"/api/v1/prompts","body":body},"native_refusal":{"status":400,"body":{"detail":"invalid prompt input"}}}));
    }
    requests.push(json!({"op":"restriction","name":"non-json-undecodable-bytes","request":{"method":"POST","path":"/api/v1/prompts","body_hex":"ff","content_type":"text/plain"},"native_refusal":{"status":400,"body":{"detail":"There was an error parsing the body"}}}));
    for nested in [false, true] {
        let prefix = if nested {
            "{\"name\":[\""
        } else {
            "{\"name\":\""
        };
        let suffix = if nested {
            "\"],\"body\":\"x\"}"
        } else {
            "\",\"body\":\"x\"}"
        };
        let units = prefix
            .encode_utf16()
            .chain(std::iter::once(0xd800))
            .chain(suffix.encode_utf16());
        let mut bytes = vec![0xff, 0xfe];
        for unit in units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        requests.push(json!({"op":"restriction","name":"encoded-lone-surrogate","request":{"method":"POST","path":"/api/v1/prompts","body_hex":hex},"native_refusal":{"status":400,"body":{"detail":"There was an error parsing the body"}}}));
    }
    let expanded = format!(
        "{{\"name\":\"x\",\"body\":\"x\",\"unused\":\"{}\"}}",
        "汉".repeat(349_600)
    );
    let encoded = expanded
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    requests.push(json!({"op":"restriction","name":"decoded-json-byte-cap","request":{"method":"POST","path":"/api/v1/prompts","body_hex":encoded},"native_refusal":{"status":400,"body":{"detail":"prompt text exceeded limit"}}}));
    let mut input = Vec::new();
    for request in &mut requests {
        // HTTPX and WHATWG URL clients differ on raw trailing spaces; use identical wire paths.
        if let Some(path) = request.get_mut("path") {
            if let Some(raw) = path.as_str() {
                *path = json!(raw.replace(' ', "%20"));
            }
        }
        serde_json::to_writer(&mut input, request)?;
        input.write_all(b"\n")?;
        if input.len() > process::INPUT {
            return Err("oracle input cap".into());
        }
    }
    let root = tempfile::tempdir()?;
    let mut command = Command::new(std::env::var_os("PYTHON").ok_or("PYTHON required")?);
    command.args(["-c", LOADER]).arg(&source).arg(root.path());
    let result = process::bounded(command, input, Duration::from_secs(60))?;
    if !result.status.success() {
        return Err("actual oracle process failed".into());
    }
    if !result.stderr.is_empty() {
        return Err("actual oracle emitted unexpected diagnostic".into());
    }
    let mut reader = BufReader::new(result.stdout.as_slice());
    let versions = answer(&mut reader)?;
    if !versions
        .get("python")
        .and_then(Value::as_str)
        .is_some_and(|v| v.starts_with("3.12."))
        || versions.get("fastapi").and_then(Value::as_str) != Some("0.115.6")
        || versions.get("unicode").and_then(Value::as_str) != Some("15.0.0")
    {
        return Err("oracle must be Python 3.12, Unicode15.0, FastAPI0.115.6".into());
    }
    let mut cases = Vec::new();
    cases.try_reserve(requests.len())?;
    for (index, request) in requests.into_iter().enumerate() {
        cases.push(json!({"id":index,"request":request,"expected":answer(&mut reader)?}));
    }
    if !reader.fill_buf()?.is_empty() {
        return Err("unexpected oracle output".into());
    }
    if let Some(directory) = literals {
        std::fs::create_dir_all(&directory)?;
        let mut code = String::from("// Generated from immutable services2360cb, CPython3.12 Unicode15.0; regenerate with generate-prompts-oracle.\n\n");
        code.push_str("pub const SEED: &[(&str, &str)] = &[\n");
        for row in versions
            .get("seed")
            .and_then(Value::as_array)
            .ok_or("seed missing")?
        {
            let name = row
                .get(0)
                .and_then(Value::as_str)
                .ok_or("seed name missing")?;
            let body = row
                .get(1)
                .and_then(Value::as_str)
                .ok_or("seed body missing")?;
            code.push_str(&format!("    ({name:?}, {body:?}),\n"));
        }
        code.push_str("];\n\npub const NONPRINTABLE: &[(u32, u32)] = &[\n");
        for range in cases
            .first()
            .and_then(|c| c.get("expected"))
            .and_then(|e| e.get("nonprintable"))
            .and_then(Value::as_array)
            .ok_or("printability missing")?
        {
            let start = range
                .get(0)
                .and_then(Value::as_u64)
                .ok_or("range start missing")?;
            let end = range
                .get(1)
                .and_then(Value::as_u64)
                .ok_or("range end missing")?;
            code.push_str(&format!("    (0x{start:x}, 0x{end:x}),\n"));
        }
        code.push_str("];\n");
        std::fs::write(directory.join("literals.rs"), code)?;
    }
    let bytes = serde_json::to_vec_pretty(
        &json!({"services":SOURCE,"sources":hashes,"versions":versions,"generator_sha256":format!("{:x}",Sha256::digest(include_bytes!("generate-prompts-oracle.rs"))),"process_sha256":format!("{:x}",Sha256::digest(include_bytes!("support/prompts_process.rs"))),"image_digest":null,"cases":cases}),
    )?;
    std::fs::write(&output, &bytes)?;
    println!("{} {:x}", output.display(), Sha256::digest(&bytes));
    Ok(())
}
