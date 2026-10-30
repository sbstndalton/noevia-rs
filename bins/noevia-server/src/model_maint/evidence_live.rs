//! The evidence routes and what feeds them (core `llamacpp-manager.cjs` `computeIdentity`,
//! `evidence`, `recordEvidence`, `importEvidence`, `routes/models.cjs`, `model-evidence-import.cjs`):
//! `GET /api/models/evidence?model=` and `POST /api/models/evidence/import`, the live identity of a
//! model's current configuration, the Hugging Face model-card import, and the inbox through which
//! Node's producers hand their records to the one writer.
//!
//! Kept on Node, with the reason: `POST /api/models/evidence/recheck` runs an image probe that may
//! load the model, after the in-process admission check (the inference budget, the memory-watchdog
//! quarantine, the probe cache). It writes its result through the inbox like every other Node producer.

use super::evidence as ev;
use super::Maint;
use js_json::{js_len, js_slice, number_to_string, parse, stringify, to_js_string, JValue};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// llamacpp-manager.cjs: the cached identity of a model is good for 30 s (only a frequent reported
/// rate may use it).
const IDENTITY_TTL: Duration = Duration::from_secs(30);
const IDENTITY_CACHE: usize = 64;
/// A model name the routes accept.
const MAX_MODEL: usize = 200;

/// The live identity of a model's configuration: the object and its hash.
#[derive(Debug, Clone)]
pub struct Live {
    pub identity: JValue,
    pub hash: String,
}

/// `errorResponse(err)` for a failure the route does not handle: a 4xx keeps its message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub status: u16,
    pub message: String,
}

impl Failure {
    fn internal() -> Failure {
        Failure {
            status: 500,
            message: "Internal error".into(),
        }
    }
}

impl From<crate::model_sizing::Thrown> for Failure {
    fn from(t: crate::model_sizing::Thrown) -> Failure {
        match t.status {
            Some(s) if (400..500).contains(&s) => Failure {
                status: s,
                message: t.message,
            },
            _ => Failure::internal(),
        }
    }
}

fn str_of(v: &JValue) -> Option<&str> {
    v.as_str().filter(|s| !s.is_empty())
}

fn coalesce(v: &JValue) -> JValue {
    match v {
        JValue::Undefined | JValue::Null => JValue::Null,
        other => other.clone(),
    }
}

/// `x || []` for a limitations field.
fn or_empty_list(v: &JValue) -> JValue {
    if v.truthy() {
        v.clone()
    } else {
        JValue::Arr(Vec::new())
    }
}

impl Maint {
    /// `computeIdentity(model)`: `Ok(None)` when the model's file cannot be read (evidence is then
    /// `unavailable`); `Err` where the JS throws (an unreadable models.ini).
    pub async fn compute_identity(&self, model: &str) -> Result<Option<Live>, Failure> {
        let Some(router) = self.router.as_ref() else {
            return Ok(None);
        };
        // modelFiles: the router's effective argv names the files, even for a cached download.
        let mut args: Vec<String> = Vec::new();
        if let Ok(listing) = router.models(None).await {
            if let Some(rows) = super::router::rows(&listing.body) {
                if let Some(a) = rows
                    .iter()
                    .find(|m| m.get("id").and_then(|i| i.as_str()) == Some(model))
                    .and_then(|m| m.get("status"))
                    .and_then(|s| s.get("args"))
                    .and_then(|a| a.as_array())
                {
                    args = a
                        .iter()
                        .map(|x| x.as_str().unwrap_or("\u{0}").to_owned())
                        .collect();
                }
            }
        }
        let flag = |names: &[&str]| -> Option<String> {
            let i = args.iter().position(|a| names.contains(&a.as_str()))?;
            args.get(i + 1)
                .filter(|v| !v.is_empty() && v.as_str() != "\u{0}")
                .cloned()
        };
        let presets = match self.ini.clone() {
            Some(store) => {
                let m = model.to_owned();
                let read = tokio::task::spawn_blocking(move || {
                    store.read().map(|p| {
                        let (fm, fp) = p.files(&m);
                        let options: Vec<(String, String)> = p
                            .current(&m)
                            .into_iter()
                            .map(|(k, v)| (k.to_owned(), v))
                            .collect();
                        (fm, fp, options, p.draft_file(&m))
                    })
                })
                .await
                .map_err(|_| Failure::internal())?;
                Some(read.map_err(Failure::from)?)
            }
            None => None,
        };
        let (preset_model, preset_mmproj, options, draft_file) =
            presets.unwrap_or((None, None, Vec::new(), None));
        let model_file = flag(&["--model", "-m"]).or(preset_model);
        let mmproj_file = flag(&["--mmproj", "-mm"]).or(preset_mmproj);
        let sizing = std::sync::Arc::clone(&self.sizing);
        let (mf, mm) = (model_file.clone(), mmproj_file.clone());
        let fingerprints = tokio::task::spawn_blocking(move || {
            let fp = |c: &Option<String>| {
                c.as_deref()
                    .and_then(|c| sizing.local_file_path(c))
                    .and_then(|(p, _)| ev::file_fingerprint(&p))
            };
            (fp(&mf), mm.as_ref().map(|_| fp(&mm)), fp(&draft_file))
        })
        .await
        .map_err(|_| Failure::internal())?;
        let Some(artifact) = fingerprints.0 else {
            return Ok(None);
        };
        let draft = fingerprints.2.map_or(JValue::Null, JValue::Str);
        let projector = match fingerprints.1 {
            Some(Some(p)) => JValue::Str(p),
            _ => JValue::Null,
        };
        let build = match router.get("/props", None, Duration::from_secs(8)).await {
            // `props?.body?.build_info || null`
            Ok(r) => r
                .body
                .as_ref()
                .and_then(|b| b.get("build_info"))
                .and_then(to_jvalue)
                .filter(JValue::truthy)
                .unwrap_or(JValue::Null),
            Err(_) => JValue::Null,
        };
        let opt = |k: &str| {
            options
                .iter()
                .find(|(ok, _)| ok == k)
                .map(|(_, v)| v.as_str())
                .filter(|v| !v.is_empty())
        };
        let text = |v: Option<&str>| v.map_or(JValue::Null, |v| JValue::Str(v.into()));
        let identity = JValue::obj([
            ("backend", JValue::Str("llamacpp".into())),
            ("build", build),
            (
                "endpoint",
                JValue::Str(
                    ev::identity_hash(&JValue::Str(router.base().into()))
                        .chars()
                        .take(16)
                        .collect(),
                ),
            ),
            ("model", JValue::Str(model.into())),
            ("artifact", JValue::Str(artifact)),
            ("projector", projector),
            ("preset", JValue::Str(ev::preset_hash(&options))),
            (
                "context",
                JValue::obj([
                    ("ctx", text(opt("ctx-size"))),
                    ("parallel", text(opt("parallel"))),
                    ("cacheK", text(opt("cache-type-k"))),
                    ("cacheV", text(opt("cache-type-v"))),
                ]),
            ),
            (
                "mtp",
                match opt("spec-type") {
                    Some(t) => JValue::obj([("type", JValue::Str(t.into())), ("draft", draft)]),
                    None => JValue::Null,
                },
            ),
        ]);
        let hash = ev::identity_hash(&identity);
        Ok(Some(Live { identity, hash }))
    }

