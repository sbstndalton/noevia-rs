#[allow(dead_code)]
#[path = "../../../tools/qualification/mutation.rs"]
mod mutation;
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
use std::{
    path::PathBuf,
    process::{Command, ExitCode},
    time::Duration,
};
struct Restore {
    path: PathBuf,
    bytes: Vec<u8>,
}
impl Drop for Restore {
    fn drop(&mut self) {
        if let Err(error) = std::fs::write(&self.path, &self.bytes) {
            eprintln!("MUTATION RESTORE FAILED: {error}")
        }
    }
}
fn run() -> Result<(), String> {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mutations = [
        (
            "src/lib.rs",
            "s.iter().rposition(|u| *u == 64)?",
            "s.iter().position(|u| *u == 64)?",
            "policy",
            "mask_last_at",
            "ROUTING_ASSERT/status/actual_node/\"last-at\"",
        ),
        (
            "src/lib.rs",
            "out.push(*first)",
            "out.push(if (0xd800..=0xdfff).contains(first) {0xfffd} else {*first})",
            "policy",
            "mask_first_utf16",
            "ROUTING_ASSERT/status/actual_node/\"astral-first\"",
        ),
        (
            "src/lib.rs",
            ".filter(|v| v.truthy())",
            ".filter(|v| matches!(v,json::Value::String(_)))",
            "policy",
            "plan_arbitrary",
            "ROUTING_ASSERT/status/actual_node/\"array-plan\"",
        ),
        (
            "src/json.rs",
            "entry.1 = v;",
            "drop(v);",
            "policy",
            "duplicate_last",
            "ROUTING_ASSERT/status/actual_node/\"duplicates\"",
        ),
        (
            "src/keys.rs",
            "Some(user.as_bytes()),",
            "Some(b\"other\"),",
            "policy",
            "wrong_user_reconnect",
            "ROUTING_ASSERT/status/actual_node/\"wrong-user\"",
        ),
        (
            "src/keys.rs",
            "OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC",
            "OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC",
            "keys",
            "nofollow_key",
            "ROUTING_ASSERT/status/nofollow_key",
        ),
    ];
    for (file, old, new, suite, test, id) in mutations {
        let path = base.join(file);
        let bytes = std::fs::read(&path).map_err(|_| "mutant source read")?;
        let source = std::str::from_utf8(&bytes).map_err(|_| "mutant source UTF8")?;
        if source.matches(old).count() != 1 {
            return Err(format!("mutant target not unique: {test}"));
        }
        let changed = source.replacen(old, new, 1);
        let restore = Restore {
            path: path.clone(),
            bytes,
        };
        std::fs::write(&path, changed).map_err(|_| "mutant source write")?;
        let mut c = Command::new(
            "/Users/sebastiandalton/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin/cargo",
        );
        c.args(["test", "--offline", "--manifest-path"])
            .arg(base.join("Cargo.toml"))
            .args([
                "--target-dir",
                "/tmp/m6-chatgpt-status-mutant-target",
                "--test",
                suite,
                test,
                "--",
                "--exact",
                "--nocapture",
            ]);
        c.env(
            "RUSTC",
            "/Users/sebastiandalton/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin/rustc",
        );
        let result = process::bounded(c, Vec::new(), Duration::from_secs(45));
        let classified = mutation::classify(result);
        std::fs::write(&path, &restore.bytes).map_err(|_| "explicit mutant restore")?;
        if std::fs::read(&path).map_err(|_| "mutant restore read")? != restore.bytes {
            return Err("mutant restore mismatch".into());
        }
        mutation::reject_mutant(test, id, classified)?;
        drop(restore);
        println!("semantic mutant rejected: {test}");
    }
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mutation qualification failed: {e}");
            ExitCode::FAILURE
        }
    }
}
