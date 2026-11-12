//! No server or model: exercise the real publication lock and replay state comparator.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use replay::{bind::Bindings, state};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

const LOCK: &str = "embedding-config.json.lock";
const CONFIG: &str = "embedding-config.json";

fn helper(operation: &str, seed: &Path, work: Option<&Path>) -> bool {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../stats-seed.py");
    let mut command = Command::new("python3");
    command.arg(script).arg(operation).arg(seed);
    if let Some(work) = work {
        command.arg(work);
    }
    command.output().unwrap().status.success()
}

fn snapshot(dir: &Path) -> serde_json::Value {
    state::snapshot(dir, "cowork.db", &[], &Bindings::new("<origin>"))
}

fn copy_seed(seed: &Path, work: &Path) {
    std::fs::create_dir(work).unwrap();
    for entry in std::fs::read_dir(seed).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), work.join(entry.file_name())).unwrap();
    }
}

#[test]
fn unseeded_real_lock_reproduces_only_the_observed_extra_path() {
    let root = tempfile::tempdir().unwrap();
    let reference = root.path().join("reference");
    let native = root.path().join("native");
    std::fs::create_dir(&reference).unwrap();
    std::fs::create_dir(&native).unwrap();
    drop(server_projects::lock::acquire(&native.join(CONFIG)).unwrap());
    let differences = state::compare(&snapshot(&reference), &snapshot(&native));
    assert!(differences.iter().any(|d| {
        d.at == "state/files (paths)"
            && d.expected == "missing []"
            && d.actual == "extra [\"embedding-config.json.lock\"]"
    }));
    assert!(differences.iter().all(|d| d.at.starts_with("state/files")));
}

#[test]
fn common_seed_is_equal_with_or_without_real_worker_lock_acquisition() {
    let root = tempfile::tempdir().unwrap();
    let seed = root.path().join("seed");
    std::fs::create_dir(&seed).unwrap();
    assert!(helper("initialize", &seed, None));
    for config in [
        None,
        Some(b"{\"version\":1,\"synthetic\":true}\n".as_slice()),
    ] {
        if let Some(bytes) = config {
            std::fs::write(seed.join(CONFIG), bytes).unwrap();
        }
        // Each schedule has its own copy; the common seed predates both copies.
        let pair = tempfile::tempdir_in(root.path()).unwrap();
        let reference = pair.path().join("reference");
        copy_seed(&seed, &reference);
        let expected = snapshot(&reference);
        for touched in [false, true] {
            let native = pair.path().join(format!("native-{touched}"));
            copy_seed(&seed, &native);
            let before = std::fs::metadata(native.join(LOCK)).unwrap();
            if touched {
                for _ in 0..2 {
                    let guard = server_projects::lock::acquire(&native.join(CONFIG)).unwrap();
                    drop(guard);
                    let after = std::fs::metadata(native.join(LOCK)).unwrap();
                    assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
                    assert_eq!(after.permissions().mode() & 0o7777, 0o600);
                    assert_eq!(after.len(), 0);
                }
            }
            assert!(helper("verify", &seed, Some(&native)));
            assert!(state::compare(&expected, &snapshot(&native)).is_empty());
            assert_eq!(std::fs::read(native.join(CONFIG)).ok().as_deref(), config);
            // Unexpected artifacts stay in the REAL comparator, with no ignores.
            for name in [
                "sibling.lock",
                "nested/embedding-config.json.lock",
                "product.json",
            ] {
                let extra = tempfile::tempdir_in(root.path()).unwrap();
                let work = extra.path().join("work");
                copy_seed(&seed, &work);
                let path = work.join(name);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, b"").unwrap();
                assert!(
                    !state::compare(&expected, &snapshot(&work)).is_empty(),
                    "{name}"
                );
            }
            std::fs::remove_file(native.join(LOCK)).unwrap();
            assert!(!helper("verify", &seed, Some(&native)));
            assert!(!state::compare(&expected, &snapshot(&native)).is_empty());
        }
        // A product-config change is rejected even though path-only snapshots
        // cannot distinguish content; the workflow runs both checks.
        std::fs::write(reference.join(CONFIG), b"changed").unwrap();
        assert!(!helper("verify", &seed, Some(&reference)));
    }
}
