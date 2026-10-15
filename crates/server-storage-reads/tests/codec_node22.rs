//! Fixed installed-runtime source capture; codec observations are a later grant.
#[allow(dead_code)]
#[path = "../examples/support/capped_capture.rs"]
mod capped_capture;
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    process::Command,
    time::Duration,
};

const NODE: &str = "/opt/homebrew/opt/node@22/bin/node";
const CAPTURE: &str = "process.stdout.write(JSON.stringify({node:process.version,undici:process.versions.undici})+'\\n');process.stdout.write(process.binding('natives')['internal/deps/undici/undici']);";

/// Where retained Node 22 captures go: M4_NODE22_EVIDENCE, else the system temp directory.
fn evidence_root() -> Result<String, String> {
    std::env::var("M4_NODE22_EVIDENCE")
        .ok()
        .or_else(|| std::env::temp_dir().to_str().map(str::to_owned))
        .ok_or_else(|| "evidence directory".into())
}
fn artifact(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes.as_ref())
}

#[test]
fn installed_builtin_source_capture() -> Result<(), String> {
    // Fixed installed Node 22 observations write retained captures under /tmp; they run only on
    // an explicitly prepared host (M4_NODE22_ORACLE=1), never in ordinary CI.
    if std::env::var("M4_NODE22_ORACLE").as_deref() != Ok("1") {
        node22_skipped("installed_builtin_source_capture");
        return Ok(());
    }
    let owned_directory = format!(
        "{}/m4-codecs-node22-builtin-capture-{}",
        evidence_root()?,
        std::process::id()
    );
    let directory = Path::new(&owned_directory);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(directory)
        .map_err(|e| e.to_string())?;
    let mut command = Command::new(NODE);
    command.env_clear().args(["-e", CAPTURE]);
    let output = process::bounded(command, Vec::new(), Duration::from_secs(15))?;
    artifact(directory.join("stdout.raw"), &output.stdout).map_err(|e| e.to_string())?;
    artifact(directory.join("stderr.raw"), &output.stderr).map_err(|e| e.to_string())?;
    artifact(
        directory.join("status.txt"),
        format!(
            "code={:?}\nsuccess={}\n",
            output.status.code(),
            output.status.success()
        ),
    )
    .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err("HOLD source capture process refused".into());
    }
    // The unchanged process collector permits 8 MiB. Refuse before snapshot copy
    // if the separately unchanged descriptor reader's 4 MiB cap is exceeded.
    if output.stdout.len() > capped_capture::CAP {
        return Err("HOLD source exceeds snapshot cap".into());
    }
    let snapshot = capped_capture::read(&directory.join("stdout.raw"))
        .map_err(|e| format!("HOLD snapshot reader {e:?}"))?;
    let newline = snapshot
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or("HOLD missing identity")?;
    let identity: serde_json::Value =
        serde_json::from_slice(snapshot.get(..newline).ok_or("identity range")?)
            .map_err(|e| e.to_string())?;
    if identity.get("node").and_then(serde_json::Value::as_str) != Some("v22.23.3")
        || identity.get("undici").and_then(serde_json::Value::as_str) != Some("6.28.1")
    {
        return Err("HOLD unexpected runtime identity".into());
    }
    let source = snapshot.get(newline + 1..).ok_or("source range")?;
    if source.is_empty() {
        return Err("HOLD empty source".into());
    }
    artifact(directory.join("builtin-undici.js"), source).map_err(|e| e.to_string())?;
    artifact(
        directory.join("identity.json"),
        snapshot.get(..newline).ok_or("identity range")?,
    )
    .map_err(|e| e.to_string())?;
    println!("fixed builtin capture identity={identity} stdout_bytes={} source_bytes={} stderr_bytes={} code={:?}", snapshot.len(), source.len(), output.stderr.len(), output.status.code());
    Ok(())
}

