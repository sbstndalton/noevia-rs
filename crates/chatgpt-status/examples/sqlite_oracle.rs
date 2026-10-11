#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
use chatgpt_status::SqliteService;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::PathBuf,
    process::{Command, ExitCode},
    time::{Duration, Instant},
};
const CORE: &str = "010735c7d86d30269ab3f4a351b2135faea1c8e7";
const NODE: &str = "/opt/homebrew/opt/node@22/bin/node";
fn bounded_file(path: &std::path::Path, cap: usize) -> Result<Vec<u8>, String> {
    let mut b = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| "fixture source")?
        .take(cap as u64 + 1)
        .read_to_end(&mut b)
        .map_err(|_| "fixture source read")?;
    if b.len() > cap {
        return Err("fixture source cap".into());
    }
    Ok(b)
}
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let core = PathBuf::from(args.next().ok_or("core required")?);
    let dependency = PathBuf::from(args.next().ok_or("dependency required")?);
    let output = PathBuf::from(args.next().ok_or("output required")?);
    let invoke = |c: Command| -> Result<Vec<u8>, String> {
        let o = process::bounded(c, Vec::new(), Duration::from_secs(20))?;
        if !o.status.success() {
            return Err(format!(
                "bounded source/oracle command failed: {}",
                String::from_utf8_lossy(&o.stderr)
            ));
        }
        Ok(o.stdout)
    };
    let mut c = Command::new("git");
    c.arg("-C").arg(&core).args(["rev-parse", "HEAD"]);
    if String::from_utf8(invoke(c)?)
        .map_err(|_| "git UTF8")?
        .trim()
        != CORE
    {
        return Err("wrong core".into());
    }
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(&core)
        .args(["status", "--porcelain", "--untracked-files=no"]);
    if !invoke(c)?.is_empty() {
        return Err("dirty core".into());
    }
    let mut c = Command::new(NODE);
    c.arg("--version");
    if String::from_utf8(invoke(c)?)
        .map_err(|_| "node UTF8")?
        .trim()
        != "v22.23.3"
    {
        return Err("wrong Node".into());
    }
    let mut sources = serde_json::Map::new();
    for p in [
        "server/auth.cjs",
        "server/features.cjs",
        "server/chatgpt-oauth.cjs",
        "server/secret-envelope.cjs",
        "server/routes/providers.cjs",
    ] {
        let mut b = Vec::new();
        std::fs::File::open(core.join(p))
            .map_err(|_| "source")?
            .take(1048577)
            .read_to_end(&mut b)
            .map_err(|_| "source read")?;
        if b.len() > 1048576 {
            return Err("source cap".into());
        }
        sources.insert(p.into(), json!(format!("{:x}", Sha256::digest(b))));
    }
    let mut native = Vec::new();
    let cases = [
        (
            "default-off",
            None,
            None,
            None,
            "u1",
            "u1",
            "connected",
            None,
            false,
        ),
        (
            "saved-true",
            None,
            Some("true"),
            Some("{}"),
            "u1",
            "u1",
            "connected",
            None,
            false,
        ),
        (
            "saved-false",
            None,
            Some("false"),
            Some("{}"),
            "u1",
            "u1",
            "connected",
            None,
            false,
        ),
        (
            "saved-invalid",
            None,
            Some("TRUE"),
            Some("{}"),
            "u1",
            "u1",
            "connected",
            None,
            false,
        ),
        (
            "env-true",
            Some(" YES "),
            Some("false"),
            Some(r#"{"email":"x@a@b","plan":[false,1e400]}"#),
            "u1",
            "u1",
            "connected",
            None,
            false,
        ),
        (
            "env-false",
            Some("off"),
            Some("true"),
            Some("{}"),
            "u1",
            "u1",
            "connected",
            None,
            false,
        ),
        (
            "blank-env",
            Some("  "),
            Some("true"),
            Some("{}"),
            "u1",
            "u1",
            "connected",
            None,
            false,
        ),
        (
            "absent",
            Some("true"),
            None,
            None,
            "u1",
            "u1",
            "connected",
            None,
            false,
        ),
        (
            "wrong-owner",
            Some("true"),
            None,
            Some("{}"),
            "u1",
            "u2",
            "connected",
            None,
            false,
        ),
        (
            "bad-json",
            Some("true"),
            None,
            Some("{oops}"),
            "u1",
            "u1",
            "connected",
            None,
            false,
        ),
        (
            "reconnect",
            Some("true"),
            None,
            Some("{}"),
            "u1",
            "u1",
            "reconnect",
            None,
            false,
        ),
        (
            "primitive",
            Some("true"),
            None,
            Some("null"),
            "u1",
            "u1",
            "connected",
            None,
            false,
        ),
        (
            "v1",
            Some("true"),
            None,
            Some(r#"{"plan":{"credential-pattern":"preserved"}}"#),
            "u1",
            "u1",
            "connected",
            None,
            true,
        ),
        (
            "previous",
            Some("true"),
            None,
            Some(r#"{"email":"\ud83d\ude00@b","plan":true}"#),
            "u1",
            "u1",
            "connected",
            Some(3u8),
            false,
        ),
    ];
    for (id, env, setting, payload, owner, sealed_owner, state, previous, v1) in cases {
        let d = tempfile::tempdir().map_err(|_| "temp")?;
        let dir = d.path().canonicalize().map_err(|_| "canonical")?;
        std::fs::write(dir.join("secrets.key"), [7; 32]).map_err(|_| "key fixture")?;
        if let Some(p) = previous {
            std::fs::write(dir.join("secrets.key.previous"), [p; 32])
                .map_err(|_| "previous fixture")?;
        }
        let db = rusqlite::Connection::open(dir.join("cowork.db")).map_err(|_| "database")?;
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL); INSERT INTO schema_migrations VALUES(5,0); CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE chatgpt_oauth_tokens(user_id TEXT PRIMARY KEY,data_enc TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'connected',updated_at INTEGER NOT NULL)").map_err(|_|"schema fixture")?;
        if let Some(s) = setting {
            db.execute(
                "INSERT INTO settings VALUES('feature:chatgptOAuth',?1)",
                [s],
            )
            .map_err(|_| "flag fixture")?;
        }
        let ciphertext = payload
            .map(|p| {
                secret_envelope::seal(
                    &[previous.unwrap_or(7); 32],
                    &[3; 12],
                    p.as_bytes(),
                    if v1 {
                        None
                    } else {
                        Some(sealed_owner.as_bytes())
                    },
                )
            })
            .transpose()
            .map_err(|_| "seal fixture")?;
        if let Some(enc) = &ciphertext {
            db.execute(
                "INSERT INTO chatgpt_oauth_tokens VALUES(?1,?2,?3,0)",
                rusqlite::params![owner, enc, state],
            )
            .map_err(|_| "row fixture")?;
        }
        let input =
            json!({"database":dir.join("cowork.db"),"env":env,"owner":owner,"previous":previous});
        let input_path = dir.join("oracle.json");
        std::fs::write(
            &input_path,
            serde_json::to_vec(&input).map_err(|_| "input encode")?,
        )
        .map_err(|_| "input write")?;
        let mut c = Command::new(NODE);
        c.arg("-e")
            .arg(include_str!("../tests/sqlite-oracle.cjs"))
            .arg(&core)
            .arg(&dependency)
            .arg(input_path)
            .env("SECRET_ENVELOPE_IMPL", "js");
        let answer: Value =
            serde_json::from_slice(&invoke(c)?).map_err(|_| "actual oracle JSON")?;
        let service = SqliteService::open(&dir, None, env).map_err(|_| "native startup")?;
        let actual = service
            .submit(owner, Instant::now())
            .map_err(|_| "native admission")?
            .finish()
            .map_err(|_| "native status")?;
        if json!(actual.0) != *answer.get("code").ok_or("missing oracle code")?
            || json!(actual.1) != *answer.get("body").ok_or("missing oracle body")?
        {
            return Err(format!("actual Node DB differential failed {id}"));
        }
        native.push(json!({"id":id,"env":env,"setting":setting,"owner":owner,"state":state,"previous":previous,"ciphertext":ciphertext,"answer":answer}));
    }
    let package: Value =
        serde_json::from_slice(&bounded_file(&dependency.join("package.json"), 65536)?)
            .map_err(|_| "package JSON")?;
    let addon = bounded_file(
        &dependency.join("build/Release/better_sqlite3.node"),
        16777216,
    )?;
    let packet = json!({"core":CORE,"node":"22.23.3","dependency_version":package.get("version").ok_or("missing dependency version")?,"dependency_addon_sha256":format!("{:x}",Sha256::digest(addon)),"sources":sources,"seam_sha256":format!("{:x}",Sha256::digest(include_bytes!("../tests/sqlite-oracle.cjs"))),"cases":native});
    std::fs::write(
        output,
        serde_json::to_vec_pretty(&packet).map_err(|_| "packet encode")?,
    )
    .map_err(|_| "packet write")?;
    println!("actual Node real SQLite differential cases=14");
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
