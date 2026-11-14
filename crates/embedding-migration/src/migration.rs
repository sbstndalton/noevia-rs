//! `embedding-migration.cjs`'s actions, as production runs them: the native embedding worker owns
//! the background re-index pass (core's `nativeOwner`), so this side changes the configuration,
//! drops the vector slots nothing reads any more, kicks the worker and reports the worker's
//! snapshot in [`Migration::status`].
//!
//!   set-next   start building a target's vectors beside the active ones (only once its server is
//!              seen serving the model).
//!   clear-next abandon a re-index; its partial vectors go.
//!   switch     next becomes active; refused until next covers every index (unless forced) and,
//!              when only the endpoint changes, until the new endpoint gives the same vectors.
//!              Never to a model the server does not serve, forced or not.
//!   rollback   previous becomes active again; a next under way is abandoned.
//!   finish     drop previous and its vectors.
//!
//! One difference from core, on purpose (filed): while a refused seeded active address has locked
//! the configuration, core's set-next called the writer outside the publication lock and answered
//! a plain 500 ("requires the publication lock"); here it answers core's own 409 `badState`.
//!
//! Calls are serialized by `&mut self`: the network checks and the write run in one turn, so the
//! `nextChanged` race core guards against cannot happen here (the guard stays).

use serde_json::{json, Map, Value};

use crate::config::{snapshot_json, EmbeddingConfig, Snapshot};
use crate::js;
use crate::served::check_served;
use crate::target::{normalize_target, opt_json, same_identity, same_target, Refusal, Target};

/// Fixed synthetic strings for the same-model endpoint check: never user content.
pub const PARITY_SAMPLE: [&str; 4] = [
    "The quick brown fox jumps over the lazy dog.",
    "Quarterly budget review moved to Tuesday afternoon.",
    "function add(a, b) { return a + b; }",
    "Heute regnet es, morgen soll die Sonne scheinen.",
];
pub const PARITY_MIN_COSINE: f64 = 0.995;

/// How many chunks of an index have a vector for a target (`rag.indexCoverage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexCoverage {
    pub chunks: u64,
    pub vectors: u64,
}

/// What the migration needs from the rest of the server. Every call is blocking and bounded.
pub trait Host {
    type Index;
    /// `rag.servedModels(target)`: the ids the target's `/v1/models` lists, or None when it could
    /// not be asked or did not answer a list.
    fn served_models(&self, target: &Target) -> Option<Vec<Value>>;
    /// `rag.listIndexes()`: every existing project index (never created here).
    fn indexes(&self) -> Vec<Self::Index>;
    /// `rag.indexCoverage(index, target)`: None when the index cannot be read.
    fn coverage(&self, index: &Self::Index, target: &Target) -> Option<IndexCoverage>;
    /// `rag.dropIndexSlot(index, target)`: drop that (profile, model)'s vectors from one index.
    fn drop_slot(&self, index: &Self::Index, target: &Target) -> Result<(), String>;
    /// `rag.embed(texts, { kind: 'document', target })`.
    fn embed(&self, texts: &[&str], target: &Target) -> Result<Vec<Vec<f64>>, String>;
    /// `nativeOwner.kick()`: ask the worker for a pass.
    fn kick(&self);
    /// `nativeOwner.snapshot()`: the worker's fresh snapshot, None when stale or missing.
    fn snapshot(&self) -> Option<Value>;
    fn log(&self, line: &str);
}

/// The pacing figures core reports (`EMBEDDING_REINDEX_DUTY`, `EMBEDDING_REINDEX_BATCH`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pacing {
    pub duty: f64,
    pub batch: f64,
}

fn clamp(v: Option<String>, dflt: f64, lo: f64, hi: f64) -> f64 {
    // Number(undefined) is NaN: the default.
    let n = v.as_deref().map_or(f64::NAN, js::number);
    if n.is_finite() {
        n.max(lo).min(hi)
    } else {
        dflt
    }
}

