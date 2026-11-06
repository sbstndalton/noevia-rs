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
fn raw_output(
    path: Option<&std::path::Path>,
    env: &std::collections::BTreeMap<String, String>,
) -> (i32, String, String) {
    match diary_operator::config::load(path, std::path::Path::new("/app/config"), env) {
        Ok(cfg) => {
            let warnings = cfg
                .warnings
                .into_iter()
                .map(|s| s + "\n")
                .collect::<String>();
            match cfg.data.pretty() {
                Ok(s) => (0, s + "\n", warnings),
                Err(e) => (1, String::new(), warnings + &e + "\n"),
            }
        }
        Err(e) => (1, String::new(), e + "\n"),
    }
}
fn compare_raw(row: &Value, result: (i32, String, String)) {
    assert_eq!(
        result.0,
        row["exit"].as_i64().unwrap() as i32,
        "{}",
        row["case"]
    );
    assert_eq!(result.1, row["stdout"].as_str().unwrap(), "{}", row["case"]);
    assert_eq!(result.2, row["stderr"].as_str().unwrap(), "{}", row["case"]);
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
        let env = row["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        compare_raw(row, raw_output(Some(&path), &env));
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
        let original = row["args"]
            .as_array()
            .unwrap()
            .first()
            .and_then(Value::as_str);
        let path = original.map(|p| {
            if p.is_empty() {
                std::path::PathBuf::from("")
            } else {
                dir.path().join(p)
            }
        });
        let env = row["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                let value = v.as_str().unwrap();
                (
                    k.clone(),
                    if value.is_empty() {
                        String::new()
                    } else {
                        dir.path().join(value).to_string_lossy().into_owned()
                    },
                )
            })
            .collect();
        let result = raw_output(path.as_deref(), &env);
        let mut expected = row.clone();
        if let Some(name) = original.filter(|s| !s.is_empty()) {
            expected["stderr"] = Value::String(row["stderr"].as_str().unwrap().replace(
                &format!("'{name}'"),
                &format!("'{}'", dir.path().join(name).display()),
            ));
        }
        fs::set_permissions(
            dir.path().join("denied.yaml"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fs::set_permissions(dir.path().join("closed"), fs::Permissions::from_mode(0o700)).unwrap();
        compare_raw(&expected, result);
    }
}
#[test]
fn frozen_review_constructors() {
    let rows: Value =
        serde_json::from_str(include_str!("fixtures/review-yaml-python312.json")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for row in rows.as_array().unwrap() {
        let path = dir.path().join("config.yaml");
        fs::write(&path, bytes(row["input_hex"].as_str().unwrap())).unwrap();
        compare_raw(
            row,
            raw_output(Some(&path), &std::collections::BTreeMap::new()),
        );
    }
}
#[test]
fn prompt_rejects_truthy_non_paths() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("prompts")).unwrap();
    fs::write(dir.path().join("prompts/system.md"), "synthetic default").unwrap();
    for (body, kind) in [
        ("42", "int"),
        ("true", "bool"),
        ("[x]", "list"),
        ("{a: x}", "dict"),
    ] {
        let data =
            diary_operator::yaml::load(format!("prompts: {{system_path: {body}}}").as_bytes())
                .unwrap();
        assert_eq!(diary_operator::config::system_prompt(&data,dir.path()).unwrap_err(),format!("TypeError: argument should be a str or an os.PathLike object where __fspath__ returns a str, not '{kind}'"));
    }
}
