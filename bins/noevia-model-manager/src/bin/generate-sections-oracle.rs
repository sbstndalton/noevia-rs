//! Synthetic case/filesystem/header generation and serialization belong to Rust.
#![forbid(unsafe_code)]
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
type Error = Box<dyn std::error::Error>;
const LOADER: &str = r#"
import ast,dataclasses,hashlib,json,os,re,sys,types
from pathlib import Path
from fastapi import HTTPException,FastAPI
from fastapi.testclient import TestClient
source=Path(sys.argv[1])
app=types.ModuleType('app');app.__path__=[str(source)];sys.modules['app']=app
settings=types.SimpleNamespace(gguf_parser='python')
config=types.ModuleType('app.config');config.settings=settings;sys.modules['app.config']=config
for name in ('db','hw','services'):
 m=types.ModuleType('app.'+name);sys.modules['app.'+name]=m
def load(name,selected=None):
 path=source/(name+'.py');tree=ast.parse(path.read_text(encoding='utf-8'),str(path))
 if selected is not None:
  tree.body=[n for n in tree.body if isinstance(n,ast.ImportFrom) and n.module=='__future__' or isinstance(n,(ast.FunctionDef,ast.ClassDef)) and n.name in selected or isinstance(n,ast.Assign) and any(isinstance(t,ast.Name) and t.id in selected for t in n.targets)]
  for n in tree.body:
   if isinstance(n,ast.FunctionDef):n.decorator_list=[]
 m=types.ModuleType('app.'+name);m.__package__='app';sys.modules[m.__name__]=m
 m.__dict__.update(Path=Path,re=re,settings=settings,dataclasses=dataclasses,hashlib=hashlib,HTTPException=HTTPException)
 exec(compile(tree,str(path),'exec'),m.__dict__);return m
core=load('autoconfig_core',{'_ROPE_FROM_GGUF_ARCHS','rope_owned_by_gguf','HEAD_MAX_BYTES','_looks_like_draft'})
discover=load('discover',{'IMATRIX'})
autoconfig=load('autoconfig',{'_head_sized'})
autoconfig.HEAD_MAX_BYTES=core.HEAD_MAX_BYTES;autoconfig._looks_like_draft=core._looks_like_draft
utils=load('utils');gguf_meta=load('gguf_meta');ini=load('ini');helpers=load('helpers')
api=load('api',{'_plain','revision','_field','_schema','sections','section'})
api.ini=ini
http=FastAPI();http.get('/api/v1/sections')(api.sections);http.get('/api/v1/sections/{name}')(api.section)
client=TestClient(http,raise_server_exceptions=False)
for line in sys.stdin:
 request=json.loads(line);settings.models_dir=Path(request['root']);settings.models_ini_path=settings.models_dir/'models.ini'
 try:
  if 'httpPath' in request:
   response=client.request(request['method'],request['httpPath'],follow_redirects=False)
   try:body=response.json()
   except ValueError:body=response.text
   print(json.dumps({'status':response.status_code,'body':body,'location':response.headers.get('location')},ensure_ascii=False,allow_nan=False),flush=True);continue
  result=api.sections() if request['name'] is None else api.section(request['name'],request['defaults'])
  print(json.dumps({'response':result},ensure_ascii=False,allow_nan=False),flush=True)
 except HTTPException as e:print(json.dumps({'status':e.status_code,'detail':e.detail}),flush=True)
 except Exception as e:print(json.dumps({'status':500,'errorType':type(e).__name__}),flush=True)