impl Pacing {
    pub fn from_env(get: &dyn Fn(&str) -> Option<String>) -> Self {
        Self {
            duty: clamp(get("EMBEDDING_REINDEX_DUTY"), 0.5, 0.05, 1.0),
            batch: js::round(clamp(get("EMBEDDING_REINDEX_BATCH"), 4.0, 1.0, 16.0)),
        }
    }
}

/// `cosine(a, b)`: NaN for different or zero lengths.
pub fn cosine(a: &[f64], b: &[f64]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return f64::NAN;
    }
    let (mut dot, mut na, mut nb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// A JavaScript number as JSON: integral values print without a fraction, as `JSON.stringify`
/// does (`4`, not `4.0`); non-finite values are null.
pub fn number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        return json!(n as i64);
    }
    serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
}

fn drop_everywhere<H: Host>(host: &H, target: &Target) {
    for entry in host.indexes() {
        if let Err(e) = host.drop_slot(&entry, target) {
            host.log(&format!(
                "[embedding] could not drop {} from an index ({e})",
                target.model
            ));
        }
    }
}

/// `dropIfOrphaned(target, now)`: drop `target`'s vectors unless `now` still holds its identity.
fn drop_if_orphaned<H: Host>(host: &H, target: Option<&Target>, now: &Snapshot) {
    let Some(t) = target else { return };
    let kept = [Some(&now.active), now.next.as_ref(), now.previous.as_ref()]
        .into_iter()
        .any(|n| same_identity(n, Some(t)));
    if !kept {
        drop_everywhere(host, t);
    }
}

pub struct Migration<H: Host> {
    pub config: EmbeddingConfig,
    pub host: H,
    pub pacing: Pacing,
}

impl<H: Host> Migration<H> {
    pub fn new(config: EmbeddingConfig, host: H, pacing: Pacing) -> Self {
        Self {
            config,
            host,
            pacing,
        }
    }

    fn check(&self, t: &Target) -> Result<(), Refusal> {
        let ids = self.host.served_models(t);
        check_served(ids.as_deref(), &t.model)
    }

    /// `coverageOf(target)`. `ready`: every index has at least as many vectors for `target` as for
    /// the active model, so a switch loses nothing.
    pub fn coverage_of(&self, target: Option<&Target>) -> Value {
        let Some(target) = target else {
            return Value::Null;
        };
        let active = self.config.active();
        let (mut chunks, mut vectors, mut unavailable, mut behind) = (0u64, 0u64, 0u64, 0u64);
        let indexes = self.host.indexes();
        for entry in &indexes {
            let Some(c) = self.host.coverage(entry, target) else {
                unavailable += 1;
                continue;
            };
            chunks += c.chunks;
            vectors += c.vectors;
            if !same_identity(Some(target), Some(active)) {
                if let Some(a) = self.host.coverage(entry, active) {
                    if c.vectors < a.vectors {
                        behind += 1;
                    }
                }
            }
        }
        json!({
            "indexes": indexes.len(), "chunks": chunks, "vectors": vectors, "unavailable": unavailable,
            "complete": unavailable == 0 && vectors >= chunks, "ready": unavailable == 0 && behind == 0,
        })
    }

