//! `DELETE /api/projects/{id}/files` (sbstndalton/noevia#2191 round 3, group `rust-project-files`):
//! core routes/projects.cjs's delete of one source file from the user's storage, answer for answer.
//!
//! In Node's order: the id percent-decoded (a bad escape is Node's URIError, 500); getProject (its
//! self-heal saved, [`crate::originals::project`]); 404 `no such project`; the body read as
//! http.cjs `readJson` reads it (empty is `{}`, not JSON 400 `invalid JSON`, past 1 MiB 413); not an
//! object 400; `String(body.path || '')` empty 400 `a path is required`; `ownsFile` refused 400
//! (only a file directly in a folder the project has attached, or one noevia stored in a type group
//! of its own folder); no browsable storage 400; storage-client.cjs `deleteFile` (a folder refused,
//! conditional on the file's ETag), its failure worded as routes/projects.cjs `replyStorageFailure`
//! words it or as the error's own status and message. Then the file leaves the list (saved under
//! the projects.json lock), and only once that is on disk (#1914) its document pages, upload
//! original and images go (document-sources.cjs and uploads.cjs `prune`: an image a chat still
//! names is retired, not deleted) and its RAG rows are queued for removal in the index outbox (an
//! `unindex` job: rag.cjs `deleteProjectFile` runs in core, which skips it if a file of that name
//! is listed again by then). A failure of that last save is `could not remove the file from the
//! project; nothing was changed`. 200 `{ ok: true, path }`.
//!
//! Not serialized against a core source operation on the same project (core's source lock is
//! in-process): the save is a read-modify-write of the current projects.json under its lock, and
//! core merges its own saves against the stored version (project-store-client.cjs), so neither side
//! loses the other's change; a pending core prune is not waited for.
use crate::js;
use crate::outbox;
use crate::project_storage::{FileError, Remote, GROUPS};
use crate::store;
use crate::{chats, Ctx, Reply};
use js_json::{wtf16, JValue};
use std::collections::BTreeSet;
use std::path::Path;

/// http.cjs readBody's default limit (readJson).
pub const BODY_LIMIT: usize = 1024 * 1024;
const PRUNE_FAILED: &str = "could not remove the file from the project; nothing was changed";
const NOT_OWNED: &str =
    "That file is not in any folder attached to this project, so it cannot be deleted from here.";

