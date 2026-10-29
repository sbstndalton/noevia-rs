//! A project's create and delete (sbstndalton/noevia#2191 round 2, group `rust-project-lifecycle`):
//! core routes/projects.cjs `POST /api/projects` and `DELETE /api/projects/{projectId}`, with the
//! store steps they ran (projects.cjs `createProject`, `deleteProject`, `purgeProjectChats`,
//! `removeChatAttachments`, `pruneDocuments`, `sweepDeletedProject`), answer for answer.
//!
//! Create
//! - The body is read up to http.cjs readBody's 1 MiB (413 `Request exceeds size limit` past it);
//!   not JSON is 400 `invalid JSON`. Then, in Node's order: an invalid `reasoningEffort` (400
//!   `Invalid reasoning effort`), an empty name after `String(...).trim().slice(0, 120)` (400
//!   `name required`), `modes` (project-modes.cjs sanitize's two 400s), `icon`/`color`
//!   (project-appearance.cjs's two 400s). A `null` body is Node's TypeError, 500.
//! - The project has Node's keys in Node's order (JSON.stringify drops the undefined ones); its id
//!   is `proj-<Date.now()>-<6 base-36 chars>`. Toolboxes are filtered against the catalogue core
//!   publishes ([`Catalogue`]: the discovered toolbox ids, which include MCP boxes, are core's
//!   runtime state); create-time files turn the Project documents box on (#659) unless the request
//!   chose its toolboxes; with browsable storage the folder is reserved (named, never created,
//!   #589) as project-folders.cjs `reserveProjectFolder` names it, among the folders the other
//!   projects hold or reserved, read under the lock.
//! - Its file texts are written as source-text.cjs `externalize` writes them
//!   (`project-sources/<id>/<sha256>.txt`, #1806), the entry keeping `textRef`, `chars` and
//!   `skillHeader`; then it is put first in the list and every project's instruction skills are reconciled
//!   (`saveProjects`), under the projects.json lock, on the file as it is then.
//! - Each create-time file gets an index job in the outbox ([`crate::outbox`]), after the save;
//!   core's executor indexes it (the vector store is core's sqlite-vec index).
//!
//! Delete
//! - The project id is percent-decoded (a bad escape is Node's URIError, 500). Not in the list:
//!   404 `no such project`. The record is removed under the lock (with every remaining project
//!   reconciled, as `saveProjects` does); a failed save answers `saveFailureReply`'s `could not
//!   delete the project; nothing was changed` and nothing else is removed (#1914).
//! - Then: the project's upload originals, document pages/originals and images (its
//!   `project-uploads/<sha256 id>`, `project-documents/<sha256 id>`, `project-assets/<id>`), the
//!   RAG index (a tombstone in the outbox first, then `rag/<id>.db`, `-wal`, `-shm`), the empty
//!   directories it leaves (never recursive), the storage folder sweep (queued for core's executor,
//!   which runs project-sweep.cjs `sweepRemote`), and its chats: each through the store's
//!   `Unlisted` delete (a chat another list holds by now is left alone), then that chat's
//!   attachments project, deleted the same way without its chats. Best effort after the save, as
//!   in Node: a failure there is logged and the answer is still 200 `{ ok: true }`.
//!
//! Stricter than Node: a symlink among those directories (or as one of them) is never followed:
//! Node's `readdirSync` followed a symlinked directory and removed the files it pointed at. A
//! project id that could name a path outside `rag/` (a `/`, `\`, NUL, `.` or `..`) removes no RAG
//! file.
//!
//! Not ported (core keeps running it after a create, see contracts/http/README.md): #687's move of
//! create-time text files into the project's storage folder. With storage connected those files
//! stay noevia's own plain-named copies, exactly as Node left them when that storage write failed;
//! the first edit of one moves it into the folder (core writeProjectTextFile `adoptTo`).

use crate::chat_delete::{self, DeleteError, Kind};
use crate::chats;
use crate::free_chats;
use crate::journal::Origin;
use crate::js;
use crate::outbox;
use crate::skills;
use crate::store;
use crate::{Ctx, Reply};
use js_json::wtf16;
use js_json::JValue;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// http.cjs readBody's default limit.
pub const BODY_LIMIT: usize = 1024 * 1024;
/// contracts/project-limits.json `nameMaxLength`.
pub const NAME_MAX: usize = 120;
/// project-modes.cjs MODES.
const MODES: [&str; 3] = ["chat", "cowork", "code"];
/// reasoning-effort.cjs EFFORTS.
const EFFORTS: [&str; 3] = ["default", "low", "high"];
/// project-docs-default.cjs BOX.
const DOCS_BOX: &str = "project-docs";
/// The catalogue core publishes beside the users directory.
pub const CATALOGUE_FILE: &str = "project-catalogue.json";
/// The delete's failure message (routes/projects.cjs).
pub const DELETE_FAILED: &str = "could not delete the project; nothing was changed";

