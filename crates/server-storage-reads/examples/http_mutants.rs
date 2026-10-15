//! Bounded actual-source plain-HTTP mutations; only owned synthetic loopback.
#[path = "support/capped_capture.rs"]
mod capped_capture;
#[allow(dead_code)]
#[path = "support/mutant_classifier.rs"]
mod classifier;
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
use std::{
    fs,
    path::Path,
    process::{Command, ExitCode},
    time::Duration,
};
fn text(path: impl AsRef<Path>) -> Result<String, String> {
    String::from_utf8(capped_capture::read(path.as_ref()).map_err(|_| "source read refused")?)
        .map_err(|_| "source encoding refused".into())
}
fn copy(source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<(), String> {
    let bytes = capped_capture::read(source.as_ref()).map_err(|_| "source read refused")?;
    fs::write(destination, bytes).map_err(|_| "source write refused".into())
}
fn run() -> Result<(), String> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let original = text(source.join("src/child_http.rs")).map_err(|_| "source")?;
    let controls = text(source.join("tests/http_policies.rs")).map_err(|_| "controls")?;
    let root = std::env::temp_dir().join(format!("m4-http-mutants-{}", std::process::id()));
    fs::create_dir(&root).map_err(|_| "directory")?;
    let mut manifest = text(source.join("Cargo.toml")).map_err(|_| "manifest")?;
    for name in [
        "dav-parse",
        "storage-path",
        "s3-list-parse",
        "s3-sign",
        "js-json",
    ] {
        let absolute = source.parent().ok_or("parent")?.join(name);
        manifest = manifest.replace(
            &format!("path = \"../{name}\""),
            &format!("path = {:?}", absolute),
        );
    }
    for(name,needle,replacement,test,id)in[("WIRE","pub const WIRE: usize = 64 * 1024 * 1024;","pub const WIRE: usize = 128 * 1024 * 1024;","wire_policy","M4_HTTP_WIRE"),("REDIRECT","if (300..400).contains(&status) && response.headers().contains_key(\"location\") {","if false && (300..400).contains(&status) && response.headers().contains_key(\"location\") {","redirect_policy","M4_HTTP_REDIRECT"),("LOOKAHEAD","let take = part.len().min(cap + 1 - payload.len());","let take = part.len().min(cap - payload.len());","lookahead_policy","M4_HTTP_LOOKAHEAD")]{if original.matches(needle).count()!=1{return Err("source anchor".into());}let policy=classifier::Policy::bind_at(&controls,test,id,"tests/network_policies.rs",5)?;let dir=root.join(name);fs::create_dir_all(dir.join("src/bin")).map_err(|_|"directory")?;fs::create_dir(dir.join("tests")).map_err(|_|"directory")?;fs::write(dir.join("Cargo.toml"),&manifest).map_err(|_|"manifest")?;copy(source.join("Cargo.lock"),dir.join("Cargo.lock")).map_err(|_|"lock")?;for file in["lib.rs","admission.rs","json.rs","protocol.rs","request_ipc.rs","response_ipc.rs","child_request.rs","isolated.rs"]{copy(source.join("src").join(file),dir.join("src").join(file)).map_err(|_|"copy")?;}fs::write(dir.join("src/child_http.rs"),original.replacen(needle,replacement,1)).map_err(|_|"mutation")?;copy(source.join("src/bin/private-http-fixture.rs"),dir.join("src/bin/private-http-fixture.rs")).map_err(|_|"fixture")?;copy(source.join("tests/http_plain.rs"),dir.join("tests/http_plain.rs")).map_err(|_|"lab")?;fs::write(dir.join("tests/network_policies.rs"),&controls).map_err(|_|"control")?;let mut command=Command::new(std::env::var_os("M4_CARGO").unwrap_or_else(||"cargo".into()));command.env("RUST_BACKTRACE","0").env("RUST_LIB_BACKTRACE","0").args(["test","--offline","--locked","--manifest-path"]).arg(dir.join("Cargo.toml")).args(["--test","network_policies",test,"--","--exact"]);let result=process::bounded(command,vec![],Duration::from_secs(60))?;fs::write(dir.join("actual.stdout"),&result.stdout).map_err(|_|"capture")?;fs::write(dir.join("actual.stderr"),&result.stderr).map_err(|_|"capture")?;fs::write(dir.join("actual.status"),format!("code={:?}\n",result.status.code())).map_err(|_|"status")?;if !policy.accepts(result.status.code(),&result.stdout,&result.stderr){return Err(format!("{name} uncontrolled failure"));}let actual=std::str::from_utf8(&result.stdout).map_err(|_|"encoding")?;for(old,new)in[(id,"SETUP"),(id,"Deadline"),(id,"OTHER_POLICY"),(policy.location.as_str(),"tests/network_policies.rs:0:0"),(test,"wrong_test")]{if policy.accepts(Some(101),actual.replace(old,new).as_bytes(),&result.stderr){return Err("forged accepted".into());}}for tail in[b"process failure\n".as_slice(),b"SETUP\n",b"error: test failed, to rerun pass `--test network_policies`\n"]{let mut out=result.stdout.clone();out.extend_from_slice(tail);let mut err=result.stderr.clone();err.extend_from_slice(tail);if policy.accepts(Some(101),&out,&result.stderr)||policy.accepts(Some(101),&result.stdout,&err){return Err("tail accepted".into());}}println!("{name}: exact selected policy killed; terminal negatives refused");}
    println!("three source mutations retained {}", root.display());
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
