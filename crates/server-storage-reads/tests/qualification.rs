use server_storage_reads::{admission::Admission, protocol::decoded_fixture};
use std::{
    error::Error,
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn admission_releases_owner_and_global_slots_without_queue() -> Result<(), Box<dyn Error>> {
    let admission = Admission::default();
    let a = admission.try_acquire("synthetic-a")?;
    assert!(admission.try_acquire("synthetic-a").is_err());
    let b = admission.try_acquire("synthetic-b")?;
    let c = admission.try_acquire("synthetic-c")?;
    let d = admission.try_acquire("synthetic-d")?;
    assert!(admission.try_acquire("synthetic-e").is_err());
    drop(a);
    let e = admission.try_acquire("synthetic-e")?;
    drop((b, c, d, e));
    let same_owner = admission.try_acquire("synthetic-a")?;
    drop(same_owner);
    assert!(admission.try_acquire("").is_err());
    Ok(())
}

#[test]
fn actual_http_malformed_utf8_and_named_last_chunk_exception() -> Result<(), Box<dyn Error>> {
    let root = std::env::var("M4_CORE_ORACLE")?;
    let source = Command::new("git")
        .args([
            "-C",
            &root,
            "show",
            "010735c7d86d30269ab3f4a351b2135faea1c8e7:server/http.cjs",
        ])
        .output()?;
    assert!(source.status.success());
    assert_eq!(
        source.stdout,
        std::fs::read(format!("{root}/server/http.cjs"))?
    );
    // A small fixed seam calls the actual HTTP helper. Synthetic Response objects
    // establish chunk layout only for this fixture; no real chunk equivalence claim.
    const SEAM: &str = r#"const fs=require('node:fs'),{readCappedText}=require(process.argv[1]+'/server/http.cjs');(async()=>{const out=[];for(const c of JSON.parse(fs.readFileSync(0,'utf8')))out.push(await readCappedText(new Response(Buffer.from(c.bytes)),c.cap));process.stdout.write(JSON.stringify(out));})().catch(()=>{process.exitCode=1});"#;
    let mut bytes: Vec<Vec<u8>> = (0..=255).map(|b| vec![b]).collect();
    for lead in [0xc0, 0xc2, 0xdf, 0xe0, 0xed, 0xef, 0xf0, 0xf4, 0xf5, 0xff] {
        for tail in [0x00, 0x7f, 0x80, 0x9f, 0xa0, 0xbf, 0xc0, 0xff] {
            bytes.push(vec![lead, tail, 0x80, 0x80]);
        }
    }
    bytes.extend([
        vec![0xe2, 0x82],
        vec![0xf0, 0x9f, 0x98],
        vec![0xed, 0xa0, 0x80],
        vec![0xef, 0xbb, 0xbf],
    ]);
    let mut cases: Vec<_> = bytes
        .iter()
        .map(|bytes| serde_json::json!({"bytes":bytes,"cap":32}))
        .collect();
    cases.push(serde_json::json!({"bytes":[97,98,99,100],"cap":3}));
    let mut child =
        Command::new(std::env::var("M4_NODE").unwrap_or_else(|_| "/opt/homebrew/bin/node".into()))
            .args(["-e", SEAM, &root])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
    child
        .stdin
        .take()
        .ok_or("no stdin")?
        .write_all(&serde_json::to_vec(&cases)?)?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "actual HTTP seam failed; diagnostics withheld"
    );
    let actual: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)?;
    assert_eq!(actual.len(), cases.len());
    for (i, bytes) in bytes.iter().enumerate() {
        let native = decoded_fixture(bytes, 32)?;
        assert_eq!(
            actual.get(i),
            Some(&serde_json::json!({"text":native.text,"capped":native.capped})),
            "UTF8 case {i}"
        );
    }
    // QUALIFIED exception CAP_LAST_CHUNK: accepted canonical cap + lookahead
    // reports overflow even though the predecessor loses excess in its last chunk.
    assert_eq!(
        actual.last(),
        Some(&serde_json::json!({"text":"abc","capped":false}))
    );
    let native = decoded_fixture(b"abcd", 3)?;
    assert_eq!(native.text, "abc");
    assert!(native.capped);
    Ok(())
}

#[test]
fn body_json_predecessor_status_and_empty_body() -> Result<(), Box<dyn Error>> {
    use server_storage_reads::{json, BODY_BYTES};
    assert_eq!(json::path(b"")?, Vec::<u16>::new());
    for bytes in [b"null".as_slice(), b"[]", b"true", b"1"] {
        let error = json::path(bytes).err().ok_or("missing object refusal")?;
        assert_eq!(error.status, 400);
        assert_eq!(error.message, "request body must be a JSON object");
    }
    assert_eq!(
        json::path(b" ").err().map(|e| e.message),
        Some("invalid JSON".into())
    );
    let error = json::path(&vec![32; BODY_BYTES + 1])
        .err()
        .ok_or("missing byte refusal")?;
    assert_eq!(error.status, 413);
    assert_eq!(error.message, "Request exceeds size limit");
    Ok(())
}

#[test]
fn oversized_endpoint_fails_closed_before_url_parsing() {
    assert!(!server_storage_reads::protocol::endpoint_approved(
        &format!("https://storage.invalid/{}", "x".repeat(65536)),
        true,
        ""
    ));
}
