#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
//! The actual Python differential: every case of `tests/fixtures/model-delete-python.v1.json`
//! (recorded from the services app by `tools/gen-model-delete-oracle.py`) is rebuilt in a
//! temporary folder and replayed: each answer, the tree left behind (inside and outside the
//! models folder) and the number of container listings must match. Docker and
//! `/proc/self/mountinfo` are fakes; every name is synthetic.
use super::*;
use crate::{Container, Job};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};

/// sha256 of the services sources the fixture came from (noevia-services 09f7d01).
const ORACLE: [(&str, &str); 6] = [
    (
        "api.py",
        "0b5e9aa8a15f5583618b02d2b42a7695a9bd21e7bc7f018ec2b92ce84feb2570",
    ),
    (
        "main.py",
        "139436d011ca7c14e76db19f4a24cc0c9419c368fb3f895d533115a7d7f74d29",
    ),
    (
        "services.py",
        "1fba80188ab565c2e00ea002217b59f8ae5354052f6641d322f8b24f141da983",
    ),
    (
        "ini.py",
        "b3cb05fd382b5f408ad16430db8841d40c82a3714115233f2e35b2f7f3b61740",
    ),
    (
        "utils.py",
        "3f56fb6b04094ec19c09c191a0d4267e2d233e3461884be0648e4875e6ef6426",
    ),
    (
        "config.py",
        "4708a112769dbc3b9da6c49eeb7057e02d06868ca4af0e0bf4567971a267581a",
    ),
];

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/model-delete-python.v1.json"
    ))
    .unwrap()
}

struct FakeDocker {
    spec: Value,
    lists: AtomicUsize,
}

impl DockerRead for FakeDocker {
    fn containers(&self) -> Job<'_, Vec<Container>> {
        Box::pin(async { Err("unused") })
    }
    fn logs<'a>(&'a self, _: &'a str, _: usize) -> Job<'a, Vec<u8>> {
        Box::pin(async { Err("unused") })
    }
    fn running_containers(&self) -> Job<'_, Option<Vec<Running>>> {
        // `containers.list()` runs only when there is a client.
        if !self.spec.is_null() {
            self.lists.fetch_add(1, Ordering::SeqCst);
        }
        let spec = self.spec.clone();
        Box::pin(async move {
            match spec {
                Value::Null => Ok(None),
                Value::String(s) if s == "fail" => Err("APIError"),
                Value::Array(list) => Ok(Some(list.iter().map(running).collect())),
                other => panic!("{other}"),
            }
        })
    }
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::Null => Vec::new(),
        // A str Entrypoint/Cmd/Args is one argument, as `_container_args` appends it.
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items
            .iter()
            .map(|i| i.as_str().unwrap().to_owned())
            .collect(),
        other => panic!("{other}"),
    }
}

