//! What a source sync and an organized upload share (sbstndalton/noevia#2191 round 8): the words and
//! status core's routes give each way storage can fail (routes/projects.cjs `STORAGE_FAILURE`), the
//! "a source named X already exists" 409, project-docs-default.cjs, and source-text.cjs's text helpers.
use crate::lifecycle::Catalogue;
use crate::source_storage::{Failure, StorageError};
use crate::{sources, Body, Reply};
use js_json::{wtf16, JValue};
use std::path::Path;

pub const STORAGE_LOGIN_REJECTED_ERROR: &str =
    "Storage login rejected. Check your storage credentials in Settings → Diary & storage.";

/// `STORAGE_FAILURE[kind]`: (status, code, error).
pub fn words(kind: &str) -> Option<(u16, &'static str, &'static str)> {
    Some(match kind {
        "login" => (502, "storageLoginRejected", STORAGE_LOGIN_REJECTED_ERROR),
        "forbidden" => (502, "storageForbidden", "Storage refused access to this folder. Check its sharing and permissions in your storage app, then retry."),
        "throttled" => (503, "storageThrottled", "Storage is limiting logins from noevia after too many failed attempts. Wait a few minutes, then retry."),
        "timeout" => (504, "storageTimeout", "Storage did not answer in time. Try again shortly."),
        "unreachable" => (502, "storageUnreachable", "Storage could not be reached. Check the server address and that it is online, then retry."),
        _ => return None,
    })
}

/// `storageReason(err, fallback)`: a storage error's words for a skipped source.
pub fn reason(err: &StorageError, fallback: &str) -> String {
    match err.failure().and_then(|f| words(f.kind)) {
        Some((_, _, error)) => error.to_owned(),
        None if err.message.is_empty() => fallback.to_owned(),
        None => err.message.clone(),
    }
}

/// `storageCode(err)`.
pub fn code(err: &StorageError) -> Option<&'static str> {
    err.failure().and_then(|f| words(f.kind)).map(|w| w.1)
}

