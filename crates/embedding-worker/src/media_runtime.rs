//! Native media/figure/document workers, co-owned with the embedding lifecycle.
// serde_json's read-only string-key indexing returns Null. All mutable document
// parents are checked as objects by the candidate/version guards before mutation.
#![allow(clippy::indexing_slicing)]

use crate::{
    fill::Embed,
    media_policy,
    media_store::{MediaItem, Snapshot},
    runtime::NativeHost,
    Control,
};
use base64::Engine as _;
use serde_json::{json, Value};
use server_projects::extract::{self, Engine, Limits};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::watch, time::Instant};
pub type EngineFactory = Arc<dyn Fn(Control) -> Arc<dyn Engine + Send + Sync> + Send + Sync>;
fn env_num(key: &str, default: u64, lo: u64, hi: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| media_policy::env_number(&s))
        .filter(|n| n.is_finite())
        .map(|n| n.round().clamp(lo as f64, hi as f64) as u64)
        .unwrap_or(default)
}
fn enabled() -> bool {
    std::env::var("NOEVIA_MULTIMODAL_RAG")
        .is_ok_and(|s| ["1", "true", "on", "yes"].contains(&s.trim().to_ascii_lowercase().as_str()))
}
fn diary(id: &str) -> bool {
    id == "cowork-diary-extras"
        || id
            .to_ascii_lowercase()
            .starts_with("cowork-chat-context-diary-")
        || id
            .to_ascii_lowercase()
            .starts_with("cowork-chat-context-diary_")
}
fn read(path: &Path) -> Option<Value> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > 256 * 1024 * 1024 {
        return None;
    }
    let raw = std::fs::read(path).ok()?;
    server_projects::source_jobs::parse_js(&raw).ok()
}
fn read_request(path: &Path) -> Option<Value> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > 8192 {
        return None;
    }
    read(path)
}
fn hex(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn string(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .into()
}
fn base(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}
fn mime(s: &str) -> Option<&'static str> {
    let ext = s.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}
