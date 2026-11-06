#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use diary_backup_format::sha256;
use serde_json::Value;
use std::{
    fs,
    process::{Command, Output},
};
fn bytes(hex: &str) -> Vec<u8> {
    hex.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}
fn compare(row: &Value, out: Output) {
    assert_eq!(
        out.status.code(),
        row["exit"].as_i64().map(|v| v as i32),
        "case {}",
        row["case"]
    );
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        row["stdout"].as_str().unwrap(),
        "stdout {}",
        row["case"]
    );
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        row["stderr"].as_str().unwrap(),
        "stderr {}",
        row["case"]
    );
}
#[test]
fn frozen_python_yaml() {
    let rows: Value =
        serde_json::from_str(include_str!("fixtures/yaml-edge-python312.json")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for row in rows.as_array().unwrap() {
        let path = dir.path().join("config.yaml");
        fs::write(&path, bytes(row["input_hex"].as_str().unwrap())).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_diary-config"));
        cmd.env_clear().arg(&path);
        for (key, value) in row["env"].as_object().unwrap() {
            cmd.env(key, value.as_str().unwrap());
        }
        compare(row, cmd.output().unwrap());
    }
}
#[test]
fn frozen_python_json() {
    let rows: Value =
        serde_json::from_str(include_str!("fixtures/json-edge-python312.json")).unwrap();
    for row in rows.as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let body = bytes(row["input_hex"].as_str().unwrap());
        let path = dir.path().join(format!("{}.json", sha256(&body)));
        fs::write(&path, body).unwrap();
        let dest = dir.path().join("destination");
        let output = Command::new(env!("CARGO_BIN_EXE_diary-backup-restore"))
            .env_clear()
            .arg(dir.path())
            .arg(&path)
            .arg(&dest)
            .output()
            .unwrap();
        compare(row, output);
        assert_eq!(dest.exists(), row["destination_created"].as_bool().unwrap());
    }
}
#[test]
fn frozen_python_argv() {
    let rows: Value = serde_json::from_str(include_str!("fixtures/argv-python312.json")).unwrap();
    for row in rows.as_array().unwrap() {
        let args = row["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap());
        let output = Command::new(env!("CARGO_BIN_EXE_diary-backup-restore"))
            .env_clear()
            .args(args)
            .output()
            .unwrap();
        compare(row, output);
    }
}
#[test]
fn frozen_python_io() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let rows: Value = serde_json::from_str(include_str!("fixtures/io-python312.json")).unwrap();
    for row in rows.as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("folder")).unwrap();
        fs::write(dir.path().join("denied.yaml"), "x: synthetic\n").unwrap();
        fs::create_dir(dir.path().join("closed")).unwrap();
        fs::write(dir.path().join("closed/config.yaml"), "x: synthetic\n").unwrap();
        symlink("absent", dir.path().join("broken")).unwrap();
        fs::set_permissions(
            dir.path().join("denied.yaml"),
            fs::Permissions::from_mode(0o0),
        )
        .unwrap();
        fs::set_permissions(dir.path().join("closed"), fs::Permissions::from_mode(0o0)).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_diary-config"));
        cmd.current_dir(dir.path()).env_clear().args(
            row["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap()),
        );
        for (key, value) in row["env"].as_object().unwrap() {
            cmd.env(key, value.as_str().unwrap());
        }
        let output = cmd.output().unwrap();
        fs::set_permissions(
            dir.path().join("denied.yaml"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fs::set_permissions(dir.path().join("closed"), fs::Permissions::from_mode(0o700)).unwrap();
        compare(row, output);
    }
}