    /// `status()`: the `projects` half of `GET /api/embedding`.
    pub fn status(&self) -> Value {
        let c = self.config.get();
        let snap = self.host.snapshot();
        let field = |k: &str| {
            snap.as_ref()
                .and_then(|s| s.get(k))
                .cloned()
                .unwrap_or(Value::Null)
        };
        let mut out = match snapshot_json(&c) {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        out.insert(
            "coverage".into(),
            json!({ "active": self.coverage_of(Some(&c.active)), "next": self.coverage_of(c.next.as_ref()) }),
        );
        out.insert("nextServed".into(), field("nextServed"));
        out.insert("nextHeld".into(), field("nextHeld"));
        out.insert("running".into(), field("running"));
        out.insert("lastPass".into(), field("lastPass"));
        out.insert("duty".into(), number(self.pacing.duty));
        out.insert("batch".into(), number(self.pacing.batch));
        out.insert("stale".into(), Value::Bool(snap.is_none()));
        out.insert("configValid".into(), field("configValid"));
        out.insert("configInvalid".into(), field("configInvalid"));
        if let Some(d) = &self.config.damaged {
            out.insert("damaged".into(), json!(d));
        }
        Value::Object(out)
    }

    /// `endpointsAgree(a, b)`: the parity sample embedded at both endpoints, every cosine at least
    /// [`PARITY_MIN_COSINE`]. `{ ok, minCosine }`, or `{ ok: false, error }` when an embed failed.
    pub fn endpoints_agree(&self, a: &Target, b: &Target) -> Value {
        let both = self
            .host
            .embed(&PARITY_SAMPLE, a)
            .and_then(|va| Ok((va, self.host.embed(&PARITY_SAMPLE, b)?)));
        let (va, vb) = match both {
            Ok(v) => v,
            Err(e) => return json!({ "ok": false, "error": e }),
        };
        let sims: Vec<f64> = va
            .iter()
            .enumerate()
            .map(|(i, v)| vb.get(i).map_or(f64::NAN, |w| cosine(v, w)))
            .collect();
        // Math.min(): NaN if any is NaN, +Infinity for none.
        let min = if sims.iter().any(|s| s.is_nan()) {
            f64::NAN
        } else {
            sims.iter().copied().fold(f64::INFINITY, f64::min)
        };
        let ok = sims.iter().all(|s| s.is_finite()) && min >= PARITY_MIN_COSINE;
        let shown = if min.is_finite() {
            number(js::round(min * 1e5) / 1e5)
        } else {
            Value::Null
        };
        json!({ "ok": ok, "minCosine": shown })
    }

    /// `setNext(target)`.
    pub fn set_next(&mut self, raw: Option<&Value>) -> Result<Value, Refusal> {
        let t = normalize_target(raw, self.config.profiles())?;
        if self.config.damaged.is_some() || same_target(Some(&t), Some(self.config.active())) {
            // Refuses: a damaged file, a locked seeded address (core: outside the lock, a 500), or
            // already active.
            if self.config.is_locked() {
                self.config.with_lock(|c| c.set_next(t))??;
            } else {
                self.config.set_next(t)?;
            }
            return Ok(self.status());
        }
        self.check(&t)?;
        let host = &self.host;
        self.config.with_lock(|config| {
            let before = config.get();
            let c = config.set_next(t)?;
            // A different next replaces an abandoned one: its partial vectors go.
            if let Some(old) = &before.next {
                let o = Some(old);
                if !same_identity(o, c.next.as_ref())
                    && !same_identity(o, Some(&c.active))
                    && !same_identity(o, c.previous.as_ref())
                {
                    drop_everywhere(host, old);
                }
            }
            host.kick();
            Ok::<_, Refusal>(())
        })??;
        Ok(self.status())
    }

    /// `clearNext()`.
    pub fn clear_next(&mut self) -> Result<Value, Refusal> {
        let host = &self.host;
        self.config.with_lock(|config| {
            let before = config.get();
            config.clear_next()?;
            host.kick();
            if let Some(old) = &before.next {
                let o = Some(old);
                if !same_identity(o, Some(&before.active))
                    && !same_identity(o, before.previous.as_ref())
                {
                    drop_everywhere(host, old);
                }
            }
            Ok::<_, Refusal>(())
        })??;
        Ok(self.status())
    }

    /// `switchToNext({ force })`.
    pub fn switch_to_next(&mut self, force: bool) -> Result<Value, Refusal> {
        let c = self.config.get();
        let Some(checked) = c.next.clone() else {
            return Err(Refusal::new(409, "noNext", "No re-index is under way."));
        };
        // Never switch to a model the server does not serve, forced or not.
        self.check(&checked)?;
        if same_identity(Some(&checked), Some(&c.active)) {
            // Same model and profile on another endpoint: the stored vectors stay valid only if the
            // new endpoint really gives the same vectors.
            let parity = self.endpoints_agree(&c.active, &checked);
            if parity.get("ok") != Some(&Value::Bool(true)) && !force {
                return Err(Refusal::new(
                    409,
                    "parityFailed",
                    "The new embeddings address does not give the same vectors as the current one.",
                )
                .with("parity", parity));
            }
        } else if !force {
            let cov = self.coverage_of(Some(&checked));
            if cov.get("ready") != Some(&Value::Bool(true)) {
                return Err(Refusal::new(
                    409,
                    "notReady",
                    "The new model has not finished indexing every project yet.",
                )
                .with("coverage", cov));
            }
        }
        let host = &self.host;
        self.config.with_lock(|config| {
            let displaced = config.previous().cloned();
            let after = config.switch_to_next(Some(&checked))?;
            // An older rollback target is displaced by the new one: its vectors would never be read again.
            drop_if_orphaned(host, displaced.as_ref(), &after);
            host.log(&format!(
                "[embedding] switched to {}/{}{}; previous {}/{} kept for rollback",
                checked.profile,
                checked.model,
                if force { " (forced)" } else { "" },
                c.active.profile,
                c.active.model
            ));
            host.kick();
            Ok::<_, Refusal>(())
        })??;
        Ok(self.status())
    }

    /// `rollback()`.
    pub fn rollback(&mut self) -> Result<Value, Refusal> {
        let host = &self.host;
        self.config.with_lock(|config| {
            let before = config.get();
            let c = config.rollback()?;
            // The re-index that was under way is abandoned by a rollback: no partly built slot stays.
            drop_if_orphaned(host, before.next.as_ref(), &c);
            host.log(&format!(
                "[embedding] rolled back to {}/{}",
                c.active.profile, c.active.model
            ));
            host.kick();
            Ok::<_, Refusal>(())
        })??;
        Ok(self.status())
    }

    /// `finish()`.
    pub fn finish(&mut self) -> Result<Value, Refusal> {
        let host = &self.host;
        self.config.with_lock(|config| {
            let c = config.get();
            let Some(old) = c.previous.clone() else {
                return Err(Refusal::new(
                    409,
                    "noPrevious",
                    "There is no previous embedding model to remove.",
                ));
            };
            config.finish()?;
            host.kick();
            if !same_identity(Some(&old), Some(&c.active))
                && !same_identity(Some(&old), c.next.as_ref())
            {
                drop_everywhere(host, &old);
            }
            host.log(&format!(
                "[embedding] removed {}/{} and its vectors",
                old.profile, old.model
            ));
            Ok(())
        })??;
        Ok(self.status())
    }

    /// The configuration's targets as JSON, for callers that need only them.
    pub fn targets_json(&self) -> Value {
        let c = self.config.get();
        json!({ "active": c.active.to_json(), "next": opt_json(c.next.as_ref()), "previous": opt_json(c.previous.as_ref()) })
    }
}

/// `projects(action, body)`'s catch in `routes/embedding.cjs`: a refusal as the projects half of an
/// answer, `{ error, status, code?, coverage?, parity?, served? }` (the message cut to 300 units).
pub fn refusal_half(r: &Refusal) -> Value {
    let mut out = Map::new();
    out.insert(
        "error".into(),
        json!(crate::served::slice_units(&r.message, 300)),
    );
    out.insert("status".into(), json!(r.status.unwrap_or(500)));
    if let Some(code) = r.code {
        out.insert("code".into(), json!(code));
    }
    for key in ["coverage", "parity", "served"] {
        if let Some(v) = r.extra.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    Value::Object(out)
}
