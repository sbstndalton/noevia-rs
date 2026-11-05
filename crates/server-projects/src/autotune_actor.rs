//! Native actor recovery protocol. The owner remains the only state writer.
//!
//! Engine adapters must provide revision-CAS file writes and chat-preemptible exclusion. Losing
//! either authority is terminal: recovery never reacquires and never performs cleanup without it.
//! No engine implementation is selected here; this module does not start a model or make a request.
use crate::autotune_state::Owner;
use js_json::JValue;
use sha2::{Digest, Sha256};
use std::future::Future;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    Busy,
    Unavailable,
    Lost,
    Engine,
}
impl Fault {
    pub fn status(self) -> u16 {
        match self {
            Self::Busy => 409,
            Self::Unavailable => 503,
            Self::Lost | Self::Engine => 409,
        }
    }
}

/// The eventual J adapter must return a no-lease activity reservation which chat can interrupt.
/// The returned value owns the reservation immediately and releases it in Drop, even if any later
/// future is aborted. Confirmation must validate that SAME reservation, never acquire a new one.
pub trait Exclusion {
    type Guard: Coverage;
    fn acquire(&self) -> impl Future<Output = Result<Self::Guard, Fault>>;
}
pub trait Coverage {
    fn confirm(&self) -> impl Future<Output = Result<(), Fault>>;
}

/// The adapter owns the actual engine admission (H), including its RAII permit. `restore` must CAS
/// against the supplied revision inside the writer, not check and subsequently write. `unload`
/// waits for this job's model only; `foreign_live` must fail if the listing is unavailable.
pub trait RecoveryEngine {
    fn revision(&self) -> impl Future<Output = Result<String, Fault>>;
    fn unload(&self, model: &str, legacy: bool) -> impl Future<Output = Result<(), Fault>>;
    fn restore(
        &self,
        revision: &str,
        text: &str,
        backup: bool,
    ) -> impl Future<Output = Result<(), Fault>>;
    fn foreign_live(&self, model: &str) -> impl Future<Output = Result<bool, Fault>>;
    fn invalidate_baseline(&self) -> impl Future<Output = Result<bool, Fault>>;
    fn reload(&self) -> impl Future<Output = Result<(), Fault>>;
}

