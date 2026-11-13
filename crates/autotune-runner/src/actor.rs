//! The live actor's conditional primitives the front's routes use (core llamacpp-full-autotune.cjs
//! `createAutotuneCancellation`, `statusFacts`, `settingsSnapshot`/`commitSettings`,
//! `clearFacts`/`compareAndClear`, `armStartupRecovery`/`recover`, `resumeFacts`/`prepareResume`/
//! `compareAndResume`). Public policy and HTTP answers stay in the routes; these only compare a
//! token with the live state and change it when it still matches.
//!
//! Tokens are process-salted digests, as in Node: opaque, never compared across processes.

use crate::deps::{Context as _, Deps, Fail};
use crate::engine::string_of;
use crate::phases::private_key;
use crate::runner::{Child, Runner, St};
use crate::{hash, js};
use js_json::{wtf16, JValue};

/// The outer job fields a front may read (cancellation and resume views).
const PUBLIC_FIELDS: [&str; 19] = [
    "id",
    "model",
    "bulk",
    "mode",
    "promptBudgetSeconds",
    "status",
    "phase",
    "startedAt",
    "log",
    "models",
    "queueProgress",
    "finishedAt",
    "error",
    "failedPhase",
    "failedStep",
    "restored",
    "skipped",
    "waiting",
    "result",
];
const HTTP_KEYS: [&str; 10] = [
    "headers",
    "authorization",
    "apiKey",
    "api_key",
    "accessToken",
    "refreshToken",
    "httpConfig",
    "requestConfig",
    "baseUrl",
    "url",
];

fn hidden_strict(key: &str) -> bool {
    private_key(key) || HTTP_KEYS.contains(&key)
}

/// statusFacts' wider rule: also any key that is a credential name once lower-cased without `_`/`-`.
fn hidden_status(key: &str) -> bool {
    if private_key(key) {
        return true;
    }
    let flat: String = key
        .to_lowercase()
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .collect();
    matches!(
        flat.as_str(),
        "headers"
            | "authorization"
            | "apikey"
            | "accesstoken"
            | "refreshtoken"
            | "httpconfig"
            | "requestconfig"
            | "baseurl"
            | "url"
            | "credentials"
            | "password"
            | "secret"
            | "token"
            | "clientsecret"
    )
}

/// `JSON.parse(JSON.stringify(v, (key, value) => hidden(key) ? undefined : value))`.
fn copy_without(v: &JValue, hidden: fn(&str) -> bool) -> JValue {
    match v {
        JValue::Obj(items) => JValue::Obj(
            items
                .iter()
                .filter(|(k, x)| !hidden(k) && !matches!(x, JValue::Undefined))
                .map(|(k, x)| (k.clone(), copy_without(x, hidden)))
                .collect(),
        ),
        JValue::Arr(items) => JValue::Arr(
            items
                .iter()
                .map(|x| match x {
                    JValue::Undefined => JValue::Null,
                    other => copy_without(other, hidden),
                })
                .collect(),
        ),
        JValue::Num(n) if !n.is_finite() => JValue::Null,
        other => other.clone(),
    }
}

/// The job's listed fields that it owns, then the privacy copy.
fn pick(job: &JValue, fields: &[&str], hidden: fn(&str) -> bool) -> JValue {
    let picked = JValue::Obj(
        fields
            .iter()
            .filter(|k| js::has(job, k))
            .map(|k| (k.to_string(), job.get(k).clone()))
            .collect(),
    );
    copy_without(&picked, hidden)
}

/// prepareResume's deadline (Node: `Date.now() + 120000`).
const PREPARE_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Gives back `resumePreparing`/`starting` when dropped (prepareResume's `finally`).
struct Preparing<'a, D: Deps>(&'a Runner<D>);

impl<D: Deps> Drop for Preparing<'_, D> {
    fn drop(&mut self) {
        let mut st = self.0.st();
        if st.resume_preparing {
            st.resume_preparing = false;
            st.starting = false;
        }
    }
}

/// A conditional change's answer: `{ applied: false }`.
fn not_applied() -> JValue {
    JValue::obj([("applied", JValue::Bool(false))])
}

fn status_of(v: &JValue) -> JValue {
    match v.as_str() {
        Some(
            s @ ("pending" | "running" | "passed" | "failed" | "interrupted" | "cancelled"
            | "skipped"),
        ) => JValue::from(s),
        _ => JValue::Null,
    }
}

fn string_or_null(v: &JValue) -> JValue {
    match v {
        JValue::Str(_) => v.clone(),
        _ => JValue::Null,
    }
}

/// Strictly increasing safe-integer indices below `length`.
fn indices(list: &JValue, length: usize) -> Option<Vec<usize>> {
    let JValue::Arr(items) = list else {
        return None;
    };
    if items.len() > length {
        return None;
    }
    let mut out = Vec::with_capacity(items.len());
    for v in items {
        let n = v.as_num()?;
        if n.fract() != 0.0 || n < 0.0 || n >= length as f64 {
            return None;
        }
        let n = n as usize;
        if out.last().is_some_and(|prev| *prev >= n) {
            return None;
        }
        out.push(n);
    }
    Some(out)
}

impl<D: Deps> Runner<D> {
    fn digest(st: &St<D::Context>, value: &JValue) -> String {
        hash(&JValue::obj([
            ("salt", JValue::from(st.salt.as_str())),
            ("value", value.clone()),
        ]))
    }

    fn cancel_generation(st: &St<D::Context>) -> String {
        let job = st.state.get("job").truthy().then_some(st.job_object);
        Self::digest(
            st,
            &JValue::obj([
                ("job", job.map_or(JValue::Null, |n| JValue::Num(n as f64))),
                ("run", JValue::Num(st.run as f64)),
            ]),
        )
    }