fn json_reply(status: u16, body: &JValue, headers: Vec<(&'static str, String)>) -> Reply {
    let text = wtf16::stringify(body).unwrap_or_default();
    if headers.is_empty() {
        return Reply::json(status, text);
    }
    let mut all = vec![
        ("Content-Type", "application/json".to_owned()),
        ("Cache-Control", "no-store".to_owned()),
    ];
    all.extend(headers);
    Reply {
        status,
        body: Body::Bytes {
            headers: all,
            bytes: text.into_bytes(),
        },
    }
}

/// `replyStorageFailure(res, failure)`: a 502/503/504 with its code, a throttle with its wait; `None`
/// when `failure` has no words.
pub fn failure_reply(failure: Option<Failure>) -> Option<Reply> {
    let failure = failure?;
    let (status, code, error) = words(failure.kind)?;
    let wait = (failure.kind == "throttled").then(|| failure.retry_after.unwrap_or(60));
    let mut body = vec![
        ("error".to_owned(), JValue::Str(error.to_owned())),
        ("code".to_owned(), JValue::Str(code.to_owned())),
    ];
    if let Some(w) = wait {
        body.push(("retryAfter".to_owned(), JValue::Num(w as f64)));
    }
    Some(json_reply(
        status,
        &JValue::Obj(body),
        wait.map(|w| vec![("Retry-After", w.to_string())])
            .unwrap_or_default(),
    ))
}

/// `holdReply(res, hold)`: the answer of an automatic call a latch holds.
pub fn hold_reply(hold: crate::storage_latch::Hold) -> Reply {
    if hold.kind == "login" {
        return failure_reply(Some(Failure {
            kind: "login",
            retry_after: None,
        }))
        .unwrap_or_else(Reply::internal);
    }
    failure_reply(Some(Failure {
        kind: "throttled",
        retry_after: hold.retry_after,
    }))
    .unwrap_or_else(Reply::internal)
}

/// A plain `{ error, code? }` reply.
pub fn error_reply(status: u16, error: &str, code: Option<&str>) -> Reply {
    let mut body = vec![("error".to_owned(), JValue::Str(error.to_owned()))];
    if let Some(c) = code {
        body.push(("code".to_owned(), JValue::Str(c.to_owned())));
    }
    json_reply(status, &JValue::Obj(body), Vec::new())
}

/// `sourceExistsReply(res, name)`: #1454, an upload never replaces an existing source unless asked.
pub fn source_exists(name: &str) -> Reply {
    let body = JValue::obj([
        ("code", JValue::Str("sourceExists".into())),
        ("name", JValue::Str(name.to_owned())),
        (
            "error",
            JValue::Str(format!(
                "A source named \"{name}\" already exists in this project, so it was not replaced. Upload it under another name, or choose to replace it."
            )),
        ),
    ]);
    json_reply(409, &body, Vec::new())
}

/// The route's JSON answer.
pub fn ok(status: u16, body: &JValue) -> Reply {
    json_reply(status, body, Vec::new())
}

// ── project-docs-default.cjs ─────────────────────────────────────────────

const BOX: &str = "project-docs";

/// `hasUploads(project)`.
pub fn has_uploads(project: &JValue) -> bool {
    let folder = project
        .get("projectFolder")
        .as_str()
        .filter(|f| !f.is_empty());
    match project.get("files") {
        JValue::Arr(files) => files.iter().any(|f| {
            f.is_object()
                && (!f.get("source").truthy()
                    || folder.is_some_and(|folder| f.get("source").as_str() == Some(folder)))
        }),
        _ => false,
    }
}

/// `internalProject(project)`: the Diary's extras project and a free chat's attachments project.
pub fn internal_project(project: &JValue) -> bool {
    let id = project.get("id").as_str().unwrap_or("");
    id == "cowork-diary-extras" || id.starts_with("cowork-chat-context-")
}

/// The route's `defaultProjectDocs(project, hadUploads)`: the Project documents box is turned on once,
/// when uploads first appear in a project. Mutates the project; true when it changed.
pub fn apply_project_docs_default(
    project: &mut JValue,
    catalogue: Option<&Catalogue>,
    had_uploads: bool,
) -> bool {
    if had_uploads || internal_project(project) {
        return false;
    }
    if project.get("docsToolboxDefaulted").as_bool() == Some(true) || !has_uploads(project) {
        return false;
    }
    let Some(catalogue) = catalogue else {
        return false;
    };
    if !catalogue.selectable.iter().any(|s| s == BOX) {
        return false;
    }
    let current: Vec<String> = match project.get("toolboxes") {
        JValue::Arr(a) => a
            .iter()
            .filter_map(|t| t.as_str().map(str::to_owned))
            .collect(),
        _ => catalogue.defaults.clone(),
    };
    if current.is_empty() {
        return false;
    }
    crate::store::set(project, "docsToolboxDefaulted", JValue::Bool(true));
    if !current.iter().any(|t| t == BOX) {
        let mut next = current;
        next.push(BOX.to_owned());
        crate::store::set(
            project,
            "toolboxes",
            JValue::Arr(next.into_iter().map(JValue::Str).collect()),
        );
    }
    true
}

// ── source-text.cjs ──────────────────────────────────────────────────────

/// A source's text: inline, or read from its text file.
pub fn text_of(dir: &Path, project: &str, file: &JValue) -> String {
    match file.get("content").as_str() {
        Some(t) => t.to_owned(),
        None => sources::text(dir, project, file).unwrap_or_default(),
    }
}

/// The source with its text inline (an external pointer dropped; a save externalizes it again).
pub fn with_text(dir: &Path, project: &str, file: &JValue) -> JValue {
    let mut f = file.clone();
    if f.get("content").as_str().is_none() {
        let text = text_of(dir, project, file);
        if let JValue::Obj(items) = &mut f {
            items.retain(|(k, _)| k != "textRef" && k != "chars" && k != "skillHeader");
            wtf16::set(items, "content", JValue::Str(text));
        }
    }
    f
}

/// `textLength(f)`: `chars` for an external text, else the length of the content in UTF-16 units.
pub fn text_length(file: &JValue) -> usize {
    let external = file.get("textRef").as_str().is_some_and(|r| !r.is_empty());
    if external {
        if let JValue::Num(n) = file.get("chars") {
            if n.fract() == 0.0 && *n >= 0.0 && *n <= 9_007_199_254_740_991.0 {
                return *n as usize;
            }
        }
    }
    wtf16::len(file.get("content").as_str().unwrap_or(""))
}

/// `sameText(a, b)`: two external texts are the same when they point at the same file.
pub fn same_text(dir: &Path, project: &str, a: &JValue, b: &JValue) -> bool {
    let ext = |f: &JValue| {
        f.get("textRef")
            .as_str()
            .filter(|r| !r.is_empty())
            .map(str::to_owned)
    };
    match (ext(a), ext(b)) {
        (Some(x), Some(y)) => x == y,
        _ => text_of(dir, project, a) == text_of(dir, project, b),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    fn project(files: Vec<JValue>) -> JValue {
        JValue::obj([
            ("id", JValue::Str("proj-a".into())),
            ("projectFolder", JValue::Str("root/proj-a".into())),
            ("files", JValue::Arr(files)),
        ])
    }
    fn file(name: &str, source: Option<&str>) -> JValue {
        let mut items = vec![("name".to_owned(), JValue::Str(name.into()))];
        if let Some(s) = source {
            items.push(("source".into(), JValue::Str(s.into())));
        }
        JValue::Obj(items)
    }
    fn catalogue() -> Catalogue {
        Catalogue {
            selectable: vec!["core".into(), "project-docs".into()],
            defaults: vec!["core".into()],
            icons: vec![],
        }
    }

    #[test]
    fn the_docs_box_is_added_once_when_uploads_first_appear() {
        let mut p = project(vec![file("a.md", None)]);
        assert!(apply_project_docs_default(
            &mut p,
            Some(&catalogue()),
            false
        ));
        assert_eq!(p.get("docsToolboxDefaulted").as_bool(), Some(true));
        let JValue::Arr(boxes) = p.get("toolboxes").clone() else {
            panic!()
        };
        assert_eq!(boxes.len(), 2);
        // Not again, not for a project that already had uploads, not for internal projects.
        assert!(!apply_project_docs_default(
            &mut p,
            Some(&catalogue()),
            false
        ));
        let mut q = project(vec![file("a.md", None)]);
        assert!(!apply_project_docs_default(
            &mut q,
            Some(&catalogue()),
            true
        ));
        let mut internal = project(vec![file("a.md", None)]);
        store_id(&mut internal, "cowork-chat-context-1");
        assert!(!apply_project_docs_default(
            &mut internal,
            Some(&catalogue()),
            false
        ));
        // Files read from an attached folder are not uploads; its own folder's are.
        let mut attached = project(vec![file("x.md", Some("elsewhere"))]);
        assert!(!apply_project_docs_default(
            &mut attached,
            Some(&catalogue()),
            false
        ));
        let mut own = project(vec![file("x.md", Some("root/proj-a"))]);
        assert!(apply_project_docs_default(
            &mut own,
            Some(&catalogue()),
            false
        ));
        // "No tools" was chosen on purpose; and without the catalogue nothing is offered.
        let mut none = project(vec![file("a.md", None)]);
        store_set(&mut none, "toolboxes", JValue::Arr(vec![]));
        assert!(!apply_project_docs_default(
            &mut none,
            Some(&catalogue()),
            false
        ));
        let mut unoffered = project(vec![file("a.md", None)]);
        assert!(!apply_project_docs_default(&mut unoffered, None, false));
    }
    fn store_id(p: &mut JValue, id: &str) {
        crate::store::set(p, "id", JValue::Str(id.into()));
    }
    fn store_set(p: &mut JValue, k: &str, v: JValue) {
        crate::store::set(p, k, v);
    }

    #[test]
    fn storage_failures_are_worded_and_coded_as_core_words_them() {
        let e = StorageError::new(502, "storage returned 401");
        assert_eq!(code(&e), Some("storageLoginRejected"));
        assert_eq!(reason(&e, "x"), STORAGE_LOGIN_REJECTED_ERROR);
        let plain = StorageError::new(404, "storage returned 404");
        assert_eq!(code(&plain), None);
        assert_eq!(reason(&plain, "x"), "storage returned 404");
        let r = failure_reply(Some(Failure {
            kind: "throttled",
            retry_after: Some(30),
        }))
        .unwrap();
        assert_eq!(r.status, 503);
        let Body::Bytes { headers, bytes } = &r.body else {
            panic!()
        };
        assert!(headers.contains(&("Retry-After", "30".to_owned())));
        assert!(String::from_utf8_lossy(bytes).contains(r#""retryAfter":30"#));
        assert!(failure_reply(Some(Failure {
            kind: "other",
            retry_after: None
        }))
        .is_none());
    }

    #[test]
    fn text_length_follows_the_pointer_and_same_text_the_ref() {
        let ext = JValue::obj([
            ("name", JValue::Str("a".into())),
            ("textRef", JValue::Str("r1".into())),
            ("chars", JValue::Num(12.0)),
        ]);
        assert_eq!(text_length(&ext), 12);
        let inline = JValue::obj([("content", JValue::Str("a\u{1f600}".into()))]);
        assert_eq!(text_length(&inline), 3, "UTF-16 units");
        let dir = tempfile::tempdir().unwrap();
        let other = JValue::obj([("textRef", JValue::Str("r1".into()))]);
        assert!(same_text(dir.path(), "p", &ext, &other));
        assert!(!same_text(
            dir.path(),
            "p",
            &JValue::obj([("content", JValue::Str("a".into()))]),
            &JValue::obj([("content", JValue::Str("b".into()))])
        ));
    }
}