pub fn media_items(root: &Path, project_id: &str, project: &Value) -> Vec<MediaItem> {
    let files: BTreeMap<_, _> = project["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| Some((f["name"].as_str()?, f)))
        .collect();
    let mut out = vec![];
    for asset in project["assets"].as_array().into_iter().flatten() {
        let id = string(asset, "id");
        if !id.strip_prefix("img-").is_some_and(|s| {
            !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        }) {
            continue;
        }
        let file = asset["sourceName"]
            .as_str()
            .or_else(|| asset["name"].as_str())
            .unwrap_or_default();
        let Some(f) = files.get(file) else {
            continue;
        };
        let hash = f
            .pointer("/attachment/id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let m = asset["mime"].as_str().unwrap_or_default();
        if !hex(hash)
            || !["image/png", "image/jpeg", "image/webp", "image/gif"].contains(&m)
            || asset["bytes"]
                .as_f64()
                .is_some_and(|n| n > 8.0 * 1024.0 * 1024.0)
        {
            continue;
        }
        out.push(MediaItem {
            file: file.into(),
            hash: format!("img:{hash}"),
            label: format!("Image {}", base(asset["name"].as_str().unwrap_or_default())),
            meta: json!({"source":file,"assetId":id,"name":asset["name"],"mime":m}),
            path: server_projects::store::asset_dir(root, project_id).join(id),
            mime: m.into(),
        });
    }
    let docs = server_projects::lifecycle::documents_dir(root, project_id);
    for (name, f) in files {
        if mime(name).is_some() {
            continue;
        }
        let Some(version) = f
            .pointer("/document/availableVersion")
            .and_then(Value::as_str)
            .filter(|s| hex(s))
        else {
            continue;
        };
        let Some(record) = read(&docs.join(format!("{version}.json"))) else {
            continue;
        };
        let pages: BTreeMap<_, _> = record["pageTexts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p["number"].as_i64()?, p)))
            .collect();
        for fig in record["figures"].as_array().into_iter().flatten() {
            let file = string(fig, "file");
            let hash = string(fig, "hash");
            let Some(suffix) = file.strip_prefix(&format!("{version}.fig")) else {
                continue;
            };
            let Some((number, ext)) = suffix.split_once('.') else {
                continue;
            };
            if number.is_empty()
                || number.len() > 3
                || !number.bytes().all(|b| b.is_ascii_digit())
                || !["png", "jpg", "webp"].contains(&ext)
                || !hex(&hash)
            {
                continue;
            }
            let page = fig["page"].as_i64().unwrap_or(0);
            if pages
                .get(&page)
                .is_some_and(|p| media_policy::scanned_page(p))
            {
                continue;
            }
            let caption = js_json::wtf16::slice(fig["caption"].as_str().unwrap_or_default(), 300);
            let m = mime(&file).unwrap_or("image/png");
            let index = fig["index"].as_i64().unwrap_or(0) + 1;
            let mut meta = json!({"source":name,"page":page,"figure":index,"version":version,"figureFile":file,"mime":m});
            if !caption.is_empty() {
                meta["caption"] = json!(caption);
            }
            out.push(MediaItem {
                file: name.into(),
                hash: format!("img:{hash}"),
                label: format!(
                    "Figure {index} on page {page} of {}{}",
                    base(name),
                    if caption.is_empty() {
                        String::new()
                    } else {
                        format!(": {caption}")
                    }
                ),
                meta,
                path: docs.join(&file),
                mime: m.into(),
            });
        }
    }
    out
}
/// Called only after the embedding lifetime-owner lock has been acquired. No
/// previous background document task can still own one of these staging trees.
pub fn sweep_staging(folder: &Path) {
    if let Ok(entries) = std::fs::read_dir(folder) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir())
                && entry
                    .file_name()
                    .to_str()
                    .and_then(|s| s.strip_prefix("media-"))
                    .is_some_and(hex)
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

/// Claim by rename, rather than read/compare/unlink: a hint published while a
/// pass runs remains at the original path. A crash leaves a reclaimable claim.
pub fn claim_requests(
    folder: &Path,
    start: &str,
) -> Vec<(std::path::PathBuf, Value, String, String)> {
    let mut requests = Vec::new();
    if let Ok(entries) = std::fs::read_dir(folder) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let base = name.split(".json").next().unwrap_or_default();
            if !hex(base)
                || !(name == format!("{base}.json")
                    || name
                        .strip_prefix(&format!("{base}.json.claimed."))
                        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())))
            {
                continue;
            }
            if !read_request(&entry.path()).is_some_and(|body| body["start"] == start) {
                continue;
            }
            let claimed = folder.join(format!("{base}.json.claimed.{}", std::process::id()));
            if std::fs::rename(entry.path(), &claimed).is_err() {
                continue;
            }
            if let Some(body) = read_request(&claimed).filter(|b| b["start"] == start) {
                if let (Some(user), Some(project)) =
                    (body["user"].as_str(), body["project"].as_str())
                {
                    requests.push((claimed, body.clone(), user.to_owned(), project.to_owned()));
                }
            }
        }
    }
    requests
}

fn same_tail_source(before: &Value, after: &Value) -> bool {
    let mut before = before.clone();
    let mut after = after.clone();
    for file in [&mut before, &mut after] {
        if let Some(document) = file.get_mut("document").and_then(Value::as_object_mut) {
            document.remove("searchable");
        }
    }
    before == after
}
struct CancelTailRead(Arc<std::sync::atomic::AtomicBool>);
impl Drop for CancelTailRead {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}
// The blocking task owns both the parsed record and its resource permit. Only
// bounded embedding rows and compact coverage leave it; no record or permit
// remains resident across HTTP/pacing awaits on the shared owner runtime.
async fn with_tail_record<T: Send + 'static>(
    path: std::path::PathBuf,
    size: usize,
    lock: Option<std::path::PathBuf>,
    control: Control,
    work: impl FnOnce(&Value) -> Result<T, String> + Send + 'static,
) -> Result<Option<T>, String> {
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _cancel_on_drop = CancelTailRead(cancelled.clone());
    tokio::task::spawn_blocking(move || {
        let cancelled = || {
            cancelled.load(std::sync::atomic::Ordering::Acquire)
                || control.stopped()
                || control.suspended()
        };
        let _held = extract::large_lock_cancellable(size, lock.as_deref(), &cancelled)?;
        let record = read(&path);
        if cancelled() {
            return Err("tail work cancelled".into());
        }
        record.as_ref().map(work).transpose()
    })
    .await
    .map_err(|_| "tail worker unavailable".to_owned())?
}
struct TailBatch {
    rows: Vec<crate::fill::Row>,
    coverage: crate::media_store::TailCoverage,
    final_window: bool,
    limited: bool,
    to_page: i64,
}

