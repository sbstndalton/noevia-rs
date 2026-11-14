//! Staged document refresh/completion. Engine calls run without project/auth
//! locks; publication compares the source and stored record again under the
//! existing account/project writer fences. Interrupted staging is never progress.
// serde_json's read-only string-key indexing returns Null. All mutable document
// parents are checked as objects by the candidate/version guards before mutation.
#![allow(clippy::indexing_slicing)]

use crate::{
    media_store::{digest, Snapshot},
    sqlite::SqlStore,
    Control,
};
use js_json::{wtf16, JValue};
use serde_json::{json, Value};
use server_projects::{
    document_ingest,
    extract::{self, Engine, Limits},
    store,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
type RecordInputs = BTreeMap<String, Option<Vec<u8>>>;
fn fault() -> String {
    "document background work unavailable".into()
}
fn js(value: &Value) -> Result<JValue, String> {
    wtf16::parse(&server_projects::source_jobs::stringify_js(value).map_err(|_| fault())?)
        .map_err(|_| fault())
}
fn value(value: &JValue) -> Result<Value, String> {
    server_projects::source_jobs::parse_js(wtf16::stringify(value).ok_or_else(fault)?.as_bytes())
        .map_err(|_| fault())
}
fn record(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    server_projects::source_jobs::parse_js(&bytes).ok()
}
struct Staged(PathBuf);
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[derive(Debug, Default)]
pub struct Outcome {
    pub status: String,
    pub unreachable: bool,
    pub timeout: bool,
}
fn outcome(status: &str) -> Outcome {
    Outcome {
        status: status.into(),
        ..Outcome::default()
    }
}
pub fn service_down(error: &str) -> bool {
    let s = error.to_ascii_lowercase();
    ![
        "timed out",
        "time out",
        "timedout",
        "timeout",
        "aborted",
        "aborterror",
    ]
    .iter()
    .any(|w| s.contains(w))
        && [
            "busy",
            "unavailable",
            "not configured",
            "fetch failed",
            "econnrefused",
            "econnreset",
            "enotfound",
            "eai_again",
            "ehostunreach",
        ]
        .iter()
        .any(|w| s.contains(w))
}
fn finished(record: &Value) -> bool {
    record["completion"] == extract::COMPLETION
        || record["pageTexts"].as_array().is_none_or(Vec::is_empty)
}
fn rank(v: &Value) -> usize {
    match v["state"].as_str() {
        Some("ready") => 2,
        Some("partial") => 1,
        _ => 0,
    }
}
fn readable(v: &Value) -> usize {
    v["pageStatus"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| matches!(p["status"].as_str(), Some("native" | "ocr" | "degraded")))
        .count()
}
impl SqlStore {
    pub fn document_step(
        &self,
        snapshot: &Snapshot,
        file: &Value,
        figures: bool,
        engine: &dyn Engine,
        limits: &Limits,
        control: &Control,
    ) -> Result<Outcome, String> {
        self.live(&snapshot.index)?;
        if !self.snapshot_current(snapshot)? {
            return Err("document changed".into());
        }
        let name = file["name"].as_str().ok_or_else(fault)?;
        let d = &file["document"];
        let version = d["version"].as_str().ok_or_else(fault)?;
        let dir = self.dir(&snapshot.index)?;
        let path = document_ingest::version_path(&dir, &snapshot.index.project, version, ".json")
            .ok_or_else(fault)?;
        let declared = d["bytes"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(extract::LARGE_BYTES + 1);
        let _large = extract::large_lock_cancellable(
            declared,
            engine.large_lock_file().as_deref(),
            &|| control.stopped() || control.suspended(),
        )?;
        let before_bytes = std::fs::read(&path).ok();
        let before = record(&path);
        if !figures && before.as_ref().is_none_or(|r| r["retryable"] == true) {
            return Ok(outcome("skipped"));
        }
        if !figures && before.as_ref().is_some_and(finished) {
            return self
                .publish_document(
                    snapshot,
                    file,
                    file.clone(),
                    None,
                    &before_bytes,
                    Some(("completionSweep", json!(version))),
                    false,
                    control,
                )
                .map(|_| outcome("complete"));
        }
        let original = document_ingest::version_path(
            &dir,
            &snapshot.index.project,
            d["byteHash"].as_str().ok_or_else(fault)?,
            ".pdf",
        )
        .ok_or_else(fault)?;
        let size = std::fs::metadata(&original).map(|m| m.len()).ok();
        let Some(size) = size else {
            return self
                .publish_document(
                    snapshot,
                    file,
                    file.clone(),
                    None,
                    &before_bytes,
                    Some((
                        if figures {
                            "figureSweep"
                        } else {
                            "completionSweep"
                        },
                        json!(version),
                    )),
                    false,
                    control,
                )
                .map(|_| outcome("missing"));
        };
        if size > limits.read_bytes as u64 {
            return self
                .publish_document(
                    snapshot,
                    file,
                    file.clone(),
                    None,
                    &before_bytes,
                    Some((
                        if figures {
                            "figureSweep"
                        } else {
                            "completionSweep"
                        },
                        json!(version),
                    )),
                    false,
                    control,
                )
                .map(|_| outcome("failed"));
        }

        let bytes = std::fs::read(&original).map_err(|_| fault())?;
        if bytes.len() as u64 != size
            || extract::sha_hex(&bytes) != d["byteHash"].as_str().unwrap_or_default()
        {
            return Err(fault());
        }
        control.renew_publication_proof()?;
        if control.stopped() || control.suspended() {
            return Err(fault());
        }
        let stage = dir.join("source-staging").join(format!(
            "media-{}",
            digest(&format!(
                "{}:{}:{}",
                std::process::id(),
                name,
                server_projects::js::random36(24)
            ))
        ));
        store::create_dirs_below(&dir, &stage).map_err(|_| fault())?;
        let stage = Staged(stage);
        let docs = server_projects::lifecycle::documents_dir(&dir, &snapshot.index.project);
        let stage_docs =
            server_projects::lifecycle::documents_dir(&stage.0, &snapshot.index.project);
        store::create_dirs_below(&stage.0, &stage_docs).map_err(|_| fault())?;
        // Copy both the previous and the current extractor's cached record. A
        // prior save failure must not force extraction of already persisted bytes.
        let expected_version = extract::sha_hex(
            format!(
                "{}:{}",
                d["byteHash"].as_str().ok_or_else(fault)?,
                extract::extractor_version(extract::figures_on(engine))
            )
            .as_bytes(),
        );
        let mut record_inputs = BTreeMap::new();
        record_inputs.insert(format!("{version}.json"), before_bytes.clone());
        if expected_version != version {
            record_inputs.insert(
                format!("{expected_version}.json"),
                std::fs::read(
                    server_projects::lifecycle::documents_dir(&dir, &snapshot.index.project)
                        .join(format!("{expected_version}.json")),
                )
                .ok(),
            );
        }
        for (name, raw) in &record_inputs {
            if let Some(raw) = raw {
                std::fs::write(stage_docs.join(name), raw).map_err(|_| fault())?;
            }
        }
        let ingested = document_ingest::ingest(
            &stage.0,
            &snapshot.index.project,
            name,
            &bytes,
            Some(&js(file)?),
            engine,
            limits,
        )
        .map_err(|_| fault())?;
        if control.stopped() || control.suspended() {
            return Err(fault());
        }
        let next = value(&ingested.file)?;
        let nd = &next["document"];
        if nd["version"] != expected_version {
            return Err("document extractor changed".into());
        }
        let error = nd["error"].as_str().unwrap_or_default();
        if figures && (nd["state"] == "failed" || nd["stale"] == true) && ingested.retryable {
            return Ok(Outcome {
                status: "retry".into(),
                unreachable: service_down(error),
                timeout: ["timed out", "time out", "timeout", "aborted", "aborterror"]
                    .iter()
                    .any(|needle| error.to_ascii_lowercase().contains(needle)),
            });
        }
        if figures
            && (nd["state"] == "failed"
                || nd["stale"] == true
                || next["content"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
                || rank(nd) < rank(d)
                || readable(nd) < readable(d))
        {
            return self
                .publish_document(
                    snapshot,
                    file,
                    file.clone(),
                    None,
                    &before_bytes,
                    Some(("figureSweep", json!(version))),
                    false,
                    control,
                )
                .map(|_| outcome("failed"));
        }
        let mut updated = file.clone();
        let mut status = "refreshed";
        let mut unreachable = false;
        if !figures {
            let after = record(&stage_docs.join(format!("{version}.json")));
            let done = after
                .as_ref()
                .is_some_and(|r| r["retryable"] != true && finished(r));
            let passing = after.as_ref().is_some_and(|r| {
                r.pointer("/stoppedAt/limit").is_some_and(|s| s == "error")
                    && r["completion"] != extract::COMPLETION
                    || r["pageTexts"].as_array().into_iter().flatten().any(|p| {
                        p.pointer("/retry/outcome").is_some_and(|s| s == "failed")
                            && p.pointer("/retry/again") == Some(&Value::Bool(true))
                    })
            });
            let failed = nd["version"] != d["version"]
                || (nd["state"] == "failed" && d["state"] != "failed");
            unreachable = if failed { service_down(error) } else { passing };
            let tries = d["completionTries"].as_u64().unwrap_or(0) + u64::from(!unreachable);
            if !failed {
                updated["content"] = next["content"].clone();
                updated["document"] = nd.clone();
            }
            for key in ["figureSweep", "completionTries"] {
                if let Some(v) = d.get(key) {
                    updated["document"][key] = v.clone();
                }
            }
            if done || tries >= 6 {
                updated["document"]["completionSweep"] = json!(version);
                if let Some(doc) = updated["document"].as_object_mut() {
                    doc.remove("completionTries");
                }
            } else if tries > 0 {
                updated["document"]["completionTries"] = json!(tries);
            }
            status = if failed {
                if ingested.retryable {
                    "retry"
                } else {
                    "failed"
                }
            } else if done {
                "completed"
            } else if unreachable {
                "retry"
            } else {
                "deferred"
            };
        } else {
            updated["content"] = next["content"].clone();
            updated["document"] = nd.clone();
        }
        let reindex = figures || updated["content"] != file["content"];
        // The publication fence checks both metadata and the input record.
        let _ = docs;
        self.publish_document(
            snapshot,
            file,
            updated,
            Some((&stage_docs, &record_inputs)),
            &before_bytes,
            None,
            reindex,
            control,
        )?;
        Ok(Outcome {
            status: status.into(),
            unreachable,
            timeout: false,
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn publish_document(
        &self,
        snapshot: &Snapshot,
        old: &Value,
        mut updated: Value,
        staged: Option<(&Path, &RecordInputs)>,
        before: &Option<Vec<u8>>,
        marker: Option<(&str, Value)>,
        reindex: bool,
        control: &Control,
    ) -> Result<(), String> {
        let dir = self.dir(&snapshot.index)?;
        let writer = server_store::Writer::open(&self.root, server_store::RustAuth::owned())
            .map_err(|_| fault())?;
        writer
            .write(|auth| {
                let user = snapshot.index.user.as_deref().unwrap_or_default();
                if auth
                    .reader()
                    .row("SELECT id FROM users WHERE id=?", [user], |r| {
                        r.get::<_, String>(0)
                    })?
                    .is_none()
                {
                    return Ok(Err(fault()));
                }
                let result = store::update(&dir, server_store::RustProjects::owned(), |projects| {
                    let run = (|| {
                        control.renew_publication_proof()?;
                        if control.stopped() || control.suspended() {
                            return Err(fault());
                        }
                        let p =
                            store::find_mut(projects, &snapshot.index.project).ok_or_else(fault)?;
                        let mut files = match p.get("files") {
                            JValue::Arr(files) => files.clone(),
                            _ => return Err(fault()),
                        };
                        let name = old["name"].as_str().ok_or_else(fault)?;
                        let live = files
                            .iter_mut()
                            .find(|f| f.get("name").as_str() == Some(name))
                            .ok_or_else(fault)?;
                        if value(live)? != *old {
                            return Err("document changed".into());
                        }
                        let record_path = document_ingest::version_path(
                            &dir,
                            &snapshot.index.project,
                            old["document"]["version"].as_str().ok_or_else(fault)?,
                            ".json",
                        )
                        .ok_or_else(fault)?;
                        // Shared with foreground ingestion: compare and publish without
                        // holding this lock during any engine request.
                        let _record = document_ingest::lock_records(&dir).map_err(|_| fault())?;
                        if std::fs::read(&record_path).ok() != *before {
                            return Err("document record changed".into());
                        }
                        if let Some((key, mark)) = marker {
                            updated["document"][key] = mark;
                            if key == "completionSweep" {
                                if let Some(d) = updated["document"].as_object_mut() {
                                    d.remove("completionTries");
                                }
                            }
                        }
                        if let Some((staged, inputs)) = staged {
                            for (name, expected) in inputs {
                                if std::fs::read(record_path.parent().ok_or_else(fault)?.join(name))
                                    .ok()
                                    != *expected
                                {
                                    return Err("document record changed".into());
                                }
                            }
                            // Figures first, JSON last: a visible record never points to missing new bytes.
                            let mut entries = std::fs::read_dir(staged)
                                .map_err(|_| fault())?
                                .collect::<Result<Vec<_>, _>>()
                                .map_err(|_| fault())?;
                            entries
                                .sort_by_key(|e| e.path().extension().is_some_and(|s| s == "json"));
                            for e in entries {
                                if !e.file_type().map_err(|_| fault())?.is_file() {
                                    continue;
                                }
                                let destination =
                                    record_path.parent().ok_or_else(fault)?.join(e.file_name());
                                if destination.extension().is_some_and(|e| e != "json")
                                    && destination.exists()
                                {
                                    continue;
                                }
                                document_ingest::write_once(
                                    &dir,
                                    &destination,
                                    &std::fs::read(e.path()).map_err(|_| fault())?,
                                )
                                .map_err(|_| fault())?;
                            }
                        }
                        let mut next = js(&updated)?;
                        if reindex {
                            document_ingest::mark_indexing(&mut next);
                            server_projects::outbox::enqueue(
                                &dir,
                                &[server_projects::outbox::Job {
                                    kind: "index",
                                    project: &snapshot.index.project,
                                    file: Some(name),
                                    payload: None,
                                }],
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis() as i64)
                                    .unwrap_or(0),
                            )
                            .map_err(|_| fault())?;
                        }
                        *live = next;
                        store::set(p, "files", JValue::Arr(files));
                        Ok(())
                    })();
                    let changed = run.is_ok();
                    (run, changed)
                })
                .map_err(|_| fault())
                .and_then(|r| r);
                Ok(result)
            })
            .map_err(|_| fault())?
    }
}