    fn cancel_token(st: &St<D::Context>) -> String {
        let id = |n: u64| {
            if n == 0 {
                JValue::Null
            } else {
                JValue::Num(n as f64)
            }
        };
        let v = JValue::obj([
            ("job", st.state.get("job").clone()),
            (
                "object",
                if st.state.get("job").truthy() {
                    id(st.job_object)
                } else {
                    JValue::Null
                },
            ),
            ("run", JValue::Num(st.run as f64)),
            ("starting", JValue::Bool(st.starting)),
            ("cancelled", JValue::Bool(st.cancelled)),
            ("child", id(st.child_id)),
            ("inflight", id(st.inflight_id)),
            ("idle", id(st.idle_id)),
        ]);
        Self::digest(st, &v)
    }

    fn cancellation_of(st: &St<D::Context>) -> JValue {
        let job = st.state.get("job");
        JValue::obj([
            ("token", JValue::from(Self::cancel_token(st))),
            ("generation", JValue::from(Self::cancel_generation(st))),
            (
                "job",
                if job.is_nullish() {
                    JValue::Null
                } else {
                    pick(job, &PUBLIC_FIELDS, hidden_strict)
                },
            ),
        ])
    }

    /// `cancellationFacts()`: `{ token, generation, job }`.
    pub fn cancellation_facts(&self) -> JValue {
        Self::cancellation_of(&self.st())
    }

    /// `compareAndAbort(expected)` (#2294): cancels the run the token was read from: the stage's
    /// calibrator, the request in flight and the wait for chat to go idle. The comparison and the
    /// flag are one critical section (Node's one synchronous turn).
    pub fn compare_and_abort(&self, expected: &str) -> JValue {
        let (before, child, inflight, idle) = {
            let mut st = self.st();
            if expected != Self::cancel_token(&st) || !st.state.get("job").truthy() {
                return not_applied();
            }
            let before = Self::cancellation_of(&st);
            st.cancelled = true;
            (
                before,
                st.child.clone(),
                st.inflight.clone(),
                st.idle_abort.clone(),
            )
        };
        match child {
            Some(Child::Stage(c)) => c.abort(),
            Some(Child::Probe(c)) => c.cancel_probe(),
            None => {}
        }
        if let Some(a) = inflight {
            a.abort();
        }
        if let Some(a) = idle {
            a.abort();
        }
        JValue::obj([
            ("applied", JValue::Bool(true)),
            ("generation", before.get("generation").clone()),
            ("job", before.get("job").clone()),
        ])
    }

    /// `statusFacts(keys)` (#2306): the job, the history of at most two keys and their switches.
    pub fn status_facts(&self, keys: &[String]) -> Result<JValue, Fail> {
        if keys.len() > 2 || keys.iter().any(|k| wtf16::len(k) > 65536) {
            return Err(Fail::new("Invalid status keys"));
        }
        const FIELDS: [&str; 21] = [
            "id",
            "model",
            "bulk",
            "mode",
            "promptBudgetSeconds",
            "status",
            "phase",
            "startedAt",
            "log",
            "models",
            "queueProgress",
            "finishedAt",
            "error",
            "failedPhase",
            "failedStep",
            "restored",
            "skipped",
            "waiting",
            "result",
            "queue",
            "steps",
        ];
        const HISTORY: [&str; 20] = [
            "at",
            "kv",
            "context",
            "plan",
            "probes",
            "kvFallback",
            "spec",
            "specLabel",
            "generation",
            "acceptance",
            "ubatch",
            "promptPerSecond",
            "quality",
            "extensions",
            "sampling",
            "baseline",
            "serving",
            "loaded",
            "version",
            "signature",
        ];
        let st = self.st();
        let job = st.state.get("job");
        if !job.is_nullish() && !job.is_object() {
            return Err(Fail::new("Invalid job metadata"));
        }
        let mut entries = Vec::new();
        for key in keys {
            let history = st.state.get("history");
            let raw = if js::has(history, key) {
                history.get(key).clone()
            } else {
                JValue::Null
            };
            let history = match &raw {
                v if !v.truthy() => v.clone(),
                JValue::Arr(rows) => {
                    if rows.iter().any(|r| !r.is_object()) {
                        return Err(Fail::new("Invalid history metadata"));
                    }
                    JValue::Arr(
                        rows.iter()
                            .map(|r| pick(r, &HISTORY, hidden_status))
                            .collect(),
                    )
                }
                _ => return Err(Fail::new("Invalid history metadata")),
            };
            let settings = st.state.get("settings");
            let allow =
                js::has(settings, key) && settings.get(key).get("allowQ5Kv") == &JValue::Bool(true);
            entries.push(JValue::obj([
                ("key", JValue::from(key.as_str())),
                ("history", history),
                ("allowQ5Kv", JValue::Bool(allow)),
            ]));
        }
        Ok(JValue::obj([
            (
                "jobPresent",
                JValue::Bool(!matches!(job, JValue::Undefined)),
            ),
            (
                "job",
                if job.is_nullish() {
                    job.clone()
                } else {
                    pick(job, &FIELDS, hidden_status)
                },
            ),
            ("entries", JValue::Arr(entries)),
            ("allowBelowQ5Kv", JValue::Bool(self.deps().allow_below_q5())),
        ]))
    }

