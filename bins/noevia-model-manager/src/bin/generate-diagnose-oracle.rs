//! Synthetic fixtures from one immutable pure Python module; no application imports.
//! Env: PYTHON (Python 3.12 interpreter) and NOEVIA_DIAGNOSE_SERVICES_SOURCE (a services
//! checkout containing REF). Only the pure `analyse` module and the actual `redact_log_line`
//! AST of api.py are executed (services #1379 redacts model and evidence after matching).
#[path = "support/diagnose_process.rs"]
mod process;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Write},
    process::Command,
    time::Duration,
};
const REF: &str = "d890ab476d1f1a8b6de799d6428e45b6af42a473";
const BLOB: &str = "17a4670f6189d215189b5a6f321cd82547dfe06e";
const API_BLOB: &str = "f783b5a8af67865af6975fc7d6f9ef0b6023ea71";
const API_SHA: &str = "800edae73e7f7764845510ddf4e108bc2ea0a639f721e06e825a4a4644600dd6";
const SOURCE_SHA: &str = "87257c86816978b3c4f82eff2765cd5247d13a5340462eb2995180e9a5790520";
fn command(path: &str) -> Command {
    let mut c = Command::new(path);
    c.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8");
    c
}
fn git(args: &[&str]) -> Result<Vec<u8>, String> {
    let mut c = command("/usr/bin/git");
    c.arg("-C").arg(repository()).args(args);
    let o = process::bounded(c, Vec::new(), Duration::from_secs(10))?;
    if !o.status.success() || !o.stderr.is_empty() {
        return Err("immutable git source read failed".into());
    }
    Ok(o.stdout)
}
fn repository() -> String {
    std::env::var("NOEVIA_DIAGNOSE_SERVICES_SOURCE")
        .unwrap_or_else(|_| "/Users/sebastiandalton/noevia-work/s1-prompts-services".into())
}
fn python() -> String {
    std::env::var("PYTHON").unwrap_or_else(|_| "/tmp/s1-prompts-native-venv/bin/python".into())
}
fn failure(lines: &[&str]) -> String {
    format!("spawning server instance with name=synthetic-model on port 18001\n{}\ninstance name=synthetic-model exited with status 1",lines.iter().map(|s|format!("[18001] {s}")).collect::<Vec<_>>().join("\n"))
}
fn cases() -> Vec<Value> {
    let mut c = Vec::new();
    let mut add = |id: String, text: String, narrowing: &str| {
        c.push(json!({"id":id,"text":text,"narrowing":narrowing}))
    };
    let causes = [
        "failed to allocate kv",
        "out of device memory",
        "failed to open",
        "invalid magic",
        "unknown model architecture",
        "unknown argument",
        "address already in use",
        "n_ctx_train",
    ];
    for (i, line) in causes.iter().enumerate() {
        add(format!("cause-{i}"), failure(&[line]), "");
        for (j, other) in causes.iter().enumerate().skip(i + 1) {
            add(format!("overlap-{i}-{j}"), failure(&[other, line]), "");
        }
    }
    for (id, lines) in [
        (
            "four-hits",
            vec![
                "failed to open 0",
                "failed to open 1",
                "failed to open 2",
                "failed to open 3",
                "failed to open 4",
                "failed to open 5",
            ],
        ),
        (
            "duplicate-pattern",
            vec!["failed to open No such file or directory"],
        ),
        (
            "unknown-errors",
            vec![
                "plain",
                "failed one",
                "plain2",
                "fatal two",
                "abort three",
                "error four",
                "failed five",
            ],
        ),
        ("fallback", vec!["a", "b", "c", "d", "e"]),
        (
            "unknown-word",
            vec!["xfailed", "‿failed", "́error", "²fatal", "fatal"],
        ),
        (
            "counters",
            vec!["failed tokens=123 n_tokens=123 token_count=123 session=12.3"],
        ),
    ] {
        add(id.into(), failure(&lines), "");
    }
    for (id,text) in [
        ("forced", "spawning server instance with name=A on port 1\n[1] fatal\nforce-killing model instance name=A\ninstance name=A exited with status 9"),
        ("unload", "spawning server instance with name=A on port 1\n[1] fatal\nunload: stopping model instance name=A\ninstance name=A exited with status -9"),
        ("spawn-clears-stop", "force-killing model instance name=A\nspawning server instance with name=A on port 1\n[1] fatal\ninstance name=A exited with status 9"),
        ("stale-zero", "instance name=A exited with status 9\ninstance name=A exited with status 0"),
        ("stale-forced", "instance name=A exited with status 9\nforce-killing model instance name=A\ninstance name=A exited with status 9"),
        ("overwrite-slot", "instance name=A exited with status 1\ninstance name=B exited with status 2\ninstance name=A exited with status 3"),
        ("output-pre-spawn", "[01] fatal old\nspawning server instance with name=A on port 01\n[01] fresh\ninstance name=A exited with status 1"),
        ("unspawned", "[1] fatal\ninstance name=A exited with status 1"),
        ("port-collision", "spawning server instance with name=A on port 1\n[1] failed old\nspawning server instance with name=B on port 1\n[1] fatal new\ninstance name=A exited with status 1\ninstance name=B exited with status 2"),
        ("prefix-precedence", "spawning server instance with name=A on port 1\n[1] instance name=A exited with status 1\n[1] force-killing model instance name=A\ninstance name=A exited with status 2"),
        ("router-precedence", "spawning server instance with name=A on port 1 instance name=A exited with status 2\n[1] fatal\nunload: stopping model instance name=A instance name=A exited with status 2"),
        ("leading-zero", "spawning server instance with name=A on port 01\n[01] fatal\n[1] model loaded\ninstance name=A exited with status 1"),
        ("prefix-spaces", "spawning server instance with name=A on port 1\n[1]  fatal\n[1]\u{1f}fatal\ninstance name=A exited with status 1"),
        ("unicode-name-port", "spawning server instance with name=模型 on port ١\n[١] fatal\ninstance name=模型 exited with status -1"),
        ("empty", ""),
    ] { add(id.into(),text.into(),""); }
    for port in ["1", "2"] {
        for loaded in ["model loaded", "MODEL LOADED", "Model loaded"] {
            add(format!("success-{port}-{loaded}"),format!("spawning server instance with name=A on port 1\n[1] fatal\ninstance name=A exited with status 1\nspawning server instance with name=A on port {port}\n[{port}] {loaded}"),"");
        }
    }
    add(
        "loaded-then-crashed".into(),
        failure(&["model loaded", "fatal"]),
        "",
    );
    for delimiter in [
        "\n", "\r", "\r\n", "\u{b}", "\u{c}", "\u{1c}", "\u{1d}", "\u{1e}", "\u{85}", "\u{2028}",
        "\u{2029}",
    ] {
        add(
            format!(
                "split-{}",
                delimiter
                    .chars()
                    .map(|c| format!("{:x}", c as u32))
                    .collect::<Vec<_>>()
                    .join("-")
            ),
            failure(&["fatal"]).replace('\n', delimiter) + delimiter,
            "",
        );
    }
    for (ascii, variants) in [('i', vec!['İ', 'ı']), ('s', vec!['ſ']), ('k', vec!['K'])] {
        for variant in variants {
            for line in &causes {
                if line.contains(ascii) {
                    add(
                        format!("fold-{variant}-{line}"),
                        failure(&[&line.replace(ascii, &variant.to_string())]),
                        "",
                    );
                }
            }
        }
    }
    for status in ["١", "-١", "9223372036854775808", "-9223372036854775809"] {
        add(
            format!("status-{status}"),
            format!("instance name=A exited with status {status}"),
            "status-dialect",
        );
    }
    for status in [
        "9223372036854775807",
        "-9223372036854775808",
        "0001",
        "-0001",
        "-0",
    ] {
        add(
            format!("status-{status}"),
            format!("instance name=A exited with status {status}"),
            "",
        );
    }
    let secrets = [
        "api_key=synthetic-secret-only",
        "Authorization: Bearer synthetic-secret-only",
        "Bearer synthetic-secret-only",
        "https://synthetic-user:synthetic-secret-only@example.invalid",
        "hf_abcdefghijklmnopqrst",
        "sk-abcdefghijklmnop",
        "ghp_abcdefghijklmnopqrst",
        "AKIAABCDEFGHIJKLMNOP",
        "eyJabcdefgh.abcdefgh.abcdefgh",
        "password=12345",
        "TOKEN=12345",
        "cookie='synthetic-secret-only'",
        "credential=synthetic-secret-only",
        "session=synthetic-secret-only",
    ];
    for (i, secret) in secrets.iter().enumerate() {
        for (j, cause) in causes.iter().chain(["failed", "plain"].iter()).enumerate() {
            add(
                format!("secret-{i}-{j}"),
                failure(&[&format!("{cause} {secret}")]),
                "redacted-evidence",
            );
        }
    }
    // services #1379: the model name is container output too and is redacted like evidence.
    for (i, secret) in secrets.iter().enumerate().filter(|(_, s)| !s.contains(' ')) {
        add(
            format!("model-secret-{i}"),
            format!("spawning server instance with name={secret} on port 1\n[1] fatal {secret}\ninstance name={secret} exited with status 1"),
            "redacted-model",
        );
    }
    for (id,text) in [("crossline", "spawning server instance with name=A on port 1\n[1] api_key=\nsynthetic-secret-only\n[1] fatal synthetic-secret-only\ninstance name=A exited with status 1"),("crossline-selected", "spawning server instance with name=A on port 1\nAuthorization:\n[1] fatal synthetic-secret-only\ninstance name=A exited with status 1")] { add(id.into(),text.into(),"cross-line-evidence-suppression"); }
    for (i, shape) in [
        "hf_short",
        "sk-short",
        "ghp_short",
        "AKIAshort",
        "eyJshort.short.short",
        "Bearer short",
        "https://example.invalid",
        "tokens=123 n_tokens=456 token_count=789 session=1.2",
    ]
    .iter()
    .enumerate()
    {
        for (j, cause) in causes.iter().chain(["failed", "plain"].iter()).enumerate() {
            add(
                format!("negative-secret-{i}-{j}"),
                failure(&[&format!("{cause} {shape}")]),
                "",
            );
        }
    }
    for (id,text) in [("snapshot-before-later-output", "spawning server instance with name=A on port 1\n[1] fatal first\ninstance name=A exited with status 1\n[1] failed later"), ("unload-before-spawn", "unload: stopping model instance name=A\nspawning server instance with name=A on port 1\n[1] fatal\ninstance name=A exited with status 1"), ("stop-cleared-by-zero", "spawning server instance with name=A on port 1\nforce-killing model instance name=A\ninstance name=A exited with status 0\n[1] fatal\ninstance name=A exited with status 1")] { add(id.into(),text.into(),""); }
    c
}
fn run() -> Result<(), String> {
    let spec = format!("{REF}:model-manager/app/diagnose.py");
    let blob = git(&["rev-parse", &spec])?;
    if String::from_utf8(blob).map_err(|e| e.to_string())?.trim() != BLOB {
        return Err("oracle blob mismatch".into());
    }
    let source = git(&["show", &spec])?;
    if source.len() > 32 * 1024 || format!("{:x}", Sha256::digest(&source)) != SOURCE_SHA {
        return Err("oracle byte identity mismatch".into());
    }
    let source = String::from_utf8(source).map_err(|e| e.to_string())?;
    let api_spec = format!("{REF}:model-manager/app/api.py");
    let api_blob = git(&["rev-parse", &api_spec])?;
    if String::from_utf8(api_blob)
        .map_err(|e| e.to_string())?
        .trim()
        != API_BLOB
    {
        return Err("api blob mismatch".into());
    }
    let api = git(&["show", &api_spec])?;
    if api.len() > 128 * 1024 || format!("{:x}", Sha256::digest(&api)) != API_SHA {
        return Err("api byte identity mismatch".into());
    }
    let api = String::from_utf8(api).map_err(|e| e.to_string())?;
    let cases = cases();
    let mut ids = std::collections::BTreeSet::new();
    for c in &cases {
        if !ids.insert(
            c.get("id")
                .and_then(Value::as_str)
                .ok_or("case ID absent")?,
        ) {
            return Err("duplicate case ID".into());
        }
    }
    let input = serde_json::to_vec(&json!({"source":source,"api":api,"cases":cases}))
        .map_err(|e| e.to_string())?;
    if input.len() > 1024 * 1024 {
        return Err("synthetic oracle input cap".into());
    }
    let script = r#"import ast,json,re,sys,types,dataclasses,unicodedata
assert sys.version_info[:2] == (3,12)
p=json.load(sys.stdin)
selected=[n for n in ast.parse(p['api']).body if (isinstance(n,ast.Assign) and any(isinstance(t,ast.Name) and t.id=='_SECRET_PATTERNS' for t in n.targets)) or (isinstance(n,ast.FunctionDef) and n.name=='redact_log_line')]
assert len(selected)==2
redaction={'re':re};exec(compile(ast.Module(body=selected,type_ignores=[]),'immutable-redaction.py','exec'),redaction)
redact=redaction['redact_log_line']
def redacted(d):
    # The actual backend_diagnose loop: model and evidence after matching, nothing else.
    d=dataclasses.asdict(d);d['model']=redact(d['model']);d['evidence']=[redact(line) for line in d['evidence']];return d
m=types.ModuleType('immutable_diagnose');sys.modules[m.__name__]=m
exec(compile(p['source'],'immutable-diagnose.py','exec'),m.__dict__)
for c in p['cases']:
    found=m.analyse(c['text'])
    c['expected_json']=json.dumps([dataclasses.asdict(d) for d in found],ensure_ascii=False,allow_nan=False)
    c['redacted_json']=json.dumps([redacted(d) for d in found],ensure_ascii=False,allow_nan=False)
json.dump({'schema':1,'python':sys.version,'unicode':unicodedata.unidata_version,'cases':p['cases']},sys.stdout,ensure_ascii=False,allow_nan=False)
"#;
    let mut c = command(&python());
    c.args(["-I", "-S", "-c", script]);
    let o = process::bounded(c, input, Duration::from_secs(10))?;
    if !o.status.success() || !o.stderr.is_empty() {
        return Err("pure Python oracle failed".into());
    }
    let mut value: Value = serde_json::from_slice(&o.stdout).map_err(|e| e.to_string())?;
    let object = value
        .as_object_mut()
        .ok_or("oracle did not return an object")?;
    object.insert(
        "oracle".into(),
        json!({"ref":REF,"blob":BLOB,"sha256":SOURCE_SHA,"physical_lines":112,"api_blob":API_BLOB,"api_sha256":API_SHA}),
    );
    object.insert(
        "generator_sha256".into(),
        json!(format!(
            "{:x}",
            Sha256::digest(include_bytes!("generate-diagnose-oracle.rs"))
        )),
    );
    object.insert(
        "process_sha256".into(),
        json!(format!(
            "{:x}",
            Sha256::digest(include_bytes!("support/diagnose_process.rs"))
        )),
    );
    let bytes = serde_json::to_vec_pretty(&value).map_err(|e| e.to_string())?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("fixture output cap".into());
    }
    io::stdout()
        .lock()
        .write_all(&bytes)
        .map_err(|e| e.to_string())
}
fn main() -> Result<(), String> {
    run()
}
