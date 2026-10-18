//! Native planning only: no queue, directory creation or download occurs in this module.
use crate::{
    download_targets::{DownloadTargets, Outcome as Targets},
    downloads::{Plan, Planned},
    draft_heads::DraftHeads,
    model_updates::{imatrix, Stop},
};
use model_files::{json::Value, HfFile};
use std::{path::Path, sync::Arc, time::Duration};
const DEADLINE: Duration = Duration::from_secs(120);
const HEAD_DEADLINE: Duration = Duration::from_secs(10);
const RESOLVE: &str = "https://huggingface.co";
#[derive(Debug)]
pub enum Error {
    Failed,
    Refused,
}
pub struct Planner {
    models: String,
    targets: DownloadTargets,
    remote: DraftHeads,
    head: reqwest::Client,
    permits: Arc<tokio::sync::Semaphore>,
}
fn message(status: u16, text: &str) -> Plan {
    Plan {
        status,
        body: serde_json::json!({"detail":text}).to_string(),
        jobs: Vec::new(),
    }
}
fn text(body: &Value, key: &str) -> Result<String, Error> {
    crate::prompts::py_str(body.get(key).filter(|v| v.truthy())).map_err(|_| Error::Failed)
}
fn name(path: &str) -> String {
    path.split('/')
        .rfind(|p| !p.is_empty() && *p != ".")
        .unwrap_or_default()
        .to_owned()
}
fn stem(path: &str) -> String {
    let b = name(path);
    if b.to_ascii_lowercase().ends_with(".gguf") {
        b.get(..b.len().saturating_sub(5))
            .unwrap_or_default()
            .to_owned()
    } else {
        b
    }
}
fn shard_stem(path: &str) -> String {
    let b = name(path);
    match b.rfind('.') {
        Some(i) if i > 0 && i + 1 < b.len() => b.get(..i).unwrap_or_default().to_owned(),
        _ => b,
    }
}
fn size(value: &model_files::BigInt) -> Result<i64, Error> {
    value.to_string().parse().map_err(|_| Error::Refused)
}
fn compare_size(a: &model_files::BigInt, b: &model_files::BigInt) -> std::cmp::Ordering {
    let a = a.to_string();
    let b = b.to_string();
    let an = a.starts_with('-');
    let bn = b.starts_with('-');
    match (an, bn) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (true, true) => b.len().cmp(&a.len()).then_with(|| b.cmp(&a)),
        (false, false) => a.len().cmp(&b.len()).then_with(|| a.cmp(&b)),
    }
}
fn job(repo: &str, file: &HfFile, dest: String) -> Result<Planned, Error> {
    Ok(Planned {
        repo_id: repo.into(),
        filename: dest,
        url: format!("{RESOLVE}/{repo}/resolve/main/{}", file.path),
        total_bytes: size(&file.size)?,
    })
}
impl Planner {
    pub fn new(
        models: &str,
        ini: &Path,
        data: &Path,
        declared: &str,
    ) -> Result<Self, &'static str> {
        Ok(Self {
            models: models.into(),
            targets: DownloadTargets::new(models, declared, Path::new("/proc/self/mountinfo")),
            remote: DraftHeads::planning(Path::new(models), ini, data)?,
            head: reqwest::Client::builder()
                .connect_timeout(HEAD_DEADLINE)
                .read_timeout(HEAD_DEADLINE)
                .timeout(HEAD_DEADLINE)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| "native download HEAD client unavailable")?,
            permits: Arc::new(tokio::sync::Semaphore::new(2)),
        })
    }
    pub async fn plan(&self, section: Option<String>, body: Value) -> Result<Plan, Error> {
        tokio::time::timeout(DEADLINE, async {
            let _permit = self.permits.acquire().await.map_err(|_| Error::Refused)?;
            let catch_outside = section.is_none()
                && model_autoconfig::pyval::py_strip(&text(&body, "url")?).is_empty();
            let plan = if let Some(section) = section {
                self.draft(&section, &body).await?
            } else {
                self.start(&body).await?
            };
            let models = self.models.clone();
            crate::docker::spawn_blocking_admitted(move || validate(&models, plan, catch_outside))
                .await
                .map_err(|_| Error::Refused)?
        })
        .await
        .map_err(|_| Error::Refused)?
    }
    async fn start(&self, body: &Value) -> Result<Plan, Error> {
        let repo = text(body, "repo")?;
        let path = text(body, "path")?;
        let url = model_autoconfig::pyval::py_strip(&text(body, "url")?).to_owned();
        let target = text(body, "target")?;
        let targets = match self.targets.response().await {
            Targets::Json(v) => {
                serde_json::from_slice::<serde_json::Value>(&v).map_err(|_| Error::Failed)?
            }
            Targets::Failed => return Err(Error::Refused),
        };
        if !targets
            .get("targets")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|t| {
                t.iter()
                    .any(|v| v.get("id").and_then(serde_json::Value::as_str) == Some(&target))
            })
        {
            return Ok(message(400, "Choose one of the offered download locations"));
        }
        let prefix = if target.is_empty() {
            String::new()
        } else {
            format!("{target}/")
        };
        if !url.is_empty() {
            return self.url(body, &url, &prefix).await;
        }
        if repo.is_empty() || !repo.contains('/') {
            return Ok(message(400, "Choose a Hugging Face repository"));
        }
        let files = self
            .remote
            .planning_files(&repo)
            .await
            .map_err(|_| Error::Failed)?;
        repository_plan(&repo, &path, &text(body, "shardBase")?, &prefix, &files)
    }
    async fn draft(&self, section: &str, body: &Value) -> Result<Plan, Error> {
        let source = self
            .remote
            .planning_source(section)
            .await
            .map_err(|e| match e {
                Stop::Failed => Error::Failed,
                Stop::Refused => Error::Refused,
            })?;
        let Some((rel, repo)) = source else {
            return Ok(message(404, "no model folder for this section"));
        };
        let Some(repo) = repo else {
            return Ok(message(409,"this model was not downloaded from Hugging Face here, so its source repository is unknown"));
        };
        let path = text(body, "path")?;
        let files = self
            .remote
            .planning_files(&repo)
            .await
            .map_err(|_| Error::Failed)?;
        let file = files.iter().find(|f| f.path == path);
        let Some(file) = file.filter(|f| {
            model_autoconfig::files::looks_like_draft(&name(&path))
                && !path.to_lowercase().contains("mmproj")
                && f.size.to_string() != "0"
                && f.size
                    .to_string()
                    .parse::<i128>()
                    .is_ok_and(|n| n <= model_autoconfig::files::HEAD_MAX_BYTES)
        }) else {
            return Ok(message(400, "that file is not a draft head for this model"));
        };
        let folder = rel.rsplit_once('/').map(|(f, _)| f).ok_or(Error::Refused)?;
        Ok(Plan {
            status: 200,
            body: serde_json::json!({"queued":[path],"folder":folder}).to_string(),
            jobs: vec![job(&repo, file, format!("{folder}/{}", name(&file.path)))?],
        })
    }
    async fn url(&self, body: &Value, url: &str, prefix: &str) -> Result<Plan, Error> {
        let mut plan = url_plan(body, url, prefix, 0)?;
        if let Some(job) = plan.jobs.first_mut() {
            job.total_bytes = match reqwest::Url::parse(url) {
                Ok(parsed) => self.head_size(parsed).await?,
                Err(_) => 0,
            };
        }
        Ok(plan)
    }
    async fn head_size(&self, mut url: reqwest::Url) -> Result<i64, Error> {
        let result = tokio::time::timeout(HEAD_DEADLINE, async {
            for redirects in 0..=20 {
                let response = match self.head.head(url.clone()).send().await {
                    Ok(v) => v,
                    Err(_) => return Ok(0),
                };
                let status = response.status().as_u16();
                if matches!(status, 301 | 302 | 303 | 307 | 308) {
                    if let Some(location) = response.headers().get("location") {
                        if redirects == 20 {
                            return Ok(0);
                        }
                        let next = location
                            .to_str()
                            .ok()
                            .and_then(|l| url.join(l).ok())
                            .ok_or(Error::Failed)?;
                        if !matches!(next.scheme(), "http" | "https")
                            || crate::downloads::engine::redirect_refused(&url, &next).is_some()
                        {
                            return Err(Error::Refused);
                        }
                        url = next;
                        continue;
                    }
                }
                if status >= 400 {
                    return Ok(0);
                }
                let length = response
                    .headers()
                    .get("content-length")
                    .map(|v| v.to_str().map_err(|_| Error::Failed))
                    .transpose()?
                    .unwrap_or("0");
                return length.trim().parse::<i64>().map_err(|_| Error::Failed);
            }
            Ok(0)
        })
        .await;
        result.unwrap_or(Ok(0))
    }
}
fn url_plan(body: &Value, url: &str, prefix: &str, total: i64) -> Result<Plan, Error> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Ok(message(400, "URL must start with http:// or https://"));
    }
    let explicit = model_autoconfig::pyval::py_strip(&text(body, "filename")?).to_owned();
    let clean = url.replace(['\t', '\r', '\n'], "");
    let rest = clean.split_once("://").map(|(_, r)| r).unwrap_or_default();
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let path = rest
        .get(authority.len()..)
        .unwrap_or_default()
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    let filename = if explicit.is_empty() {
        crate::py_unquote(path.rsplit('/').next().unwrap_or_default())
    } else {
        explicit
    };
    if filename.is_empty() || filename.contains('/') || filename.contains("..") {
        return Ok(message(400, "Set a plain file name for this URL"));
    }
    if imatrix(&filename.to_lowercase()) {
        return Ok(message(
            400,
            "That is an imatrix calibration file, not a model — it cannot be downloaded as one",
        ));
    }
    let authority = if authority.is_empty() {
        "url"
    } else {
        authority
    };
    Ok(Plan {
        status: 200,
        body: serde_json::json!({"queued":[filename]}).to_string(),
        jobs: vec![Planned {
            repo_id: authority.into(),
            filename: format!("{prefix}{}/{filename}", stem(&filename)),
            url: url.into(),
            total_bytes: total,
        }],
    })
}