    fn settings_token(&self, st: &St<D::Context>, model: &str) -> (String, JValue) {
        let profile = self.deps().presets_get(model);
        let job = st.state.get("job");
        let job_guard = if job.truthy() {
            let models = match job.get("models") {
                JValue::Arr(items) => JValue::Arr(
                    items
                        .iter()
                        .map(|m| {
                            if m.truthy() {
                                JValue::obj([
                                    ("model", m.get("model").clone()),
                                    ("base", m.get("base").clone()),
                                    ("status", m.get("status").clone()),
                                ])
                            } else {
                                m.clone()
                            }
                        })
                        .collect(),
                ),
                other => other.clone(),
            };
            JValue::obj([
                ("id", job.get("id").clone()),
                ("startedAt", job.get("startedAt").clone()),
                ("status", job.get("status").clone()),
                ("models", models),
            ])
        } else {
            JValue::Null
        };
        let allow = st.state.get("settings").get(model).get("allowQ5Kv") == &JValue::Bool(true);
        let guard = JValue::obj([
            ("configured", JValue::Bool(profile.get("exists").truthy())),
            (
                "system",
                JValue::Bool(self.deps().is_system_model(model, None)),
            ),
            ("revision", string_or_null(profile.get("revision"))),
            ("starting", JValue::Bool(st.starting)),
            ("job", job_guard),
            ("allowQ5Kv", JValue::Bool(allow)),
            ("allowBelowQ5Kv", JValue::Bool(self.deps().allow_below_q5())),
        ]);
        let generation = st.settings_generation;
        let salt = st.salt.clone();
        let token = hash(&JValue::obj([
            ("process", JValue::from(salt)),
            ("generation", JValue::Num(generation as f64)),
            ("model", JValue::from(model)),
            ("guard", guard.clone()),
        ]));
        (token, guard)
    }

    fn settings_of(&self, st: &St<D::Context>, model: &str) -> JValue {
        let (token, guard) = self.settings_token(st, model);
        let history = st.state.get("history");
        let list = |k: &str| {
            let v = history.get(k);
            if v.truthy() {
                v.clone()
            } else {
                JValue::Arr(Vec::new())
            }
        };
        JValue::obj([
            ("token", JValue::from(token)),
            ("guard", guard),
            ("job", crate::phases::public_job(st.state.get("job"))),
            ("history", list(model)),
            (
                "longHistory",
                list(&format!("{model}{}", crate::runner::LONG_SUFFIX)),
            ),
        ])
    }

    /// `settingsFacts(model)` (#2287): the token Rust's settings route compares, and the views.
    pub fn settings_facts(&self, model: &str) -> JValue {
        let st = self.st();
        self.settings_of(&st, model)
    }

    /// `commitSettings(model, allowQ5Kv, expected)`: the one map change, only over the exact state
    /// the token was read from (compared and changed under one lock).
    pub fn commit_settings(
        &self,
        model: &str,
        allow_q5: bool,
        expected: &str,
    ) -> Result<JValue, Fail> {
        let mut st = self.st();
        if model.is_empty()
            || wtf16::len(model) > 200
            || self.settings_token(&st, model).0 != expected
        {
            return Ok(not_applied());
        }
        let had = js::has(st.state.get("settings"), model);
        let prior = st.state.get("settings").get(model).clone();
        if let Some(settings) = js::get_mut(&mut st.state, "settings") {
            if allow_q5 {
                js::set(
                    settings,
                    model,
                    JValue::obj([("allowQ5Kv", JValue::Bool(true))]),
                );
            } else {
                js::del(settings, model);
            }
        }
        if let Err(e) = self.save_locked(&mut st) {
            if let Some(settings) = js::get_mut(&mut st.state, "settings") {
                if had {
                    js::set(settings, model, prior);
                } else {
                    js::del(settings, model);
                }
            }
            return Err(e);
        }
        let facts = self.settings_of(&st, model);
        Ok(JValue::obj([
            ("applied", JValue::Bool(true)),
            ("facts", facts),
        ]))
    }

    fn clear_of(st: &St<D::Context>) -> JValue {
        let job = st.state.get("job");
        let generation = hash(&JValue::obj([
            ("process", JValue::from(st.salt.as_str())),
            ("generation", JValue::Num(st.clear_generation as f64)),
        ]));
        let guard = JValue::obj([
            ("starting", JValue::Bool(st.starting)),
            ("jobPresent", JValue::Bool(job.truthy())),
            (
                "status",
                if job.get("status").is_nullish() {
                    JValue::Null
                } else {
                    job.get("status").clone()
                },
            ),
            (
                "reloadPending",
                JValue::Bool(job.get("_reloadPending").truthy()),
            ),
        ]);
        let token = hash(&JValue::obj([
            ("generation", JValue::from(generation.as_str())),
            ("guard", guard.clone()),
            (
                "job",
                if job.is_nullish() {
                    JValue::Null
                } else {
                    job.clone()
                },
            ),
        ]));
        let mut out = JValue::obj([
            ("generation", JValue::from(generation)),
            ("token", JValue::from(token)),
        ]);
        for (k, v) in js::entries(&guard).into_iter().flatten() {
            js::set(&mut out, k, v.clone());
        }
        out
    }

    /// `clearFacts()` (#1978).
    pub fn clear_facts(&self) -> JValue {
        Self::clear_of(&self.st())
    }

    /// `compareAndClear(token)`: drops a finished job record (history, settings and models.ini
    /// stay), never an active one or one whose restore the router has not re-read. Compared and
    /// removed under one lock.
    pub fn compare_and_clear(&self, token: &str) -> Result<JValue, Fail> {
        let mut st = self.st();
        if Self::clear_of(&st).get("token").as_str() != Some(token) {
            return Ok(not_applied());
        }
        let job = st.state.get("job").clone();
        let finished = matches!(
            job.get("status").as_str(),
            Some("failed" | "cancelled" | "interrupted" | "passed")
        );
        if st.starting
            || !job.truthy()
            || !finished
            || job.get("_reloadPending").truthy()
            || job.get("_recovery").truthy()
        {
            return Ok(not_applied());
        }
        js::del(&mut st.state, "job");
        if let Err(e) = self.save_locked(&mut st) {
            js::set(&mut st.state, "job", job);
            return Err(e);
        }
        Ok(JValue::obj([("applied", JValue::Bool(true))]))
    }

