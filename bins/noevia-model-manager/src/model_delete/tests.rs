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

/// sha256 of the services sources the fixture came from (noevia-services 54b8b51, sbstndalton/noevia-services#81).
const ORACLE: [(&str, &str); 6] = [
    (
        "api.py",
        "55651c96d06267840dfcbdbe9c167919ebb81cb4d65b0b595728b27d37949e22",
    ),
    (
        "main.py",
        "d6124693930a163c47a9856c71486570798e2581850cb23a84dc25304bce4f3d",
    ),
    (
        "services.py",
        "c148d1ca50d78c9adf0a41dcf3cbae66064965981bfb203616d3fca9bfc474c5",
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

// ------------------------------------------------ noevia#1966: active downloads

/// A server that accepts and never answers: a job it serves stays queued or downloading.
async fn silent_server() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
        }
    });
    (port, task)
}

fn plain_config(models: &Path, info: &Path) -> Config {
    Config {
        models: models.to_str().unwrap().to_owned(),
        ini: models.join("models.ini"),
        host_path: String::new(),
        targets: String::new(),
        mountinfo: info.join("absent-mountinfo"),
        hostname: String::new(),
        embed: None,
    }
}

fn no_docker() -> Arc<FakeDocker> {
    Arc::new(FakeDocker {
        spec: Value::Null,
        lists: AtomicUsize::new(0),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_active_download_into_the_folder_refuses_the_delete() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir_all(models.join("sub")).unwrap();
    std::fs::write(models.join("sub/a.gguf"), [0u8; 6]).unwrap();
    std::fs::write(models.join("sub/b.gguf.download"), [0u8; 7]).unwrap();
    std::fs::write(models.join("flat.gguf"), [0u8; 3]).unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let table = crate::downloads::Downloads::new(crate::downloads::Config::production(
        models.to_str().unwrap().to_owned(),
        data.to_str().unwrap(),
        1,
    ))
    .unwrap();
    let (port, server) = silent_server().await;
    let queued = table.queue(vec![crate::downloads::Planned {
        repo_id: "example.test".into(),
        filename: "sub/b.gguf".into(),
        url: format!("http://127.0.0.1:{port}/b.gguf"),
        total_bytes: 0,
    }]);
    assert_eq!(queued, crate::downloads::Queued::Done);
    assert_eq!(table.active(), 1);
    let delete = ModelDelete::with_root(
        plain_config(&models, dir.path()),
        no_docker(),
        Root::synthetic,
    )
    .with_downloads(table.clone());

    let (status, _, body) = answer(
        &delete,
        Some("application/json"),
        br#"{"models":["sub/a.gguf"]}"#,
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(
        body,
        serde_json::json!({"detail": "a.gguf has a download queued or running in its folder: sub/b.gguf. Wait for it to finish or cancel it first; nothing was deleted."}).to_string()
    );
    assert!(models.join("sub/a.gguf").exists());
    assert!(models.join("sub/b.gguf.download").exists());

    // A flat model is blocked only by its own files.
    let (status, _, body) = answer(
        &delete,
        Some("application/json"),
        br#"{"models":["flat.gguf"]}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(!models.join("flat.gguf").exists());

    // Once the job is cancelled (which removes its partial file) the delete goes ahead. A partial
    // file without a job (a restart lost it) is still another model on its way: per file, and the
    // folder stays.
    let list: Value = serde_json::from_str(&table.list()).unwrap();
    assert!(table.cancel(list["jobs"][0]["id"].as_str().unwrap()));
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while table.active() > 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap(); // the cancelled job settles
    std::fs::write(models.join("sub/c.gguf.download"), [0u8; 5]).unwrap();
    let (status, _, body) = answer(
        &delete,
        Some("application/json"),
        br#"{"models":["sub/a.gguf"]}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""message":"deleted 1 file(s)""#), "{body}");
    assert!(!models.join("sub/a.gguf").exists());
    assert!(models.join("sub/c.gguf.download").exists());
    server.abort();
}

#[test]
fn download_blockers_cover_own_files_the_folder_and_the_hf_repository() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    let snap = "models--o--r/snapshots/rev";
    std::fs::create_dir_all(models.join(snap)).unwrap();
    std::fs::create_dir_all(models.join("sub")).unwrap();
    std::fs::write(models.join(format!("{snap}/q.gguf")), [0u8; 4]).unwrap();
    std::fs::write(models.join("sub/a.gguf"), [0u8; 4]).unwrap();
    std::fs::write(models.join("mmproj-flat.gguf"), [0u8; 2]).unwrap();
    std::fs::write(models.join("flat.gguf"), [0u8; 4]).unwrap();
    let listed =
        crate::model_detail::snapshot(&models, &models.join("models.ini"), Root::synthetic)
            .ok()
            .flatten()
            .unwrap();
    let entries = &listed.entries;
    let find = |key: &str| entries.iter().find(|e| e.route_key() == key).unwrap();
    let dests: Vec<String> = [
        "sub/./b.gguf",
        "models--o--r/blobs/x.gguf",
        "flat.gguf",
        "subway/c.gguf",
        "other/d.gguf",
    ]
    .map(String::from)
    .to_vec();
    assert_eq!(blockers(find("sub/a.gguf"), &dests), ["sub/b.gguf"]);
    assert_eq!(
        blockers(find(&format!("{snap}/q.gguf")), &dests),
        ["models--o--r/blobs/x.gguf"]
    );
    assert_eq!(blockers(find("flat.gguf"), &dests), ["flat.gguf"]);
    assert!(blockers(find("flat.gguf"), &["sub/../mmproj-x.gguf".into()]).is_empty());
}