    fn cache_identity(&self, model: &str, live: Option<&Live>) {
        if let Ok(mut cache) = self.identities.lock() {
            if cache.len() >= IDENTITY_CACHE && !cache.iter().any(|(k, _, _)| k == model) {
                cache.remove(0);
            }
            cache.retain(|(k, _, _)| k != model);
            cache.push((model.to_owned(), Instant::now(), live.cloned()));
        }
    }

    /// The identity of `model`, from the 30 s cache when it has one (only a frequent reported rate
    /// may use it); a fresh read otherwise.
    async fn identity_for_record(
        &self,
        model: &str,
        result: &str,
    ) -> Result<Option<Live>, Failure> {
        if result == "reported" {
            let hit = self.identities.lock().ok().and_then(|c| {
                c.iter()
                    .find(|(k, at, _)| k == model && at.elapsed() < IDENTITY_TTL)
                    .map(|(_, _, v)| v.clone())
            });
            if let Some(hit) = hit {
                return Ok(hit);
            }
            let live = self.compute_identity(model).await?;
            self.cache_identity(model, live.as_ref());
            return Ok(live);
        }
        self.compute_identity(model).await
    }

    /// `recordEvidence(model, record)`: files `record` under the model's live configuration.
    /// `Ok(None)` when there is no store or no identity.
    pub async fn record_evidence(
        &self,
        model: &str,
        record: &JValue,
    ) -> Result<Option<JValue>, Failure> {
        if self.evidence.is_none() {
            return Ok(None);
        }
        let result = record.get("result").as_str().unwrap_or("").to_owned();
        let Some(live) = self.identity_for_record(model, &result).await? else {
            return Ok(None);
        };
        self.file_record(model, record, &live, crate::identity::now_ms())
            .await
    }

    /// Files `record` under `live`, stamped `at`.
    async fn file_record(
        &self,
        model: &str,
        record: &JValue,
        live: &Live,
        at: i64,
    ) -> Result<Option<JValue>, Failure> {
        let Some(store) = self.evidence.clone() else {
            return Ok(None);
        };
        let result = record.get("result").as_str().unwrap_or("").to_owned();
        let mut entry: Vec<(String, JValue)> = vec![
            ("model".into(), JValue::Str(model.into())),
            ("identityHash".into(), JValue::Str(live.hash.clone())),
            ("identity".into(), live.identity.clone()),
        ];
        if let JValue::Obj(items) = record {
            for (k, v) in items {
                match entry.iter_mut().find(|(ek, _)| ek == k) {
                    Some(slot) => slot.1 = v.clone(),
                    None => entry.push((k.clone(), v.clone())),
                }
            }
        }
        let entry = JValue::Obj(entry);
        let rate = result == "reported"
            && record
                .get("value")
                .get("rate")
                .as_num()
                .is_some_and(f64::is_finite);
        tokio::task::spawn_blocking(move || {
            if rate {
                store.append_reported_rate(&entry, at)
            } else {
                store.append_if_changed(&entry, at)
            }
        })
        .await
        .map_err(|_| Failure::internal())?
        .map(Some)
        .map_err(|e| match e {
            // The record itself is unusable; trying again cannot help.
            ev::StoreError::Io => Failure::internal(),
            _ => Failure {
                status: 400,
                message: "Invalid evidence record".into(),
            },
        })
    }