    /// `armStartupRecovery(j)`: a trial an earlier process left unrestored keeps chat closed from
    /// the start (called while the actor is built, before the listener admits chat).
    pub(crate) fn arm_startup_recovery(&self) -> Result<bool, Fail> {
        let j = self.job();
        if !j.truthy() || !self.deps().recoverable() {
            return Ok(false);
        }
        if !j.get("_recovery").truthy() {
            let phase_text = js::items(j.get("models"))
                .iter()
                .flat_map(|m| js::items(m.get("phases")).iter())
                .find(|p| !p.get("_beforeText").is_nullish())
                .map(|p| p.get("_beforeText").clone());
            let text = phase_text.unwrap_or_else(|| j.get("originalText").clone());
            if !text.is_nullish() || j.get("_reloadPending").truthy() {
                let revision = [
                    j.get("_pendingRevision"),
                    j.get("_revision"),
                    j.get("lastRevision"),
                ]
                .into_iter()
                .find(|v| v.truthy())
                .cloned()
                .unwrap_or_else(|| j.get("lastRevision").clone());
                let text = if text.is_nullish() {
                    JValue::from(self.deps().presets_snapshot().0)
                } else {
                    text
                };
                let rec = JValue::obj([
                    ("revision", revision),
                    ("text", text),
                    ("backedUp", JValue::Bool(j.get("_backedUp").truthy())),
                ]);
                self.with_job(|j| js::set(j, "_recovery", rec));
                self.save()?;
            }
        }
        if !self.job().get("_recovery").truthy() {
            return Ok(false);
        }
        self.require_trial_recovery();
        Ok(true)
    }

    /// `recover()`: a run a restart interrupted is marked so (its open phase restored when that is
    /// still safe); called once at boot.
    pub async fn recover(&self) -> Result<(), Fail> {
        if self.arm_startup_recovery()? {
            if self.job().get("status").as_str() == Some("running") {
                let now = self.deps().now();
                self.with_job(|j| {
                    if let Some(items) = js::get_mut(j, "models").and_then(js::items_mut) {
                        for item in items
                            .iter_mut()
                            .filter(|i| i.get("status").as_str() == Some("running"))
                        {
                            js::set(item, "status", "interrupted");
                            if let Some(phases) =
                                js::get_mut(item, "phases").and_then(js::items_mut)
                            {
                                for p in phases
                                    .iter_mut()
                                    .filter(|p| p.get("status").as_str() == Some("running"))
                                {
                                    js::set(p, "status", "interrupted");
                                    js::set(p, "reason", "Restart interrupted this phase.");
                                    if let Some(steps) =
                                        js::get_mut(p, "steps").and_then(js::items_mut)
                                    {
                                        for s in steps
                                            .iter_mut()
                                            .filter(|s| s.get("status").as_str() == Some("running"))
                                        {
                                            js::set(s, "status", "interrupted");
                                        }
                                    }
                                }
                            }
                        }
                    }
                    crate::phases::stopped_at(j);
                    js::set(j, "status", "interrupted");
                    js::set(j, "phase", "Interrupted");
                    js::set(j, "finishedAt", JValue::Num(now));
                    js::set(j, "restored", false);
                    js::set(
                        j,
                        "error",
                        "Restart interrupted tuning. Completed settings remain saved.",
                    );
                });
                self.save()?;
            }
            self.deps().recover_now().await;
            let j = self.job();
            if j.get("_recovery").truthy() && j.get("restored") == &JValue::Bool(false) {
                self.with_job(|j| {
                    js::set(
                        j,
                        "error",
                        "Restart interrupted tuning; settings could not safely be restored. Inspect models.ini before resuming.",
                    )
                });
                self.save()?;
            }
            return Ok(());
        }
        let j = self.job();
        if j.get("_reloadPending").truthy() {
            self.schedule_reload(0);
        }
        if j.truthy() {
            self.adopt_pending_revision()?;
        }
        let j = self.job();
        if j.get("status").as_str() != Some("running") {
            return Ok(());
        }
        if !matches!(j.get("models"), JValue::Arr(_)) {
            return self.recover_pre_v3().await;
        }
        let item = js::items(j.get("models"))
            .iter()
            .position(|m| m.get("status").as_str() == Some("running"));
        let phase = item.and_then(|mi| {
            js::items(self.item(mi).get("phases"))
                .iter()
                .position(|p| p.get("status").as_str() == Some("running"))
                .map(|pi| (mi, pi))
        });
        if let Some((mi, pi)) = phase {
            let restored = self.restore_phase(mi, pi).await?;
            self.with_phase(mi, pi, |p| {
                js::set(p, "status", "interrupted");
                js::set(p, "reason", "Restart interrupted this phase.");
                if let Some(steps) = js::get_mut(p, "steps").and_then(js::items_mut) {
                    for s in steps
                        .iter_mut()
                        .filter(|s| s.get("status").as_str() == Some("running"))
                    {
                        js::set(s, "status", "interrupted");
                        js::set(s, "reason", "Restart interrupted this phase.");
                    }
                }
            });
            if !restored {
                self.with_job(|j| js::set(j, "_unsafe", true));
            }
            self.with_item(mi, |i| js::set(i, "status", "interrupted"));
        } else if let Some(mi) = item {
            self.with_item(mi, |i| js::set(i, "status", "interrupted"));
        }
        let now = self.deps().now();
        self.with_job(|j| {
            js::set(j, "status", "interrupted");
            js::set(j, "phase", "Interrupted");
            js::set(j, "finishedAt", JValue::Num(now));
            crate::phases::stopped_at(j);
            let message = if j.get("_unsafe").truthy() {
                "Restart interrupted tuning; settings could not safely be restored. Inspect models.ini before resuming."
            } else {
                "Restart interrupted tuning. Completed settings remain saved."
            };
            js::set(j, "error", message);
        });
        self.save()
    }