/// What a create needs of core's runtime state, read from `UI_DATA_DIR/project-catalogue.json`
/// (core server/project-catalogue.cjs writes it whenever it changes): the toolbox ids a project
/// may select (`allToolboxes()` without connector boxes, MCP boxes included), the default list and
/// the project icon ids (contracts/project-icons.json).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalogue {
    pub selectable: Vec<String>,
    pub defaults: Vec<String>,
    pub icons: Vec<String>,
}

fn strings(v: &JValue) -> Option<Vec<String>> {
    match v {
        JValue::Arr(items) => items
            .iter()
            .map(|i| i.as_str().map(str::to_string))
            .collect(),
        _ => None,
    }
}

impl Catalogue {
    /// The catalogue, or `None` while core has not published one (a create is then refused 503).
    pub fn read(data_dir: &Path) -> Option<Catalogue> {
        let v = store::read_json(&data_dir.join(CATALOGUE_FILE)).ok()??;
        if v.get("version").as_num() != Some(1.0) {
            return None;
        }
        Some(Catalogue {
            selectable: strings(v.get("selectable"))?,
            defaults: strings(v.get("defaults"))?,
            icons: strings(v.get("icons"))?,
        })
    }

    /// [`Catalogue::read`], waiting up to `wait` for core to publish it (a create that arrives while
    /// core is still starting is served once it has, instead of a 503).
    pub fn read_waiting(data_dir: &Path, wait: std::time::Duration) -> Option<Catalogue> {
        let until = std::time::Instant::now() + wait;
        loop {
            if let Some(c) = Catalogue::read(data_dir) {
                return Some(c);
            }
            if std::time::Instant::now() >= until {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// toolboxes.cjs `sanitizeToolboxes(value)`.
    fn sanitize(&self, value: &JValue) -> Option<Vec<String>> {
        let JValue::Arr(items) = value else {
            return None;
        };
        let mut out: Vec<String> = Vec::new();
        for item in items {
            if let Some(id) = item.as_str() {
                if self.selectable.iter().any(|s| s == id) && !out.iter().any(|o| o == id) {
                    out.push(id.to_string());
                }
            }
        }
        Some(out)
    }
}

/// The user's storage as a create sees it (`authService.getStorage(userId, true)`):
/// `None` when it is not browsable or cannot be read (Node's try/catch skips the reservation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    S3,
    Dav,
}

/// index.cjs PROJECT_ROOT_FOLDER.
pub fn project_root_folder() -> String {
    let raw = std::env::var("PROJECT_ROOT_FOLDER").unwrap_or_else(|_| "noevia projects".into());
    raw.trim_start_matches('/')
        .trim_end_matches('/')
        .to_string()
}

fn str_arr<I: IntoIterator<Item = String>>(items: I) -> JValue {
    JValue::Arr(items.into_iter().map(JValue::Str).collect())
}

fn throw() -> Reply {
    Reply::internal()
}

/// `String(v)`, a TypeError being Node's 500.
fn js_string(v: &JValue) -> Result<String, Reply> {
    js_json::to_js_string(v).map_err(|_| throw())
}

/// `String(v || '')`.
fn string_or_empty(v: &JValue) -> Result<String, Reply> {
    if v.truthy() {
        js_string(v)
    } else {
        Ok(String::new())
    }
}

fn has_own(body: &JValue, key: &str) -> bool {
    matches!(body, JValue::Obj(items) if items.iter().any(|(k, _)| k == key))
}

/// project-appearance.cjs `projectAppearance(body)`.
fn appearance(body: &JValue, icons: &[String]) -> Result<Vec<(String, JValue)>, Reply> {
    let mut out = Vec::new();
    if has_own(body, "icon") {
        match body.get("icon").as_str() {
            Some(icon) if icons.iter().any(|i| i == icon) => {
                out.push(("icon".to_string(), JValue::Str(icon.to_string())));
            }
            _ => return Err(Reply::error(400, "Choose a supported project icon.")),
        }
    }
    if has_own(body, "color") {
        let ok = |c: &str| {
            c == "default"
                || (c.len() == 7
                    && c.starts_with('#')
                    && c.bytes().skip(1).all(|b| b.is_ascii_hexdigit()))
        };
        match body.get("color").as_str() {
            Some(c) if ok(c) => {
                out.push(("color".to_string(), JValue::Str(c.to_ascii_lowercase())));
            }
            _ => {
                return Err(Reply::error(
                    400,
                    "Project color must be default or a six-digit hex color.",
                ))
            }
        }
    }
    Ok(out)
}

/// project-modes.cjs `sanitize(value)`.
fn modes(value: &JValue) -> Result<Vec<String>, Reply> {
    let JValue::Arr(items) = value else {
        return Err(Reply::error(400, "modes must be an array"));
    };
    if items
        .iter()
        .any(|m| !m.as_str().is_some_and(|s| MODES.contains(&s)))
    {
        return Err(Reply::error(
            400,
            &format!("modes may only contain {}", MODES.join(", ")),
        ));
    }
    let mut out = vec!["chat".to_string()];
    for mode in MODES.iter().filter(|m| **m != "chat") {
        if items.iter().any(|m| m.as_str() == Some(mode)) {
            out.push((*mode).to_string());
        }
    }
    Ok(out)
}

/// JS `/[\x00-\x1f\x7f/\\:*?"<>|]/g` → `-` and `/\s+/g` → ` `.
fn folder_clean(raw: &str) -> String {
    let mut out = String::new();
    let mut in_space = false;
    for c in raw.chars() {
        let c = if (c as u32) < 0x20 || c == '\u{7f}' || "/\\:*?\"<>|".contains(c) {
            '-'
        } else {
            c
        };
        if js::is_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// `.trim().replace(/^\.+|[.\s]+$/g, '')`.
fn folder_trim(s: &str) -> String {
    let t = js::trim(s).trim_start_matches('.');
    t.trim_end_matches(|c: char| c == '.' || js::is_space(c))
        .to_string()
}

/// project-folders.cjs `projectFolderKey(project)` for the project's id.
pub fn folder_key(raw: &str) -> String {
    let whole = folder_trim(&folder_clean(raw));
    let cut = wtf16::slice(&whole, 80);
    let cut = cut.trim_end_matches(|c: char| c == '.' || js::is_space(c));
    let key = if cut.is_empty() {
        "Untitled project".to_string()
    } else {
        cut.to_string()
    };
    if wtf16::len(&whole) <= 80 {
        return key;
    }
    let digest: String = Sha256::digest(raw.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let head = wtf16::slice(&whole, 80 - 12 - 1);
    let head = head.trim_end_matches(|c: char| c == '.' || js::is_space(c));
    format!("{head}-{}", digest.get(..12).unwrap_or(""))
}

/// project-folders.cjs `reserveProjectFolder(connection, root, project, others)`.
fn reserve(storage: Storage, root: &str, id: &str, others: &[JValue]) -> Option<String> {
    let name = folder_key(id);
    if storage == Storage::S3 {
        return Some(format!("{root}/{name}"));
    }
    let taken: Vec<&str> = others
        .iter()
        .flat_map(|o| {
            [o.get("projectFolder"), o.get("reservedFolder")]
                .into_iter()
                .filter(|v| v.truthy())
                .filter_map(JValue::as_str)
        })
        .collect();
    (1..=1000).find_map(|n| {
        let folder = if n == 1 {
            format!("{root}/{name}")
        } else {
            format!("{root}/{name} ({n})")
        };
        (!taken.contains(&folder.as_str())).then_some(folder)
    })
}

/// Everything the create reads from outside the request.
pub struct CreateEnv<'a> {
    pub catalogue: Option<&'a Catalogue>,
    pub storage: Option<Storage>,
    pub root: &'a str,
}

/// The project createProject builds before its save (without the reservation, which is made
/// under the lock), or the request's refusal.
fn build(
    body: &JValue,
    cat: &Catalogue,
    default_manual: bool,
    now_ms: i64,
) -> Result<JValue, Reply> {
    if matches!(body, JValue::Null) {
        return Err(throw()); // body.reasoningEffort: TypeError
    }
    let effort = body.get("reasoningEffort");
    if !matches!(effort, JValue::Undefined)
        && !effort.as_str().is_some_and(|e| EFFORTS.contains(&e))
    {
        return Err(Reply::error(400, "Invalid reasoning effort"));
    }
    let name = string_or_empty(body.get("name"))?;
    let name = wtf16::slice(js::trim(&name), NAME_MAX);
    if name.is_empty() {
        return Err(Reply::error(400, "name required"));
    }
    let project_modes = match body.get("modes") {
        JValue::Undefined => vec!["chat".to_string()],
        v => modes(v)?,
    };
    let look = appearance(body, &cat.icons)?;
    let goal = wtf16::slice(&string_or_empty(body.get("goal"))?, 2000);
    let instructions = wtf16::slice(&string_or_empty(body.get("instructions"))?, 8000);
    let files: Vec<JValue> = match body.get("files") {
        JValue::Arr(items) => items
            .iter()
            .filter(|f| {
                f.truthy()
                    && f.get("name").as_str().is_some()
                    && f.get("content").as_str().is_some()
            })
            .take(20)
            .map(|f| {
                JValue::obj([
                    (
                        "name",
                        JValue::Str(wtf16::slice(f.get("name").as_str().unwrap_or(""), 200)),
                    ),
                    (
                        "content",
                        JValue::Str(wtf16::slice(
                            f.get("content").as_str().unwrap_or(""),
                            200_000,
                        )),
                    ),
                ])
            })
            .collect(),
        _ => Vec::new(),
    };
    let nonempty = |v: &JValue| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    let routing = match body.get("routing").as_str() {
        Some(r @ ("manual" | "auto")) => r.to_string(),
        _ if default_manual => "manual".into(),
        _ => "auto".into(),
    };
    let chosen = cat.sanitize(body.get("toolboxes"));
    let explicit_boxes = matches!(body.get("toolboxes"), JValue::Arr(_));
    let toolboxes = chosen.unwrap_or_else(|| cat.defaults.clone());
    let tools_mode = match body.get("toolsMode").as_str() {
        Some(m @ ("auto" | "manual")) => m.to_string(),
        _ if explicit_boxes => "manual".into(),
        _ => "auto".into(),
    };
    let id = format!(
        "proj-{}-{}",
        u64::try_from(now_ms).unwrap_or(0),
        js::random36(6)
    );
    let now = JValue::Num(now_ms as f64);
    let mut fields: Vec<(String, JValue)> = look;
    let has_files = !files.is_empty();
    fields.extend([
        ("id".to_string(), JValue::Str(id)),
        ("name".into(), JValue::Str(name)),
        ("goal".into(), JValue::Str(goal)),
        ("instructions".into(), JValue::Str(instructions)),
        ("pinned".into(), JValue::Bool(false)),
        ("archived".into(), JValue::Bool(false)),
        ("sourceFolders".into(), JValue::Arr(Vec::new())),
        ("memories".into(), JValue::Arr(Vec::new())),
        ("files".into(), JValue::Arr(files)),
    ]);
    if let Some(m) = nonempty(body.get("model")) {
        fields.push(("model".into(), JValue::Str(m)));
    }
    if let Some(p) = nonempty(body.get("provider")) {
        fields.push(("provider".into(), JValue::Str(p)));
    }
    if let Some(e) = effort.as_str() {
        fields.push(("reasoningEffort".into(), JValue::Str(e.to_string())));
    }
    fields.extend([
        ("routing".to_string(), JValue::Str(routing)),
        ("modes".into(), str_arr(project_modes)),
        ("toolboxes".into(), str_arr(toolboxes.clone())),
        ("toolsMode".into(), JValue::Str(tools_mode)),
        ("chats".into(), JValue::Arr(Vec::new())),
        ("createdAt".into(), now.clone()),
        ("updatedAt".into(), now),
    ]);
    let mut project = JValue::Obj(fields);
    // project-docs-default.cjs applyProjectDocsDefault (hadUploads: false): every create-time file
    // is an upload (none has a `source`).
    if !explicit_boxes
        && has_files
        && cat.selectable.iter().any(|s| s == DOCS_BOX)
        && !toolboxes.is_empty()
    {
        store::set(&mut project, "docsToolboxDefaulted", JValue::Bool(true));
        if !toolboxes.iter().any(|t| t == DOCS_BOX) {
            let mut next = toolboxes;
            next.push(DOCS_BOX.to_string());
            store::set(&mut project, "toolboxes", str_arr(next));
        }
    }
    Ok(project)
}

/// source-text.cjs `headerNamesSkill(text)`: a front-matter block with a `name:` or `description:`
/// line (`/^(?:name|description)\s*:/im`).
fn header_names_skill(text: &str) -> bool {
    let Some(header) = skills::front_matter(text) else {
        return false;
    };
    header
        .split(['\n', '\r', '\u{2028}', '\u{2029}'])
        .any(|line| {
            let lower = line
                .get(..12)
                .map(str::to_ascii_lowercase)
                .unwrap_or_else(|| line.to_ascii_lowercase());
            let rest = if lower.starts_with("name") {
                line.get(4..)
            } else if lower.starts_with("description") {
                line.get(11..)
            } else {
                None
            };
            rest.is_some_and(|r| r.trim_start_matches(js::is_space).starts_with(':'))
        })
}

fn fsync_dir(d: &Path) -> std::io::Result<()> {
    std::fs::File::open(d)?.sync_all()
}

/// source-text.cjs `writeText(wsDir, projectId, ref, text)`: `project-sources/<safe id>/<ref>.txt`
/// (0600 in 0700 folders, temp file fsynced and renamed, the folders up to the user's fsynced); an
/// existing file with that text is kept.
fn write_text(dir: &Path, project_id: &str, reference: &str, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let sources = dir.join("project-sources");
    let folder = sources.join(js::id_chars(project_id));
    let file = folder.join(format!("{reference}.txt"));
    if let Ok(bytes) = std::fs::read(&file) {
        if sha_hex_bytes(&bytes) == reference {
            for d in [&folder, &sources, &dir.to_path_buf()] {
                fsync_dir(d)?;
            }
            return Ok(());
        }
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)?;
    let tmp = folder.join(format!("{reference}.txt.{}.tmp", std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &file)?;
    for d in [&folder, &sources, &dir.to_path_buf()] {
        fsync_dir(d)?;
    }
    Ok(())
}

fn sha_hex_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// source-text.cjs `externalize` for the new project's files: each text is written to its file
/// first, then the file entry becomes `{ name, textRef, chars, skillHeader? }`.
fn externalize(dir: &Path, project: &mut JValue) -> std::io::Result<()> {
    let id = project.get("id").as_str().unwrap_or("").to_string();
    let JValue::Obj(fields) = project else {
        return Ok(());
    };
    let Some((_, JValue::Arr(files))) = fields.iter_mut().find(|(k, _)| k == "files") else {
        return Ok(());
    };
    for f in files.iter_mut() {
        let Some(text) = f.get("content").as_str().map(str::to_string) else {
            continue;
        };
        let reference = sha_hex_bytes(text.as_bytes());
        write_text(dir, &id, &reference, &text)?;
        if let JValue::Obj(items) = f {
            items.retain(|(k, _)| k != "content" && k != "skillHeader");
            items.push(("textRef".into(), JValue::Str(reference)));
            items.push(("chars".into(), JValue::Num(wtf16::len(&text) as f64)));
            if header_names_skill(&text) {
                items.push(("skillHeader".into(), JValue::Bool(true)));
            }
        }
    }
    Ok(())
}

/// `saveProjects(PROJECTS)`'s reconcile of every project, planned before any is changed.
fn reconcile_all(projects: &mut [JValue]) -> bool {
    let mut plans = Vec::with_capacity(projects.len());
    for p in projects.iter() {
        match skills::plan(p) {
            Ok(plan) => plans.push(plan),
            Err(_) => return false,
        }
    }
    for (p, plan) in projects.iter_mut().zip(plans) {
        if let skills::Plan::Set { value, .. } = plan {
            store::set(p, "instructionSkills", value);
        }
    }
    true
}

/// workspace.cjs `preferences.defaultRouting === 'manual'`.
fn default_manual(dir: &Path) -> bool {
    store::read_json(&dir.join("preferences.json"))
        .ok()
        .flatten()
        .is_some_and(|p| p.get("defaultRouting").as_str() == Some("manual"))
}

enum Made {
    Project(JValue),
    Fail,
}

/// POST /api/projects. `body` was read up to [`BODY_LIMIT`]; `over` when it was longer.
pub fn create(ctx: Ctx<'_>, body: &[u8], over: bool, env: &CreateEnv<'_>) -> Reply {
    if over {
        return Reply::error(413, "Request exceeds size limit");
    }
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return chats::store_failure(&e),
    };
    if let Err(reply) = free_chats::workspace_projects(&dir) {
        return reply;
    }
    let Ok(parsed) = wtf16::parse(&String::from_utf8_lossy(body)) else {
        return chats::invalid_json();
    };
    let Some(cat) = env.catalogue else {
        return Reply::error(
            503,
            "noevia-core has not published its toolbox catalogue yet; try again shortly.",
        );
    };
    let mut project = match build(&parsed, cat, default_manual(&dir), ctx.now_ms) {
        Ok(p) => p,
        Err(reply) => return reply,
    };
    let done = store::update(&dir, ctx.switch, |projects| {
        if let Some(storage) = env.storage {
            if let Some(id) = project.get("id").as_str().map(str::to_string) {
                if let Some(folder) = reserve(storage, env.root, &id, projects) {
                    store::set(&mut project, "reservedFolder", JValue::Str(folder));
                }
            }
        }
        // workspace.cjs save: the texts are on disk before the projects.json that points at them.
        if externalize(&dir, &mut project).is_err() {
            return (Made::Fail, false);
        }
        projects.insert(0, project.clone());
        if !reconcile_all(projects) {
            return (Made::Fail, false);
        }
        match projects.first() {
            Some(p) => (Made::Project(p.clone()), true),
            None => (Made::Fail, false),
        }
    });
    let project = match done {
        Ok(Made::Project(p)) => p,
        Ok(Made::Fail) => return Reply::internal(),
        // A create's save failure is not caught in Node: the server's generic answer.
        Err(e) => return chats::store_failure(&e),
    };
    if let (Some(id), JValue::Arr(files)) = (project.get("id").as_str(), project.get("files")) {
        let jobs: Vec<outbox::Job<'_>> = files
            .iter()
            .filter_map(|f| f.get("name").as_str())
            .map(|name| outbox::Job {
                kind: "index",
                project: id,
                file: Some(name),
                payload: None,
            })
            .collect();
        if !jobs.is_empty() {
            if let Err(e) = outbox::enqueue(&dir, &jobs, ctx.now_ms) {
                eprintln!("noevia-server: index outbox write failed for a new project: {e:?}");
            }
        }
    }
    Reply::json(200, wtf16::stringify(&project).unwrap_or_default())
}

fn sha_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// uploads.cjs `directory(workspace, id)`.
pub fn uploads_dir(dir: &Path, id: &str) -> PathBuf {
    dir.join("project-uploads").join(sha_hex(id))
}

/// document-sources.cjs `directory(workspace, id)`.
pub fn documents_dir(dir: &Path, id: &str) -> PathBuf {
    dir.join("project-documents").join(sha_hex(id))
}

/// The RAG index files of `id` (rag.cjs `openIndex`'s `<ragDir>/<id>.db` and SQLite's WAL pair),
/// none for an id that could leave `rag/`.
pub fn rag_files(dir: &Path, id: &str) -> Vec<PathBuf> {
    if id.is_empty() || id == "." || id == ".." || id.contains(['/', '\\', '\u{0}']) {
        return Vec::new();
    }
    ["db", "db-wal", "db-shm"]
        .iter()
        .map(|s| dir.join("rag").join(format!("{id}.{s}")))
        .collect()
}

/// Every file directly in `d` (never through a symlink), then `d` itself if that left it empty.
fn clear_dir(d: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(d) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(d) {
        for entry in entries.flatten() {
            let p = entry.path();
            match std::fs::symlink_metadata(&p) {
                Ok(m) if !m.is_dir() => {
                    if let Err(e) = std::fs::remove_file(&p) {
                        eprintln!(
                            "noevia-server: a deleted project's file stayed: {}",
                            e.kind()
                        );
                    }
                }
                _ => {}
            }
        }
    }
    let _ = std::fs::remove_dir(d); // not recursive: a directory left non-empty stays
}

/// `deleteProject(id, { purgeChats })` on the user's directory `dir`: whether a project was
/// removed. `Err` is the save's failure, with nothing removed.
pub fn remove(ctx: Ctx<'_>, dir: &Path, id: &str, purge_chats: bool) -> Result<bool, Reply> {
    let done = store::update(dir, ctx.switch, |projects| {
        let at = match chats::locate(projects, id) {
            Err(()) => return (Err(chats::save_failed(DELETE_FAILED)), false),
            Ok(None) => return (Ok(None), false),
            Ok(Some(at)) => at,
        };
        let removed = projects.remove(at);
        if !reconcile_all(projects) {
            return (Err(chats::save_failed(DELETE_FAILED)), false);
        }
        (Ok(Some(removed)), true)
    });
    let removed = match done {
        Ok(Ok(Some(p))) => p,
        Ok(Ok(None)) => return Ok(false),
        Ok(Err(reply)) => return Err(reply),
        Err(e) => return Err(chats::save_failure(&e, DELETE_FAILED)),
    };
    // #1914: everything below runs only now that the list without it is on disk.
    clear_dir(&uploads_dir(dir, id));
    clear_dir(&documents_dir(dir, id));
    clear_dir(&store::asset_dir(dir, id));
    let folder = removed
        .get("projectFolder")
        .as_str()
        .filter(|f| !f.is_empty())
        .map(|f| {
            let mut text = String::from("{\"folder\":");
            js_json::quote(f, &mut text);
            text.push('}');
            text
        });
    match outbox::tombstone(dir, id, folder, ctx.now_ms) {
        Ok(_) => {
            for f in rag_files(dir, id) {
                match std::fs::remove_file(&f) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => eprintln!(
                        "noevia-server: a deleted project's index file stayed: {}",
                        e.kind()
                    ),
                }
            }
        }
        // Without the tombstone an index run in core could recreate the files: they stay, and a
        // repeated delete (404) cannot retry, so say so.
        Err(e) => {
            eprintln!("noevia-server: index outbox tombstone failed; the RAG index stays: {e:?}")
        }
    }
    if purge_chats {
        purge(ctx, dir, &removed);
    }
    Ok(true)
}

/// projects.cjs `purgeProjectChats(project)`.
fn purge(ctx: Ctx<'_>, dir: &Path, project: &JValue) {
    let JValue::Arr(list) = project.get("chats") else {
        return;
    };
    for chat in list {
        let Some(chat_id) = chat.get("id").as_str() else {
            continue;
        };
        match chat_delete::delete(
            ctx.data_dir,
            ctx.user_id,
            chat_id,
            Kind::Unlisted,
            Origin::Remove,
            ctx.switch,
        ) {
            Ok(_) => {}
            Err(DeleteError::Listed) => continue,
            Err(e) => {
                eprintln!("noevia-server: could not delete a chat of a deleted project: {e:?}");
                continue;
            }
        }
        remove_attachments(ctx, dir, chat_id);
    }
}

/// projects.cjs `removeChatAttachments(chatId)`: the chat's attachments project, without its chats.
fn remove_attachments(ctx: Ctx<'_>, dir: &Path, chat_id: &str) {
    let Some(id) = crate::context::chat_project_id(chat_id) else {
        return;
    };
    let listed = store::read(dir)
        .map(|ps| store::find(&ps, &id).is_some())
        .unwrap_or(false);
    if listed {
        if let Err(r) = remove(ctx, dir, &id, false) {
            eprintln!(
                "noevia-server: could not remove the attachments of a deleted chat ({})",
                r.status
            );
        }
    }
}

/// DELETE /api/projects/{projectId}; `raw` is the path segment as the URL spelled it.
pub fn delete(ctx: Ctx<'_>, raw: &str) -> Reply {
    let Some(id) = js::decode_uri_component(raw) else {
        return Reply::internal();
    };
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return chats::store_failure(&e),
    };
    if let Err(reply) = free_chats::workspace_projects(&dir) {
        return reply;
    }
    match remove(ctx, &dir, &id, true) {
        Ok(true) => Reply::json(200, "{\"ok\":true}".into()),
        Ok(false) => Reply::error(404, "no such project"),
        Err(reply) => reply,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn cat() -> Catalogue {
        Catalogue {
            selectable: vec!["core".into(), "project-docs".into(), "web-search".into()],
            defaults: vec!["core".into()],
            icons: vec!["folder".into(), "book".into()],
        }
    }

    fn built(body: &str) -> Result<JValue, Reply> {
        build(
            &wtf16::parse(body).unwrap(),
            &cat(),
            false,
            1_700_000_000_000,
        )
    }

    #[test]
    fn validation_runs_in_nodes_order() {
        assert_eq!(
            built(r#"{"reasoningEffort":"max","name":""}"#).unwrap_err(),
            Reply::error(400, "Invalid reasoning effort")
        );
        assert_eq!(
            built(r#"{"name":"   "}"#).unwrap_err(),
            Reply::error(400, "name required")
        );
        assert_eq!(
            built(r#"{"name":"a","modes":"chat","icon":"x"}"#).unwrap_err(),
            Reply::error(400, "modes must be an array")
        );
        assert_eq!(
            built(r#"{"name":"a","modes":["chat","x"]}"#).unwrap_err(),
            Reply::error(400, "modes may only contain chat, cowork, code")
        );
        assert_eq!(
            built(r#"{"name":"a","icon":"x"}"#).unwrap_err(),
            Reply::error(400, "Choose a supported project icon.")
        );
        assert_eq!(
            built(r##"{"name":"a","color":"#12345"}"##).unwrap_err(),
            Reply::error(
                400,
                "Project color must be default or a six-digit hex color."
            )
        );
        assert_eq!(built("null").unwrap_err().status, 500);
        assert_eq!(
            built("[1]").unwrap_err(),
            Reply::error(400, "name required")
        );
    }

    #[test]
    fn the_project_has_nodes_keys_in_nodes_order() {
        let p = built(r##"{"name":"  Notes ","color":"#ABCDEF","icon":"book","files":[{"name":"a.md","content":"x"},{"name":1}],"model":"m","reasoningEffort":"low","modes":["code","chat"]}"##).unwrap();
        let JValue::Obj(items) = &p else { panic!() };
        let keys: Vec<&str> = items.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "icon",
                "color",
                "id",
                "name",
                "goal",
                "instructions",
                "pinned",
                "archived",
                "sourceFolders",
                "memories",
                "files",
                "model",
                "reasoningEffort",
                "routing",
                "modes",
                "toolboxes",
                "toolsMode",
                "chats",
                "createdAt",
                "updatedAt",
                "docsToolboxDefaulted"
            ]
        );
        assert_eq!(p.get("name").as_str(), Some("Notes"));
        assert_eq!(p.get("color").as_str(), Some("#abcdef"));
        let id = p.get("id").as_str().unwrap();
        assert!(
            id.starts_with("proj-1700000000000-") && id.len() == "proj-1700000000000-".len() + 6
        );
        assert_eq!(
            wtf16::stringify(p.get("toolboxes")).unwrap(),
            r#"["core","project-docs"]"#
        );
        assert_eq!(
            wtf16::stringify(p.get("modes")).unwrap(),
            r#"["chat","code"]"#
        );
        assert_eq!(p.get("toolsMode").as_str(), Some("auto"));
    }

    #[test]
    fn chosen_toolboxes_are_filtered_and_make_the_mode_manual() {
        let p = built(r#"{"name":"a","toolboxes":["web-search","gdrive","nope","web-search"],"files":[{"name":"a","content":"b"}]}"#).unwrap();
        assert_eq!(
            wtf16::stringify(p.get("toolboxes")).unwrap(),
            r#"["web-search"]"#
        );
        assert_eq!(p.get("toolsMode").as_str(), Some("manual"));
        assert!(matches!(p.get("docsToolboxDefaulted"), JValue::Undefined));
    }

    #[test]
    fn reservation_skips_the_folders_other_projects_hold() {
        let others = vec![
            wtf16::parse(r#"{"projectFolder":"root/p1"}"#).unwrap(),
            wtf16::parse(r#"{"reservedFolder":"root/p1 (2)"}"#).unwrap(),
        ];
        assert_eq!(
            reserve(Storage::Dav, "root", "p1", &others).as_deref(),
            Some("root/p1 (3)")
        );
        assert_eq!(
            reserve(Storage::S3, "root", "p1", &others).as_deref(),
            Some("root/p1")
        );
        assert_eq!(folder_key(" ..a/b:c.. "), "a-b-c");
        assert_eq!(folder_key(""), "Untitled project");
        let long = "x".repeat(90);
        let k = folder_key(&long);
        assert_eq!(k.len(), 80);
        assert!(k.starts_with(&"x".repeat(67)));
    }

    #[test]
    fn a_skill_header_is_recognised_as_source_text_cjs_does() {
        assert!(header_names_skill("---\nName: x\n---\nbody"));
        assert!(header_names_skill(
            "---\r\ntitle: t\r\ndescription : d\r\n---"
        ));
        assert!(!header_names_skill("---\ntitle: t\n---\n"));
        assert!(!header_names_skill("name: x"));
        assert!(!header_names_skill("---\n names: x\n---"));
    }

    #[test]
    fn rag_files_never_leave_the_rag_directory() {
        let d = Path::new("/u");
        assert_eq!(rag_files(d, "proj-1-a").len(), 3);
        for bad in ["", ".", "..", "a/b", "a\\b", "a\u{0}"] {
            assert!(rag_files(d, bad).is_empty(), "{bad:?}");
        }
    }
}