    /// `evidence(model)`: the answer of `GET /api/models/evidence`, with Node's key order.
    pub async fn evidence_body(&self, model: &str) -> Result<String, Failure> {
        let live = if self.evidence.is_some() {
            self.compute_identity(model).await?
        } else {
            None
        };
        if self.evidence.is_some() {
            self.cache_identity(model, live.as_ref());
        }
        let store = self.evidence.clone();
        let records = match store {
            Some(s) => tokio::task::spawn_blocking(move || s.list())
                .await
                .map_err(|_| Failure::internal())?,
            None => Vec::new(),
        };
        let live_hash = live.as_ref().map(|l| l.hash.as_str());
        let artifact_hash = live.as_ref().and_then(|l| {
            l.identity
                .get("artifact")
                .as_str()
                .map(ev::artifact_identity_hash)
        });
        let external = ev::derive_external(&records, model, artifact_hash.as_deref());
        let sampling = ev::derive_sampling(&records, model, artifact_hash.as_deref());
        let current = (sampling.state == "reported")
            .then(|| sampling.record.clone())
            .flatten();
        let source_values = current
            .as_ref()
            .map_or(JValue::Null, |r| coalesce(r.get("value")));
        let source_provenance = current
            .as_ref()
            .map_or(JValue::Null, |r| coalesce(r.get("provenance")));
        let plan = ev::sampling_plan(model, &source_values, &source_provenance);
        let categories: Vec<JValue> = ev::CATEGORIES
            .iter()
            .map(|category| {
                let d = ev::derive(&records, model, category, live_hash);
                let r = d.record.as_ref();
                let g = |k: &str| r.map_or(JValue::Null, |r| coalesce(r.get(k)));
                let lims = r.map_or(JValue::Arr(Vec::new()), |r| {
                    or_empty_list(r.get("limitations"))
                });
                JValue::obj([
                    ("category", JValue::Str((*category).into())),
                    ("state", JValue::Str(d.state.into())),
                    ("value", g("value")),
                    ("result", g("result")),
                    ("at", g("at")),
                    ("suite", g("suite")),
                    ("limitations", lims.clone()),
                    ("limitationKeys", ev::limitation_keys(&lims)),
                ])
            })
            .collect();
        let ext = external.record.as_ref();
        let eg = |k: &str| ext.map_or(JValue::Null, |r| coalesce(r.get(k)));
        let ext_lims = ext.map_or(JValue::Arr(Vec::new()), |r| {
            or_empty_list(r.get("limitations"))
        });
        let samp = sampling.record.as_ref();
        let samp_lims = samp.map_or(JValue::Arr(Vec::new()), |r| {
            or_empty_list(r.get("limitations"))
        });
        let body = JValue::obj([
            ("model", JValue::Str(model.into())),
            ("tracked", JValue::Bool(self.evidence.is_some())),
            (
                "identityHash",
                live.as_ref()
                    .map_or(JValue::Null, |l| JValue::Str(l.hash.clone())),
            ),
            ("categories", JValue::Arr(categories)),
            (
                "external",
                JValue::obj([
                    ("category", JValue::Str("external_model_card".into())),
                    ("state", JValue::Str(external.state.into())),
                    ("value", eg("value")),
                    ("at", eg("at")),
                    ("suite", eg("suite")),
                    ("provenance", eg("provenance")),
                    ("limitations", ext_lims.clone()),
                    ("limitationKeys", ev::limitation_keys(&ext_lims)),
                ]),
            ),
            (
                "samplingRecommendation",
                JValue::obj([
                    ("state", JValue::Str(sampling.state.into())),
                    (
                        "values",
                        if sampling.state == "reported" {
                            samp.map_or(JValue::Null, |r| coalesce(r.get("value")))
                        } else {
                            JValue::Null
                        },
                    ),
                    (
                        "source",
                        if samp.is_some() {
                            JValue::Str("generation_config.json".into())
                        } else {
                            JValue::Null
                        },
                    ),
                    (
                        "provenance",
                        samp.map_or(JValue::Null, |r| coalesce(r.get("provenance"))),
                    ),
                    ("limitations", samp_lims),
                ]),
            ),
            ("samplingPlan", plan_view(&plan)),
        ]);
        stringify(&body).ok_or_else(Failure::internal)
    }

    /// The model-card source's base URL, no trailing slash.
    fn card_url(&self, repo: &str) -> String {
        format!("{}/api/models/{repo}", self.card_base)
    }