#[path = "support/codec_lab.rs"]
mod lab;
const COMPRESS_BR: &str = "const z=require('node:zlib');console.log(JSON.stringify({node:process.version,undici:process.versions.undici,small:z.brotliCompressSync(Buffer.from('ordinary brotli')).toString('hex'),empty:z.brotliCompressSync(Buffer.alloc(0)).toString('hex'),overText:z.brotliCompressSync(Buffer.alloc(800001,120)).toString('hex'),overList:z.brotliCompressSync(Buffer.alloc(4194305,120)).toString('hex')}));";
const FETCH_CAP: &str = "const fs=require('node:fs');const x=JSON.parse(fs.readFileSync(0,'utf8'));const {readCappedText}=require(process.env.M4_CORE_ORACLE+'/server/http.cjs');const v={node:process.version,undici:process.versions.undici};fetch(x.url,{method:x.method,redirect:'error',signal:AbortSignal.timeout(5000)}).then(r=>readCappedText(r,x.cap).then(b=>console.log(JSON.stringify({...v,status:r.status,capped:b.capped,length:b.text.length,prefix:b.text.slice(0,16),suffix:b.text.slice(-16)})))).catch(()=>console.log(JSON.stringify({...v,refused:true})));";
fn fixed_node(
    script: &'static str,
    input: Vec<u8>,
    label: &str,
) -> Result<serde_json::Value, String> {
    let mut command = Command::new(NODE);
    command.env_clear().args(["-e", script]);
    // The fetch seams load the pinned core's http.cjs from the operator's oracle checkout.
    if let Some(core) = std::env::var_os("M4_CORE_ORACLE") {
        command.env("M4_CORE_ORACLE", core);
    }
    let output = process::bounded(command, input, Duration::from_secs(8))?;
    let owned_directory = format!(
        "{}/m4-codecs-node22-observations-{}",
        evidence_root()?,
        std::process::id()
    );
    let directory = Path::new(&owned_directory);
    if !directory.exists() {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(directory)
            .map_err(|e| e.to_string())?;
    }
    artifact(directory.join(format!("{label}.stdout")), &output.stdout)
        .map_err(|e| e.to_string())?;
    artifact(directory.join(format!("{label}.stderr")), &output.stderr)
        .map_err(|e| e.to_string())?;
    artifact(
        directory.join(format!("{label}.status")),
        format!("code={:?}\n", output.status.code()),
    )
    .map_err(|e| e.to_string())?;
    if !output.status.success() || !output.stderr.is_empty() {
        return Err("CONTROL Node process refused".into());
    }
    if output.stdout.is_empty() {
        return Ok(serde_json::json!({"oracleFault":"code0-empty-stdout"}));
    }
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    if result.get("node").and_then(serde_json::Value::as_str) != Some("v22.23.3")
        || result.get("undici").and_then(serde_json::Value::as_str) != Some("6.28.1")
    {
        return Err("CONTROL identity".into());
    }
    Ok(result)
}
fn hex(value: &serde_json::Value, key: &str) -> Result<Vec<u8>, String> {
    let source = value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or("hex missing")?;
    if source.len() > 4096 || source.len() % 2 != 0 {
        return Err("hex cap".into());
    }
    source
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            std::str::from_utf8(pair)
                .map_err(|_| "hex encoding".to_owned())
                .and_then(|s| u8::from_str_radix(s, 16).map_err(|_| "hex digit".to_owned()))
        })
        .collect()
}
fn observe(
    message: Vec<u8>,
    split: usize,
    cap: usize,
    label: &str,
) -> Result<serde_json::Value, String> {
    let listener = lab::listener();
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    std::thread::scope(|scope| {
        let server = scope.spawn(move || lab::serve(listener, &message, split));
        let input = process::json_input(
            &serde_json::json!({"url":format!("http://{address}/synthetic"),"method":"GET","cap":cap}),
        )?;
        let result = fixed_node(FETCH_CAP, input, label);
        let request = server.join().map_err(|_| "server join")?;
        if request.is_empty() {
            return Err("CONTROL no owned request".into());
        }
        result
    })
}
#[test]
fn actual_builtin_codec_observations_and_malformed_overcap_gate() -> Result<(), String> {
    // Fixed installed Node 22 observations write retained captures under /tmp; they run only on
    // an explicitly prepared host (M4_NODE22_ORACLE=1), never in ordinary CI.
    if std::env::var("M4_NODE22_ORACLE").as_deref() != Ok("1") {
        node22_skipped("actual_builtin_codec_observations_and_malformed_overcap_gate");
        return Ok(());
    }
    use server_storage_reads::{
        protocol::{Method, TransportFailure},
        response_ipc::Reply,
        LIST_BYTES, TEXT_BYTES,
    };
    let br = fixed_node(COMPRESS_BR, Vec::new(), "fixed-br-compressor")?;
    let small = hex(&br, "small")?;
    let empty = hex(&br, "empty")?;
    for (name, body, expected) in [
        ("small", small.as_slice(), b"ordinary brotli".as_slice()),
        ("empty", empty.as_slice(), b"".as_slice()),
    ] {
        lab::plaintext(b"br", body, expected);
        let observed = observe(
            lab::response(200, &[b"br"], body),
            1,
            TEXT_BYTES,
            &format!("br-{name}"),
        )?;
        assert_eq!(
            observed.get("length").and_then(serde_json::Value::as_u64),
            Some(expected.len() as u64)
        );
    }
    let stacked = lab::encode("gzip", &small);
    lab::plaintext(b"br,gzip", &stacked, b"ordinary brotli");
    let mut node_refusals = 0;
    let mut oracle_faults = 0;
    for cap in [TEXT_BYTES, LIST_BYTES] {
        let plain = vec![b'x'; cap + 1];
        for kind in ["gzip", "deflate", "br"] {
            let encoded = if kind == "br" {
                hex(
                    &br,
                    if cap == TEXT_BYTES {
                        "overText"
                    } else {
                        "overList"
                    },
                )?
            } else {
                lab::encode(kind, &plain)
            };
            for split in [1, 4096] {
                let mut bad = encoded.clone();
                *bad.last_mut().ok_or("empty encoded")? ^= 0xff;
                for (name, body) in [("valid", encoded.as_slice()), ("bad-tail", bad.as_slice())] {
                    let label = format!("{kind}-{cap}-{split}-{name}");
                    let observed = observe(
                        lab::response(200, &[kind.as_bytes()], body),
                        split,
                        cap,
                        &label,
                    )?;
                    println!("actual cap gate {label} {observed}");
                    if observed.get("oracleFault").is_some() {
                        oracle_faults += 1;
                    }
                    if observed.get("refused").and_then(serde_json::Value::as_bool) == Some(true) {
                        node_refusals += 1;
                    }
                    lab::run(
                        lab::response(200, &[kind.as_bytes()], body),
                        split,
                        Method::Get,
                        cap,
                        |r| {
                            assert!(
                                matches!(
                                    r,
                                    Reply::Failure {
                                        failure: TransportFailure::Body,
                                        ..
                                    }
                                ),
                                "CONTROL compressed cap remains refused"
                            )
                        },
                    );
                }
            }
        }
    }
    println!("GATE compressed cap+1 HOLD; actual selected Node refusals={node_refusals}; oracle faults={oracle_faults}; native never succeeds on these cases");
    assert!(node_refusals > 0, "CONTROL demonstrated scheduling gate");
    belowcap_matrix(&br)?;
    Ok(())
}