    /// A pre-v3 job (one snapshot, no queue): restored if it still can be, never resumed.
    async fn recover_pre_v3(&self) -> Result<(), Fail> {
        let restored: Result<(), Fail> = async {
            let j = self.job();
            let (_, revision) = self.deps().presets_snapshot();
            if !j.get("originalText").truthy()
                || !j.get("lastRevision").truthy()
                || j.get("lastRevision").as_str() != Some(revision.as_str())
            {
                return Err(Fail::new("The profile changed after the run."));
            }
            let unloaded = self
                .request(
                    crate::Call::post(
                        "/models/unload",
                        JValue::obj([("model", j.get("model").clone())]),
                        60000,
                    ),
                    None,
                )
                .await?;
            if !unloaded.get("ok").truthy() {
                return Err(Fail::new("Could not unload the test model."));
            }
            self.deps()
                .presets_commit(
                    JValue::obj([
                        ("baseRevision", j.get("lastRevision").clone()),
                        ("text", j.get("originalText").clone()),
                    ]),
                    JValue::Undefined,
                )
                .await?;
            if !self.deps().invalidate_baseline("autotune").await {
                self.with_job(|j| js::set(j, "_reloadPending", true));
                return Err(Fail::new("The router did not confirm restored settings."));
            }
            let reload = self.reread().await?;
            if !reload.get("ok").truthy() {
                return Err(Fail::new("The router did not confirm restored settings."));
            }
            Ok(())
        }
        .await;
        let ok = restored.is_ok();
        self.with_job(|j| {
            js::set(j, "restored", ok);
            js::del(j, "originalText");
            js::set(j, "status", "interrupted");
            js::set(
                j,
                "error",
                if ok {
                    "An older tuning run was interrupted and restored. Start a new tune."
                } else {
                    "An older tuning run was interrupted; settings could not safely be restored. Inspect models.ini before tuning again."
                },
            );
        });
        self.save()?;
        if self.job().get("_reloadPending").truthy() {
            self.schedule_reload(0);
        }
        Ok(())
    }

    fn resume_outside(&self, st: &St<D::Context>) -> JValue {
        let (_, revision) = self.deps().presets_snapshot();
        let system: Vec<JValue> = js::items(st.state.get("job").get("models"))
            .iter()
            .map(|item| {
                JValue::Bool(
                    item.get("model")
                        .as_str()
                        .is_some_and(|m| self.deps().is_system_model(m, None)),
                )
            })
            .collect();
        JValue::obj([
            ("process", JValue::from(st.salt.as_str())),
            ("job", JValue::Num(st.job_object as f64)),
            ("completion", JValue::Num(st.run as f64)),
            ("starting", JValue::Bool(st.starting)),
            ("cancelled", JValue::Bool(st.cancelled)),
            ("revision", JValue::from(revision)),
            ("system", JValue::Arr(system)),
        ])
    }

    fn resume_token(&self, st: &St<D::Context>) -> String {
        let mut v = self.resume_outside(st);
        js::set(
            &mut v,
            "generation",
            JValue::Num(st.resume_generation as f64),
        );
        js::set(&mut v, "state", st.state.clone());
        hash(&v)
    }

    fn resume_queue(&self, j: &JValue) -> JValue {
        let JValue::Arr(items) = j.get("models") else {
            return JValue::Null;
        };
        JValue::Arr(
            items
                .iter()
                .map(|item| {
                    if !item.is_object() {
                        return JValue::Null;
                    }
                    let identity = item.get("_identity");
                    let hex64 = identity.as_str().is_some_and(|s| {
                        s.len() == 64
                            && s.bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    });
                    let configured = |k: &str| {
                        item.get(k)
                            .as_str()
                            .is_some_and(|m| self.deps().presets_get(m).get("exists").truthy())
                    };
                    let phases = match item.get("phases") {
                        JValue::Arr(ps) => JValue::Arr(
                            ps.iter()
                                .map(|p| {
                                    if !p.is_object() {
                                        return JValue::Null;
                                    }
                                    JValue::obj([
                                        ("id", string_or_null(p.get("id"))),
                                        ("status", status_of(p.get("status"))),
                                        (
                                            "restored",
                                            match p.get("restored") {
                                                JValue::Bool(b) => JValue::Bool(*b),
                                                _ => JValue::Null,
                                            },
                                        ),
                                        (
                                            "steps",
                                            match p.get("steps") {
                                                JValue::Arr(ss) => JValue::Arr(
                                                    ss.iter()
                                                        .map(|s| {
                                                            JValue::obj([(
                                                                "status",
                                                                status_of(s.get("status")),
                                                            )])
                                                        })
                                                        .collect(),
                                                ),
                                                _ => JValue::Null,
                                            },
                                        ),
                                    ])
                                })
                                .collect(),
                        ),
                        _ => JValue::Null,
                    };
                    JValue::obj([
                        ("model", string_or_null(item.get("model"))),
                        ("base", string_or_null(item.get("base"))),
                        ("mode", string_or_null(item.get("mode"))),
                        ("status", status_of(item.get("status"))),
                        (
                            "sectionCreated",
                            JValue::Bool(item.get("sectionCreated").truthy()),
                        ),
                        ("identitySet", JValue::Bool(identity.truthy())),
                        (
                            "identity",
                            if hex64 {
                                identity.clone()
                            } else {
                                JValue::Null
                            },
                        ),
                        ("planned", JValue::Bool(item.get("plan").truthy())),
                        ("configured", JValue::Bool(configured("model"))),
                        ("baseConfigured", JValue::Bool(configured("base"))),
                        ("phases", phases),
                    ])
                })
                .collect(),
        )
    }

    fn resume_facts_of(&self, st: &St<D::Context>) -> JValue {
        let j = st.state.get("job");
        let (_, revision) = self.deps().presets_snapshot();
        let generation = hash(&JValue::obj([
            ("process", JValue::from(st.salt.as_str())),
            ("job", JValue::Num(st.job_object as f64)),
            ("completion", JValue::Num(st.run as f64)),
        ]));
        let job = if j.truthy() {
            JValue::obj([
                ("status", status_of(j.get("status"))),
                ("model", string_or_null(j.get("model"))),
                ("unsafe", JValue::Bool(j.get("_unsafe").truthy())),
                ("revision", string_or_null(j.get("_revision"))),
                ("pendingRevision", string_or_null(j.get("_pendingRevision"))),
                ("models", self.resume_queue(j)),
            ])
        } else {
            JValue::Null
        };
        JValue::obj([
            ("token", JValue::from(self.resume_token(st))),
            ("generation", JValue::from(generation)),
            ("starting", JValue::Bool(st.starting)),
            ("revision", JValue::from(revision)),
            ("job", job),
        ])
    }

