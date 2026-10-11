//! Four new fixed, actual source mutations. No Node/oracle/old codec control executes.
#[allow(dead_code)]
#[path = "support/capped_capture.rs"]
mod capture;
#[allow(dead_code)]
#[path = "support/mutant_classifier.rs"]
mod classifier;
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    process::{Command, ExitCode},
    time::Duration,
};
fn read(p: &Path) -> Result<String, String> {
    String::from_utf8(capture::read(p).map_err(|_| "read refused")?)
        .map_err(|_| "source encoding".into())
}
fn put(p: &Path, b: &[u8]) -> Result<(), String> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(p)
        .map_err(|_| "capture create")?;
    f.write_all(b).map_err(|_| "capture write".into())
}
fn directory(p: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(p)
        .map_err(|_| "directory create".into())
}
fn copy_tree(
    from: &Path,
    to: &Path,
    depth: usize,
    count: &mut usize,
    total: &mut usize,
) -> Result<(), String> {
    if depth > 8 {
        return Err("depth".into());
    }
    directory(to)?;
    for entry in fs::read_dir(from).map_err(|_| "source directory")? {
        let entry = entry.map_err(|_| "source entry")?;
        let m = entry.file_type().map_err(|_| "source type")?;
        let dest = to.join(entry.file_name());
        if m.is_dir() {
            copy_tree(&entry.path(), &dest, depth + 1, count, total)?;
        } else if m.is_file() {
            if entry.file_name() == "owner_secret.rs" {
                continue;
            }
            *count = count.checked_add(1).filter(|&n| n <= 128).ok_or("count")?;
            let bytes = capture::read(&entry.path()).map_err(|_| "copy read")?;
            *total = total
                .checked_add(bytes.len())
                .filter(|&n| n <= 16 * 1024 * 1024)
                .ok_or("total")?;
            put(&dest, &bytes)?;
        } else {
            return Err("nonregular source".into());
        }
    }
    Ok(())
}
fn run() -> Result<(), String> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let continuing = std::env::args().nth(1).as_deref() == Some("--continue-owner-87763");
    if std::env::args().count() != if continuing { 2 } else { 1 } {
        return Err("arguments".into());
    }
    let root = if continuing {
        std::path::PathBuf::from(
            "/var/folders/_4/jbtxcfjd7hj2x9kvw4z5k7gm0000gp/T/m4-auth-row-mutants-87763",
        )
    } else {
        std::env::temp_dir().join(format!("m4-auth-row-mutants-{}", std::process::id()))
    };
    if !continuing {
        directory(&root)?;
    }
    let snapshot = root.join("source");
    if !continuing {
        directory(&snapshot)?;
    }
    let original = read(&source.join("src/owner_secret.rs"))?;
    let controls = read(&source.join("tests/owner_secret_controls.rs"))?;
    let mut manifest = read(&source.join("Cargo.toml"))?;
    for name in [
        "dav-parse",
        "storage-path",
        "s3-list-parse",
        "s3-sign",
        "js-json",
        "server-auth",
        "server-store",
        "secret-envelope",
    ] {
        manifest = manifest.replace(
            &format!("path = \"../{name}\""),
            &format!("path = {:?}", source.parent().ok_or("parent")?.join(name)),
        );
    }
    let schema = source
        .parent()
        .ok_or("parent")?
        .join("server-store/tests/fixtures/node-schema.sql");
    let transformed = controls.replace(
        "\"../../server-store/tests/fixtures/node-schema.sql\"",
        &format!("{schema:?}"),
    );
    if continuing {
        if read(&snapshot.join("owner_secret.original.rs"))? != original
            || read(&snapshot.join("controls.original.rs"))? != controls
            || read(&snapshot.join("manifest.original.toml"))? != read(&source.join("Cargo.toml"))?
        {
            return Err("retained source binding".into());
        }
    } else {
        put(
            &snapshot.join("owner_secret.original.rs"),
            original.as_bytes(),
        )?;
        put(&snapshot.join("controls.original.rs"), controls.as_bytes())?;
        put(
            &snapshot.join("manifest.original.toml"),
            read(&source.join("Cargo.toml"))?.as_bytes(),
        )?;
    }
    for (name, needle, replacement, test, id) in [
        (
            "OWNER",
            "bound.push_str(owner);",
            "bound.push_str(\"foreign\");",
            "owner_binding_policy",
            "M4_OWNER_BINDING",
        ),
        (
            "ROLE",
            "role: identity.user.role,",
            "role: identity.account_role,",
            "effective_role_policy",
            "M4_EFFECTIVE_ROLE",
        ),
        (
            "AAD",
            "self.keys.open(value, Some(&self.owner))",
            "self.keys.open(value, None)",
            "aad_owner_policy",
            "M4_AAD_OWNER",
        ),
        (
            "CAP",
            "plain.len() > DECRYPTED_BYTES",
            "plain.len() > DECRYPTED_BYTES + 1",
            "decrypted_cap_policy",
            "M4_DECRYPTED_CAP",
        ),
    ] {
        if original.matches(needle).count() != 1 {
            return Err("unique anchor".into());
        }
        let policy =
            classifier::Policy::bind_at(&transformed, test, id, "tests/network_policies.rs", 7)?;
        let dir = root.join(name);
        if continuing && name == "OWNER" {
            if read(&dir.join("src/owner_secret.rs"))? != original.replacen(needle, replacement, 1)
                || read(&dir.join("tests/network_policies.rs"))? != transformed
                || read(&dir.join("Cargo.toml"))? != manifest
                || read(&dir.join("Cargo.lock"))? != read(&source.join("Cargo.lock"))?
                || read(&dir.join("status"))? != "101\n"
            {
                return Err("retained mutation binding".into());
            }
            let stdout = capture::read(&dir.join("stdout")).map_err(|_| "retained stdout")?;
            let stderr = capture::read(&dir.join("stderr")).map_err(|_| "retained stderr")?;
            if !exact_target_accepts(&policy, Some(101), &stdout, &stderr, source)? {
                return Err("retained OWNER rejected".into());
            }
            println!("OWNER retained capture reclassified; no execution; captures={dir:?}");
            continue;
        }
        directory(&dir)?;
        copy_tree(&source.join("src"), &dir.join("src"), 0, &mut 0, &mut 0)?;
        // The copied file is our own private snapshot; replacement is the sole source mutation.
        put(
            &dir.join("src/owner_secret.rs"),
            original.replacen(needle, replacement, 1).as_bytes(),
        )?;
        directory(&dir.join("tests"))?;
        put(
            &dir.join("tests/network_policies.rs"),
            transformed.as_bytes(),
        )?;
        put(&dir.join("Cargo.toml"), manifest.as_bytes())?;
        put(
            &dir.join("Cargo.lock"),
            read(&source.join("Cargo.lock"))?.as_bytes(),
        )?;
        let cargo = std::env::var_os("M4_CARGO").ok_or("fixed cargo missing")?;
        let mut cmd = Command::new(&cargo);
        cmd.current_dir(&dir).env_clear()
   .env("PATH","/Users/sebastiandalton/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin:/usr/bin:/bin")
   .env("HOME",std::env::var_os("HOME").ok_or("home")?).env("LC_ALL","C").env("CARGO_TARGET_DIR",source.join("target/auth-row-mutants"))
   .args(["test","--offline","--locked","--test","network_policies",test,"--","--exact"]);
        put(&dir.join("command.txt"),format!("cargo={cargo:?}\ncwd={dir:?}\ntarget={:?}\ntest --offline --locked --test network_policies {test} -- --exact\n",source.join("target/auth-row-mutants")).as_bytes())?;
        let output = process::bounded(cmd, Vec::new(), Duration::from_secs(45))?;
        put(&dir.join("stdout"), &output.stdout)?;
        put(&dir.join("stderr"), &output.stderr)?;
        put(
            &dir.join("status"),
            format!("{}\n", output.status.code().unwrap_or(-1)).as_bytes(),
        )?;
        if !exact_target_accepts(
            &policy,
            output.status.code(),
            &output.stdout,
            &output.stderr,
            source,
        )? {
            return Err(format!(
                "{name} not a strict semantic kill; captures={dir:?}"
            ));
        }
        println!("{name} strict semantic kill {test} {id}; captures={dir:?}");
    }
    println!("frozen source and four exact mutations at {root:?}");
    Ok(())
}
fn exact_target_accepts(
    policy: &classifier::Policy,
    status: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    source: &Path,
) -> Result<bool, String> {
    let raw = std::str::from_utf8(stderr).map_err(|_| "stderr encoding")?;
    let actual = format!(
        "{}/target/auth-row-mutants/debug/deps/network_policies-",
        source.display()
    );
    let canonical = format!("{}/target/debug/deps/network_policies-", source.display());
    let mut count = 0;
    let mut view = String::new();
    for line in raw.split_inclusive('\n') {
        if line.trim_start().starts_with("Running ") && line.contains(&actual) {
            if line.matches(&actual).count() != 1 {
                return Err("target ambiguity".into());
            }
            count += 1;
            view.push_str(&line.replacen(&actual, &canonical, 1));
        } else {
            view.push_str(line);
        }
    }
    if count != 1 {
        return Err("exact target binding".into());
    }
    Ok(policy.accepts(status, stdout, view.as_bytes()))
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("auth-row mutation refused: {e}");
            ExitCode::FAILURE
        }
    }
}