"#;
fn string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u64).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}
fn gguf(arch: &str, ctx: u32, extras: bool) -> Vec<u8> {
    let mut pairs = vec![
        ("general.architecture".to_owned(), json!(arch)),
        (format!("{arch}.context_length"), json!(ctx)),
    ];
    if extras {
        pairs.extend([
            (format!("{arch}.expert_count"), json!(8)),
            (format!("{arch}.expert_used_count"), json!(2)),
            (format!("{arch}.rope.scaling.type"), json!("yarn")),
            (
                format!("{arch}.rope.scaling.original_context_length"),
                json!(4096),
            ),
            ("tokenizer.chat_template".into(), json!("{{ messages }}")),
        ]);
    }
    gguf_pairs(pairs)
}
fn gguf_pairs(pairs: Vec<(String, Value)>) -> Vec<u8> {
    let mut out = b"GGUF".to_vec();
    out.extend_from_slice(&3u32.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&(pairs.len() as u64).to_le_bytes());
    for (key, value) in pairs {
        string(&mut out, &key);
        if let Some(s) = value.as_str() {
            out.extend_from_slice(&8u32.to_le_bytes());
            string(&mut out, s);
        } else if let Some(b) = value.as_bool() {
            out.extend_from_slice(&7u32.to_le_bytes());
            out.push(u8::from(b));
        } else if let Some(values) = value.as_array() {
            out.extend_from_slice(&9u32.to_le_bytes());
            out.extend_from_slice(&8u32.to_le_bytes());
            out.extend_from_slice(&(values.len() as u64).to_le_bytes());
            for item in values {
                string(&mut out, item.as_str().unwrap_or_default());
            }
        } else if let Some(number) = value.as_u64() {
            out.extend_from_slice(&4u32.to_le_bytes());
            out.extend_from_slice(&(number as u32).to_le_bytes());
        } else if let Some(number) = value.as_f64() {
            out.extend_from_slice(&12u32.to_le_bytes());
            out.extend_from_slice(&number.to_le_bytes());
        }
    }
    out
}
fn materialize(root: &Path, ini: Option<&str>, files: &[(String, Vec<u8>)]) -> Result<(), Error> {
    if let Some(ini) = ini {
        std::fs::write(root.join("models.ini"), ini)?;
    }
    for (name, bytes) in files {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().ok_or("parent")?)?;
        std::fs::write(&path, bytes)?;
        let file = std::fs::File::options().write(true).open(&path)?;
        if let Some(size) = synthetic_size(name) {
            file.set_len(size)?;
        }
        if name.starts_with("models.ini.bak-") {
            file.set_times(std::fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_200_000_000),
            ))?;
        }
    }
    Ok(())
}
fn synthetic_size(name: &str) -> Option<u64> {
    match name {
        "below/tiny-MTP.gguf" => Some(2 * 1024 * 1024 * 1024 - 1),
        "equal/tiny-MTP.gguf" => Some(2 * 1024 * 1024 * 1024),
        "above/tiny-MTP.gguf" => Some(2 * 1024 * 1024 * 1024 + 1),
        _ => None,
    }
}
fn main() -> Result<(), Error> {
    let mut args = std::env::args_os().skip(1);
    let source = PathBuf::from(args.next().ok_or("actual app path")?);
    let output = PathBuf::from(args.next().ok_or("output path")?);
    if args.next().is_some() {
        return Err("unexpected arguments".into());
    }
    let mut child = Command::new(std::env::var_os("PYTHON").unwrap_or_else(|| "python3".into()))
        .args(["-c", LOADER])
        .arg(&source)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or("stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or("stdout")?);
    let plain = gguf("llama", 8192, false);
    let rich = gguf("gemma3", 32768, true);
    type Case = (
        String,
        Option<String>,
        Vec<(String, Vec<u8>)>,
        Vec<(Option<String>, bool)>,
    );
    let mut cases: Vec<Case> = Vec::new();
    let ini_inputs:[Option<&str>;12]=[None,Some("version = 1\n"),Some("[tiny]\nctx-size=4096\nparallel=1\nmain-gpu=0\n"),Some("[DEFAULT]\nparallel=1\nMiXeD=default\n[tiny]\nctx-size=8192\nMiXeD=override\n[other]\n"),Some("top=one\n;pre\n[tiny]\nfoo: x=y\nBAR = A # literal\n"),Some("[tiny]\na=first\nb=second\na=last\n[tiny]\nc=third\n"),Some("[tiny]\nunknown=one\n two\n\n three\n"),Some("[tiny]\njinja=no\nswa-full=yes\nparallel=0\nfoo=1\nbar=0\n"),Some("[tiny]\nmodel=missing.gguf\nchat-template=a'b \"c\"\n"),Some("[alias]\nmodel = /models/sub/main.gguf\n[shared]\nmodel=sub/main.gguf\n"),Some("[DEFAULT]\na=1\n[tiny]\nb=2\n"),Some("[tiny]\nMiXeD=ONE\nmixed=two\n")];
    for (i, ini) in ini_inputs.into_iter().enumerate() {
        cases.push((
            format!("ini-{i}"),
            ini.map(str::to_owned),
            vec![("tiny.gguf".into(), plain.clone())],
            vec![
                (None, false),
                (Some("tiny".into()), false),
                (Some("tiny".into()), true),
                (Some("absent".into()), false),
            ],
        ));
    }
    for (i, text) in [
        "preamble\u{2028}[tiny]\nparallel=1\n",
        "[tiny]\n\u{001c}unknown\u{001f}=\u{001c}value\u{001f}\n",
        "[tiny]\nunknown=one\n\t two\n#comment\n three\n",
        "[tiny]\nfoo = one\n\n[tiny]\nbar = two\n",
        "[broken\nkey=value\n",
        "[tiny]\nno delimiter\n",
        "preamble\r\n[tiny]\r\nparallel=1\r\n",
        "preamble\r[tiny]\rparallel=1\r",
    ]
    .into_iter()
    .enumerate()
    {
        cases.push((
            format!("dialect-{i}"),
            Some(text.into()),
            vec![("tiny.gguf".into(), plain.clone())],
            vec![(None, false), (Some("tiny".into()), false)],
        ));
    }
    for (i, files) in [
        vec![
            ("sub/main.gguf", rich.clone()),
            ("sub/mmproj.gguf", plain.clone()),
        ],
        vec![
            ("sub/tiny-00001-of-00002.gguf", rich.clone()),
            ("sub/tiny-00002-of-00002.gguf", plain.clone()),
            ("sub/mmproj.gguf", plain.clone()),
        ],
        vec![
            ("tiny.gguf", plain.clone()),
            ("a/tiny.gguf", rich.clone()),
            ("a/b/c/d/deep.gguf", plain.clone()),
            ("a/b/c/d/e/too-deep.gguf", plain.clone()),
            ("blobs/hidden.gguf", plain.clone()),
            (".hidden/hidden.gguf", plain.clone()),
        ],
        vec![
            ("draft-tiny.gguf", plain.clone()),
            ("tiny-MTP.gguf", plain.clone()),
            ("tiny-noMTP.gguf", plain.clone()),
            ("tiny-imatrix.gguf", plain.clone()),
            ("tiny-mmproj.gguf", plain.clone()),
        ],
    ]
    .into_iter()
    .enumerate()
    {
        cases.push((
            format!("layout-{i}"),
            Some("[tiny]\nparallel=1\n".into()),
            files.into_iter().map(|(n, b)| (n.into(), b)).collect(),
            vec![
                (None, false),
                (Some("tiny".into()), false),
                (Some("tiny".into()), true),
            ],
        ));
    }
    cases.push((
        "explicit-alias".into(),
        Some("[alias]\nmodel=/models/sub/main.gguf\n".into()),
        vec![("sub/main.gguf".into(), rich.clone())],
        vec![
            (None, false),
            (Some("alias".into()), false),
            (Some("alias".into()), true),
        ],
    ));
    cases.push((
        "malformed-magic".into(),
        Some("[tiny]\n".into()),
        vec![("tiny.gguf".into(), b"NOPE".to_vec())],
        vec![(Some("tiny".into()), true)],
    ));
    cases.push((
        "backups".into(),
        Some("[tiny]\n".into()),
        vec![
            ("models.ini.bak-z".into(), b"z".to_vec()),
            ("models.ini.bak-a".into(), b"old".to_vec()),
            ("models.ini.bak-mid".into(), b"mid".to_vec()),
        ],
        vec![(None, false)],
    ));
    cases.push((
        "head-size-boundary".into(),
        None,
        vec![
            ("below/tiny-MTP.gguf".into(), plain.clone()),
            ("equal/tiny-MTP.gguf".into(), plain.clone()),
            ("above/tiny-MTP.gguf".into(), plain.clone()),
        ],
        vec![(None, false)],
    ));
    cases.push((
        "path-suffix-sibling-defaults".into(),
        Some("[tiny]\n".into()),
        vec![
            ("sub/.gguf".into(), rich.clone()),
            ("sub/tiny.GGUF".into(), plain.clone()),
            ("sub/mmproj.GGUF".into(), plain.clone()),
        ],
        vec![(Some("tiny".into()), true), (None, false)],
    ));
    for (index, number) in [1e39, -1e39, 1e308].into_iter().enumerate() {
        cases.push((
            format!("finite-big-integer-{index}"),
            None,
            vec![(
                "tiny.gguf".into(),
                gguf_pairs(vec![
                    ("general.architecture".into(), json!("llama")),
                    ("llama.context_length".into(), json!(number)),
                    (
                        "llama.rope.scaling.original_context_length".into(),
                        json!(number),
                    ),
                    ("llama.expert_count".into(), json!(number)),
                    ("llama.expert_used_count".into(), json!(2)),
                    ("llama.rope.scaling.type".into(), json!("yarn")),
                ]),
            )],
            vec![(Some("tiny".into()), true)],
        ));
    }
    for (i, pairs) in [
        vec![
            ("general.architecture", json!("llama")),
            ("llama.context_length", json!(0)),
        ],
        vec![
            ("general.architecture", json!("gemma3")),
            ("gemma3.context_length", json!(8192)),
            ("gemma3.rope.scaling.original_context_length", json!(4096)),
        ],
        vec![
            ("general.architecture", json!("llama")),
            ("llama.context_length", json!(1024)),
            ("llama.rope.scaling.type", json!("none")),
            ("llama.rope.scaling.factor", json!(2.0)),
            ("llama.rope.scaling.original_context_length", json!(1000000)),
        ],
        vec![
            ("general.architecture", json!("gemma2")),
            ("gemma2.expert_count", json!(8)),
            ("gemma2.expert_used_count", json!(0)),
            ("tokenizer.chat_template", json!("")),
        ],
        vec![
            ("general.architecture", json!("llama")),
            ("llama.rope.scaling.type", json!(["yarn"])),
            ("llama.rope.scaling.original_context_length", json!(4096)),
        ],
        vec![
            ("general.architecture", json!("llama")),
            ("llama.rope.scaling.type", json!("\u{1c}none\u{1f}")),
            ("llama.rope.scaling.original_context_length", json!(4096)),
        ],
    ]
    .into_iter()
    .enumerate()
    {
        cases.push((
            format!("metadata-{i}"),
            None,
            vec![(
                "tiny.gguf".into(),
                gguf_pairs(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect()),
            )],
            vec![(Some("tiny".into()), true)],
        ));
    }
    let mut fixtures = Vec::new();
    for (id, ini, files, requests) in cases {
        let temporary = tempfile::tempdir()?;
        materialize(temporary.path(), ini.as_deref(), &files)?;
        let manifest: Vec<Value> = files
            .iter()
            .map(|(name, bytes)| json!({"name":name,"bytes":bytes,"size":synthetic_size(name)}))
            .collect();
        for (index, (name, defaults)) in requests.into_iter().enumerate() {
            serde_json::to_writer(
                &mut stdin,
                &json!({"root":temporary.path(),"name":name,"defaults":defaults}),
            )?;
            stdin.write_all(b"\n")?;
            stdin.flush()?;
            let mut answer = String::new();
            if stdout.read_line(&mut answer)? == 0 {
                return Err("oracle ended".into());
            }
            if answer.len() > 8 * 1024 * 1024 {
                return Err("oracle response cap".into());
            }
            let expected: Value = serde_json::from_str(&answer)?;
            fixtures.push(json!({"id":format!("{id}-{index}"),"ini":ini,"files":manifest,"name":name,"defaults":defaults,"expected":expected}));
        }
    }
    let temp = tempfile::tempdir()?;
    materialize(
        temp.path(),
        Some("[tiny]\nparallel=1\n"),
        &[("tiny.gguf".into(), plain)],
    )?;
    let mut http = Vec::new();
    for (method, path) in [
        ("GET", "/api/v1/sections/tiny?defaults=true"),
        ("GET", "/api/v1/sections/tiny?defaults=Y"),
        ("GET", "/api/v1/sections/tiny?defaults=off"),
        ("GET", "/api/v1/sections/tiny?defaults=bad"),
        ("GET", "/api/v1/sections/tiny?defaults=false&defaults=true"),
        ("GET", "/api/v1/sections/tiny?defaults=true&defaults=bad"),
        ("GET", "/api/v1/sections?defaults=bad"),
        ("GET", "/api/v1/sections/"),
        ("GET", "/api/v1/sections/tiny/"),
        ("GET", "/api/v1/sections/tiny%2Fother"),
        ("GET", "/api/v1/sections/tiny%2Fother?defaults=bad"),
        ("GET", "/api/v1/sections/bad!name?defaults=bad"),
        ("GET", "/api/v1/sections/bad!name?defaults=true"),
        ("GET", "/api/v1/sections/%FF?defaults=bad"),
        ("GET", "/api/v1/sections/%FF"),
        ("GET", "/api/v1/sections/tiny%00"),
        ("HEAD", "/api/v1/sections/tiny"),
    ] {
        serde_json::to_writer(
            &mut stdin,
            &json!({"root":temp.path(),"name":null,"defaults":false,"httpPath":path,"method":method}),
        )?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        let mut answer = String::new();
        if stdout.read_line(&mut answer)? == 0 {
            return Err("http oracle ended".into());
        }
        let expected: Value = serde_json::from_str(&answer)?;
        http.push(json!({"method":method,"path":path,"expected":expected}));
    }
    drop(stdin);
    if !child.wait()?.success() {
        return Err("oracle failed".into());
    }
    let mut sources = serde_json::Map::new();
    for name in [
        "api.py",
        "ini.py",
        "utils.py",
        "helpers.py",
        "autoconfig.py",
        "autoconfig_core.py",
        "gguf_meta.py",
        "discover.py",
    ] {
        sources.insert(
            name.into(),
            json!(format!(
                "{:x}",
                Sha256::digest(std::fs::read(source.join(name))?)
            )),
        );
    }
    let bytes = serde_json::to_vec_pretty(
        &json!({"services":"641c9c342256de69251097d4839d05ee41044fe0","sources":sources,"cases":fixtures,"http":http}),
    )?;
    std::fs::write(&output, &bytes)?;
    println!("{} {:x}", output.display(), Sha256::digest(&bytes));
    Ok(())
}
