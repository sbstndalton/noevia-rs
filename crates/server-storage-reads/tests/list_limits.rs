//! Named resource qualifications are separate from the zero-difference domain.
mod support;
use server_storage_reads::{protocol::*, *};
use std::{error::Error, process::Command, time::Duration};
use zeroize::Zeroizing;
struct Fixture {
    body: Option<String>,
}
impl Transport for Fixture {
    fn elapsed(&self) -> Duration {
        Duration::ZERO
    }
    fn amz_date(&self) -> String {
        "20261011T120000Z".into()
    }
    fn delay(&mut self, _: Duration) -> Result<(), TransportFailure> {
        Err(TransportFailure::Other)
    }
    fn attempt(&mut self, r: Request) -> Result<Response, TransportFailure> {
        let body = self.body.take().ok_or(TransportFailure::Other)?;
        Ok(Response {
            status: 207,
            body: decoded_fixture(body.as_bytes(), r.body_cap)
                .map_err(|_| TransportFailure::Body)?,
        })
    }
}
#[test]
fn list_output_16m_refusal_is_named_legacy_difference() -> Result<(), Box<dyn Error>> {
    let root = std::env::var("M4_CORE_ORACLE")?;
    support::checked_source(&root, "storage-client.cjs")?;
    let name = format!("{}.txt", "a".repeat(76));
    let body = format!("<response><href>{name}</href></response>").repeat(25000);
    assert!(body.len() < LIST_BYTES);
    let path = "x".repeat(500);
    let c = Connection {
        kind: Kind::WebDav,
        base_url: "https://storage.invalid/dav".into(),
        username: String::new(),
        secret: Zeroizing::new(Vec::new()),
        corpus_root: String::new(),
        bucket: String::new(),
        region: String::new(),
    };
    let error = execute(
        &c,
        &path.encode_utf16().collect::<Vec<_>>(),
        Operation::List,
        &mut Fixture {
            body: Some(body.clone()),
        },
    )
    .err()
    .ok_or("missing output refusal")?;
    assert_eq!(error.status, 502);
    assert_eq!(error.message, "storage listing exceeds resource limit");
    // Fixed fetch fixture; actual client builds the large legacy reply. Only
    // count and byte length cross stdout, capped by the reviewed Rust seam.
    const SEAM: &str = r#"const fs=require('node:fs'),c=JSON.parse(fs.readFileSync(0,'utf8')),client=require(process.argv[1]+'/server/storage-client.cjs');global.fetch=async()=>new Response(c.body,{status:207});client.listFiles({kind:'webdav',baseUrl:'https://storage.invalid/dav'},c.path).then(v=>process.stdout.write(JSON.stringify({count:v.length,bytes:Buffer.byteLength(JSON.stringify(v))}))).catch(()=>{process.exitCode=1});"#;
    let mut command =
        Command::new(std::env::var("M4_NODE").unwrap_or_else(|_| "/opt/homebrew/bin/node".into()));
    command
        .args(["-e", SEAM, &root])
        .env("STORAGE_PATH_IMPL", "js");
    let output = support::run(
        command,
        support::process::json_input(&serde_json::json!({"body":body,"path":path}))?,
    )?;
    assert!(output.status.success());
    let legacy: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        legacy.get("count").and_then(serde_json::Value::as_u64),
        Some(25000)
    );
    assert!(legacy
        .get("bytes")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|n| n > LIST_OUTPUT_BYTES as u64));
    Ok(())
}

#[test]
fn snapshot_endpoint_64k_refusal_is_named_legacy_difference() -> Result<(), Box<dyn Error>> {
    let root = std::env::var("M4_CORE_ORACLE")?;
    support::checked_source(&root, "ssrf.cjs")?;
    let base = format!("https://storage.invalid/{}", "a".repeat(65536));
    assert!(!endpoint_approved(&base, true, ""));
    const SEAM: &str = r#"const fs=require('node:fs'),url=JSON.parse(fs.readFileSync(0,'utf8')),approved=require(process.argv[1]+'/server/ssrf.cjs').createEndpointApproved();process.stdout.write(JSON.stringify(approved({user:{role:'admin'}},url)));"#;
    let mut command =
        Command::new(std::env::var("M4_NODE").unwrap_or_else(|_| "/opt/homebrew/bin/node".into()));
    command.args(["-e", SEAM, &root]);
    let output = support::run(
        command,
        support::process::json_input(&serde_json::json!(base))?,
    )?;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"true");
    Ok(())
}