fn running(spec: &Value) -> Running {
    Running {
        name: spec["name"].as_str().unwrap().to_owned(),
        id: spec["id"].as_str().unwrap().to_owned(),
        entrypoint: strings(&spec["entrypoint"]),
        cmd: strings(&spec["cmd"]),
        args: strings(&spec["args"]),
        env: strings(&spec["env"]),
        mounts: spec["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["Destination"].as_str().unwrap_or_default().to_owned(),
                    m["Source"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect(),
    }
}

fn build(root: &Path, case: &Value) -> PathBuf {
    let models = root.join("models");
    match case["models"].as_str() {
        Some("file") => std::fs::write(&models, b"").unwrap(),
        Some("missing") => {}
        _ => std::fs::create_dir(&models).unwrap(),
    }
    for f in case["files"].as_array().into_iter().flatten() {
        let path = models.join(f["name"].as_str().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::File::create(&path)
            .unwrap()
            .set_len(f["size"].as_u64().unwrap())
            .unwrap();
    }
    for link in case["links"].as_array().into_iter().flatten() {
        let path = models.join(link[0].as_str().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(link[1].as_str().unwrap(), &path).unwrap();
    }
    if let Some(text) = case["ini"].as_str() {
        std::fs::write(models.join("models.ini"), text).unwrap();
    }
    models
}

/// The generator's `tree(root)`.
fn tree(root: &Path) -> Value {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<Value>) {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        for name in names {
            let path = dir.join(&name);
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                out.push(serde_json::json!([rel, "link", target.to_str().unwrap()]));
            } else if meta.is_dir() {
                out.push(serde_json::json!([rel, "dir", null]));
                walk(root, &path, out);
            } else {
                out.push(serde_json::json!([rel, "file", meta.len()]));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort_by_key(|v| v.to_string());
    Value::Array(out)
}

/// The router's `fastapi_dict_body_capped` and answer, through the same shared helpers.
async fn answer(
    delete: &ModelDelete,
    content_type: Option<&str>,
    bytes: &[u8],
) -> (u16, String, String) {
    use crate::prompts;
    let json = (
        StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
        "application/json",
    );
    if !content_type.is_none_or(|v| prompts::json_content_type(Some(v))) && !bytes.is_empty() {
        let error = prompts::non_json_validation(bytes).unwrap();
        return (json.0, json.1.into(), error.get().to_owned());
    }
    let decoded = prompts::decode_body(bytes).unwrap();
    if let Some(error) = prompts::validation_with_empty(decoded.as_bytes(), bytes.is_empty()) {
        return (json.0, json.1.into(), error.get().to_owned());
    }
    let body = prompts::parse_json(&decoded).unwrap();
    match delete.post(&body).await {
        Outcome::Json(bytes) => (
            200,
            "application/json".into(),
            String::from_utf8(bytes).unwrap(),
        ),
        Outcome::Conflict(text) => (
            409,
            "application/json".into(),
            serde_json::json!({ "detail": text }).to_string(),
        ),
        Outcome::Failed => (
            500,
            "text/plain; charset=utf-8".into(),
            "Internal Server Error".into(),
        ),
        Outcome::Refused => (
            503,
            "application/json".into(),
            serde_json::json!({ "detail": REFUSAL }).to_string(),
        ),
    }
}

use axum::http::StatusCode;

fn config(models: &Path, mountinfo: &Path, case: &Value) -> Config {
    let env = &case["env"];
    let text = |key: &str| env[key].as_str().unwrap_or_default().to_owned();
    Config {
        models: models.to_str().unwrap().to_owned(),
        ini: models.join("models.ini"),
        host_path: text("MODELS_HOST_PATH"),
        targets: text("MODEL_DOWNLOAD_TARGETS"),
        mountinfo: mountinfo.to_owned(),
        hostname: text("HOSTNAME"),
        embed: env["NOEVIA_EMBED_SERVICE_GUARDED"]
            .as_str()
            .filter(|v| *v == "1")
            .map(|_| {
                (
                    text("NOEVIA_EMBED_SERVICE_CONTAINER"),
                    text("NOEVIA_EMBED_SERVICE_URL"),
                )
            }),
    }
}

#[test]
fn fixture_provenance_is_the_pinned_services_source() {
    let fixture = fixture();
    for (name, digest) in ORACLE {
        assert_eq!(fixture["oracle"][name].as_str(), Some(digest), "{name}");
    }
    assert!(fixture["cases"].as_array().unwrap().len() >= 30);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_python_differential() {
    let fixture = fixture();
    let info = tempfile::tempdir().unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let id = case["id"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let models = build(root, case);
        let mountinfo = info.path().join(format!("{id}-mountinfo"));
        if let Some(lines) = case["mountinfo"].as_array() {
            let text: Vec<String> = lines
                .iter()
                .map(|l| {
                    l.as_str()
                        .unwrap()
                        .replace("@MODELS@", models.to_str().unwrap())
                })
                .collect();
            std::fs::write(&mountinfo, text.join("\n") + "\n").unwrap();
        }
        for name in case["readonly"].as_array().into_iter().flatten() {
            use std::os::unix::fs::PermissionsExt;
            let path = models.join(name.as_str().unwrap());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        let docker = Arc::new(FakeDocker {
            spec: case["docker"].clone(),
            lists: AtomicUsize::new(0),
        });
        let delete = ModelDelete::with_root(
            config(&models, &mountinfo, case),
            docker.clone(),
            Root::synthetic,
        );
        let diverges = case.get("diverges").is_some();
        for (step, (request, expected)) in case["requests"]
            .as_array()
            .unwrap()
            .iter()
            .zip(case["results"].as_array().unwrap())
            .enumerate()
        {
            docker.lists.store(0, Ordering::SeqCst);
            let (status, kind, body) = answer(
                &delete,
                request["content_type"].as_str(),
                request["body"].as_str().unwrap().as_bytes(),
            )
            .await;
            let body = body.replace(root.to_str().unwrap(), "@ROOT@");
            if diverges {
                flat_fallback_is_refused(id, status, &body, root);
                continue;
            }
            let ctx = format!("{id} step {step}: {}", request["body"]);
            assert_eq!(
                status,
                expected["status"].as_u64().unwrap() as u16,
                "{ctx}: {body}"
            );
            assert_eq!(
                Some(kind.as_str()),
                expected["content_type"].as_str(),
                "{ctx}"
            );
            assert_eq!(body, expected["body"].as_str().unwrap(), "{ctx}");
            assert_eq!(tree(root), expected["tree"], "{ctx}");
            assert_eq!(
                docker.lists.load(Ordering::SeqCst) as u64,
                expected["docker_lists"].as_u64().unwrap(),
                "{ctx}"
            );
        }
        for name in case["readonly"].as_array().into_iter().flatten() {
            use std::os::unix::fs::PermissionsExt;
            let path = models.join(name.as_str().unwrap());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

/// The one deliberate difference in the fixture: Python's second `dup.gguf` deletes
/// `sub/dup.gguf`, which nobody asked for. Natively it is "not found" and stays.
fn flat_fallback_is_refused(id: &str, status: u16, body: &str, root: &Path) {
    assert_eq!(id, "flat-fallback");
    assert_eq!(status, 200);
    assert_eq!(
        body,
        r#"{"results":[{"key":"dup.gguf","ok":true,"message":"deleted 1 file(s)","freed":3,"freedH":"3 B"},{"key":"dup.gguf","ok":false,"message":"not found: dup.gguf","freed":0,"freedH":""}]}"#
    );
    assert!(root.join("models/sub/dup.gguf").exists());
    assert!(root.join("models/sub/other.gguf").exists());
    assert!(!root.join("models/dup.gguf").exists());
}

#[test]
fn normpath_is_posixpath() {
    for (input, expected) in [
        ("/a/b/../c", "/a/c"),
        ("//a//b/", "//a/b"),
        ("///a", "/a"),
        ("/../a/./b", "/a/b"),
        ("/", "/"),
    ] {
        assert_eq!(normpath(input), expected, "{input}");
    }
}

#[test]
fn hf_cache_folders() {
    assert_eq!(
        fs::hf_repo("models--o--r/snapshots/rev").as_deref(),
        Some("models--o--r")
    );
    assert_eq!(
        fs::hf_repo("hf/models--o--r/snapshots/rev/Q4").as_deref(),
        Some("hf/models--o--r")
    );
    assert_eq!(fs::hf_repo("models--o--r/snapshots"), None);
    assert_eq!(fs::hf_repo("models--/snapshots/rev"), None);
    assert_eq!(fs::hf_repo("x/models--a/blobs/rev"), None);
}

#[test]
fn host_ports_follow_urllib_or_refuse() {
    assert_eq!(
        host_port(" http://Embed:8080/v1"),
        Some(("embed".into(), 8080))
    );
    assert_eq!(host_port("https://e"), Some(("e".into(), 443)));
    assert_eq!(host_port("http://u:p@[::1]:81/x"), Some(("::1".into(), 81)));
    assert_eq!(host_port("http://e:0"), Some(("e".into(), 80)));
    for bad in ["", "embed:8080", "http://", "http://e:x", "http://e:99999"] {
        assert_eq!(host_port(bad), None, "{bad}");
    }
}

/// Values Python cannot iterate are its 500; nested containers in the list are refused (503)
/// rather than printed by a `repr` this port does not carry.
#[test]
fn keys_follow_python_iteration() {
    let parse = |t: &str| crate::prompts::parse_json(t).unwrap();
    assert_eq!(
        keys(&parse(r#"{"models": [0, false, "", -0.0, 7]}"#)).unwrap(),
        ["0", "False", "", "-0.0", "7"]
    );
    assert_eq!(
        keys(&parse(r#"{"models": 0}"#)).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(keys(&parse(r#"{"models": true}"#)), Err(Outcome::Failed));
    assert_eq!(keys(&parse(r#"{"models": 2.5}"#)), Err(Outcome::Failed));
    assert_eq!(keys(&parse(r#"{"models": "ab"}"#)).unwrap(), ["a", "b"]);
    assert_eq!(
        keys(&parse(r#"{"models": {"b": 1, "a": 2, "b": 3}}"#)).unwrap(),
        ["b", "a"]
    );
    assert_eq!(keys(&parse(r#"{"models": [[]]}"#)).unwrap(), ["[]"]);
}

/// A folder swapped for a link between the snapshot and the deletion (here: before `delete`
/// runs) is never entered: nothing outside the models folder is touched.
#[test]
fn a_folder_swapped_for_a_link_is_never_entered() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let models = root.join("models");
    let outside = root.join("outside");
    std::fs::create_dir_all(&models).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("a.gguf"), b"keep").unwrap();
    std::fs::write(outside.join("notes.txt"), b"keep").unwrap();
    std::os::unix::fs::symlink(&outside, models.join("sub")).unwrap();
    let entry = Entry {
        model: crate::model_updates::Model {
            base: "a.gguf".into(),
            subdir: "sub".into(),
            parts: vec![crate::model_updates::Part {
                name: "a.gguf".into(),
                size: 4,
                mtime: 0.0,
            }],
            total: 4,
            mtime: 0.0,
            companion: false,
            companion_name: String::new(),
            companion_bytes: 0,
            companion_parts: Vec::new(),
        },
        aliases: Vec::new(),
        unknowable: false,
    };
    let config = Config {
        models: models.to_str().unwrap().to_owned(),
        ini: models.join("models.ini"),
        host_path: String::new(),
        targets: String::new(),
        mountinfo: root.join("none"),
        hostname: String::new(),
        embed: None,
    };
    let (ok, message, freed) = fs::delete(&config, &models, &entry);
    assert!(!ok, "{message}");
    assert_eq!(freed, 0);
    assert!(
        message.starts_with("failed to delete a.gguf: "),
        "{message}"
    );
    assert_eq!(std::fs::read(outside.join("a.gguf")).unwrap(), b"keep");
    assert_eq!(std::fs::read(outside.join("notes.txt")).unwrap(), b"keep");
}