    /// `resumeFacts()` (#2301).
    pub fn resume_facts(&self) -> JValue {
        let st = self.st();
        self.resume_facts_of(&st)
    }

    // Indices into `models` are checked against its length before use.
    #[allow(clippy::indexing_slicing)]
    /// `prepareResume(expected, { stage, selectors })`: the listing or identity facts the front's
    /// resume policy needs, read while the actor is held `starting` so nothing else changes it.
    /// prepareResume's entry, in one critical section: the token compared, the request checked,
    /// and `resumePreparing`/`starting` taken. `Err(answer)` is the reply to give at once.
    #[allow(clippy::type_complexity)]
    fn prepare_entry(
        &self,
        expected: &str,
        stage: &str,
        selectors: &JValue,
    ) -> Result<(Vec<JValue>, Vec<(usize, String)>, u64, u64, String), Result<JValue, Fail>> {
        let mut st = self.st();
        if st.resume_preparing || st.starting || !st.state.get("job").truthy() {
            return Err(Ok(not_applied()));
        }
        if expected != self.resume_token(&st) {
            return Err(Ok(not_applied()));
        }
        let selectors = match selectors {
            JValue::Undefined => Vec::new(),
            JValue::Arr(s) if s.len() <= 512 => s.clone(),
            _ => return Err(Err(Fail::new("Invalid preparation request"))),
        };
        if stage != "listing" && stage != "identities" {
            return Err(Err(Fail::new("Invalid preparation request")));
        }
        let models = match st.state.get("job").get("models") {
            JValue::Arr(m) if m.len() <= 512 => m.clone(),
            _ => return Err(Err(Fail::new("Resume queue exceeds preparation bounds"))),
        };
        let mut picks = Vec::new();
        for pick in &selectors {
            let index = pick
                .get("index")
                .as_num()
                .filter(|n| n.fract() == 0.0 && *n >= 0.0 && *n < models.len() as f64);
            let field = pick
                .get("field")
                .as_str()
                .filter(|f| *f == "model" || *f == "base");
            let (Some(index), Some(field)) = (index, field) else {
                return Err(Err(Fail::new("Invalid queued identity request")));
            };
            let index = index as usize;
            if picks.iter().any(|(i, _)| *i == index) || models[index].get(field).as_str().is_none()
            {
                return Err(Err(Fail::new("Invalid queued identity request")));
            }
            picks.push((index, field.to_string()));
        }
        // Checked and taken in one critical section; given back by `Preparing` however this ends
        // (Node's `finally`), also when the caller drops this future.
        st.resume_preparing = true;
        st.starting = true;
        let (job_object, run) = (st.job_object, st.run);
        let baseline = self.resume_token(&st);
        Ok((models, picks, job_object, run, baseline))
    }

