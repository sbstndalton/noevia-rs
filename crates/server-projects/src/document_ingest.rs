//! core document-sources.cjs `ingest`: a document's original, its result record and the source
//! record that points at them, in core's files (sbstndalton/noevia#2191 round 5).
//!
//! - `project-documents/<sha256(projectId)>/<byteHash>.pdf`: the original (the suffix is always
//!   `.pdf`, as core names it), written once.
//! - `.../<version>.json` with `version = sha256(byteHash + ':' + EXTRACTOR_VERSION)`: the result
//!   record ([`crate::extract`]), written atomically (temp file fsynced, renamed, folder fsynced, 0600
//!   in 0700 folders), and `<version>.fig<index>.<ext>` beside it for each figure.
//! - The source record `{ name, content, document: { version, byteHash, bytes, extractor, state,
//!   stale, availableVersion, availableByteHash, pages, truncated, pageStatus, stoppedAt?, error,
//!   errorId?, indexing } }`, field for field.
//!
//! A stored retryable record is never reused: it is read again (core's rule). A stored record
//! without the completion marker gets the completion pass once more; any other stored record is
//! used as it is: an already stored document is never extracted again.
use crate::extract::{self, Engine, FigureBytes, Limits};
use js_json::{wtf16, JValue};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// The result of one ingest.
pub struct Ingested {
    /// The source record.
    pub file: JValue,
    /// The stored result is a transient failure: the document is to be read again.
    pub retryable: bool,
    /// The stored result is readable but its completion pass stopped for a passing reason.
    pub open: bool,
    /// What this read came to, for a sync's log line: `extracted` (a new extraction), `reopened` (a
    /// stored result completed) or `unchanged` (the stored result reused as it was).
    pub outcome: &'static str,
}

fn valid_version(v: &str) -> bool {
    v.len() == 64
        && v.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// document-sources.cjs `versionPath(workspace, projectId, version, ext)`.
pub fn version_path(dir: &Path, project: &str, version: &str, ext: &str) -> Option<PathBuf> {
    valid_version(version)
        .then(|| crate::lifecycle::documents_dir(dir, project).join(format!("{version}{ext}")))
}

/// document-sources.cjs `writeOnce`: temp file fsynced, renamed over, folder fsynced. The folders are made
/// below `user_dir` one at a time and never `users/<id>` itself: an account removed while the document was
/// being extracted stays removed (`NotFound`, which the routes answer as 410; #1425).
pub fn write_once(user_dir: &Path, file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = file
        .parent()
        .ok_or_else(|| std::io::Error::other("no folder"))?;
    crate::store::create_dirs_below(user_dir, parent)?;
    let name = file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| std::io::Error::other("no name"))?;
    let temp = parent.join(format!("{name}.{}.tmp", crate::js::random36(16)));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&temp, file) {
        let _ = std::fs::remove_file(&temp);
        return Err(e);
    }
    if let Ok(d) = std::fs::File::open(parent) {
        let _ = d.sync_all();
    }
    Ok(())
}

fn read_record(file: &Path) -> Option<JValue> {
    let md = std::fs::symlink_metadata(file).ok()?;
    if !md.is_file() || md.len() > 256 * 1024 * 1024 {
        return None;
    }
    let text = std::fs::read_to_string(file).ok()?;
    wtf16::parse(&text).ok()
}

fn set(v: &mut JValue, key: &str, value: JValue) {
    if let JValue::Obj(items) = v {
        wtf16::set(items, key, value);
    }
}

/// storeFigures: each figure with bytes becomes `{ page, index, file, hash, caption?, text? }`.
fn store_figures(
    dir: &Path,
    project: &str,
    version: &str,
    out: &mut JValue,
    bytes: &FigureBytes,
) -> std::io::Result<()> {
    let JValue::Arr(list) = out.get("figures").clone() else {
        return Ok(());
    };
    let mut stored = Vec::with_capacity(list.len());
    for f in list {
        let index = f.get("index").as_num().unwrap_or(-1.0);
        let Some(b) = bytes
            .get(&(index as i64))
            .filter(|_| f.get("file").is_nullish())
        else {
            stored.push(f);
            continue;
        };
        let ext = f.get("ext").as_str().unwrap_or("png").to_string();
        let name = format!("{version}.fig{}.{ext}", js_json::number_to_string(index));
        if let Some(path) = version_path(
            dir,
            project,
            version,
            &format!(".fig{}.{ext}", js_json::number_to_string(index)),
        ) {
            if !path.exists() {
                write_once(dir, &path, b)?;
            }
        }
        let mut fig = JValue::obj([
            ("page", f.get("page").clone()),
            ("index", f.get("index").clone()),
            ("file", JValue::Str(name)),
            ("hash", JValue::Str(extract::sha_hex(b))),
        ]);
        for k in ["caption", "text"] {
            if f.get(k).truthy() {
                set(&mut fig, k, f.get(k).clone());
            }
        }
        stored.push(fig);
    }
    set(out, "figures", JValue::Arr(stored));
    Ok(())
}