/// JS `String(v)` for the values a JSON body holds.
pub(crate) fn js_string(v: &JValue) -> String {
    match v {
        JValue::Str(s) => s.clone(),
        JValue::Num(n) => js_json::number_to_string(*n),
        JValue::Bool(b) => b.to_string(),
        JValue::Null => "null".into(),
        JValue::Arr(items) => items
            .iter()
            .map(|i| {
                if i.is_nullish() {
                    String::new()
                } else {
                    js_string(i)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        JValue::Obj(_) => "[object Object]".into(),
        _ => "undefined".into(),
    }
}

/// `String(x)` of a stored folder value (`${folder}`).
fn folder_text(v: &JValue) -> String {
    match v {
        JValue::Str(s) => s.clone(),
        other => js_string(other),
    }
}

/// projects.cjs `ownsFile(project, target)`.
pub fn owns_file(project: &JValue, target: &str) -> bool {
    if target.is_empty() {
        return false;
    }
    match storage_path::safe_relative_path(target) {
        Ok(p) if p == target => {}
        _ => return false,
    }
    let folder = project.get("projectFolder");
    let own = folder_text(folder);
    if let JValue::Arr(files) = project.get("files") {
        let stored = files.iter().any(|f| {
            f.truthy()
                && f.get("name").as_str() == Some(target)
                && f.get("attachment").truthy()
                && f.get("source") == folder
                && GROUPS.iter().any(|g| {
                    let prefix = format!("{own}/{g}/");
                    target.starts_with(&prefix) && target.len() > prefix.len()
                })
        });
        if stored {
            return true;
        }
    }
    let mut folders: Vec<String> = Vec::new();
    if folder.truthy() {
        folders.push(own);
    }
    if let JValue::Arr(list) = project.get("sourceFolders") {
        folders.extend(list.iter().filter(|f| f.truthy()).map(folder_text));
    }
    folders.iter().any(|folder| {
        let prefix = format!("{folder}/");
        target
            .strip_prefix(&prefix)
            .is_some_and(|rel| !rel.is_empty() && !rel.contains('/') && rel != "." && rel != "..")
    })
}

/// routes/projects.cjs STORAGE_FAILURE, by storage-client.cjs `storageFailure` kind.
fn failure_reply(e: &FileError) -> Reply {
    let (status, code, error) = match e.kind {
        Some("login") => (502, "storageLoginRejected", "Storage login rejected. Check your storage credentials in Settings → Diary & storage."),
        Some("forbidden") => (502, "storageForbidden", "Storage refused access to this folder. Check its sharing and permissions in your storage app, then retry."),
        Some("throttled") => (503, "storageThrottled", "Storage is limiting logins from noevia after too many failed attempts. Wait a few minutes, then retry."),
        Some("timeout") => (504, "storageTimeout", "Storage did not answer in time. Try again shortly."),
        Some("unreachable") => (502, "storageUnreachable", "Storage could not be reached. Check the server address and that it is online, then retry."),
        _ => return Reply::error(e.status, &e.message),
    };
    let mut fields = vec![
        ("error", JValue::Str(error.into())),
        ("code", JValue::Str(code.into())),
    ];
    if e.kind == Some("throttled") {
        // Node's deleteFile errors carry no Retry-After: its default wait.
        fields.push(("retryAfter", JValue::Num(60.0)));
    }
    Reply::json(
        status,
        wtf16::stringify(&JValue::obj(fields)).unwrap_or_default(),
    )
}

/// Regular files and symlinks of `dir` (never followed, never a directory).
fn entries(dir: &Path) -> Vec<(String, std::path::PathBuf)> {
    let Ok(md) = std::fs::symlink_metadata(dir) else {
        return Vec::new();
    };
    if !md.is_dir() {
        return Vec::new();
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()))
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
        .collect()
}

fn remove(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("[documents] cleanup failed: {}", e.kind()),
    }
}

/// document-sources.cjs `prune(workspace, project)`.
fn prune_documents(dir: &Path, project: &JValue) {
    let id = project.get("id").as_str().unwrap_or("");
    let mut keep = BTreeSet::new();
    if let JValue::Arr(files) = project.get("files") {
        for f in files {
            let d = f.get("document");
            if !d.truthy() {
                continue;
            }
            for k in ["version", "availableVersion"] {
                if let Some(v) = d.get(k).as_str().filter(|v| !v.is_empty()) {
                    keep.insert(format!("{v}.json"));
                }
            }
            for k in ["byteHash", "availableByteHash"] {
                if let Some(v) = d.get(k).as_str().filter(|v| !v.is_empty()) {
                    keep.insert(format!("{v}.pdf"));
                }
            }
        }
    }
    // #1726: `<version>.fig<n>.<ext>` of a kept version.
    let kept_figure = |name: &str| {
        let b = name.as_bytes();
        let Some(head) = name.get(..64) else {
            return false;
        };
        if !head
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return false;
        }
        let Some(rest) = name.get(64..).and_then(|r| r.strip_prefix(".fig")) else {
            return false;
        };
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let ext = rest.get(digits..).unwrap_or("");
        (1..=3).contains(&digits)
            && matches!(ext, ".png" | ".jpg" | ".webp")
            && b.len() > 64
            && keep.contains(&format!("{head}.json"))
    };
    for (name, path) in entries(&crate::lifecycle::documents_dir(dir, id)) {
        if !keep.contains(&name) && !kept_figure(&name) {
            remove(&path);
        }
    }
}

