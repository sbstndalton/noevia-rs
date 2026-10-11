//! Reclassify retained finite captures without executing mutants.
#[allow(dead_code)]
#[path = "support/mutant_classifier.rs"]
mod classifier;
use std::{fs, path::Path, process::ExitCode};
fn read(path: &Path) -> Result<Vec<u8>, String> {
    if fs::metadata(path).map_err(|_| "metadata")?.len() > 4 * 1024 * 1024 {
        return Err("capture cap".into());
    }
    fs::read(path).map_err(|_| "capture read".into())
}
fn run() -> Result<(), String> {
    let root =
        Path::new("/var/folders/_4/jbtxcfjd7hj2x9kvw4z5k7gm0000gp/T/m4-network-mutants-21014");
    let source =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/network_policies.rs"))
            .map_err(|_| "source")?;
    for (name, test, id) in [
        ("TAG", "tag_policy", "M4_NET_TAG"),
        ("METADATA", "metadata_policy", "M4_NET_METADATA"),
        ("EOF", "eof_policy", "M4_NET_EOF"),
        ("EXIT", "exit_policy", "M4_NET_EXIT"),
        ("CANCEL_IGNORED", "cancel_policy", "M4_NET_CANCEL"),
        (
            "ADMISSION_EARLY_RELEASE",
            "admission_policy",
            "M4_NET_ADMISSION",
        ),
    ] {
        let dir = root.join(name);
        let out = read(&dir.join("actual.stdout"))?;
        let err = read(&dir.join("actual.stderr"))?;
        if read(&dir.join("actual.status"))? != b"code=Some(101)\n" {
            return Err("status".into());
        }
        let p = classifier::Policy::bind_at(&source, test, id, "tests/network_policies.rs", 5)?;
        if !p.accepts(Some(101), &out, &err) {
            return Err(format!("{name} original refused"));
        }
        let footer = b"error: test failed, to rerun pass `--test network_policies`\n";
        for tail in [
            b"SETUP failed\n".as_slice(),
            b"process deadline\n",
            b"unrelated output\n",
            footer,
        ] {
            let mut o = out.clone();
            o.extend_from_slice(tail);
            if p.accepts(Some(101), &o, &err) {
                return Err("stdout tail accepted".into());
            }
            let mut e = err.clone();
            e.extend_from_slice(tail);
            if p.accepts(Some(101), &out, &e) {
                return Err("stderr tail accepted".into());
            }
        }
        let mut misplaced = footer.to_vec();
        misplaced.extend_from_slice(&err);
        if p.accepts(Some(101), &out, &misplaced) || p.accepts(Some(101), &out, b"") {
            return Err("footer accepted".into());
        }
        for forged in [
            format!("SETUP\n{}", String::from_utf8_lossy(&out)),
            String::from_utf8_lossy(&out).replace(id, "OTHER_POLICY"),
            String::from_utf8_lossy(&out).replace(&p.location, "tests/network_policies.rs:0:0"),
            String::from_utf8_lossy(&out).replace("running 1 test", "running 2 tests"),
        ] {
            if p.accepts(Some(101), forged.as_bytes(), &err) {
                return Err("grammar accepted".into());
            }
        }
        println!("{name}: original recertified; combined terminal/grammar negatives refused");
    }
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
