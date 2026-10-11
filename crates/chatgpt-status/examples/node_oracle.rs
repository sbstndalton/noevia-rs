#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::{
    path::PathBuf,
    process::{Command, ExitCode},
    time::Duration,
};
const CORE: &str = "010735c7d86d30269ab3f4a351b2135faea1c8e7";
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let core = PathBuf::from(args.next().ok_or("core path required")?);
    let output = PathBuf::from(args.next().ok_or("fixture path required")?);
    let invoke = |mut c: Command| -> Result<Vec<u8>, String> {
        let o = process::bounded_with(&mut c, Vec::new(), Duration::from_secs(20), |_| {})?;
        if !o.status.success() {
            return Err("source check failed".into());
        }
        Ok(o.stdout)
    };
    let mut c = Command::new("git");
    c.args(["-C"]).arg(&core).args(["rev-parse", "HEAD"]);
    if String::from_utf8(invoke(c)?)
        .map_err(|_| "git UTF8")?
        .trim()
        != CORE
    {
        return Err("wrong core commit".into());
    }
    let mut c = Command::new("git");
    c.args(["-C"])
        .arg(&core)
        .args(["status", "--porcelain", "--untracked-files=no"]);
    if !invoke(c)?.is_empty() {
        return Err("dirty core source".into());
    }
    let mut sources = serde_json::Map::new();
    for p in [
        "server/chatgpt-oauth.cjs",
        "server/secret-envelope.cjs",
        "server/secrets.cjs",
        "server/routes/providers.cjs",
        "server/features.cjs",
    ] {
        let mut b = Vec::new();
        std::fs::File::open(core.join(p))
            .map_err(|_| "source unavailable")?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut b)
            .map_err(|_| "source read")?;
        if b.len() > 1024 * 1024 {
            return Err("source cap".into());
        }
        sources.insert(p.into(), json!(format!("{:x}", Sha256::digest(&b))));
    }
    let payloads = [
        ("empty", "{}"),
        (
            "normal",
            r#"{"email":"alice@example.com","plan":"plus","accessToken":"synthetic-access","refreshToken":"synthetic-refresh","accountId":"synthetic-account"}"#,
        ),
        ("last-at", r#"{"email":"alice@b@example.com","plan":""}"#),
        ("empty-name", r#"{"email":"@","plan":false}"#),
        (
            "astral-first",
            r#"{"email":"\ud83d\ude00@example.com","plan":0}"#,
        ),
        (
            "lone-surrogates",
            r#"{"email":"\ud800@\udfff","plan":"\ud800"}"#,
        ),
        (
            "duplicates",
            r#"{"email":"first@a","email":"last@b","plan":false,"pl\u0061n":{"2":"two","1":"one","01":"leading","__proto__":"data"}}"#,
        ),
        (
            "overflow",
            r#"{"plan":{"huge":1e400,"small":-1e400,"minuszero":-0,"normal":1e21}}"#,
        ),
        ("array-plan", r#"{"plan":[false,0,"",null,{},[],1e400]}"#),
        ("true-plan", r#"{"plan":true}"#),
        ("number-plan", r#"{"plan":0.1}"#),
        ("null-plan", r#"{"plan":null}"#),
        ("top-null", "null"),
        ("top-array", "[]"),
        ("top-number", "42"),
        ("malformed", "{oops}"),
        ("nan", "{\"plan\":NaN}"),
        ("infinity-token", "{\"plan\":Infinity}"),
    ];
    let mut cases = Vec::new();
    for (id, payload) in payloads {
        cases.push(json!({"id":id,"user":"u1","state":"connected","payload":payload}));
    }
    for (id, patch) in [
        ("absent", json!({"absent":true})),
        ("reconnect", json!({"state":"reconnect"})),
        ("sql-error", json!({"sqlError":true})),
        ("feature-off", json!({"feature":false,"sqlError":true})),
        ("wrong-user", json!({"sealUser":"other"})),
        ("legacy-v1", json!({"v1":true})),
        ("previous", json!({"current":8,"previous":7})),
        ("wrong-key", json!({"current":8})),
        (
            "raw-plain",
            json!({"raw":"{\"email\":\"raw@b\",\"plan\":[]}"}),
        ),
        ("raw-truncated", json!({"raw":"enc:v2:AAA"})),
    ] {
        let mut c = json!({"id":id,"user":"u1","state":"connected","payload":"{}"});
        if let (Some(c), Some(p)) = (c.as_object_mut(), patch.as_object()) {
            c.extend(p.clone())
        }
        cases.push(c);
    }
    let long = format!("{{\"email\":\"a@{}😀\"}}", "x".repeat(116));
    cases.push(json!({"id":"truncate-pair","user":"u1","state":"connected","payload":long}));
    let mut rng = 0x735c665u64;
    for i in 0..128 {
        let mut encoded = String::new();
        for _ in 0..(i % 40) {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            encoded.push_str(&format!("\\u{:04x}", (rng >> 32) as u16));
        }
        let payload=format!("{{\"email\":\"{encoded}@first@{encoded}\",\"plan\":{{\"3\":{},\"1\":1e400,\"x\":\"{encoded}\",\"x\":\"{encoded}\"}}}}",i as f64/7.0);
        cases.push(json!({"id":format!("generated-{i}"),"user":"u1","state":"connected","payload":payload}));
    }
    for (i, number) in [
        1e23,
        1e-7,
        1e-6,
        1e21,
        1000000000000000100.0,
        5e-324,
        f64::MAX,
    ]
    .into_iter()
    .enumerate()
    {
        cases.push(json!({"id":format!("number-edge-{i}"),"user":"u1","state":"connected","payload":format!("{{\"plan\":{number:e}}}")}));
    }
    for i in 0..128 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let number = f64::from_bits(rng);
        if number.is_finite() {
            cases.push(json!({"id":format!("number-bits-{i}"),"user":"u1","state":"connected","payload":format!("{{\"plan\":{number:e}}}")}));
        }
    }
    let invalid = secret_envelope::seal(
        &[7; 32],
        &[3; 12],
        b"{\"email\":\"\xff@domain\",\"plan\":\"\xf0(\x8c(\"}",
        Some(b"u1"),
    )
    .map_err(|_| "invalid UTF8 fixture seal")?;
    cases.push(
        json!({"id":"invalid-utf8-replacement","user":"u1","state":"connected","raw":invalid}),
    );
    let mut version = Command::new("/opt/homebrew/opt/node@22/bin/node");
    version.arg("--version");
    if String::from_utf8(invoke(version)?)
        .map_err(|_| "node version")?
        .trim()
        != "v22.23.3"
    {
        return Err("wrong Node runtime".into());
    }
    let mut c = Command::new("/opt/homebrew/opt/node@22/bin/node");
    c.arg("-e")
        .arg(include_str!("../tests/oracle.cjs"))
        .arg(&core)
        .env("SECRET_ENVELOPE_IMPL", "js");
    let o = process::bounded(
        c,
        process::json_input(&json!(cases))?,
        Duration::from_secs(20),
    )?;
    if !o.status.success() {
        return Err(format!(
            "actual Node oracle failed: {}",
            String::from_utf8_lossy(&o.stderr)
        ));
    }
    let cases: serde_json::Value =
        serde_json::from_slice(&o.stdout).map_err(|_| "bad oracle output")?;
    let fixture = json!({"core":CORE,"node":"22.23.3","sources":sources,"seam_sha256":format!("{:x}",Sha256::digest(include_bytes!("../tests/oracle.cjs"))),"cases":cases});
    std::fs::write(
        output,
        serde_json::to_vec_pretty(&fixture).map_err(|_| "fixture encoding")?,
    )
    .map_err(|_| "fixture write")?;
    println!(
        "actual Node status cases={}",
        cases.as_array().map_or(0, Vec::len)
    );
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