/// uploads.cjs `prune(workspace, project)`, on the project in the list (its assets lists changed
/// in place); the files it removes are removed here too.
fn prune_uploads(dir: &Path, project: &mut JValue, now_ms: i64) {
    let id = project.get("id").as_str().unwrap_or("").to_owned();
    let files: Vec<JValue> = match project.get("files") {
        JValue::Arr(a) => a.clone(),
        _ => Vec::new(),
    };
    let keep: BTreeSet<String> = files
        .iter()
        .filter_map(|f| f.get("attachment").get("id").as_str().map(str::to_owned))
        .filter(|s| !s.is_empty())
        .collect();
    for (name, path) in entries(&crate::lifecycle::uploads_dir(dir, &id)) {
        if !keep.contains(&name) {
            remove(&path);
        }
    }
    let live: BTreeSet<String> = files
        .iter()
        .filter_map(|f| f.get("name").as_str().map(str::to_owned))
        .collect();
    let assets: Vec<JValue> = match project.get("assets") {
        JValue::Arr(a) => a.clone(),
        _ => Vec::new(),
    };
    let orphaned = |a: &JValue| {
        a.get("sourceName").truthy()
            && !a
                .get("sourceName")
                .as_str()
                .is_some_and(|n| live.contains(n))
    };
    // retire(project, assets whose source is gone)
    let mut retired: Vec<JValue> = match project.get("retiredAssets") {
        JValue::Arr(a) => a.clone(),
        _ => Vec::new(),
    };
    let leaving: Vec<&JValue> = assets.iter().filter(|a| orphaned(a)).collect();
    if !leaving.is_empty() {
        let known: BTreeSet<String> = retired
            .iter()
            .filter_map(|a| a.get("id").as_str().map(str::to_owned))
            .collect();
        for a in leaving {
            let Some(aid) = a.get("id").as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            if known.contains(aid) {
                continue;
            }
            let mut copy = a.clone();
            store::set(&mut copy, "retiredAt", JValue::Num(now_ms as f64));
            retired.push(copy);
        }
    }
    let current: Vec<JValue> = assets.into_iter().filter(|a| !orphaned(a)).collect();
    let current_ids: BTreeSet<String> = current
        .iter()
        .filter_map(|a| a.get("id").as_str().map(str::to_owned))
        .collect();
    store::set(project, "assets", JValue::Arr(current));
    let asset_dir = store::asset_dir(dir, &id);
    let names: Vec<(String, std::path::PathBuf)> = entries(&asset_dir)
        .into_iter()
        .filter(|(n, _)| !current_ids.contains(n))
        .collect();
    let referenced = referenced_assets(dir, project, &names);
    retired.retain(|a| a.get("id").as_str().is_some_and(|i| referenced.contains(i)));
    if retired.is_empty() {
        if let JValue::Obj(items) = project {
            items.retain(|(k, _)| k != "retiredAssets");
        }
    } else {
        store::set(project, "retiredAssets", JValue::Arr(retired));
    }
    for (name, path) in names {
        if !referenced.contains(&name) {
            remove(&path);
        }
    }
}

/// uploads.cjs `referencedAssets`: the candidate ids named anywhere in the project's transcripts.
fn referenced_assets(
    dir: &Path,
    project: &JValue,
    candidates: &[(String, std::path::PathBuf)],
) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    if candidates.is_empty() {
        return found;
    }
    let JValue::Arr(chats) = project.get("chats") else {
        return found;
    };
    for chat in chats {
        let id = match chat {
            JValue::Obj(_) => chat.get("id").as_str(),
            other => other.as_str(),
        };
        let Some(id) = id.filter(|s| !s.is_empty()) else {
            continue;
        };
        let file = crate::history::history_file(dir, &crate::history::safe_chat_id(id));
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for (c, _) in candidates {
            if text.contains(c.as_str()) {
                found.insert(c.clone());
            }
        }
        if found.len() == candidates.len() {
            break;
        }
    }
    found
}