    /// `importEvidence(model, { checkpoint })`: best-effort; failures resolve, never throw.
    pub async fn import_evidence(
        &self,
        model: &str,
        checkpoint: Option<&str>,
    ) -> Result<(), Failure> {
        let Some(store) = self.evidence.clone() else {
            return Ok(());
        };
        let Some(live) = self.compute_identity(model).await? else {
            return Ok(());
        };
        let Some(artifact) = live.identity.get("artifact").as_str().map(str::to_owned) else {
            return Ok(());
        };
        let resolved = match checkpoint {
            None => model.to_owned(),
            Some(c) => match (repo_from_checkpoint(model), repo_from_checkpoint(c)) {
                (Some(a), Some(b)) if a == b => c.to_owned(),
                _ => return Ok(()),
            },
        };
        let Some(repo) = repo_from_checkpoint(&resolved) else {
            return Ok(());
        };
        let now = crate::identity::now_ms();
        let url = self.card_url(&repo);
        if !self.card_url_allowed(&url) {
            return Ok(());
        }
        let Some(body) = self.fetch_source(&url, 1024 * 1024).await.filter(|r| r.ok) else {
            return Ok(());
        };
        let body = body.body;
        if body.get("private").truthy() {
            return Ok(());
        }
        let Some(value) = normalize_card(&repo, &body) else {
            return Ok(());
        };
        let artifact_hash = ev::artifact_identity_hash(&artifact);
        let card = JValue::obj([
            ("model", JValue::Str(model.into())),
            ("category", JValue::Str("external_model_card".into())),
            ("result", JValue::Str("reported".into())),
            ("identityHash", JValue::Str(artifact_hash.clone())),
            ("value", value.clone()),
            ("suite", JValue::obj([("name", "huggingface-model-card".into()), ("version", JValue::Num(1.0))])),
            ("source", JValue::Str("external".into())),
            ("provenance", JValue::obj([("sourceUrl", JValue::Str(url.clone())), ("retrievedAt", JValue::Num(now as f64))])),
            ("limitations", JValue::Arr(vec!["unverified, from source: published by the model repository, not measured locally".into()])),
        ]);
        let revision = coalesce(value.get("revision"));
        let sampling = self
            .fetch_sampling(model, &repo, &revision, &artifact_hash, now)
            .await;
        let model_owned = model.to_owned();
        let at = now;
        tokio::task::spawn_blocking(move || -> Result<(), ev::StoreError> {
            let list = store.list();
            let same = |r: &&JValue| r.get("model").as_str() == Some(model_owned.as_str());
            let newest = list.iter().filter(same).rev().find(|r| {
                r.get("category").as_str() == Some("external_model_card")
                    && r.get("identityHash").as_str() == Some(artifact_hash.as_str())
            });
            // Compared without `revision`: a repository that re-pushes the same card under a new commit
            // must not append a record every time it is refetched.
            let key = |v: &JValue| match v {
                JValue::Obj(items) => stringify(&ev::sorted(&JValue::Obj(
                    items
                        .iter()
                        .filter(|(k, _)| k != "revision")
                        .cloned()
                        .collect(),
                ))),
                _ => None,
            };
            if !newest.is_some_and(|n| key(n.get("value")) == key(card.get("value"))) {
                store.append(&card, at)?;
            }
            match sampling {
                Sampling::Config(record) => {
                    let list = store.list();
                    let latest = list.iter().rev().find(|r| {
                        r.get("model").as_str() == Some(model_owned.as_str())
                            && r.get("category").as_str() == Some("external_sampling_config")
                            && r.get("identityHash").as_str() == Some(artifact_hash.as_str())
                    });
                    let same_value = latest.is_some_and(|l| {
                        stringify(&ev::sorted(l.get("value")))
                            == stringify(&ev::sorted(record.get("value")))
                    });
                    if !same_value {
                        store.append(&record, at)?;
                    }
                }
                Sampling::Retire(record) => {
                    // A definite removal must not leave a previously imported value looking current.
                    let old = list.iter().rev().find(|r| {
                        r.get("model").as_str() == Some(model_owned.as_str())
                            && r.get("category").as_str() == Some("external_sampling_config")
                            && r.get("identityHash").as_str() == Some(artifact_hash.as_str())
                    });
                    if old.is_some_and(|o| !matches!(o.get("value"), JValue::Null)) {
                        store.append(&record, at)?;
                    }
                }
                Sampling::Keep => {}
            }
            Ok(())
        })
        .await
        .map_err(|_| Failure::internal())?
        .map_err(|_| Failure::internal())
    }