/// document-sources.cjs `retryRecord(r)`.
fn retry_record(r: &JValue) -> JValue {
    let outcome = r
        .get("outcome")
        .as_str()
        .filter(|o| ["recovered", "empty", "failed", "unavailable", "skipped"].contains(o))
        .unwrap_or("failed");
    let mut out = JValue::obj([("outcome", JValue::Str(outcome.into()))]);
    if outcome == "failed" && r.get("error").truthy() {
        set(
            &mut out,
            "error",
            JValue::Str(wtf16::slice(&crate::files::js_string(r.get("error")), 200)),
        );
    }
    out
}

/// document-sources.cjs `stoppedRecord(s)`.
fn stopped_record(s: &JValue) -> JValue {
    let limit = s
        .get("limit")
        .as_str()
        .filter(|l| ["pages", "text", "error"].contains(l))
        .unwrap_or("error");
    let page = s
        .get("page")
        .as_num()
        .filter(|n| !n.is_nan())
        .unwrap_or(0.0);
    let mut out = JValue::obj([
        ("page", JValue::Num(page.max(0.0))),
        ("limit", JValue::Str(limit.into())),
    ]);
    if limit == "error" && s.get("error").truthy() {
        set(
            &mut out,
            "error",
            JValue::Str(wtf16::slice(&crate::files::js_string(s.get("error")), 200)),
        );
    }
    out
}