    pub async fn prepare_resume(
        &self,
        expected: &str,
        stage: &str,
        selectors: &JValue,
    ) -> Result<JValue, Fail> {
        let (models, picks, job_object, run, mut baseline) =
            match self.prepare_entry(expected, stage, selectors) {
                Ok(entry) => entry,
                Err(answer) => return answer,
            };
        let _preparing = Preparing(self);
        let started = std::time::Instant::now();
        let stale = || Fail {
            code: Some("resume_stale".into()),
            ..Fail::new("Resume state changed")
        };
        let current = |baseline: &str| -> Result<(), Fail> {
            let st = self.st();
            let held =
                st.resume_preparing && st.starting && st.job_object == job_object && st.run == run;
            if started.elapsed() >= PREPARE_LIMIT || !held || self.resume_token(&st) != baseline {
                return Err(stale());
            }
            Ok(())
        };
        // Node races each wait against the deadline (`wait()`): a read that hangs ends at it. The
        // flush below is not raced: dropping it half done would leave its re-read unrecorded
        // (Node's flush keeps running after its race is lost); its own router call is bounded.

        let outcome: Result<JValue, Fail> = async {
            current(&baseline)?;
            if stage == "listing" {
                self.adopt_pending_revision()?;
                baseline = self.resume_token(&self.st());
                let rows = self
                    .by_deadline(started, Box::pin(self.raw_models()))
                    .await
                    .ok_or_else(stale)?
                    .ok();
                current(&baseline)?;
                let data = rows
                    .as_ref()
                    .filter(|r| r.get("ok").truthy())
                    .and_then(|r| match r.get("body").get("data") {
                        JValue::Arr(d) if d.len() <= 8192 => Some(d.clone()),
                        _ => None,
                    });
                let listing = match data {
                    None => JValue::Null,
                    Some(data) => JValue::Arr(
                        models
                            .iter()
                            .enumerate()
                            .map(|(index, item)| {
                                let row = data.iter().find(|r| r.get("id") == item.get("model"));
                                let path = row.and_then(|r| {
                                    crate::model_path_from_args(r.get("status").get("args"))
                                });
                                let id = string_of(item.get("model"));
                                JValue::obj([
                                    ("index", JValue::Num(index as f64)),
                                    (
                                        "system",
                                        JValue::Bool(
                                            self.deps().is_system_model(&id, path.as_deref()),
                                        ),
                                    ),
                                    (
                                        "modelListed",
                                        JValue::Bool(
                                            data.iter().any(|r| r.get("id") == item.get("model")),
                                        ),
                                    ),
                                    (
                                        "baseListed",
                                        JValue::Bool(
                                            data.iter().any(|r| r.get("id") == item.get("base")),
                                        ),
                                    ),
                                ])
                            })
                            .collect(),
                    ),
                };
                return Ok(JValue::obj([("models", listing)]));
            }
            let owed = self.job().get("_reloadPending").truthy();
            let before_generation = self.st().resume_generation;
            let b = baseline.clone();
            self.flush_reload_guarded(None, &|| current(&b)).await?;
            if owed {
                // The only change allowed across the flush is its one note.
                let last = js::items(self.job().get("log")).last().cloned();
                if last.as_ref().and_then(|n| n.get("text").as_str())
                    != Some("The model server re-read the restored models.ini.")
                    || self.st().resume_generation != before_generation + 1
                {
                    return Err(stale());
                }
                baseline = self.resume_token(&self.st());
            } else {
                current(&baseline)?;
            }
            current(&baseline)?;
            let mut identities = Vec::new();
            for (index, field) in &picks {
                let id = string_of(
                    models
                        .get(*index)
                        .map_or(&JValue::Undefined, |m| m.get(field)),
                );
                let identity = self
                    .by_deadline(started, Box::pin(self.deps().identity_for(&id)))
                    .await
                    .ok_or_else(stale)?
                    .ok()
                    .flatten();
                current(&baseline)?;
                identities.push(JValue::obj([
                    ("index", JValue::Num(*index as f64)),
                    ("field", JValue::from(field.as_str())),
                    (
                        "identity",
                        match identity.filter(JValue::truthy) {
                            Some(id) => JValue::from(crate::stable_identity(&id)),
                            None => JValue::Null,
                        },
                    ),
                ]));
            }
            Ok(JValue::obj([("identities", JValue::Arr(identities))]))
        }
        .await;
        drop(_preparing);
        let result = match outcome {
            Ok(r) => r,
            Err(e) if e.code.as_deref() == Some("resume_stale") => return Ok(not_applied()),
            Err(e)
                if e.public_message.as_deref()
                    == Some("The model server has not re-read the restored settings yet. Try again shortly.") =>
            {
                return Ok(JValue::obj([("applied", JValue::Bool(false)), ("fault", JValue::from("reload"))]));
            }
            Err(e) => return Err(e),
        };
        {
            let st = self.st();
            if st.job_object != job_object || st.run != run {
                return Ok(not_applied());
            }
        }
        let mut out = JValue::obj([("applied", JValue::Bool(true))]);
        for (k, v) in js::entries(&self.resume_facts()).into_iter().flatten() {
            js::set(&mut out, k, v.clone());
        }
        for (k, v) in js::entries(&result).into_iter().flatten() {
            js::set(&mut out, k, v.clone());
        }
        Ok(out)
    }

