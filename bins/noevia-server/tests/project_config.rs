//! sbstndalton/noevia#2191 round 9 `rust-project-config`: a project's configuration, its instruction
//! skills and a published skill's install through the whole front, with Node absent (an unreachable
//! upstream: anything proxied is a 502) and the skills host a synthetic server on 127.0.0.1
//! (NOEVIA_QA_SKILLS_RAW). Synthetic data only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use noevia_server::config::Config;
use noevia_server::serve::{serve, Limits};
use server_store::rusqlite::Connection;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SCHEMA: &str = include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql");
const CODE: &str = "synthetic-project-config-code";
const ORIGIN: &str = "http://127.0.0.1:18021";

struct Front {
    _data: tempfile::TempDir,
    data: PathBuf,
    addr: SocketAddr,
    jar: String,
    csrf: String,
    user: String,
}

impl Front {
    fn user_dir(&self) -> PathBuf {
        self.data.join("users").join(&self.user)
    }
}

/// One HTTP/1.1 exchange at once: the status, the response head (lower-case names) and the body.
async fn call(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    // Lossy: an original's bytes need not be UTF-8 (lengths are checked through content-length).
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text[9..12].parse().unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    (status, head.to_ascii_lowercase(), body.to_string())
}

async fn front() -> Front {
    front_with(None).await
}

/// The signed-in owner's front; `raw` is the skills host (NOEVIA_QA_SKILLS_RAW).
async fn front_with(raw: Option<&str>) -> Front {
    let f = front_without_folder(raw).await;
    // Node's workspace load made the directory on the user's first request.
    std::fs::create_dir_all(f.user_dir()).unwrap();
    f
}

async fn front_without_folder(value: Option<&str>) -> Front {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let c = Connection::open(server_store::db_path(&data)).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    c.execute(
        "INSERT INTO settings VALUES('setup_code_hash', ?1)",
        [server_auth::js::digest(CODE)],
    )
    .unwrap();
    // Node answers nothing here: every route under test is the front's.
    let mut env: HashMap<&str, String> = HashMap::new();
    env.insert("NOEVIA_LEGACY_UPSTREAM", "http://127.0.0.1:9".into());
    env.insert("UI_PORT", "1".into());
    env.insert("UI_DATA_DIR", data.display().to_string());
    env.insert("NOEVIA_WEB_DIST", data.join("dist").display().to_string());
    env.insert("PUBLIC_ORIGIN", ORIGIN.into());
    if let Some(value) = value {
        env.insert("NOEVIA_QA_SKILLS_RAW", value.into());
    }
    let mut app = noevia_server::App::new(Config::from_lookup(|k| env.get(k).cloned()).unwrap());
    // The test harness is not noevia-server: the owned storage child is the real binary.
    std::sync::Arc::get_mut(&mut app).unwrap().storage_child =
        Some(env!("CARGO_BIN_EXE_noevia-server").into());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(
        listener,
        app,
        Limits::default(),
        std::future::pending(),
    ));
    // Sign the owner in (setup), as the corpus does.
    let setup = format!(
        r#"{{"setupCode":"{CODE}","username":"owner","password":"synthetic project-writes password","diaryEnabled":false}}"#
    );
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "POST /api/setup/complete HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\norigin: {ORIGIN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{setup}",
        setup.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("HTTP/1.1 201"), "{text}");
    let cookies: Vec<String> = text
        .lines()
        .filter_map(|l| {
            l.strip_prefix("set-cookie: ")
                .map(|v| v.split(';').next().unwrap().to_string())
        })
        .collect();
    assert_eq!(cookies.len(), 2, "{text}");
    let csrf = cookies[1].split_once('=').unwrap().1.to_string();
    let jar = cookies.join("; ");
    let user: String = c
        .query_row("SELECT id FROM users", [], |r| r.get(0))
        .unwrap();
    assert!(
        !data.join("users").exists(),
        "setup makes no user directory"
    );
    Front {
        _data: dir,
        data,
        addr,
        jar,
        csrf,
        user,
    }
}

fn headers(f: &Front) -> Vec<(&str, &str)> {
    vec![
        ("cookie", f.jar.as_str()),
        ("x-csrf-token", f.csrf.as_str()),
        ("origin", ORIGIN),
    ]
}