/// DELETE /api/projects/{id}/files. `raw` is the id segment as the URL spelled it; `body` was read
/// up to [`BODY_LIMIT`] (`over` past it); `remote` opens the user's storage.
pub fn delete(
    ctx: Ctx<'_>,
    raw: &str,
    body: &[u8],
    over: bool,
    remote: &crate::project_storage::OpenRemote<'_>,
) -> Reply {
    let Some(id) = js::decode_uri_component(raw) else {
        return Reply::internal();
    };
    let (dir, project) = match crate::originals::project(ctx, &id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(project) = project else {
        return Reply::error(404, "no such project");
    };
    if over {
        return Reply::error(413, "Request exceeds size limit");
    }
    let text = String::from_utf8_lossy(body);
    let parsed = if text.is_empty() {
        JValue::Obj(Vec::new())
    } else {
        match wtf16::parse(&text) {
            Ok(v) => v,
            Err(_) => return chats::invalid_json(),
        }
    };
    if !matches!(parsed, JValue::Obj(_)) {
        return Reply::error(400, "request body must be a JSON object");
    }
    let path = parsed.get("path");
    let target = if path.truthy() {
        js_string(path)
    } else {
        String::new()
    };
    if target.is_empty() {
        return Reply::error(400, "a path is required");
    }
    if !owns_file(&project, &target) {
        return Reply::error(400, NOT_OWNED);
    }
    let mut answer: Option<Reply> = None;
    remote(&mut |r: &Remote<'_>| {
        answer = Some(match r {
            Remote::None => Reply::error(400, "no browsable storage connected"),
            Remote::S3 => Reply::error(400, "deleting from S3 is not supported"),
            Remote::Refused(status, message) => Reply::error(*status, message),
            Remote::Dav(storage) => match storage.delete_file(&target) {
                Ok(_) => return,
                Err(e) => failure_reply(&e),
            },
        });
    });
    if let Some(reply) = answer {
        return reply;
    }
    // The file leaves the list first (on disk before anything else goes, #1914).
    let first = store::update(&dir, ctx.switch, |projects| {
        let Some(p) = store::find_mut(projects, &id) else {
            return (false, false);
        };
        let kept: Vec<JValue> = match p.get("files") {
            JValue::Arr(files) => files
                .iter()
                .filter(|f| f.get("name").as_str() != Some(target.as_str()))
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        store::set(p, "files", JValue::Arr(kept));
        (true, true)
    });
    let listed = match first {
        Ok(v) => v,
        Err(e) => return chats::store_failure(&e),
    };
    if listed {
        let second = store::update(&dir, ctx.switch, |projects| {
            let Some(p) = store::find_mut(projects, &id) else {
                return (Ok(false), false);
            };
            prune_documents(&dir, p);
            prune_uploads(&dir, p, ctx.now_ms);
            let still = !matches!(p.get("files"), JValue::Arr(files) if files.iter().any(|f| f.get("name").as_str() == Some(target.as_str())));
            (Ok(still), true)
        });
        match second {
            Ok(Ok(true)) => {
                let job = outbox::Job {
                    kind: "unindex",
                    project: &id,
                    file: Some(&target),
                    payload: None,
                };
                if let Err(e) = outbox::enqueue(&dir, &[job], ctx.now_ms) {
                    eprintln!("noevia-server: index outbox write failed for a removed file: {e:?}");
                }
            }
            Ok(Ok(false)) => {}
            Ok(Err(())) => return Reply::error(500, PRUNE_FAILED),
            Err(e) => return chats::save_failure(&e, PRUNE_FAILED),
        }
    }
    Reply::json(
        200,
        wtf16::stringify(&JValue::obj([
            ("ok", JValue::Bool(true)),
            ("path", JValue::Str(target)),
        ]))
        .unwrap_or_default(),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn p(text: &str) -> JValue {
        wtf16::parse(text).unwrap()
    }

    #[test]
    fn owns_file_is_cores_rule() {
        let project = p(
            r#"{"projectFolder":"root/p","sourceFolders":["root/p","shared/docs"],
            "files":[{"name":"root/p/Text/sub/a.md","attachment":{"id":"x"},"source":"root/p"},
                     {"name":"root/p/Text/b.md","source":"root/p"}]}"#,
        );
        assert!(
            owns_file(&project, "root/p/Text/sub/a.md"),
            "a stored file in a type group, nested"
        );
        assert!(
            !owns_file(&project, "root/p/Text/b.md"),
            "no attachment: only direct children count"
        );
        assert!(owns_file(&project, "root/p/c.md"));
        assert!(owns_file(&project, "shared/docs/n.txt"));
        for bad in [
            "",
            "shared/docs",
            "shared/docs/",
            "shared/docs/x/y.md",
            "shared/docs/../x",
            "/root/p/c.md",
            "root/p/./c.md",
            "other/c.md",
            "root/p//c.md",
        ] {
            assert!(!owns_file(&project, bad), "{bad}");
        }
        // A direct child that is a folder is owned here (as in core); storage's check refuses it.
        assert!(owns_file(&project, "root/p/Text"));
        // Without a folder of its own, only the attached ones.
        let bare = p(r#"{"sourceFolders":[7,"s"],"files":[]}"#);
        assert!(owns_file(&bare, "7/a.md"));
        assert!(owns_file(&bare, "s/a.md"));
        assert!(!owns_file(&bare, "undefined/Text/a.md"));
    }

    #[test]
    fn js_string_matches_string_of_a_json_value() {
        assert_eq!(js_string(&p("5")), "5");
        assert_eq!(js_string(&p("1.5e300")), "1.5e+300");
        assert_eq!(js_string(&p("true")), "true");
        assert_eq!(js_string(&p("[1,null,\"a\"]")), "1,,a");
        assert_eq!(js_string(&p("{}")), "[object Object]");
    }

    #[test]
    fn prune_keeps_what_the_list_still_names_and_retires_referenced_images() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let docs = crate::lifecycle::documents_dir(dir, "p");
        let ups = crate::lifecycle::uploads_dir(dir, "p");
        let assets = store::asset_dir(dir, "p");
        for d in [&docs, &ups, &assets] {
            std::fs::create_dir_all(d).unwrap();
        }
        let v = "a".repeat(64);
        let gone = "b".repeat(64);
        for n in [
            format!("{v}.json"),
            format!("{v}.fig1.png"),
            format!("{gone}.json"),
            format!("{gone}.fig2.png"),
            "x.pdf".into(),
        ] {
            std::fs::write(docs.join(n), "d").unwrap();
        }
        std::fs::create_dir(docs.join("nested")).unwrap();
        for n in ["keep-id", "gone-id"] {
            std::fs::write(ups.join(n), "u").unwrap();
        }
        for n in ["img-live", "img-said", "img-unsaid"] {
            std::fs::write(assets.join(n), "i").unwrap();
        }
        std::fs::write(
            dir.join("history-c1.json"),
            r#"{"history":[{"content":"see img-said"}]}"#,
        )
        .unwrap();
        let mut project = p(&format!(
            r#"{{"id":"p","chats":[{{"id":"c1"}}],"files":[{{"name":"k.pdf","attachment":{{"id":"keep-id"}},"document":{{"version":"{v}"}}}}],
            "assets":[{{"id":"img-live","sourceName":"k.pdf"}},{{"id":"img-said","sourceName":"gone.pdf"}},{{"id":"img-unsaid","sourceName":"gone.pdf"}},{{"id":"img-paste"}}]}}"#
        ));
        prune_documents(dir, &project);
        prune_uploads(dir, &mut project, 42);
        let left = |d: &Path| {
            let mut n: Vec<String> = std::fs::read_dir(d)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            n.sort();
            n
        };
        assert_eq!(
            left(&docs),
            vec![
                format!("{v}.fig1.png"),
                format!("{v}.json"),
                "nested".to_string()
            ]
        );
        assert_eq!(left(&ups), vec!["keep-id"]);
        assert_eq!(left(&assets), vec!["img-live", "img-said"]);
        let ids = |k: &str| match project.get(k) {
            JValue::Arr(a) => a
                .iter()
                .map(|x| x.get("id").as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            _ => vec![],
        };
        assert_eq!(ids("assets"), vec!["img-live", "img-paste"]);
        assert_eq!(ids("retiredAssets"), vec!["img-said"]);
        assert!(wtf16::stringify(&project)
            .unwrap()
            .contains(r#""retiredAt":42"#));
        // Nothing retired and nothing referenced: the key goes.
        let mut quiet = p(r#"{"id":"q","files":[],"retiredAssets":[{"id":"old"}]}"#);
        prune_uploads(dir, &mut quiet, 1);
        assert!(matches!(quiet.get("retiredAssets"), JValue::Undefined));
    }
}
