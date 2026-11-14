//! `embedding-migration.cjs` with a `nativeOwner` (production): the one path that changes the
//! projects' embedding model, profile or endpoint. The re-index pass runs in the native worker; a
//! change here kicks it, and `status()` reads its snapshot.

use crate::config::{no_previous, Config};
use crate::target::{check_served, json_of, normalize_target, same_identity, Target};
use crate::{Detail, Failure, Host};
use chat_turn::js;
use js_json::{wtf16, JValue};

/// Fixed synthetic strings for the same-model endpoint check: never user content.
pub const PARITY_SAMPLE: [&str; 4] = [
    "The quick brown fox jumps over the lazy dog.",
    "Quarterly budget review moved to Tuesday afternoon.",
    "function add(a, b) { return a + b; }",
    "Heute regnet es, morgen soll die Sonne scheinen.",
];
pub const PARITY_MIN_COSINE: f64 = 0.995;

/// One index's vectors for a target (`rag.indexCoverage`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coverage {
    pub chunks: f64,
    pub vectors: f64,
}

/// The pass's pacing, reported by `status()` (`EMBEDDING_REINDEX_DUTY`, `EMBEDDING_REINDEX_BATCH`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pacing {
    pub duty: f64,
    pub batch: f64,
}

/// `Math.round`: halves towards +∞.
fn js_round(x: f64) -> f64 {
    let floor = x.floor();
    if x - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

fn clamp(v: Option<&str>, default: f64, lo: f64, hi: f64) -> f64 {
    let n = v.map_or(f64::NAN, js::string_to_number);
    if n.is_finite() {
        n.max(lo).min(hi)
    } else {
        default
    }
}

impl Pacing {
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Pacing {
        Pacing {
            duty: clamp(env("EMBEDDING_REINDEX_DUTY").as_deref(), 0.5, 0.05, 1.0),
            batch: js_round(clamp(
                env("EMBEDDING_REINDEX_BATCH").as_deref(),
                4.0,
                1.0,
                16.0,
            )),
        }
    }
}

/// `cosine(a, b)`: NaN unless both have the same, non-zero length.
pub fn cosine(a: &[f64], b: Option<&Vec<f64>>) -> f64 {
    let Some(b) = b.filter(|b| b.len() == a.len() && !a.is_empty()) else {
        return f64::NAN;
    };
    let (mut dot, mut na, mut nb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    dot / (na.sqrt() * nb.sqrt())
}

#[derive(Debug, Clone)]
pub struct Migration {
    pub config: Config,
    pub pacing: Pacing,
}

impl Migration {
    pub fn new(config: Config, pacing: Pacing) -> Migration {
        Migration { config, pacing }
    }

    /// `coverageOf(target)`: how far `target`'s vectors cover the indexes, `None` without a
    /// target. `ready`: no index is unreadable or behind the active model.
    pub fn coverage_of<H: Host>(&self, host: &mut H, target: Option<&Target>) -> Option<JValue> {
        let target = target?;
        let active = self.config.active();
        let (mut chunks, mut vectors, mut unavailable, mut behind) = (0.0, 0.0, 0.0, 0);
        let indexes = host.indexes();
        for entry in &indexes {
            let Some(c) = host.coverage(entry, target) else {
                unavailable += 1.0;
                continue;
            };
            chunks += c.chunks;
            vectors += c.vectors;
            if !same_identity(Some(target), Some(active)) {
                if let Some(a) = host.coverage(entry, active) {
                    if c.vectors < a.vectors {
                        behind += 1;
                    }
                }
            }
        }
        Some(JValue::obj([
            ("indexes", JValue::Num(indexes.len() as f64)),
            ("chunks", JValue::Num(chunks)),
            ("vectors", JValue::Num(vectors)),
            ("unavailable", JValue::Num(unavailable)),
            (
                "complete",
                JValue::Bool(unavailable == 0.0 && vectors >= chunks),
            ),
            ("ready", JValue::Bool(unavailable == 0.0 && behind == 0)),
        ]))
    }

    /// `status()` with the native worker's snapshot fields.
    pub fn status<H: Host>(&self, host: &mut H) -> JValue {
        let c = &self.config;
        let snapshot = host.snapshot();
        let coverage = JValue::obj([
            (
                "active",
                self.coverage_of(host, Some(c.active()))
                    .unwrap_or(JValue::Null),
            ),
            (
                "next",
                self.coverage_of(host, c.next()).unwrap_or(JValue::Null),
            ),
        ]);
        let mut out = vec![
            ("active".to_string(), c.active().to_json()),
            ("next".to_string(), json_of(c.next())),
            ("previous".to_string(), json_of(c.previous())),
            ("source".to_string(), c.source().as_str().into()),
            ("coverage".to_string(), coverage),
        ];
        if c.next().is_some() {
            // Its place in the object; the snapshot's value replaces it below.
            out.push(("nextServed".into(), JValue::Null));
        }
        out.push(("running".into(), JValue::Bool(false)));
        out.push(("lastPass".into(), JValue::Null));
        out.push(("duty".into(), JValue::Num(self.pacing.duty)));
        out.push(("batch".into(), JValue::Num(self.pacing.batch)));
        let field = |key: &str| match snapshot.as_ref().map(|s| s.get(key)) {
            Some(v) if !v.is_nullish() => v.clone(),
            _ => JValue::Null,
        };
        for key in ["running", "lastPass", "nextServed", "nextHeld"] {
            wtf16::set(&mut out, key, field(key));
        }
        wtf16::set(&mut out, "stale", JValue::Bool(snapshot.is_none()));
        for key in ["configValid", "configInvalid"] {
            wtf16::set(&mut out, key, field(key));
        }
        if let Some(damaged) = c.damaged() {
            out.push(("damaged".into(), damaged.into()));
        }
        JValue::Obj(out)
    }

    /// `config.withLock(action)`: the action runs holding the config lock (when changes are
    /// saved); the lock is released whatever the action did.
    fn locked<H: Host>(
        &mut self,
        host: &mut H,
        action: impl FnOnce(&mut Self, &mut H) -> Result<JValue, Failure>,
    ) -> Result<JValue, Failure> {
        if !self.config.holds_lock() {
            return action(self, host);
        }
        host.lock().map_err(Failure::plain)?;
        self.config.in_hold = true;
        let result = action(self, host);
        self.config.in_hold = false;
        host.unlock();
        result
    }

    fn drop_everywhere<H: Host>(host: &mut H, target: &Target) {
        for entry in host.indexes() {
            let _ = host.drop_slot(&entry, target);
        }
    }

    /// `dropIfOrphaned(target, now)`: unless the configuration still holds that model and profile.
    fn drop_if_orphaned<H: Host>(&self, host: &mut H, target: Option<&Target>) {
        let c = self.config.state();
        let kept = [Some(&c.active), c.next.as_ref(), c.previous.as_ref()];
        if let Some(t) = target {
            if !kept.iter().any(|k| same_identity(*k, Some(t))) {
                Migration::drop_everywhere(host, t);
            }
        }
    }

    pub fn set_next<H: Host>(&mut self, host: &mut H, raw: &JValue) -> Result<JValue, Failure> {
        let t = normalize_target(raw)?;
        let config = &mut self.config;
        if config.damaged().is_some() || crate::target::same_target(Some(&t), Some(config.active()))
        {
            // Refuses: a damaged file, a locked configuration, or the active target.
            config.set_next(t)?;
            return Ok(self.status(host));
        }
        check_served(host, t.base_url.as_deref(), &t.model)?;
        self.locked(host, |me, host| {
            let before = me.config.state().clone();
            me.config.set_next(t)?;
            let c = me.config.state();
            // A different next replaces an abandoned one: its partial vectors go.
            if let Some(old) = &before.next {
                if !same_identity(Some(old), c.next.as_ref())
                    && !same_identity(Some(old), Some(&c.active))
                    && !same_identity(Some(old), c.previous.as_ref())
                {
                    Migration::drop_everywhere(host, old);
                }
            }
            host.kick();
            Ok(me.status(host))
        })
    }

    pub fn clear_next<H: Host>(&mut self, host: &mut H) -> Result<JValue, Failure> {
        self.locked(host, |me, host| {
            let before = me.config.state().clone();
            me.config.clear_next()?;
            host.kick();
            if let Some(old) = &before.next {
                if !same_identity(Some(old), Some(&before.active))
                    && !same_identity(Some(old), before.previous.as_ref())
                {
                    Migration::drop_everywhere(host, old);
                }
            }
            Ok(me.status(host))
        })
    }

    pub fn switch_to_next<H: Host>(
        &mut self,
        host: &mut H,
        force: bool,
    ) -> Result<JValue, Failure> {
        let Some(checked) = self.config.next().cloned() else {
            return Err(Failure::new(409, "noNext", "No re-index is under way."));
        };
        let active = self.config.active().clone();
        // Never a model the server does not serve, forced or not.
        check_served(host, checked.base_url.as_deref(), &checked.model)?;
        if same_identity(Some(&checked), Some(&active)) {
            // Same model and profile elsewhere: the stored vectors stay valid only if the new
            // endpoint gives the same vectors.
            let parity = endpoints_agree(host, &active, &checked);
            if parity.get("ok") != &JValue::Bool(true) && !force {
                return Err(Failure::new(
                    409,
                    "parityFailed",
                    "The new embeddings address does not give the same vectors as the current one.",
                )
                .with(Detail::Parity(parity)));
            }
        } else if !force {
            let coverage = self
                .coverage_of(host, Some(&checked))
                .unwrap_or(JValue::Null);
            if coverage.get("ready") != &JValue::Bool(true) {
                return Err(Failure::new(
                    409,
                    "notReady",
                    "The new model has not finished indexing every project yet.",
                )
                .with(Detail::Coverage(coverage)));
            }
        }
        self.locked(host, |me, host| {
            let displaced = me.config.previous().cloned();
            me.config.switch_to_next(Some(&checked))?;
            me.drop_if_orphaned(host, displaced.as_ref());
            host.kick();
            Ok(me.status(host))
        })
    }

    pub fn rollback<H: Host>(&mut self, host: &mut H) -> Result<JValue, Failure> {
        self.locked(host, |me, host| {
            let abandoned = me.config.next().cloned();
            me.config.rollback()?;
            me.drop_if_orphaned(host, abandoned.as_ref());
            host.kick();
            Ok(me.status(host))
        })
    }

    pub fn finish<H: Host>(&mut self, host: &mut H) -> Result<JValue, Failure> {
        self.locked(host, |me, host| {
            let Some(old) = me.config.previous().cloned() else {
                return Err(no_previous("remove"));
            };
            let (active, next) = (me.config.active().clone(), me.config.next().cloned());
            me.config.finish()?;
            host.kick();
            if !same_identity(Some(&old), Some(&active))
                && !same_identity(Some(&old), next.as_ref())
            {
                Migration::drop_everywhere(host, &old);
            }
            Ok(me.status(host))
        })
    }
}

/// `endpointsAgree(a, b)`: `{ ok, minCosine }`, or `{ ok: false, error }` when an embed failed.
fn endpoints_agree<H: Host>(host: &mut H, a: &Target, b: &Target) -> JValue {
    let failed = |message: String| {
        let message = if message.is_empty() {
            "Error".into()
        } else {
            message
        };
        JValue::obj([("ok", JValue::Bool(false)), ("error", JValue::Str(message))])
    };
    let va = match host.embed(&PARITY_SAMPLE, a) {
        Ok(v) => v,
        Err(e) => return failed(e),
    };
    let vb = match host.embed(&PARITY_SAMPLE, b) {
        Ok(v) => v,
        Err(e) => return failed(e),
    };
    let sims: Vec<f64> = va
        .iter()
        .enumerate()
        .map(|(i, v)| cosine(v, vb.get(i)))
        .collect();
    // Math.min(...sims): Infinity for none, NaN if any is NaN.
    let min = sims.iter().try_fold(f64::INFINITY, |m, &s| {
        if s.is_nan() {
            None
        } else {
            Some(m.min(s))
        }
    });
    let min = min.unwrap_or(f64::NAN);
    let ok = sims.iter().all(|s| s.is_finite()) && min >= PARITY_MIN_COSINE;
    let shown = if min.is_finite() {
        JValue::Num(js_round(min * 1e5) / 1e5)
    } else {
        JValue::Null
    };
    JValue::obj([("ok", JValue::Bool(ok)), ("minCosine", shown)])
}