fn hex_sha256(s: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The catalogue core publishes (core server/project-catalogue.cjs).
fn publish_catalogue(f: &Front) {
    std::fs::write(
        f.data.join("project-catalogue.json"),
        r#"{"version":1,"selectable":["core","project-docs","web-search"],"defaults":["core"],"icons":["folder","book"],"known":["core","project-docs","web-search","nextcloud"]}"#,
    )
    .unwrap();
}

fn projects(f: &Front) -> serde_json::Value {
    let text = std::fs::read_to_string(f.user_dir().join("projects.json")).unwrap();
    serde_json::from_str::<serde_json::Value>(&text).unwrap()["projects"].clone()
}

fn json_headers(f: &Front) -> Vec<(&str, &str)> {
    let mut h = headers(f);
    h.push(("content-type", "application/json"));
    h
}

const GOOD: &str =
    "---\nname: Fixture\ndescription: A synthetic skill\nversion: 1.0\n---\nBody of the skill.";

fn seed_project(f: &Front, extra: &str) {
    std::fs::write(
        f.user_dir().join("projects.json"),
        format!(
            r#"{{"projects":[{{"id":"p1","name":"Base","goal":"g","instructions":"i","pinned":false,"archived":false,"sourceFolders":[],"memories":[],"files":[],"routing":"auto","modes":["chat"],"toolboxes":["core"],"toolsMode":"auto","chats":[],"createdAt":1,"updatedAt":2{extra}}}]}}"#
        ),
    )
    .unwrap();
}

fn stored(f: &Front) -> serde_json::Value {
    projects(f)[0].clone()
}

async fn config(f: &Front, body: &str) -> (u16, serde_json::Value) {
    let h = json_headers(f);
    let (status, _, text) = call(f.addr, "POST", "/api/projects/p1/config", &h, body).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn config_answers_the_gate_then_nodes_refusals_and_changes_only_what_the_patch_names() {
    let f = front().await;
    publish_catalogue(&f);
    seed_project(&f, "");
    // Signed out, then without the CSRF token: the gate's answers, before the body is looked at.
    let (status, _, _) = call(
        f.addr,
        "POST",
        "/api/projects/p1/config",
        &[("origin", ORIGIN)],
        "{}",
    )
    .await;
    assert_eq!(status, 401);
    let (status, _, _) = call(
        f.addr,
        "POST",
        "/api/projects/p1/config",
        &[("cookie", f.jar.as_str()), ("origin", ORIGIN)],
        r#"{"name":"x"}"#,
    )
    .await;
    assert_eq!(status, 403);
    let before = std::fs::read(f.user_dir().join("projects.json")).unwrap();
    for (body, status, error) in [
        ("{", 400, "invalid JSON"),
        (r#"{"icon":"rocket"}"#, 400, "Choose a supported project icon."),
        (r#"{"reasoningEffort":"max"}"#, 400, "Invalid reasoning effort"),
        (r#"{"contextProfile":"huge"}"#, 400, "Invalid context profile"),
        (r#"{"modes":["x"]}"#, 400, "modes may only contain chat, cowork, code"),
        (r#"{"routing":"sometimes"}"#, 400, "routing must be 'auto' or 'manual'"),
        (r#"{"provider":"nobody"}"#, 400, "no such provider"),
        (r#"{"toolboxes":"core"}"#, 400, "toolboxes must be an array of toolbox ids"),
        (r#"{"model":"embed-small"}"#, 400, "embed-small is an embedding, reranking or routing model and cannot be used as a chat model — pick a chat model instead."),
        ("null", 500, "Internal error"),
    ] {
        let (got, v) = config(&f, body).await;
        assert_eq!((got, v["error"].as_str()), (status, Some(error)), "{body}");
    }
    assert_eq!(
        std::fs::read(f.user_dir().join("projects.json")).unwrap(),
        before,
        "nothing was written"
    );
    let h = json_headers(&f);
    let (status, _, text) = call(f.addr, "POST", "/api/projects/nope/config", &h, "{}").await;
    assert_eq!(
        (status, text.as_str()),
        (404, r#"{"error":"no such project"}"#)
    );
    let big = format!(r#"{{"name":"{}"}}"#, "x".repeat(1024 * 1024));
    let (status, _, text) = call(f.addr, "POST", "/api/projects/p1/config", &h, &big).await;
    assert_eq!(
        (status, text.as_str()),
        (413, r#"{"error":"Request exceeds size limit"}"#)
    );
    // An empty body is not JSON for this route (JSON.parse('')).
    assert_eq!(config(&f, "").await.0, 400);
    // A patch that changes nothing is a read; one that does moves updatedAt and nothing it did not name.
    let (status, v) = config(&f, r#"{"name":"Base","goal":"g","pinned":false}"#).await;
    assert_eq!((status, v["unchanged"].as_bool()), (200, Some(true)));
    assert_eq!(
        std::fs::read(f.user_dir().join("projects.json")).unwrap(),
        before
    );
    let (status, v) = config(
        &f,
        r##"{"name":"  Renamed  ","pinned":true,"icon":"book","color":"#ABCDEF","routingMode":{"mode":"hybrid"},"sampling":{"temperature":0.4,"bad":1},"toolboxes":["project-docs","nope","core"],"memories":[" a ",""],"provider":"lemonade"}"##,
    )
    .await;
    assert_eq!((status, v), (200, serde_json::json!({"ok": true})));
    let p = stored(&f);
    assert_eq!(p["name"], "Renamed");
    assert_eq!(p["pinned"], true);
    assert_eq!(p["color"], "#abcdef");
    assert_eq!(
        p["routingMode"],
        serde_json::json!({"mode":"hybrid","whenSensitive":"ask"})
    );
    assert_eq!(p["sampling"], serde_json::json!({"temperature": 0.4}));
    assert_eq!(p["toolboxes"], serde_json::json!(["project-docs", "core"]));
    assert_eq!(p["memories"], serde_json::json!(["a"]));
    assert_eq!(p["goal"], "g", "what the patch did not name is as it was");
    assert!(p["updatedAt"].as_i64().unwrap() > 1_600_000_000_000);
    // A model is a manual choice; a model of another registered provider is not judged.
    std::fs::write(
        f.user_dir().join("providers.json"),
        r#"{"providers":[{"id":"cloud-1","label":"C","baseUrl":"https://cloud.example/v1","apiKey":""}]}"#,
    )
    .unwrap();
    let (status, _) = config(&f, r#"{"model":"chat-1"}"#).await;
    assert_eq!(status, 200);
    assert_eq!(
        (
            stored(&f)["routing"].clone(),
            stored(&f)["routingChosen"].clone()
        ),
        ("manual".into(), true.into())
    );
    let (status, _) = config(&f, r#"{"model":"embed-x","provider":"cloud-1"}"#).await;
    assert_eq!(status, 200);
    assert_eq!(stored(&f)["provider"], "cloud-1");
}

#[tokio::test]
async fn the_uploads_a_patch_lists_are_the_projects_and_their_index_work_is_queued() {
    let f = front().await;
    publish_catalogue(&f);
    seed_project(&f, "");
    let (status, _) = config(
        &f,
        r#"{"files":[{"name":"a.md","content":"alpha"},{"name":"b.md","content":"beta"}]}"#,
    )
    .await;
    assert_eq!(status, 200);
    let p = stored(&f);
    assert_eq!(
        p["files"][0],
        serde_json::json!({"name":"a.md","textRef":hex_sha256("alpha"),"chars":5})
    );
    assert!(f
        .user_dir()
        .join("project-sources")
        .join("p1")
        .join(format!("{}.txt", hex_sha256("alpha")))
        .exists());
    let jobs = |f: &Front| -> Vec<(String, String)> {
        server_projects::outbox::jobs(&f.user_dir())
            .unwrap()
            .into_iter()
            .map(|j| (j.kind, j.file.unwrap_or_default()))
            .collect()
    };
    let mut queued = jobs(&f);
    queued.sort();
    assert_eq!(
        queued,
        vec![
            ("index".into(), "a.md".into()),
            ("index".into(), "b.md".into())
        ]
    );
    // The web client's save: the held sources without text, one removed, one added.
    let (status, _) = config(
        &f,
        r#"{"files":[{"name":"a.md"},{"name":"c.md","content":"gamma"}]}"#,
    )
    .await;
    assert_eq!(status, 200);
    let names: Vec<String> = stored(&f)["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["a.md", "c.md"]);
    assert_eq!(
        stored(&f)["files"][0]["textRef"],
        hex_sha256("alpha"),
        "the kept source keeps its text"
    );
    let mut queued = jobs(&f);
    queued.sort();
    assert!(
        queued.contains(&("unindex".into(), "b.md".into())),
        "{queued:?}"
    );
    assert!(
        queued.contains(&("index".into(), "c.md".into())),
        "{queued:?}"
    );
    // A metadata-only entry that names no held source is not a source.
    let (status, _) = config(
        &f,
        r#"{"files":[{"name":"a.md"},{"name":"c.md"},{"name":"ghost.md"}]}"#,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(stored(&f)["files"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn instruction_skills_are_listed_enabled_and_resolved_through_the_routes() {
    let f = front().await;
    publish_catalogue(&f);
    seed_project(&f, "");
    let (status, _) = config(
        &f,
        &format!(
            r#"{{"files":[{{"name":"fix/SKILL.md","content":{}}}]}}"#,
            serde_json::to_string(GOOD).unwrap()
        ),
    )
    .await;
    assert_eq!(status, 200);
    let h = json_headers(&f);
    let get = |path: &'static str| {
        let h = h.clone();
        let addr = f.addr;
        async move {
            let (s, _, t) = call(addr, "GET", path, &h, "").await;
            (
                s,
                serde_json::from_str::<serde_json::Value>(&t).unwrap_or(serde_json::Value::Null),
            )
        }
    };
    let (status, v) = get("/api/projects/p1/instruction-skills").await;
    assert_eq!(status, 200);
    assert_eq!(v["skills"][0]["status"], "review");
    assert_eq!(v["skills"][0]["name"], "Fixture");
    assert_eq!(
        v["skills"][0]["content"], GOOD,
        "the review body rides along for Sources"
    );
    assert_eq!(
        get("/api/projects/nope/instruction-skills").await.1["error"],
        "No such project"
    );
    // Enable it: a stale hash is 409, the current one is stored and listed.
    let put = |body: String| {
        let h = h.clone();
        let addr = f.addr;
        async move {
            let (s, _, t) = call(
                addr,
                "PUT",
                "/api/projects/p1/instruction-skills",
                &h,
                &body,
            )
            .await;
            (
                s,
                serde_json::from_str::<serde_json::Value>(&t).unwrap_or(serde_json::Value::Null),
            )
        }
    };
    assert_eq!(
        put(r#"{"file":"fix/SKILL.md","enabled":true,"hash":"stale"}"#.into())
            .await
            .0,
        409
    );
    assert_eq!(
        put(r#"{"file":"fix/SKILL.md"}"#.into()).await.1["error"],
        "File and enabled boolean required."
    );
    let hash = v["skills"][0]["hash"].as_str().unwrap().to_string();
    let (status, v) = put(format!(
        r#"{{"file":"fix/SKILL.md","enabled":true,"hash":"{hash}"}}"#
    ))
    .await;
    assert_eq!(
        (status, v["skills"][0]["status"].as_str()),
        (200, Some("enabled"))
    );
    assert_eq!(
        stored(&f)["instructionSkills"]["fix/SKILL.md"]["reviewedHash"],
        hash
    );
    // The manifests and a manifest's content.
    let (status, v) = get("/api/projects/p1/instruction-skills/manifests").await;
    assert_eq!((status, v["schemaVersion"].clone()), (200, 1.into()));
    assert_eq!(v["skills"][0]["resolvable"], true);
    assert!(
        !v.to_string().contains("Body of the skill."),
        "metadata only"
    );
    let id = v["skills"][0]["id"].as_str().unwrap().to_string();
    let path = format!("/api/projects/p1/instruction-skills/manifests/{id}/content?version={hash}");
    let (s, _, t) = call(f.addr, "GET", &path, &h, "").await;
    let content: serde_json::Value = serde_json::from_str(&t).unwrap();
    assert_eq!((s, content["content"].as_str()), (200, Some(GOOD)));
    let (s, _, _) = call(
        f.addr,
        "GET",
        &format!("/api/projects/p1/instruction-skills/manifests/{id}/content"),
        &h,
        "",
    )
    .await;
    assert_eq!(s, 400, "a version is required");
    // Before core has published which toolboxes exist, the manifests wait (503), the list does not.
    std::fs::remove_file(f.data.join("project-catalogue.json")).unwrap();
    assert_eq!(
        get("/api/projects/p1/instruction-skills/manifests").await.0,
        503
    );
    assert_eq!(get("/api/projects/p1/instruction-skills").await.0, 200);
    // A browser task's start still goes to Node (502: it is absent here).
    let (s, _, _) = call(f.addr, "POST", "/api/projects/p1/browser", &h, "{}").await;
    assert_eq!(s, 502);
}

/// One synthetic skills host: what each skill name answers.
struct Raw {
    hits: std::sync::Mutex<Vec<String>>,
    hold: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

async fn raw_host() -> (SocketAddr, std::sync::Arc<Raw>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = std::sync::Arc::new(Raw {
        hits: std::sync::Mutex::new(Vec::new()),
        hold: std::sync::Mutex::new(None),
    });
    let keep = std::sync::Arc::clone(&state);
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let state = std::sync::Arc::clone(&keep);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                state.hits.lock().unwrap().push(path.clone());
                let name = path
                    .strip_prefix("/anthropics/skills/main/skills/")
                    .and_then(|p| p.strip_suffix("/SKILL.md"))
                    .unwrap_or("");
                let hold = state.hold.lock().unwrap().take();
                if let (true, Some(rx)) = (name == "slow-skill", hold) {
                    let _ = rx.await;
                }
                let (status, extra, body) = match name {
                    "good-skill" | "slow-skill" => ("200 OK", "", GOOD.to_string()),
                    "plain-skill" => ("200 OK", "", "no front matter".to_string()),
                    "big-skill" => ("200 OK", "", "x".repeat(32 * 1024 + 1)),
                    "exact-skill" => (
                        "200 OK",
                        "",
                        format!("{GOOD}{}", " ".repeat(32 * 1024 - GOOD.len())),
                    ),
                    "gone-skill" => ("404 Not Found", "", "no".to_string()),
                    "broken-skill" => ("500 Internal Server Error", "", "no".to_string()),
                    "moved-skill" => ("302 Found", "location: /elsewhere\r\n", String::new()),
                    _ => ("404 Not Found", "", String::new()),
                };
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    });
    (addr, state)
}

async fn install(f: &Front, body: &str) -> (u16, serde_json::Value) {
    let h = json_headers(f);
    let (s, _, t) = call(f.addr, "POST", "/api/projects/p1/skills/install", &h, body).await;
    (
        s,
        serde_json::from_str(&t).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn a_published_skill_is_fetched_from_the_fixed_host_by_a_checked_name_within_its_caps() {
    let (raw, seen) = raw_host().await;
    let f = front_with(Some(&format!("http://{raw}"))).await;
    publish_catalogue(&f);
    seed_project(&f, "");
    // The gate first, the project second (before the body), the name third (before the network).
    let (s, _, _) = call(
        f.addr,
        "POST",
        "/api/projects/p1/skills/install",
        &[("origin", ORIGIN)],
        "{}",
    )
    .await;
    assert_eq!(s, 401);
    let h = json_headers(&f);
    let (s, _, t) = call(
        f.addr,
        "POST",
        "/api/projects/nope/skills/install",
        &h,
        "not json",
    )
    .await;
    assert_eq!((s, t.as_str()), (404, r#"{"error":"no such project"}"#));
    assert_eq!(
        install(&f, "not json").await,
        (400, serde_json::json!({"error":"invalid JSON"}))
    );
    for bad in [
        r#"{"skill":"../etc"}"#,
        r#"{"skill":"a/b"}"#,
        r#"{"skill":""}"#,
        r#"{"skill":"X"}"#,
        r#"{"skill":"a b"}"#,
        "{}",
        "null",
    ] {
        assert_eq!(
            install(&f, bad).await,
            (400, serde_json::json!({"error":"Unknown skill"})),
            "{bad}"
        );
    }
    assert!(
        seen.hits.lock().unwrap().is_empty(),
        "an invalid name never reaches the network"
    );
    // The skill: one fetch of exactly SKILL.md, saved as a project file that awaits review.
    let (s, v) = install(&f, r#"{"skill":"good-skill"}"#).await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["file"], "good-skill/SKILL.md");
    assert_eq!(v["skills"][0]["status"], "review");
    assert!(v["skills"][0].get("content").is_none());
    assert_eq!(
        seen.hits.lock().unwrap().as_slice(),
        ["/anthropics/skills/main/skills/good-skill/SKILL.md"]
    );
    let file = stored(&f)["files"][0].clone();
    assert_eq!(file["name"], "good-skill/SKILL.md");
    assert_eq!(file["skillOrigin"]["kind"], "published");
    assert_eq!(
        file["skillOrigin"]["sourcePath"],
        "skills/good-skill/SKILL.md"
    );
    assert_eq!(file["skillOrigin"]["digest"], hex_sha256(GOOD));
    assert!(file["skillOrigin"]["retrievedAt"]
        .as_str()
        .unwrap()
        .ends_with('Z'));
    assert_eq!(
        file["textRef"],
        hex_sha256(GOOD),
        "the text is in its own file"
    );
    // The refusals of the host and the text, each its own answer; nothing more is saved.
    for (name, status, message) in [
        ("gone-skill", 404, "That skill has no SKILL.md"),
        ("broken-skill", 502, "The skills repository could not be reached right now."),
        ("big-skill", 422, "That skill is larger than the 32 KiB limit for project skills."),
        ("plain-skill", 422, "This skill cannot be used as a project skill: Use a closed frontmatter block of at most 2 KiB."),
    ] {
        let (s, v) = install(&f, &format!(r#"{{"skill":"{name}"}}"#)).await;
        assert_eq!((s, v["error"].as_str()), (status, Some(message)), "{name}");
    }
    // Exactly 32 KiB is allowed (the cap is on what is more).
    assert_eq!(install(&f, r#"{"skill":"exact-skill"}"#).await.0, 201);
    // A redirect is an error, never followed.
    let (s, v) = install(&f, r#"{"skill":"moved-skill"}"#).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (
            502,
            Some("The skills repository could not be reached right now.")
        )
    );
    assert!(!seen.hits.lock().unwrap().iter().any(|p| p == "/elsewhere"));
    // The same skill again replaces it, and the replaced skill needs review again.
    let hash = {
        let (_, _, t) = call(f.addr, "GET", "/api/projects/p1/instruction-skills", &h, "").await;
        serde_json::from_str::<serde_json::Value>(&t).unwrap()["skills"][0]["hash"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let enable = format!(r#"{{"file":"good-skill/SKILL.md","enabled":true,"hash":"{hash}"}}"#);
    let (s, _, _) = call(
        f.addr,
        "PUT",
        "/api/projects/p1/instruction-skills",
        &h,
        &enable,
    )
    .await;
    assert_eq!(s, 200);
    let (s, v) = install(&f, r#"{"skill":"good-skill"}"#).await;
    assert_eq!(s, 201);
    let row = v["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["file"] == "good-skill/SKILL.md")
        .unwrap()
        .clone();
    assert_eq!(row["status"], "review");
    assert_eq!(
        stored(&f)["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|x| x["name"] == "good-skill/SKILL.md")
            .count(),
        1
    );
}

#[tokio::test]
async fn an_install_the_client_abandons_is_still_saved() {
    let (raw, seen) = raw_host().await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    *seen.hold.lock().unwrap() = Some(rx);
    let f = front_with(Some(&format!("http://{raw}"))).await;
    publish_catalogue(&f);
    seed_project(&f, "");
    let h = json_headers(&f);
    let mut s = TcpStream::connect(f.addr).await.unwrap();
    let body = r#"{"skill":"slow-skill"}"#;
    let mut req = format!(
        "POST /api/projects/p1/skills/install HTTP/1.1\r\nhost: front.test\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (k, v) in &h {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).await.unwrap();
    for _ in 0..200 {
        if !seen.hits.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(
        seen.hits.lock().unwrap().len(),
        1,
        "the front is waiting on the host"
    );
    drop(s);
    let _ = tx.send(());
    for _ in 0..200 {
        if stored(&f)["files"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(stored(&f)["files"][0]["name"], "slow-skill/SKILL.md");
}