    // Indices into `models`, phases and steps are validated (`indices`) before any use.
    #[allow(clippy::indexing_slicing)]
    /// `compareAndResume(expected, plan)`: a filter of the queue, or a relaunch with the given items,
    /// phases and steps reset.
    pub fn compare_and_resume(&self, expected: &str, plan: &JValue) -> Result<JValue, Fail> {
        // Compared, validated, changed and saved under one lock (Node's one synchronous turn).
        let mut st = self.st();
        if st.resume_preparing || st.starting || !st.state.get("job").truthy() {
            return Ok(not_applied());
        }
        if expected != self.resume_token(&st) {
            return Ok(not_applied());
        }
        let j = st.state.get("job").clone();
        let models = match j.get("models") {
            JValue::Arr(m) if m.len() <= 512 && plan.is_object() => m.clone(),
            _ => return Err(Fail::new("Invalid resume patch")),
        };
        let kind = plan.get("kind").as_str().unwrap_or("");
        if kind == "filter" {
            let keep = indices(plan.get("keep"), models.len());
            let model_ok = match plan.get("model") {
                JValue::Null => true,
                JValue::Num(n) => {
                    n.fract() == 0.0
                        && keep
                            .as_ref()
                            .is_some_and(|k| k.iter().any(|i| *i as f64 == *n))
                }
                _ => false,
            };
            let (Some(keep), JValue::Arr(skipped), JValue::Bool(_), true) =
                (keep, plan.get("skipped"), plan.get("complete"), model_ok)
            else {
                return Err(Fail::new("Invalid queue patch"));
            };
            let removed: Vec<usize> = (0..models.len()).filter(|i| !keep.contains(i)).collect();
            if skipped.len() != removed.len()
                || skipped.iter().zip(&removed).any(|(p, i)| {
                    p.get("index").as_num() != Some(*i as f64)
                        || p.get("reason").as_str().is_none_or(|r| wtf16::len(r) > 256)
                })
            {
                return Err(Fail::new("Invalid skipped queue patch"));
            }
            // `[...(j.skipped || []), ...]`: a truthy non-iterable throws (Node then rolls back).
            let earlier: Vec<JValue> = match j.get("skipped") {
                JValue::Arr(list) => list.clone(),
                JValue::Str(text) => text.chars().map(|c| JValue::from(c.to_string())).collect(),
                v if !v.truthy() => Vec::new(),
                _ => return Err(Fail::new("j.skipped is not iterable")),
            };
            let now = self.deps().now();
            if let Some(j) = js::get_mut(&mut st.state, "job") {
                js::set(
                    j,
                    "models",
                    JValue::Arr(keep.iter().map(|i| models[*i].clone()).collect()),
                );
                let mut list = earlier;
                list.extend(skipped.iter().map(|p| {
                    let i = p.get("index").as_num().unwrap_or(0.0) as usize;
                    JValue::obj([
                        ("model", models[i].get("model").clone()),
                        ("reason", p.get("reason").clone()),
                    ])
                }));
                js::set(j, "skipped", JValue::Arr(list));
                if let Some(n) = plan.get("model").as_num() {
                    js::set(j, "model", models[n as usize].get("model").clone());
                }
                if plan.get("complete") == &JValue::Bool(true) {
                    js::set(j, "status", "passed");
                    js::set(j, "phase", "Done");
                    js::set(j, "finishedAt", JValue::Num(now));
                    js::set(j, "error", JValue::Undefined);
                }
            }
        } else if kind == "launch" {
            let JValue::Arr(reset) = plan.get("reset") else {
                return Err(Fail::new("Invalid item reset patch"));
            };
            let index_list = JValue::Arr(reset.iter().map(|p| p.get("index").clone()).collect());
            if indices(&index_list, models.len()).is_none() {
                return Err(Fail::new("Invalid item reset patch"));
            }
            for item in reset {
                let phases =
                    models[item.get("index").as_num().unwrap_or(0.0) as usize].get("phases");
                let JValue::Arr(ps) = phases else {
                    return Err(Fail::new("Invalid phase reset patch"));
                };
                let JValue::Arr(patches) = item.get("phases") else {
                    return Err(Fail::new("Invalid phase reset patch"));
                };
                let list = JValue::Arr(patches.iter().map(|p| p.get("index").clone()).collect());
                if indices(&list, ps.len()).is_none() {
                    return Err(Fail::new("Invalid phase reset patch"));
                }
                for p in patches {
                    let steps = ps[p.get("index").as_num().unwrap_or(0.0) as usize].get("steps");
                    let JValue::Arr(st) = steps else {
                        return Err(Fail::new("Invalid step reset patch"));
                    };
                    if !(matches!(p.get("keepSteps"), JValue::Null)
                        || indices(p.get("keepSteps"), st.len()).is_some())
                    {
                        return Err(Fail::new("Invalid step reset patch"));
                    }
                }
            }
            if let Some(j) = js::get_mut(&mut st.state, "job") {
                js::set(j, "status", "running");
                js::set(j, "error", JValue::Undefined);
                js::set(j, "finishedAt", JValue::Undefined);
                js::del(j, "failedPhase");
                js::del(j, "failedStep");
                for item in reset {
                    let mi = item.get("index").as_num().unwrap_or(0.0) as usize;
                    let Some(row) = js::get_mut(j, "models")
                        .and_then(js::items_mut)
                        .and_then(|m| m.get_mut(mi))
                    else {
                        continue;
                    };
                    js::set(row, "status", "pending");
                    js::del(row, "error");
                    js::del(row, "_foreignYields");
                    for patch in js::items(item.get("phases")) {
                        let pi = patch.get("index").as_num().unwrap_or(0.0) as usize;
                        let Some(p) = js::get_mut(row, "phases")
                            .and_then(js::items_mut)
                            .and_then(|ps| ps.get_mut(pi))
                        else {
                            continue;
                        };
                        js::set(p, "status", "pending");
                        js::del(p, "reason");
                        js::del(p, "restored");
                        let steps = js::items(p.get("steps")).to_vec();
                        let next = match patch.get("keepSteps") {
                            JValue::Arr(keep) => keep
                                .iter()
                                .filter_map(|i| {
                                    steps.get(i.as_num().unwrap_or(0.0) as usize).cloned()
                                })
                                .collect(),
                            _ => steps
                                .iter()
                                .map(|s| {
                                    let mut s = js::spread(&[s]);
                                    js::set(&mut s, "status", "pending");
                                    js::set(&mut s, "reason", JValue::Undefined);
                                    s
                                })
                                .collect(),
                        };
                        js::set(p, "steps", JValue::Arr(next));
                    }
                }
            }
        } else {
            return Err(Fail::new("Invalid resume patch kind"));
        }
        if let Err(e) = self.save_locked(&mut st) {
            // The worker has not started: the live object goes back to the patch's starting point
            // (`Object.assign(j, JSON.parse(JSON.stringify(before)))`: `undefined` members gone).
            js::set(&mut st.state, "job", js::json_copy(&j));
            return Err(e);
        }
        if kind == "launch" {
            st.cancelled = false;
        }
        drop(st);
        if kind == "launch" {
            self.launch();
        }
        let mut out = JValue::obj([("applied", JValue::Bool(true))]);
        for (k, v) in js::entries(&self.resume_facts()).into_iter().flatten() {
            js::set(&mut out, k, v.clone());
        }
        js::set(
            &mut out,
            "publicJob",
            pick(&self.job(), &PUBLIC_FIELDS, hidden_strict),
        );
        Ok(out)
    }

    /// `fut`, unless prepareResume's deadline passes first (`None`).
    async fn by_deadline<T>(
        &self,
        started: std::time::Instant,
        fut: std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + '_>>,
    ) -> Option<T> {
        let left = PREPARE_LIMIT.saturating_sub(started.elapsed());
        let mut fut = fut;
        let timer = self
            .deps()
            .tick(u64::try_from(left.as_millis()).unwrap_or(u64::MAX));
        let mut timer = std::pin::pin!(timer);
        use std::future::Future as _;
        std::future::poll_fn(|cx| {
            if let std::task::Poll::Ready(v) = fut.as_mut().poll(cx) {
                return std::task::Poll::Ready(Some(v));
            }
            if timer.as_mut().poll(cx).is_ready() {
                return std::task::Poll::Ready(None);
            }
            std::task::Poll::Pending
        })
        .await
    }

    /// `flushReload(model)` for the routes and a start's caller.
    pub async fn flush_reload_public(&self, model: Option<&str>) -> Result<(), Fail> {
        self.flush_reload(model).await
    }

    /// `scanFacts()`: the untuned scan's per-row facts (Rust's untuned route applies the policy).
    pub async fn scan_facts_public(&self) -> Result<JValue, Fail> {
        self.scan_facts().await
    }

    /// `starting` (for `tuningActive` and the routes).
    pub fn starting(&self) -> bool {
        self.st().starting
    }

    /// The raw job record (with private keys), for the routes' own views.
    pub fn job_record(&self) -> JValue {
        self.job()
    }
}