const FETCH_BUFFER: &str = "const fs=require('node:fs');const x=JSON.parse(fs.readFileSync(0,'utf8'));const {readCappedBuffer}=require(process.env.M4_CORE_ORACLE+'/server/http.cjs');const v={node:process.version,undici:process.versions.undici};fetch(x.url,{method:x.method,redirect:'error',signal:AbortSignal.timeout(5000)}).then(r=>readCappedBuffer(r,800000).then(b=>console.log(JSON.stringify({...v,status:r.status,capped:b.capped,length:b.bytes.length,prefix:b.bytes.subarray(0,32).toString('hex'),suffix:b.bytes.subarray(-32).toString('hex')})))).catch(()=>console.log(JSON.stringify({...v,refused:true})));";
fn belowcap_matrix(br: &serde_json::Value) -> Result<(), String> {
    let small = hex(br, "small")?;
    let gzip = lab::encode("gzip", b"hello");
    let zlib = lab::encode("deflate", b"hello");
    let raw = lab::encode("raw", b"hello");
    let double = lab::encode("deflate", &gzip);
    type Case<'a> = (Vec<&'a [u8]>, Vec<u8>, Vec<u8>);
    let mut cases: Vec<Case<'_>> = vec![
        (vec![], b"hello".to_vec(), b"hello".to_vec()),
        (vec![b""], b"hello".to_vec(), b"hello".to_vec()),
        (vec![b"identity", b"identity"], gzip.clone(), gzip.clone()),
        (vec![b"gzip"], gzip.clone(), b"hello".to_vec()),
        (vec![b" \xa0GZIP\xa0 "], gzip.clone(), b"hello".to_vec()),
        (vec![b"deflate"], zlib, b"hello".to_vec()),
        (vec![b"deflate"], raw, b"hello".to_vec()),
        (vec![b"br"], small, b"ordinary brotli".to_vec()),
        (vec![b"gzip", b"deflate"], double, b"hello".to_vec()),
        (vec![b"gzip,unknown"], gzip.clone(), gzip.clone()),
        (vec![b"gzip,identity"], gzip.clone(), gzip.clone()),
        (vec![b"gzip,"], gzip.clone(), gzip),
    ];
    for count in [3, 5] {
        let mut body = b"hello".to_vec();
        for _ in 0..count {
            body = lab::encode("gzip", &body);
        }
        cases.push((
            if count == 3 {
                vec![b"gzip,gzip,gzip"]
            } else {
                vec![b"gzip,gzip,gzip,gzip,gzip"]
            },
            body,
            b"hello".to_vec(),
        ));
    }
    for (index, (headers, body, expected)) in cases.into_iter().enumerate() {
        for method in ["GET", "PROPFIND"] {
            let listener = lab::listener();
            let address = listener.local_addr().map_err(|e| e.to_string())?;
            let message = lab::response(207, &headers, &body);
            let result = std::thread::scope(|scope| {
                let server = scope.spawn(move || lab::serve(listener, &message, 1));
                let input = process::json_input(
                    &serde_json::json!({"url":format!("http://{address}/synthetic"),"method":method}),
                )?;
                let result = fixed_node(FETCH_BUFFER, input, &format!("below-{index}-{method}"));
                let request = server.join().map_err(|_| "server join")?;
                if request.is_empty() {
                    return Err("CONTROL no request".into());
                }
                result
            })?;
            let hex_expected: String = expected.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(
                result.get("length").and_then(serde_json::Value::as_u64),
                Some(expected.len() as u64)
            );
            assert_eq!(
                result.get("prefix").and_then(serde_json::Value::as_str),
                Some(hex_expected.as_str())
            );
            println!("actual belowcap case={index} method={method} {result}");
        }
    }
    Ok(())
}

/// Record a skipped Node 22 observation so CI can count and show it (noevia#1827).
fn node22_skipped(test: &str) {
    eprintln!("M4 oracle skipped: {test} (set M4_NODE22_ORACLE=1 to run it)");
    if let Some(log) = std::env::var_os("M4_ORACLE_SKIP_LOG") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
        {
            let _ = writeln!(f, "{test}");
        }
    }
}