struct Worker<'a> {
    host: &'a NativeHost,
    control: Control,
    state: &'a Mutex<Value>,
    engine: Arc<dyn Engine + Send + Sync>,
    cfg: Value,
    spent: VecDeque<(Instant, usize)>,
    backoff: BTreeMap<String, Instant>,
}
impl Worker<'_> {
    async fn document_step(
        &self,
        snapshot: &Snapshot,
        file: &Value,
        figures: bool,
        pages: usize,
    ) -> Result<(crate::media_documents::Outcome, usize), String> {
        let store = self.host.store.clone();
        let snapshot = snapshot.clone();
        let file = file.clone();
        let engine = self.engine.clone();
        let control = self.control.clone();
        tokio::task::spawn_blocking(move || {
            let budget = extract::Budgeted {
                inner: engine.as_ref(),
                budget: extract::OcrBudget::new(pages),
            };
            let result = store.document_step(
                &snapshot,
                &file,
                figures,
                if figures { engine.as_ref() } else { &budget },
                &Limits::from_env(),
                &control,
            );
            result.map(|r| (r, budget.budget.remaining.get()))
        })
        .await
        .map_err(|_| "document worker unavailable".to_owned())?
    }
    async fn ready(&self, foreground: bool) -> bool {
        if self.control.stopped()
            || self.control.suspended()
            || self.host.store.config().map_or(true, |c| c.next.is_some())
        {
            return false;
        }
        if self.host.http.authorize(&self.control).await.is_err() {
            return false;
        }
        self.host
            .http
            .media_ready(
                &self.host.core,
                &self.host.token,
                &self.host.start,
                foreground,
            )
            .await
            .unwrap_or(false)
    }
    async fn pause(&self, started: Instant, duty: f64, minimum: u64) {
        let wait = Duration::from_millis(minimum).max(Duration::from_secs_f64(
            started.elapsed().as_secs_f64() * (1.0 - duty) / duty,
        ));
        tokio::select! {_=tokio::time::sleep(wait)=>{},_=self.control.cancelled()=>{}}
    }
    fn projects(&self, priority: &[(String, String)]) -> Vec<Snapshot> {
        let mut order = priority.to_vec();
        for user in self.host.store.users().unwrap_or_default() {
            let Ok(dir) = server_projects::store::user_dir(&self.host.store.root, &user) else {
                continue;
            };
            let Ok(projects) = server_projects::store::read(&dir) else {
                continue;
            };
            for p in projects {
                if let Some(id) = p.get("id").as_str() {
                    order.push((user.clone(), id.into()));
                }
            }
        }
        let mut seen = BTreeSet::new();
        order
            .into_iter()
            .filter(|(_, p)| !diary(p))
            .filter(|p| seen.insert(p.clone()))
            .filter_map(|(u, p)| self.host.store.media_snapshot(&u, &p).ok())
            .collect()
    }
    fn service(&self) -> Option<Value> {
        self.state
            .lock()
            .ok()
            .and_then(|s| s.get("service").cloned())
            .filter(|s| !s.is_null())
    }
    fn media_model(&self, target: &crate::Target) -> Option<(String, bool)> {
        if target.profile != "embeddinggemma-2" {
            return None;
        }
        if let Some(service) = self.service().filter(|_| {
            target.base_url.is_none()
                || crate::service::same_address(
                    target.base_url.as_deref(),
                    self.host.service_base.as_deref(),
                )
        }) {
            if service["enabled"] != true {
                return None;
            }
            let alias = service["vision"]
                .as_array()?
                .iter()
                .find(|v| v["model"] == target.model)?
                .get("alias")?
                .as_str()?
                .to_owned();
            return Some((alias, true));
        }
        std::env::var("EMBEDDING_IMAGE_MODEL")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .map(|s| (s, false))
    }
    async fn media(&mut self, priority: &[(String, String)]) -> bool {
        let mut unavailable = false;
        let mut used = None;
        let mut gap = false;
        let interval = env_num("NOEVIA_MULTIMODAL_RAG_INTERVAL_MS", 2000, 0, 600000);
        'walk: for snapshot in self.projects(priority) {
            if !enabled() {
                break;
            }
            if !self.ready(false).await {
                unavailable = true;
                break;
            }
            let Ok(dir) = self.host.store.dir(&snapshot.index) else {
                continue;
            };
            let items = media_items(&dir, &snapshot.index.project, &snapshot.project);
            if items.is_empty()
                && !self
                    .host
                    .store
                    .index_path(&snapshot.index)
                    .is_ok_and(|p| p.exists())
            {
                continue;
            }
            if !items.is_empty()
                && self
                    .host
                    .store
                    .create_media_index(&snapshot, &self.control)
                    .is_err()
            {
                continue;
            }
            loop {
                if gap {
                    self.pause(Instant::now(), 1.0, interval).await;
                    gap = false;
                }
                if !enabled() {
                    break 'walk;
                }
                if !self.ready(false).await {
                    unavailable = true;
                    break 'walk;
                }
                let Ok(config) = self.host.store.config() else {
                    break 'walk;
                };
                if config.active.profile != "embeddinggemma-2" {
                    break;
                }
                let Ok(rows) = self.host.store.reconcile_media(
                    &snapshot,
                    &items,
                    &config.active,
                    &self.control,
                ) else {
                    break;
                };
                if rows.is_empty() {
                    break;
                }
                let Some((alias, managed)) = self.media_model(&config.active) else {
                    unavailable = true;
                    break 'walk;
                };
                let mut embedded = 0;
                for row in &rows {
                    let Some(item) = items.iter().find(|i| i.hash == row.hash) else {
                        continue;
                    };
                    let Ok(bytes) = std::fs::read(&item.path) else {
                        continue;
                    };
                    if bytes.len() > 8 * 1024 * 1024 {
                        continue;
                    }
                    let part = json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{}",item.mime,base64::engine::general_purpose::STANDARD.encode(bytes))}});
                    used = Some((config.active.clone(), alias.clone(), managed));
                    match self
                        .host
                        .http
                        .media(&config.active, &alias, part, &self.control)
                        .await
                    {
                        Ok(vector) => {
                            embedded = self
                                .host
                                .store
                                .publish_media_rows(
                                    &snapshot,
                                    &config.active,
                                    std::slice::from_ref(row),
                                    &[vector],
                                    config.version,
                                    &self.control,
                                )
                                .unwrap_or(0);
                            gap = embedded > 0;
                            break;
                        }
                        Err(_) => {
                            unavailable = true;
                            break 'walk;
                        }
                    }
                }
                if embedded == 0 {
                    break;
                }
            }
        }
        if let Some((target, alias, true)) = used {
            if target.base_url.is_some()
                && crate::service::same_address(
                    target.base_url.as_deref(),
                    self.host.service_base.as_deref(),
                )
            {
                self.host.http.release_media(&target, &alias).await;
            }
        }
        unavailable
    }
    async fn figures(&mut self) {
        if !enabled() || !extract::figures_on(self.engine.as_ref()) {
            return;
        }
        let mut done = 0;
        let mut timeouts = 0;
        for snapshot in self.projects(&[]) {
            let files = snapshot.project["files"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for selected in files {
                let Ok(snapshot) = self.host.store.media_snapshot(
                    snapshot.index.user.as_deref().unwrap_or_default(),
                    &snapshot.index.project,
                ) else {
                    continue;
                };
                let Some(file) = snapshot.project["files"]
                    .as_array()
                    .and_then(|files| files.iter().find(|file| file["name"] == selected["name"]))
                    .cloned()
                else {
                    continue;
                };
                if done >= env_num("NOEVIA_MULTIMODAL_FIGURE_SWEEP_MAX", 10, 1, 100) {
                    return;
                }
                if !enabled() || !self.ready(false).await {
                    return;
                }
                let record = self
                    .host
                    .store
                    .dir(&snapshot.index)
                    .ok()
                    .and_then(|d| {
                        server_projects::document_ingest::version_path(
                            &d,
                            &snapshot.index.project,
                            file.pointer("/document/availableVersion")?.as_str()?,
                            ".json",
                        )
                    })
                    .and_then(|p| read(&p))
                    .unwrap_or(Value::Null);
                if !media_policy::needs_figures(&file, &record) {
                    continue;
                }
                let started = Instant::now();
                let Ok(result) = self
                    .document_step(&snapshot, &file, true, 0)
                    .await
                    .map(|(r, _)| r)
                else {
                    continue;
                };
                if result.status == "retry" && result.unreachable {
                    return;
                }
                if ["retry", "refreshed", "failed", "missing", "worse"]
                    .contains(&result.status.as_str())
                {
                    done += 1;
                    timeouts = if result.timeout { timeouts + 1 } else { 0 };
                    if timeouts >= 2 {
                        return;
                    }
                    self.pause(
                        started,
                        std::env::var("EMBEDDING_REINDEX_DUTY")
                            .ok()
                            .and_then(|s| media_policy::env_number(&s))
                            .filter(|n| n.is_finite())
                            .unwrap_or(0.5)
                            .clamp(0.05, 1.0),
                        env_num("NOEVIA_MULTIMODAL_RAG_INTERVAL_MS", 2000, 0, 600000),
                    )
                    .await;
                }
            }
        }
    }
    async fn documents(&mut self) {
        while self
            .spent
            .front()
            .is_some_and(|(at, _)| at.elapsed() >= Duration::from_secs(3600))
        {
            self.spent.pop_front();
        }
        let per_hour = self.cfg["perHour"].as_f64().unwrap_or(480.0) as usize;
        let mut left = (self.cfg["perPass"].as_f64().unwrap_or(48.0) as usize)
            .min(per_hour.saturating_sub(self.spent.iter().map(|(_, n)| n).sum()));
        let mut uploads = 0;
        let mut uploads_held = false;
        let extractor = extract::extractor_version(extract::figures_on(self.engine.as_ref()));
        let mut budget_left = env_num("PROJECT_SYNC_OCR_PAGES", 40, 0, 100000) as usize;
        for snapshot in self.projects(&[]) {
            let files = snapshot.project["files"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for selected in files {
                let Ok(snapshot) = self.host.store.media_snapshot(
                    snapshot.index.user.as_deref().unwrap_or_default(),
                    &snapshot.index.project,
                ) else {
                    continue;
                };
                let Some(file) = snapshot.project["files"]
                    .as_array()
                    .and_then(|files| files.iter().find(|file| file["name"] == selected["name"]))
                    .cloned()
                else {
                    continue;
                };
                if self.control.stopped() {
                    return;
                }
                let user = snapshot.index.user.as_deref().unwrap_or_default();
                let name = string(&file, "name");
                if media_policy::upload_candidate(&file, &extractor)
                    && !uploads_held
                    && uploads < (self.cfg["uploads"].as_f64().unwrap_or(2.0) as usize)
                {
                    if !self.ready(false).await {
                        return;
                    }
                    let started = Instant::now();
                    if let Ok((r, remaining)) = self
                        .document_step(&snapshot, &file, false, budget_left)
                        .await
                    {
                        budget_left = remaining;
                        if !["complete", "changed", "skipped"].contains(&r.status.as_str()) {
                            uploads += 1;
                            uploads_held = r.status == "retry" && r.unreachable;
                            self.document_pause(started).await;
                        }
                    }
                }
                let Ok(mut current) = self
                    .host
                    .store
                    .media_snapshot(user, &snapshot.index.project)
                else {
                    continue;
                };
                let Some(file) = current.project["files"]
                    .as_array()
                    .and_then(|f| f.iter().find(|f| f["name"] == name))
                    .cloned()
                else {
                    continue;
                };
                let dir = match self.host.store.dir(&current.index) {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                let js = match server_projects::source_jobs::stringify_js(&file)
                    .ok()
                    .and_then(|s| js_json::wtf16::parse(&s).ok())
                {
                    Some(v) => v,
                    None => continue,
                };
                let head = server_projects::sources::text(&dir, &current.index.project, &js)
                    .ok()
                    .unwrap_or_default();
                if !media_policy::tail_candidate(&file, 200000, head.encode_utf16().count())
                    || left == 0
                {
                    continue;
                }
                let key = format!(
                    "{user}:{}:{name}:{}",
                    current.index.project, file["document"]["availableVersion"]
                );
                if self
                    .backoff
                    .get(&key)
                    .is_some_and(|at| *at > Instant::now())
                {
                    continue;
                }
                let declared = file
                    .pointer("/document/bytes")
                    .and_then(Value::as_u64)
                    .and_then(|n| usize::try_from(n).ok())
                    .unwrap_or(extract::LARGE_BYTES + 1);
                let Some(path) = server_projects::document_ingest::version_path(
                    &dir,
                    &current.index.project,
                    file["document"]["availableVersion"]
                        .as_str()
                        .unwrap_or_default(),
                    ".json",
                ) else {
                    continue;
                };
                let mut from = file
                    .pointer("/document/searchable/page")
                    .and_then(Value::as_i64)
                    .filter(|_| {
                        file.pointer("/document/searchable/version")
                            == file.pointer("/document/availableVersion")
                    })
                    .unwrap_or(0)
                    + 1;
                let mut allowance =
                    left.min(self.cfg["perDocument"].as_f64().unwrap_or(24.0) as usize);
                while allowance > 0 {
                    if !self.ready(true).await {
                        return;
                    }
                    let limit = allowance.min(self.cfg["batch"].as_f64().unwrap_or(8.0) as usize);
                    let started = Instant::now();
                    let Ok(config) = self.host.store.config() else {
                        return;
                    };
                    let store = self.host.store.clone();
                    let snapshot = current.clone();
                    let head_copy = head.clone();
                    let name_copy = name.clone();
                    let target = config.active.clone();
                    let control = self.control.clone();
                    let options = json!({"textLimit":Limits::from_env().text,"fromPage":from,"maxChars":(limit+2)*1200});
                    let prepared = with_tail_record(
                        path.clone(),
                        declared,
                        self.engine.large_lock_file(),
                        self.control.clone(),
                        move |record| {
                            let window = media_policy::tail_window(
                                &head_copy,
                                record,
                                &options,
                                crate::outbox::chunks,
                            );
                            if window.is_null() {
                                return Ok(None);
                            }
                            let (rows, coverage) = store.prepare_tail(
                                &snapshot, &name_copy, &head_copy, &window, &target, limit,
                                &control,
                            )?;
                            Ok(Some(TailBatch {
                                rows,
                                coverage,
                                final_window: window["final"] == true,
                                limited: window["limited"] == true,
                                to_page: window["toPage"].as_i64().unwrap_or(0),
                            }))
                        },
                    )
                    .await;
                    let TailBatch {
                        rows,
                        coverage,
                        final_window,
                        limited,
                        to_page,
                    } = match prepared {
                        Ok(Some(Some(batch))) => batch,
                        Ok(Some(None)) => {
                            self.backoff
                                .insert(key.clone(), Instant::now() + Duration::from_secs(3600));
                            break;
                        }
                        _ => break,
                    };
                    let attempted = rows.len();
                    let mut embedded = 0;
                    let mut failed = false;
                    for batch in rows.chunks(4) {
                        if !self.ready(true).await {
                            return;
                        }
                        let Ok(profile) = prompt_framing::embedding_input::Profile::try_from(
                            config.active.profile.as_str(),
                        ) else {
                            return;
                        };
                        let inputs: Vec<_> = batch
                            .iter()
                            .map(|r| {
                                prompt_framing::embedding_input::frame(
                                    profile,
                                    prompt_framing::embedding_input::Kind::Document,
                                    &r.text,
                                    None,
                                )
                            })
                            .collect();
                        match self
                            .host
                            .http
                            .embed(&config.active, &inputs, &self.control)
                            .await
                        {
                            Ok(v) => {
                                embedded += self
                                    .host
                                    .store
                                    .publish_media_rows(
                                        &current,
                                        &config.active,
                                        batch,
                                        &v,
                                        config.version,
                                        &self.control,
                                    )
                                    .unwrap_or(0)
                            }
                            Err(_) => {
                                failed = true;
                                break;
                            }
                        }
                    }
                    allowance = allowance.saturating_sub(attempted.max(embedded));
                    left = left.saturating_sub(embedded);
                    if embedded > 0 {
                        self.spent.push_back((Instant::now(), embedded));
                    }
                    let Ok((pending, page)) =
                        self.host
                            .store
                            .tail_coverage(&current, &name, &config.active, &coverage)
                    else {
                        break;
                    };
                    let complete = final_window && pending == 0;
                    let mut marker = json!({"version":file["document"]["availableVersion"],"page":if complete{None}else{page},"complete":complete});
                    if limited {
                        marker["limited"] = json!(true);
                    }
                    if self.save_coverage(&current, &name, &file, marker).is_err() {
                        break;
                    }
                    let Ok(s) = self.host.store.media_snapshot(user, &current.index.project) else {
                        break;
                    };
                    current = s;
                    if failed {
                        return;
                    }
                    // Our coverage marker may change; a foreground source change
                    // must not make a later window continue with the old record.
                    if !current.project["files"]
                        .as_array()
                        .and_then(|files| files.iter().find(|file| file["name"] == name))
                        .is_some_and(|now| same_tail_source(&file, now))
                    {
                        break;
                    }
                    if complete {
                        break;
                    }
                    if attempted > 0 && embedded == 0 {
                        self.backoff
                            .insert(key.clone(), Instant::now() + Duration::from_secs(3600));
                        break;
                    }
                    if pending > 0 && attempted == 0 {
                        break;
                    }
                    if pending == 0 {
                        from = to_page + 1;
                    }
                    if attempted > 0 {
                        self.document_pause(started).await;
                    }
                }
            }
        }
    }
    async fn document_pause(&self, started: Instant) {
        self.pause(
            started,
            self.cfg["duty"].as_f64().unwrap_or(0.25),
            self.cfg["minPauseMs"].as_f64().unwrap_or(250.0) as u64,
        )
        .await;
    }
    fn save_coverage(
        &self,
        snapshot: &Snapshot,
        name: &str,
        old: &Value,
        marker: Value,
    ) -> Result<(), String> {
        let dir = self.host.store.dir(&snapshot.index)?;
        let writer =
            server_store::Writer::open(&self.host.store.root, server_store::RustAuth::owned())
                .map_err(|_| "coverage unavailable")?;
        writer
            .write(|auth| {
                if auth
                    .reader()
                    .row(
                        "SELECT id FROM users WHERE id=?",
                        [snapshot.index.user.as_deref().unwrap_or_default()],
                        |r| r.get::<_, String>(0),
                    )?
                    .is_none()
                {
                    return Ok(Err("account removed".into()));
                }
                Ok(server_projects::store::update(
                    &dir,
                    server_store::RustProjects::owned(),
                    |projects| {
                        let result = (|| {
                            self.control.renew_publication_proof()?;
                            if self.control.stopped() || self.control.suspended() {
                                return Err("worker stopped".into());
                            }
                            let project =
                                server_projects::store::find_mut(projects, &snapshot.index.project)
                                    .ok_or("project removed")?;
                            let mut files = match project.get("files") {
                                js_json::JValue::Arr(f) => f.clone(),
                                _ => return Err("source removed".into()),
                            };
                            let file = files
                                .iter_mut()
                                .find(|f| f.get("name").as_str() == Some(name))
                                .ok_or("source removed")?;
                            let expected = snapshot.project["files"]
                                .as_array()
                                .and_then(|files| files.iter().find(|f| f["name"] == name))
                                .ok_or("source removed")?;
                            let actual = server_projects::source_jobs::parse_js(
                                js_json::wtf16::stringify(file)
                                    .ok_or("source invalid")?
                                    .as_bytes(),
                            )
                            .map_err(|_| "source invalid")?;
                            if actual != *expected
                                || file.get("document").get("availableVersion").as_str()
                                    != old
                                        .pointer("/document/availableVersion")
                                        .and_then(Value::as_str)
                            {
                                return Err("source changed".into());
                            }
                            let mut doc = file.get("document").clone();
                            let text = server_projects::source_jobs::stringify_js(&marker)
                                .map_err(|_| "marker invalid")?;
                            server_projects::store::set(
                                &mut doc,
                                "searchable",
                                js_json::wtf16::parse(&text).map_err(|_| "marker invalid")?,
                            );
                            server_projects::store::set(file, "document", doc);
                            server_projects::store::set(
                                project,
                                "files",
                                js_json::JValue::Arr(files),
                            );
                            Ok(())
                        })();
                        let changed = result.is_ok();
                        (result, changed)
                    },
                )
                .map_err(|_| "coverage unavailable".to_owned())
                .and_then(|r| r))
            })
            .map_err(|_| "coverage unavailable".to_owned())?
    }
}
struct Schedule {
    began: Instant,
    media_at: Duration,
    figure_at: Duration,
    document_at: Duration,
    retry: Duration,
    document_interval: Duration,
    document_delay: Duration,
    initial_document: bool,
}
impl Schedule {
    fn new(delay: u64, interval: u64, retry: u64) -> Self {
        Self {
            began: Instant::now(),
            media_at: Duration::from_secs(45),
            figure_at: Duration::from_secs(60),
            document_at: Duration::from_millis(delay.min(interval)),
            retry: Duration::from_millis(retry),
            document_interval: Duration::from_millis(interval),
            document_delay: Duration::from_millis(delay),
            initial_document: false,
        }
    }
    fn media_due(&self, requests: bool) -> bool {
        requests || self.began.elapsed() >= self.media_at
    }
    fn next_period(&self, period: Duration) -> Duration {
        let milliseconds = period.as_millis().max(1);
        Duration::from_millis(
            ((self.began.elapsed().as_millis() / milliseconds + 1) * milliseconds)
                .min(u64::MAX as u128) as u64,
        )
    }
    fn media_done(&mut self, unavailable: bool) {
        let recurring = self.next_period(Duration::from_secs(1800));
        self.media_at = if unavailable {
            recurring.min(self.began.elapsed() + self.retry)
        } else {
            recurring
        };
    }
    fn figures_due(&self) -> bool {
        self.began.elapsed() >= self.figure_at
    }
    fn figures_done(&mut self) {
        self.figure_at = self.next_period(Duration::from_secs(1800));
    }
    fn documents_due(&self) -> bool {
        self.began.elapsed() >= self.document_at
    }
    fn documents_done(&mut self) {
        let elapsed = self.began.elapsed();
        if elapsed >= self.document_delay {
            self.initial_document = true;
        }
        let recurring = self.next_period(self.document_interval);
        self.document_at = if self.initial_document {
            recurring
        } else {
            recurring.min(self.document_delay)
        };
    }
}
pub(crate) async fn run(
    host: &NativeHost,
    control: Control,
    state: &Mutex<Value>,
    factory: Option<EngineFactory>,
    mut stop: watch::Receiver<bool>,
) {
    let Some(factory) = factory else {
        let _ = stop.wait_for(|s| *s).await;
        return;
    };
    for user in host.store.users().unwrap_or_default() {
        if let Ok(dir) = server_projects::store::user_dir(&host.store.root, &user) {
            sweep_staging(&dir.join("source-staging"));
        }
    }
    let parent_control = control.clone();
    let engine = factory(control.clone());
    let env: serde_json::Map<String, Value> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, Value::String(v.into_string().ok()?))))
        .collect();
    let cfg = media_policy::settings(&Value::Object(env));
    let mut worker = Worker {
        host,
        control,
        state,
        engine,
        cfg,
        spent: VecDeque::new(),
        backoff: BTreeMap::new(),
    };
    let mut schedule = Schedule::new(
        worker.cfg["delayMs"].as_f64().unwrap_or(90000.0) as u64,
        worker.cfg["intervalMs"].as_f64().unwrap_or(300000.0) as u64,
        env_num("NOEVIA_MULTIMODAL_RAG_RETRY_MS", 300000, 1000, 86400000),
    );
    let folder = host.store.root.join("media-worker-requests");
    loop {
        if *stop.borrow() {
            break;
        }
        worker.control = parent_control.pass();
        worker.engine = factory(worker.control.clone());
        let requests = claim_requests(&folder, &host.start);
        if enabled() && schedule.media_due(!requests.is_empty()) && worker.ready(false).await {
            let priority: Vec<_> = requests
                .iter()
                .map(|(_, _, u, p)| (u.clone(), p.clone()))
                .collect();
            let unavailable = worker.media(&priority).await;
            schedule.media_done(unavailable);
            for (path, _, _, _) in requests {
                let _ = std::fs::remove_file(path);
            }
        }
        if enabled() && schedule.figures_due() && worker.ready(false).await {
            worker.figures().await;
            schedule.figures_done();
        }
        if schedule.documents_due() && worker.ready(false).await {
            worker.documents().await;
            schedule.documents_done();
        }
        tokio::select! {_=tokio::time::sleep(Duration::from_secs(2))=>{},_=stop.changed()=>{}}
    }
}