fn put(v: &mut JValue, key: &str, value: JValue) {
    if let JValue::Obj(fields) = v {
        if let Some(f) = fields.iter_mut().find(|(k, _)| k == key) {
            f.1 = value;
        } else {
            fields.push((key.into(), value));
        }
    }
}
fn remove(v: &mut JValue, key: &str) {
    if let JValue::Obj(fields) = v {
        fields.retain(|(k, _)| k != key);
    }
}
fn digest(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn running(v: &JValue) -> bool {
    v.get("status").as_str() == Some("running")
}
fn rows(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

/// A capability obtained from the owner's attach response by the runtime's single actor startup.
/// There is intentionally no attach/retry method on this type. All checks use K's exact writer,
/// job revision and epoch. A rejected check permanently poisons this session.
pub struct Session<'a> {
    owner: &'a Owner,
    writer: String,
    job_rev: f64,
    epoch: f64,
    state: JValue,
    lost: bool,
}
impl<'a> Session<'a> {
    pub fn from_snapshot(owner: &'a Owner, snapshot: &JValue) -> Result<Self, Fault> {
        let session = Self {
            owner,
            writer: snapshot
                .get("writer")
                .as_str()
                .ok_or(Fault::Unavailable)?
                .into(),
            job_rev: snapshot.get("jobRev").as_num().ok_or(Fault::Unavailable)?,
            epoch: snapshot.get("epoch").as_num().ok_or(Fault::Unavailable)?,
            state: snapshot.get("state").clone(),
            lost: false,
        };
        if !session.current() {
            return Err(Fault::Lost);
        }
        Ok(session)
    }
    fn current(&self) -> bool {
        !self.lost
            && self
                .owner
                .actor_current(&self.writer, self.job_rev, self.epoch)
    }
    fn check(&mut self) -> Result<(), Fault> {
        if self.current() {
            Ok(())
        } else {
            self.lost = true;
            Err(Fault::Lost)
        }
    }
    async fn barrier(&mut self, guard: &impl Coverage) -> Result<(), Fault> {
        self.check()?;
        if let Err(e) = guard.confirm().await {
            self.lost = true;
            return Err(e);
        }
        self.check()
    }
    fn save(&mut self, job: &JValue) -> Result<(), Fault> {
        self.check()?;
        let body = JValue::obj([
            ("writer", self.writer.as_str().into()),
            ("jobRev", JValue::Num(self.job_rev)),
            ("nativeEpoch", JValue::Num(self.epoch)),
            ("job", job.clone()),
            ("history", self.state.get("history").clone()),
        ]);
        let answer = self.owner.commit(&body);
        if answer.0 != 200 {
            self.lost = true;
            return Err(if answer.0 == 503 {
                Fault::Unavailable
            } else {
                Fault::Lost
            });
        }
        put(&mut self.state, "job", job.clone());
        Ok(())
    }

    /// Recover a persisted running job. A dropped future releases the exclusion through its guard;
    /// durable restore intent lets the next process recognize a file write completed before a crash.
    /// The caller schedules `_reloadPending` separately; no timer or engine escapes this future.
    pub async fn recover(
        &mut self,
        engine: &impl RecoveryEngine,
        exclusion: &impl Exclusion,
        now: f64,
    ) -> Result<(), Fault> {
        self.check()?;
        let mut job = self.state.get("job").clone();
        if !job.truthy() {
            return Ok(());
        }
        if job.get("_pendingRevision").as_str().is_some() {
            let revision = engine.revision().await?;
            self.check()?;
            if job.get("_pendingRevision").as_str() == Some(revision.as_str()) {
                put(&mut job, "_revision", revision.into());
            }
            remove(&mut job, "_pendingRevision");
            self.save(&job)?;
        }
        if !running(&job) {
            return Ok(());
        }
        // Guard exists before the next suspension point. Never retake on failure.
        let guard = match exclusion.acquire().await {
            Ok(guard) => guard,
            Err(e) => {
                if matches!(e, Fault::Lost | Fault::Unavailable) {
                    self.lost = true;
                }
                return Err(e);
            }
        };
        self.barrier(&guard).await?;
        let legacy = !matches!(job.get("models"), JValue::Arr(_));
        let item = rows(job.get("models")).iter().position(running);
        let phase = item
            .and_then(|i| rows(job.get("models")).get(i))
            .and_then(|m| rows(m.get("phases")).iter().position(running));
        let snapshot = if legacy {
            job.get("originalText").clone()
        } else {
            item.and_then(|i| rows(job.get("models")).get(i))
                .and_then(|m| phase.and_then(|p| rows(m.get("phases")).get(p)))
                .map(|p| p.get("_beforeText").clone())
                .unwrap_or(JValue::Undefined)
        };
        let mut restored = true;
        if legacy || !matches!(snapshot, JValue::Undefined | JValue::Null) {
            let expected = job
                .get(if legacy { "lastRevision" } else { "_revision" })
                .as_str()
                .map(str::to_owned);
            restored = false;
            if let (Some(expected), Some(text)) = (expected, snapshot.as_str()) {
                let text = text.to_owned();
                let result = self
                    .restore(engine, &guard, &mut job, &expected, &text, now)
                    .await;
                match result {
                    Ok(()) => restored = true,
                    // Authority loss must never be turned into a best-effort restoration failure.
                    Err(e) if self.lost || matches!(e, Fault::Lost | Fault::Unavailable) => {
                        self.lost = true;
                        return Err(e);
                    }
                    Err(_) => {}
                }
            }
        }
        if legacy {
            put(&mut job, "restored", restored.into());
            remove(&mut job, "originalText");
            put(&mut job, "status", "interrupted".into());
            put(&mut job, "error", if restored {
                "An older tuning run was interrupted and restored. Start a new tune."
            } else {
                "An older tuning run was interrupted; settings could not safely be restored. Inspect models.ini before tuning again."
            }.into());
        } else {
            if let Some(i) = item {
                if let Some(JValue::Arr(items)) = job_field(&mut job, "models") {
                    if let Some(m) = items.get_mut(i) {
                        put(m, "status", "interrupted".into());
                        if let Some(p) = phase {
                            if let Some(JValue::Arr(phases)) = job_field(m, "phases") {
                                if let Some(ph) = phases.get_mut(p) {
                                    if !matches!(snapshot, JValue::Undefined | JValue::Null) {
                                        put(ph, "restored", restored.into());
                                        if restored {
                                            remove(ph, "_beforeText");
                                        }
                                    }
                                    put(ph, "status", "interrupted".into());
                                    put(ph, "reason", "Restart interrupted this phase.".into());
                                    if let Some(JValue::Arr(steps)) = job_field(ph, "steps") {
                                        for step in steps.iter_mut().filter(|s| running(s)) {
                                            put(step, "status", "interrupted".into());
                                            put(
                                                step,
                                                "reason",
                                                "Restart interrupted this phase.".into(),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !restored {
                put(&mut job, "_unsafe", true.into());
            }
            put(&mut job, "status", "interrupted".into());
            put(&mut job, "phase", "Interrupted".into());
            put(&mut job, "finishedAt", JValue::Num(now));
            remove(&mut job, "failedPhase");
            remove(&mut job, "failedStep");
            let stopped =
                |p: &&JValue| matches!(p.get("status").as_str(), Some("failed" | "interrupted"));
            let items = rows(job.get("models"));
            let stopped_item = items
                .iter()
                .find(|m| {
                    m.get("model") == job.get("model")
                        && rows(m.get("phases")).iter().any(|p| stopped(&p))
                })
                .or_else(|| {
                    items
                        .iter()
                        .find(|m| rows(m.get("phases")).iter().any(|p| stopped(&p)))
                });
            let stopped_phase = stopped_item
                .and_then(|m| {
                    let phases = rows(m.get("phases"));
                    phases
                        .iter()
                        .find(|p| p.get("status").as_str() == Some("failed"))
                        .or_else(|| {
                            phases
                                .iter()
                                .find(|p| p.get("status").as_str() == Some("interrupted"))
                        })
                })
                .cloned();
            if let Some(p) = stopped_phase.filter(|p| p.get("label").as_str().is_some()) {
                put(&mut job, "failedPhase", p.get("label").clone());
                let steps = rows(p.get("steps"));
                let stopped_step = steps
                    .iter()
                    .find(|s| s.get("status").as_str() == Some("failed"))
                    .or_else(|| {
                        steps
                            .iter()
                            .find(|s| s.get("status").as_str() == Some("interrupted"))
                    });
                if let Some(s) = stopped_step.filter(|s| s.get("label").as_str().is_some()) {
                    put(&mut job, "failedStep", s.get("label").clone());
                }
            }
            let message = if job.get("_unsafe").as_bool() == Some(true) {
                "Restart interrupted tuning; settings could not safely be restored. Inspect models.ini before resuming."
            } else {
                "Restart interrupted tuning. Completed settings remain saved."
            };
            put(&mut job, "error", message.into());
        }
        self.barrier(&guard).await?;
        self.save(&job)
    }

    async fn restore(
        &mut self,
        engine: &impl RecoveryEngine,
        guard: &impl Coverage,
        job: &mut JValue,
        expected: &str,
        text: &str,
        now: f64,
    ) -> Result<(), Fault> {
        let legacy = !matches!(job.get("models"), JValue::Arr(_));
        let actual = engine.revision().await?;
        self.barrier(guard).await?;
        if actual != expected || (legacy && text.is_empty()) {
            return Err(Fault::Engine);
        }
        let model = job.get("model").as_str().ok_or(Fault::Engine)?.to_owned();
        engine.unload(&model, legacy).await?;
        self.barrier(guard).await?;
        // Intent is durable BEFORE CAS; neither future cancellation nor a crash loses its fence.
        put(job, "_pendingRevision", digest(text).into());
        self.save(job)?;
        self.barrier(guard).await?;
        engine
            .restore(expected, text, job.get("_backedUp").as_bool() != Some(true))
            .await?;
        self.barrier(guard).await?;
        if !legacy {
            put(job, "_revision", digest(text).into());
        }
        remove(job, "_pendingRevision");
        self.save(job)?;
        let foreign = if legacy {
            false
        } else {
            engine.foreign_live(&model).await?
        };
        self.barrier(guard).await?;
        let invalidate = if foreign {
            false
        } else {
            engine.invalidate_baseline().await?
        };
        self.barrier(guard).await?;
        if foreign || !invalidate {
            put(job, "_reloadPending", true.into());
            if legacy {
                self.save(job)?;
                return Err(Fault::Engine);
            }
            let note = if foreign {
                "models.ini is restored; the model server re-reads it once the other client is done."
            } else {
                "The model server has not re-read the restored models.ini; it does so before this model next loads."
            };
            if !matches!(job.get("log"), JValue::Arr(_)) {
                put(job, "log", JValue::Arr(Vec::new()));
            }
            if let Some(JValue::Arr(log)) = job_field(job, "log") {
                log.push(JValue::obj([
                    ("at", JValue::Num(now)),
                    ("text", note.into()),
                ]));
                if log.len() > 400 {
                    log.remove(0);
                }
            }
        } else {
            engine.reload().await?;
            self.barrier(guard).await?;
            if !legacy {
                put(job, "_reloadPending", false.into());
            }
        }
        Ok(())
    }
}

fn job_field<'a>(value: &'a mut JValue, key: &str) -> Option<&'a mut JValue> {
    match value {
        JValue::Obj(fields) => fields.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
        _ => None,
    }
}