fn repository_plan(
    repo: &str,
    path: &str,
    shard: &str,
    prefix: &str,
    files: &[HfFile],
) -> Result<Plan, Error> {
    let mut jobs = Vec::new();
    let mut queued = Vec::new();
    let main;
    if !shard.is_empty() {
        let subdir = shard_stem(shard);
        if !files.iter().any(|f| f.shard_base == shard)
            || matches!(subdir.as_str(), "" | "." | "..")
            || subdir.contains(['/', '\\'])
        {
            return Ok(message(400, "That shard set is not in the repository"));
        }
        if imatrix(&shard.to_lowercase()) {
            return Ok(message(
                400,
                "That is an imatrix calibration file, not a model — it cannot be downloaded as one",
            ));
        }
        for f in files.iter().filter(|f| f.shard_base == shard) {
            jobs.push(job(repo, f, format!("{prefix}{subdir}/{}", name(&f.path)))?);
            queued.push(f.path.clone());
        }
        main = subdir;
    } else {
        let Some(f) = files.iter().find(|f| f.path == path) else {
            return Ok(message(404, "That file is not in the repository"));
        };
        let base = name(path);
        if imatrix(&base.to_lowercase()) {
            return Ok(message(
                400,
                "That is an imatrix calibration file, not a model — it cannot be downloaded as one",
            ));
        }
        main = stem(&base);
        jobs.push(job(repo, f, format!("{prefix}{main}/{base}"))?);
        queued.push(path.to_owned());
        if base.to_lowercase().contains("mmproj") {
            return Ok(Plan {
                status: 200,
                body: serde_json::json!({"queued":queued}).to_string(),
                jobs,
            });
        }
    }
    for f in files
        .iter()
        .filter(|f| name(&f.path).to_lowercase().contains("mmproj"))
        .take(4)
    {
        jobs.push(job(repo, f, format!("{prefix}{main}/{}", name(&f.path)))?);
        queued.push(f.path.clone());
    }
    if shard.is_empty() {
        if let Some(f) = files
            .iter()
            .filter(|f| {
                f.path.to_lowercase().ends_with(".gguf")
                    && !name(&f.path).to_lowercase().contains("mmproj")
                    && model_autoconfig::files::looks_like_draft(&name(&f.path))
            })
            .min_by(|a, b| compare_size(&a.size, &b.size))
        {
            jobs.push(job(repo, f, format!("{prefix}{main}/{}", name(&f.path)))?);
            queued.push(f.path.clone());
        }
    }
    Ok(Plan {
        status: 200,
        body: serde_json::json!({"queued":queued}).to_string(),
        jobs,
    })
}
#[cfg(test)]
mod tests;

fn validate(models: &str, plan: Plan, catch_outside: bool) -> Result<Plan, Error> {
    if plan.jobs.len() > crate::downloads::PLAN_FILES_CAP {
        return Err(Error::Refused);
    }
    let mut bytes = plan.body.len().saturating_add(64);
    if bytes > crate::downloads::PLAN_CAP {
        return Err(Error::Refused);
    }
    for p in &plan.jobs {
        for s in [&p.repo_id, &p.filename, &p.url] {
            bytes =
                bytes.saturating_add(serde_json::to_string(s).map_err(|_| Error::Failed)?.len());
        }
        bytes = bytes.saturating_add(128);
        if bytes > crate::downloads::PLAN_CAP {
            return Err(Error::Refused);
        }
    }
    if !plan.jobs.iter().all(|p| {
        matches!(
            crate::downloads::pyfmt::safe_dest(models, &p.filename),
            crate::downloads::pyfmt::Dest::Inside(_)
        )
    }) {
        return if catch_outside {
            Ok(message(
                400,
                "That download would land outside the models folder",
            ))
        } else {
            Err(Error::Failed)
        };
    }
    Ok(plan)
}