#[cfg(test)]
mod timing_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn native_deadlines_backoff_and_source_hints_use_a_fake_clock() {
        let mut schedule = Schedule::new(90000, 300000, 300000);
        assert!(!schedule.media_due(false));
        assert!(!schedule.figures_due());
        assert!(!schedule.documents_due());
        tokio::time::advance(Duration::from_secs(45)).await;
        assert!(schedule.media_due(false));
        schedule.media_done(true);
        tokio::time::advance(Duration::from_secs(15)).await;
        assert!(schedule.figures_due());
        schedule.figures_done();
        assert!(!schedule.media_due(false));
        assert!(schedule.media_due(true));
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(schedule.documents_due());
        schedule.documents_done();
        tokio::time::advance(Duration::from_secs(210)).await;
        assert!(
            schedule.documents_due(),
            "the five-minute timer stays anchored at boot"
        );
        tokio::time::advance(Duration::from_secs(44)).await;
        assert!(!schedule.media_due(false));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(schedule.media_due(false));
        schedule.media_done(false);
        assert!(!schedule.media_due(false));
    }
}

#[cfg(test)]
#[path = "media_regression_tests.rs"]
mod regression_tests;

#[cfg(test)]
mod source_fence_tests {
    use super::*;
    #[test]
    fn tail_continuation_accepts_only_its_own_coverage_change() {
        let file = json!({"name":"a.pdf","content":"same head","document":{"availableVersion":"a","byteHash":"original"}});
        let mut progress = file.clone();
        progress["document"]["searchable"] = json!({"version":"a","page":2,"complete":false});
        assert!(same_tail_source(&file, &progress));
        progress["document"]["availableVersion"] = json!("b");
        assert!(
            !same_tail_source(&file, &progress),
            "same head cannot authorize a different record"
        );
        let mut edit = file.clone();
        edit["content"] = json!("edited head");
        assert!(!same_tail_source(&file, &edit));
        assert!(!same_tail_source(&file, &Value::Null));
    }
}