    async fn fetch_sampling(
        &self,
        model: &str,
        repo: &str,
        revision: &JValue,
        artifact_hash: &str,
        now: i64,
    ) -> Sampling {
        let Some(url) = generation_config_url(&self.card_base, repo, revision) else {
            return Sampling::Keep;
        };
        let Some(reply) = self.fetch_source(&url, 64 * 1024).await else {
            return Sampling::Keep;
        };
        let rev = revision.clone();
        let retire = |url: &str| {
            Sampling::Retire(JValue::obj([
                ("model", JValue::Str(model.into())),
                ("category", JValue::Str("external_sampling_config".into())),
                ("result", JValue::Str("reported".into())),
                ("identityHash", JValue::Str(artifact_hash.into())),
                ("value", JValue::Null),
                ("source", JValue::Str("external".into())),
                ("suite", JValue::obj([("name", "huggingface-generation-config".into()), ("version", JValue::Num(1.0))])),
                ("provenance", JValue::obj([("sourceUrl", JValue::Str(url.into())), ("retrievedAt", JValue::Num(now as f64)), ("revision", rev.clone()), ("artifactIdentityHash", JValue::Str(artifact_hash.into()))])),
                ("limitations", JValue::Arr(vec!["Current source config has no supported sampling recommendation; previous source values were retired".into()])),
            ]))
        };
        if !reply.ok {
            return if reply.status == 404 {
                retire(&url)
            } else {
                Sampling::Keep
            };
        }
        let Some(values) = normalize_generation_config(&reply.body) else {
            return retire(&url);
        };
        Sampling::Config(JValue::obj([
            ("model", JValue::Str(model.into())),
            ("category", JValue::Str("external_sampling_config".into())),
            ("result", JValue::Str("reported".into())),
            ("identityHash", JValue::Str(artifact_hash.into())),
            ("value", values),
            ("suite", JValue::obj([("name", "huggingface-generation-config".into()), ("version", JValue::Num(1.0))])),
            ("source", JValue::Str("external".into())),
            ("provenance", JValue::obj([("sourceUrl", JValue::Str(url)), ("retrievedAt", JValue::Num(now as f64)), ("revision", rev), ("artifactIdentityHash", JValue::Str(artifact_hash.into()))])),
            ("limitations", JValue::Arr(vec![
                "Source-reported generation config, not measured locally or applied to the model".into(),
                "Values may describe the source weights rather than this GGUF conversion or runtime".into(),
            ])),
        ]))
    }

    /// `host allowed`: the model-info endpoint of the fixed source (https huggingface.co), or the QA source.
    fn card_url_allowed(&self, url: &str) -> bool {
        url.starts_with(&format!("{}/api/models/", self.card_base))
    }