/// document-sources.cjs `ingest(workspace, projectId, name, bytes, previous)`.
#[allow(clippy::too_many_arguments)]
pub fn ingest(
    dir: &Path,
    project: &str,
    name: &str,
    bytes: &[u8],
    previous: Option<&JValue>,
    engine: &dyn Engine,
    limits: &Limits,
) -> std::io::Result<Ingested> {
    let byte_hash = extract::sha_hex(bytes);
    let extractor = extract::extractor_version(extract::figures_on(engine));
    let version = extract::sha_hex(format!("{byte_hash}:{extractor}").as_bytes());
    let bad = || std::io::Error::other("invalid document version");
    let original = version_path(dir, project, &byte_hash, ".pdf").ok_or_else(bad)?;
    if !original.exists() {
        write_once(dir, &original, bytes)?;
    }
    let result_file = version_path(dir, project, &version, ".json").ok_or_else(bad)?;
    let cached = read_record(&result_file).filter(|c| !c.get("retryable").truthy());
    let mut figures = FigureBytes::new();
    let mut outcome = "unchanged";
    let out = match cached {
        Some(c) if c.get("completion").as_str() != Some(extract::COMPLETION) => {
            let completed =
                extract::complete_document(name, bytes, c.clone(), engine, limits, &mut figures);
            if completed != c {
                let mut completed = completed;
                store_figures(dir, project, &version, &mut completed, &figures)?;
                write_once(
                    dir,
                    &result_file,
                    wtf16::stringify(&completed).unwrap_or_default().as_bytes(),
                )?;
                outcome = "reopened";
                completed
            } else {
                c
            }
        }
        Some(c) => c,
        None => {
            outcome = "extracted";
            let mut out =
                match extract::extract_document_text(name, bytes, engine, limits, &mut figures) {
                    Ok(o) => o,
                    Err(extract::Permanent(m)) => extract::thrown(&m, false),
                };
            store_figures(dir, project, &version, &mut out, &figures)?;
            write_once(
                dir,
                &result_file,
                wtf16::stringify(&out).unwrap_or_default().as_bytes(),
            )?;
            out
        }
    };
    let state = out.get("state").as_str().unwrap_or("failed").to_string();
    let prev_content = previous
        .map(|p| p.get("content").as_str().unwrap_or("").to_string())
        .unwrap_or_default();
    let prev_doc = previous
        .map(|p| p.get("document").clone())
        .unwrap_or(JValue::Undefined);
    let stale = state == "failed" && !prev_content.is_empty();
    let available_version = if stale {
        prev_doc.get("availableVersion").clone()
    } else if state != "failed" {
        JValue::Str(version.clone())
    } else {
        JValue::Undefined
    };
    let available_hash = if stale {
        prev_doc.get("availableByteHash").clone()
    } else {
        JValue::Str(byte_hash.clone())
    };
    let page_status: Vec<JValue> = match out.get("pageTexts") {
        JValue::Arr(pages) => pages
            .iter()
            .map(|p| {
                let mut e = JValue::obj([
                    ("number", p.get("number").clone()),
                    ("status", p.get("status").clone()),
                ]);
                if p.get("reason").truthy() {
                    set(&mut e, "reason", p.get("reason").clone());
                }
                if p.get("retry").truthy() {
                    set(&mut e, "retry", retry_record(p.get("retry")));
                }
                e
            })
            .collect(),
        _ => Vec::new(),
    };
    // `stale || previous?.document?.availableVersion === availableVersion` (both undefined counts).
    let indexing = if stale || prev_doc.get("availableVersion") == &available_version {
        let i = prev_doc.get("indexing");
        if i.truthy() {
            i.clone()
        } else {
            JValue::Str("unavailable".into())
        }
    } else {
        JValue::Str("pending".into())
    };
    let mut document = JValue::obj([
        ("version", JValue::Str(version.clone())),
        ("byteHash", JValue::Str(byte_hash)),
        ("bytes", JValue::Num(bytes.len() as f64)),
        ("extractor", JValue::Str(extractor)),
        ("state", JValue::Str(state)),
        ("stale", JValue::Bool(stale)),
        ("availableVersion", available_version),
        ("availableByteHash", available_hash),
        ("pages", out.get("pages").clone()),
        ("truncated", out.get("truncated").clone()),
        ("pageStatus", JValue::Arr(page_status)),
    ]);
    if out.get("stoppedAt").truthy() {
        set(
            &mut document,
            "stoppedAt",
            stopped_record(out.get("stoppedAt")),
        );
    }
    set(&mut document, "error", out.get("error").clone());
    if out.get("errorId").truthy() {
        set(&mut document, "errorId", out.get("errorId").clone());
    }
    set(&mut document, "indexing", indexing);
    let content = if stale {
        prev_content
    } else {
        out.get("text").as_str().unwrap_or("").to_string()
    };
    let file = JValue::obj([
        ("name", JValue::Str(name.to_string())),
        ("content", JValue::Str(content)),
        ("document", document),
    ]);
    Ok(Ingested {
        file,
        retryable: out.get("retryable").truthy(),
        open: !out.get("retryable").truthy()
            && out.get("completion").as_str() != Some(extract::COMPLETION)
            && out.get("pageTexts").truthy()
            && matches!(out.get("pageTexts"), JValue::Arr(a) if !a.is_empty())
            && extract::is_pdf(name),
        outcome,
    })
}

/// projects.cjs `indexSource`'s synchronous half for a document: `document.indexing` is
/// `unavailable` for a source with no text or an unreadable one, else `pending` (the index job
/// does the rest in core).
pub fn mark_indexing(file: &mut JValue) {
    let content = file.get("content").as_str().unwrap_or("").to_string();
    let failed_blank = file.get("document").get("state").as_str() == Some("failed")
        && crate::js::trim(&content).is_empty();
    let state = if content.is_empty() || failed_blank {
        "unavailable"
    } else {
        "pending"
    };
    if let JValue::Obj(items) = file {
        if let Some((_, doc)) = items.iter_mut().find(|(k, _)| k == "document") {
            set(doc, "indexing", JValue::Str(state.into()));
        }
    }
}

/// withSourceLock's prune once the operation ends, against the list as it is on disk (#1914).
pub fn prune_now(dir: &std::path::Path, id: &str) {
    if let Ok(projects) = crate::store::read(dir) {
        if let Some(p) = crate::store::find(&projects, id) {
            crate::files::prune_documents(dir, p);
        }
    }
}
