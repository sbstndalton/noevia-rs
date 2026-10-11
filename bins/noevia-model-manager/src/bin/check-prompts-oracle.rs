//! Compare fresh actual Python results in full; retain runtime version evidence separately.
#![forbid(unsafe_code)]
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{io::Read, path::PathBuf};
fn load(path: PathBuf) -> Result<Value, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(16_777_217)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 16_777_216 {
        return Err("oracle artifact exceeded cap".into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let fresh = load(PathBuf::from(
        args.next().ok_or("fresh actual oracle required")?,
    ))?;
    if args.next().is_some() {
        return Err("unexpected arguments".into());
    }
    let stored: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/prompts-python.v1.json"))?;
    let generator = format!(
        "{:x}",
        Sha256::digest(include_bytes!("generate-prompts-oracle.rs"))
    );
    for artifact in [&fresh, &stored] {
        if artifact.get("generator_sha256").and_then(Value::as_str) != Some(generator.as_str())
            || artifact.get("process_sha256").and_then(Value::as_str)
                != Some(
                    format!(
                        "{:x}",
                        Sha256::digest(include_bytes!("support/prompts_process.rs"))
                    )
                    .as_str(),
                )
            || artifact.get("services").and_then(Value::as_str)
                != Some("2360cb51f6475122ba0aa3d32272849a63028d66")
        {
            return Err("stale source or generator provenance".into());
        }
    }
    for field in [
        "services",
        "sources",
        "generator_sha256",
        "process_sha256",
        "cases",
        "image_digest",
    ] {
        if fresh.get(field) != stored.get(field) {
            return Err(format!("complete oracle mismatch: {field}").into());
        }
    }
    for field in [
        "seed",
        "fastapi",
        "pydantic",
        "pydantic_core",
        "starlette",
        "unicode",
    ] {
        if fresh.get("versions").and_then(|v| v.get(field))
            != stored.get("versions").and_then(|v| v.get(field))
        {
            return Err(format!("framework or seed mismatch: {field}").into());
        }
    }
    // Python patch/platform build text and SQLite versions remain in the fresh artifact.
    // Full cases, including index ties, must still match; this is not an image digest claim.
    println!("all actual oracle cases and immutable provenance match; runtime versions retained");
    Ok(())
}