    async fn fetch_source(&self, url: &str, cap: usize) -> Option<Source> {
        let res = self
            .source_client
            .get(url)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .ok()?;
        let status = res.status().as_u16();
        let ok = res.status().is_success();
        let mut res = res;
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = res.chunk().await.ok()? {
            if bytes.len() + chunk.len() > cap {
                return Some(Source {
                    ok: false,
                    status,
                    body: JValue::Undefined,
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let body = parse(&text).unwrap_or(JValue::Str(text));
        Some(Source { ok, status, body })
    }
}

struct Source {
    ok: bool,
    status: u16,
    body: JValue,
}

enum Sampling {
    Config(JValue),
    Retire(JValue),
    Keep,
}

/// A router JSON value as a `JValue` (the build identity is stored as it came).
fn to_jvalue(v: &serde_json::Value) -> Option<JValue> {
    parse(&v.to_string()).ok()
}

/// The recommendation as `samplingPlan` shows it.
fn plan_view(plan: &JValue) -> JValue {
    JValue::obj([
        ("tier", plan.get("tier").clone()),
        ("source", plan.get("source").clone()),
        ("sourceId", plan.get("sourceId").clone()),
        ("values", plan.get("values").clone()),
        ("family", coalesce(plan.get("familyId"))),
        ("familyLabel", coalesce(plan.get("familyLabel"))),
        ("presetId", coalesce(plan.get("presetId"))),
        ("quirks", plan.get("quirks").clone()),
        ("note", plan.get("note").clone()),
        ("noteId", plan.get("noteId").clone()),
        ("provenance", plan.get("provenance").clone()),
    ])
}

// ── model-evidence-import.cjs ───────────────────────────────────────────────────────────────

/// `repoFromCheckpoint`: `org/repo` of `org/repo[:quant]`, none for a dot-only segment.
pub fn repo_from_checkpoint(checkpoint: &str) -> Option<String> {
    let word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    let (repo, quant) = match checkpoint.split_once(':') {
        Some((r, q)) => (r, Some(q)),
        None => (checkpoint, None),
    };
    let (org, name) = repo.split_once('/')?;
    let seg = |s: &str| !s.is_empty() && s.chars().all(word);
    if !seg(org) || !seg(name) || quant.is_some_and(|q| !seg(q)) {
        return None;
    }
    if [org, name].iter().any(|s| s.chars().all(|c| c == '.')) {
        return None;
    }
    Some(repo.to_owned())
}

/// `sanitizeText(value, maxLen)`: markup and control characters stripped, whitespace collapsed,
/// capped in UTF-16 units. `None` for nothing left.
pub fn sanitize_text(value: &JValue, max: usize) -> Option<String> {
    if value.is_nullish() {
        return None;
    }
    let raw = to_js_string(value).unwrap_or_else(|_| "[object Object]".into());
    // /<[^>]*>/g -> ' '
    let mut stripped = String::new();
    let mut rest = raw.as_str();
    while let Some(i) = rest.find('<') {
        match rest.get(i..).and_then(|t| t.find('>')) {
            Some(j) => {
                stripped.push_str(rest.get(..i).unwrap_or(""));
                stripped.push(' ');
                rest = rest.get(i + j + 1..).unwrap_or("");
            }
            None => break,
        }
    }
    stripped.push_str(rest);
    let controls: String = stripped
        .chars()
        .filter(|c| !matches!(*c as u32, 0..=8 | 0x0b | 0x0c | 0x0e..=0x1f))
        .collect();
    let is_space = |c: char| u16::try_from(c as u32).is_ok_and(server_stats::js::is_space);
    let mut collapsed = String::new();
    let mut in_space = false;
    for c in controls.chars() {
        if is_space(c) {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(c);
            in_space = false;
        }
    }
    let text = collapsed
        .trim_matches(|c: char| is_space(c) || c == '\u{feff}')
        .to_owned();
    if text.is_empty() {
        return None;
    }
    Some(if js_len(&text) > max {
        format!("{}\u{2026}", js_slice(&text, max))
    } else {
        text
    })
}

fn clean_text(v: &JValue, max: usize) -> JValue {
    sanitize_text(v, max).map_or(JValue::Null, JValue::Str)
}

/// `a || b` over JS values.
fn or(a: &JValue, b: &JValue) -> JValue {
    if a.truthy() {
        a.clone()
    } else {
        b.clone()
    }
}

fn hex_between(s: &str, lo: usize, hi: usize) -> bool {
    (lo..=hi).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// `normalizeCard(repo, body)`.
pub fn normalize_card(repo: &str, body: &JValue) -> Option<JValue> {
    if !body.is_object() {
        return None;
    }
    let card_data = if body.get("cardData").is_object() {
        body.get("cardData").clone()
    } else {
        JValue::Obj(Vec::new())
    };
    let sha = match body.get("sha") {
        JValue::Str(s) if hex_between(s, 6, 64) => JValue::Str(s.clone()),
        JValue::Num(_) | JValue::Bool(_) => {
            let s = to_js_string(body.get("sha")).unwrap_or_default();
            if hex_between(&s, 6, 64) {
                body.get("sha").clone()
            } else {
                JValue::Null
            }
        }
        _ => JValue::Null,
    };
    let license = clean_text(&or(card_data.get("license"), body.get("license")), 100);
    let tags: Vec<JValue> = match body.get("tags") {
        JValue::Arr(items) => items
            .iter()
            .filter(|t| t.as_str().is_some())
            .take(8)
            .map(|t| clean_text(t, 60))
            .collect(),
        _ => Vec::new(),
    };
    let mut claims: Vec<JValue> = Vec::new();
    if let JValue::Arr(index) = card_data.get("model-index") {
        for entry in index.iter().take(3) {
            let results: &[JValue] = match entry.get("results") {
                JValue::Arr(r) => r,
                _ => &[],
            };
            for result in results.iter().take(8) {
                if claims.len() >= 8 {
                    break;
                }
                let metrics: &[JValue] = match result.get("metrics") {
                    JValue::Arr(m) => m,
                    _ => &[],
                };
                for metric in metrics.iter().take(3) {
                    let value = match metric.get("value") {
                        JValue::Num(n) => clean_text(&JValue::Str(number_to_string(*n)), 40),
                        JValue::Str(s) => clean_text(&JValue::Str(s.clone()), 40),
                        _ => JValue::Null,
                    };
                    claims.push(JValue::obj([
                        (
                            "task",
                            clean_text(
                                &or(
                                    result.get("task").get("type"),
                                    result.get("task").get("name"),
                                ),
                                120,
                            ),
                        ),
                        (
                            "dataset",
                            clean_text(result.get("dataset").get("name"), 120),
                        ),
                        (
                            "metric",
                            clean_text(&or(metric.get("type"), metric.get("name")), 60),
                        ),
                        ("value", value),
                    ]));
                }
            }
        }
    }
    Some(JValue::obj([
        ("repo", JValue::Str(repo.into())),
        ("revision", sha),
        ("license", license),
        ("pipelineTag", clean_text(body.get("pipeline_tag"), 80)),
        ("libraryName", clean_text(body.get("library_name"), 80)),
        ("tags", JValue::Arr(tags)),
        ("evaluationClaims", JValue::Arr(claims)),
        (
            "cardExcerpt",
            clean_text(
                &or(
                    &or(card_data.get("model_summary"), card_data.get("summary")),
                    body.get("description"),
                ),
                1200,
            ),
        ),
    ]))
}

/// `generationConfigUrl(repo, revision)`: only from a full, immutable commit.
pub fn generation_config_url(base: &str, repo: &str, revision: &JValue) -> Option<String> {
    let rev = revision.as_str()?;
    if repo_from_checkpoint(repo).as_deref() != Some(repo)
        || rev.len() != 40
        || !rev.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    Some(format!(
        "{base}/api/resolve-cache/models/{repo}/{rev}/generation_config.json"
    ))
}

/// `normalizeGenerationConfig(body)`: structured source values only.
pub fn normalize_generation_config(body: &JValue) -> Option<JValue> {
    let JValue::Obj(items) = body else {
        return None;
    };
    let has = |k: &str| items.iter().any(|(ik, _)| ik == k);
    if body.get("do_sample") == &JValue::Bool(false)
        || (has("do_sample") && body.get("do_sample") != &JValue::Bool(true))
        || (has("num_beams") && body.get("num_beams").as_num() != Some(1.0))
    {
        return None;
    }
    let valid = |key: &str, v: &JValue| {
        let Some(n) = v.as_num().filter(|n| n.is_finite()) else {
            return false;
        };
        match key {
            "temperature" => (0.0..=2.0).contains(&n),
            "top_p" => n > 0.0 && n <= 1.0,
            "top_k" => n.fract() == 0.0 && n >= 0.0,
            "repeat_penalty" => (0.0..=3.0).contains(&n),
            _ => false,
        }
    };
    let mut values: Vec<(String, JValue)> = Vec::new();
    for key in ["temperature", "top_p", "top_k", "min_p"] {
        let v = body.get(key);
        let ok = if key == "min_p" {
            v.as_num()
                .is_some_and(|n| n.is_finite() && (0.0..=1.0).contains(&n))
        } else {
            valid(key, v) && (key != "top_k" || v.as_num().is_some_and(|n| n <= 100_000.0))
        };
        if ok {
            values.push((key.into(), v.clone()));
        }
    }
    if valid("repeat_penalty", body.get("repetition_penalty")) {
        values.push((
            "repeat_penalty".into(),
            body.get("repetition_penalty").clone(),
        ));
    }
    (!values.is_empty()).then_some(JValue::Obj(values))
}

// ── The inbox: how Node's producers reach the one writer ─────────────────────────────────────

/// The most inbox entries read in one pass.
const DRAIN_BATCH: usize = 50;
const MAX_ENTRY_BYTES: u64 = 64 * 1024;

/// How many times an entry whose apply failed is tried, the backoff doubling from [`RETRY_BASE`]. A
/// failed entry waits its turn without holding up the entries behind it.
const MAX_ATTEMPTS: u32 = 5;
const RETRY_BASE: Duration = Duration::from_secs(2);
/// A `.tmp` this old was left by a Node that died while writing it.
const ORPHAN_AGE: Duration = Duration::from_secs(60 * 60);

/// What the drain remembers about entries still in the inbox.
#[derive(Debug, Default)]
pub struct DrainState {
    entries: std::collections::HashMap<String, Tried>,
    retry_base: Option<Duration>,
}

#[derive(Debug)]
struct Tried {
    attempts: u32,
    next: Instant,
    /// Finished with; only its removal failed, so it is never applied again.
    finished: bool,
}

/// What became of one entry.
enum Applied {
    /// Filed, or nothing to do: remove it.
    Done,
    /// Unusable for good: remove it, saying why.
    Discard(&'static str),
    /// May work later.
    Retry(&'static str),
}

impl Maint {
    /// Applies the entries Node's producers left in `evidence-inbox-<hash>/`, oldest first. Returns how
    /// many were taken (removed, or finished with). A failed apply stays for a bounded number of tries.
    pub async fn drain_inbox(&self) -> usize {
        let Some(dir) = self.inbox_dir.clone() else {
            return 0;
        };
        let names = tokio::task::spawn_blocking({
            let dir = dir.clone();
            move || {
                sweep_orphans(&dir, ORPHAN_AGE);
                inbox_names(&dir)
            }
        })
        .await
        .unwrap_or_default();
        if let Ok(mut state) = self.drain.lock() {
            // Forget what is no longer there.
            state.entries.retain(|n, _| names.contains(n));
        }
        let mut taken = 0;
        for name in names.into_iter().take(DRAIN_BATCH) {
            let path = dir.join(&name);
            let (finished, due) = self
                .drain
                .lock()
                .ok()
                .and_then(|s| {
                    s.entries
                        .get(&name)
                        .map(|t| (t.finished, Instant::now() >= t.next))
                })
                .unwrap_or((false, true));
            if !finished {
                if !due {
                    continue;
                }
                match self.apply_entry(&path).await {
                    Applied::Done => {}
                    Applied::Discard(why) => {
                        eprintln!("[evidence] an inbox entry was discarded: {why}");
                    }
                    Applied::Retry(why) => {
                        if self.note_failure(&name, why) {
                            continue;
                        }
                    }
                }
            }
            taken += 1;
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {
                    if let Ok(mut state) = self.drain.lock() {
                        state.entries.remove(&name);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    if !finished {
                        eprintln!(
                            "[evidence] an applied inbox entry could not be removed; it will not be applied again: {e}"
                        );
                    }
                    if let Ok(mut state) = self.drain.lock() {
                        state.entries.insert(
                            name,
                            Tried {
                                attempts: 0,
                                next: Instant::now(),
                                finished: true,
                            },
                        );
                    }
                }
            }
        }
        taken
    }

    /// True while the entry may be tried again; false when it is out of tries (logged).
    fn note_failure(&self, name: &str, why: &str) -> bool {
        let Ok(mut state) = self.drain.lock() else {
            return false;
        };
        let base = state.retry_base.unwrap_or(RETRY_BASE);
        let tried = state.entries.entry(name.to_owned()).or_insert(Tried {
            attempts: 0,
            next: Instant::now(),
            finished: false,
        });
        tried.attempts += 1;
        if tried.attempts >= MAX_ATTEMPTS {
            eprintln!("[evidence] an inbox entry was discarded after {MAX_ATTEMPTS} tries: {why}");
            return false;
        }
        tried.next = Instant::now() + base.saturating_mul(1u32 << (tried.attempts - 1));
        true
    }

    /// The first retry's wait (doubling after it); tests shorten it.
    pub fn set_retry_base(&self, base: Duration) {
        if let Ok(mut state) = self.drain.lock() {
            state.retry_base = Some(base);
        }
    }

    async fn apply_entry(&self, path: &Path) -> Applied {
        let text = tokio::task::spawn_blocking({
            let path = path.to_path_buf();
            move || read_entry(&path)
        })
        .await
        .ok()
        .flatten();
        let Some(entry) = text.and_then(|t| parse(&t).ok()) else {
            return Applied::Discard("unreadable");
        };
        let Some(model) = entry
            .get("model")
            .as_str()
            .filter(|m| !m.is_empty() && js_len(m) <= MAX_MODEL)
            .map(str::to_owned)
        else {
            return Applied::Discard("no usable model");
        };
        match entry.get("kind").as_str() {
            Some("record") => self.apply_record(&model, &entry).await,
            Some("import") => match self
                .import_evidence(&model, entry.get("checkpoint").as_str())
                .await
            {
                Ok(()) => Applied::Done,
                Err(_) => Applied::Retry("the import failed"),
            },
            _ => Applied::Discard("unknown kind"),
        }
    }

    async fn apply_record(&self, model: &str, entry: &JValue) -> Applied {
        let record = entry.get("record");
        let filed = |r: Result<Option<JValue>, Failure>| match r {
            Ok(_) => Applied::Done,
            Err(f) if f.status == 400 => Applied::Discard("an invalid record"),
            Err(_) => Applied::Retry("the store or the router failed"),
        };
        // A frequent rate is filed under the identity live now, as it always was.
        if record.get("result").as_str() == Some("reported") {
            return filed(self.record_evidence(model, record).await);
        }
        // Any other result is true of the configuration it was measured on, which Node carries. An
        // entry without it was left by an earlier version (a rollback), its configuration unknown.
        let Some(carried) = entry.get("identityHash").as_str().map(str::to_owned) else {
            return Applied::Discard("a result with no identity, left by an earlier version");
        };
        let live = match self.compute_identity(model).await {
            Ok(Some(live)) => live,
            Ok(None) => return Applied::Retry("the model's identity cannot be read now"),
            Err(_) => return Applied::Retry("the router failed"),
        };
        // A /props that failed when Node measured is no change of configuration.
        let matches = if entry.get("buildUnknown") == &JValue::Bool(true) {
            ev::identity_hash(&without_build(&live.identity)) == carried
        } else {
            live.hash == carried
        };
        if !matches {
            // A /props that failed just now also changes the hash, and passes.
            if live.identity.get("build") == &JValue::Null {
                return Applied::Retry("the identity differs while the engine's build is unknown");
            }
            return Applied::Discard(
                "the configuration has changed since this result was measured",
            );
        }
        let now = crate::identity::now_ms();
        let at = entry
            .get("at")
            .as_num()
            .filter(|n| n.is_finite() && *n > 0.0 && *n <= now as f64)
            .map_or(now, |n| n as i64);
        filed(self.file_record(model, record, &live, at).await)
    }

    /// The drain loop: once at start, then every second. A panic ends that pass, not the loop.
    pub fn spawn_inbox(self: &std::sync::Arc<Self>) {
        if self.inbox_dir.is_none() || self.evidence.is_none() {
            return;
        }
        let me = std::sync::Arc::clone(self);
        tokio::spawn(supervise(Duration::from_secs(1), move || {
            let me = std::sync::Arc::clone(&me);
            async move {
                me.drain_inbox().await;
            }
        }));
    }
}

/// Runs `pass` forever, `pause` apart, each on its own task so that a panic is logged and survived.
pub async fn supervise<F, Fut>(pause: Duration, pass: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    loop {
        if tokio::spawn(pass()).await.is_err() {
            eprintln!("[evidence] the inbox drain failed; it starts again");
        }
        tokio::time::sleep(pause).await;
    }
}

fn without_build(identity: &JValue) -> JValue {
    match identity {
        JValue::Obj(items) => JValue::Obj(
            items
                .iter()
                .map(|(k, v)| {
                    let v = if k == "build" { JValue::Null } else { v.clone() };
                    (k.clone(), v)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Removes `.tmp` files older than `age`.
fn sweep_orphans(dir: &Path, age: Duration) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for e in read.flatten() {
        if !e.file_name().to_string_lossy().ends_with(".tmp") {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|a| a > age);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn inbox_names(dir: &Path) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        // A file being written is `.tmp`; only a finished entry is `.json`. A link is taken too (and
        // removed unread: `read_entry` refuses anything but a regular file).
        .filter(|n| n.ends_with(".json"))
        .collect();
    names.sort();
    names
}

fn read_entry(path: &Path) -> Option<String> {
    use std::io::Read;
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_ENTRY_BYTES {
        return None;
    }
    let mut s = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_ENTRY_BYTES)
        .read_to_string(&mut s)
        .ok()?;
    Some(s)
}

/// The path of the inbox beside the evidence directory.
pub fn inbox_path(data_dir: &Path, suffix: &str) -> PathBuf {
    data_dir.join(format!("evidence-inbox-{suffix}"))
}

pub fn model_ok(model: &str) -> bool {
    !model.is_empty() && js_len(model) <= MAX_MODEL
}

pub fn str_nonempty(v: &JValue) -> Option<&str> {
    str_of(v)
}
