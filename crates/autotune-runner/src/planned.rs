//! The planned order (#1003; core llamacpp-full-autotune.cjs lines 1152-1466): the planner's
//! request, one fill-and-recall step at a time, the stages it opens and closes, and the fallback to
//! the standard order when a model cannot be planned.

use crate::deps::{Context as _, Deps, Fail, Release};
use crate::engine::string_of;
use crate::runner::{unsafe_restore, Child, Runner};
use crate::stages::{keys_of, spec_off, uint, BaselineOpts, Sized, SPEC_KEYS};
use crate::{js, kv, phases, quality};
use js_json::JValue;
use std::sync::Arc;

/// The most planner replies one model may take.
const PLAN_STEP_LIMIT: usize = 64;
const LOAD_TIMEOUT: &str = "The test profile did not finish loading in time twice, so auto-tune cannot judge this setting. Check the model server, then resume.";
const TEMPLATE_STOP: &str = "The engine could not use this model's chat template, so auto-tune stopped instead of trying smaller settings. Check the model's chat template, then resume.";
const FILL_FAILED: &str = "The fill-and-recall test failed.";

/// `PLAN_CAUSE[cause]`.
fn plan_cause(cause: &JValue) -> Option<&'static str> {
    match cause.as_str()? {
        "oom" => Some("oom"),
        "load" => Some("load_failed"),
        "time" => Some("over_time"),
        "recall" => Some("recall_failed"),
        _ => None,
    }
}

/// `layerList(v)`: a per-layer list (booleans or small integers) or one small integer.
fn layer_list(v: &JValue) -> JValue {
    let small = |x: &JValue| uint(x, f64::from(1u32 << 20)) == x.as_num().unwrap_or(f64::NAN);
    match v {
        JValue::Arr(items) => {
            if items.len() <= 4096
                && items
                    .iter()
                    .all(|x| matches!(x, JValue::Bool(_)) || small(x))
            {
                v.clone()
            } else {
                JValue::Null
            }
        }
        _ if small(v) => v.clone(),
        _ => JValue::Null,
    }
}

/// `{ ...a, k1: v1, ... }` with the pairs set in order.
fn with(base: &JValue, pairs: Vec<(&str, JValue)>) -> JValue {
    let mut out = js::spread(&[base]);
    for (k, v) in pairs {
        js::set(&mut out, k, v);
    }
    out
}

/// One fill-and-recall outcome (`fillCheck`'s answer).
struct Fill {
    outcome: String,
    reason: JValue,
    silent: bool,
    classification: JValue,
    prompt_seconds: JValue,
}

impl Fill {
    fn of(outcome: &str, reason: impl Into<JValue>) -> Self {
        Fill {
            outcome: outcome.to_string(),
            reason: reason.into(),
            silent: false,
            classification: JValue::Undefined,
            prompt_seconds: JValue::Undefined,
        }
    }
}

impl<D: Deps> Runner<D> {
    fn plan_of(&self, mi: usize) -> JValue {
        self.item(mi).get("plan").clone()
    }

    fn with_plan<R>(&self, mi: usize, f: impl FnOnce(&mut JValue) -> R) -> Option<R> {
        self.with_item(mi, |item| js::get_mut(item, "plan").map(f))
            .flatten()
    }

    /// `planRequest(model, kv)`: the model's facts, the budget (MemAvailable read now, every model
    /// unloaded), the ladder and the item's KV list; or `{ fallback }`.
    async fn plan_request(&self, model: &str, kvs: &[JValue]) -> Result<JValue, Fail> {
        let read = self.deps().plan_facts(model).await?;
        if !read.get("meta").truthy() {
            return Ok(JValue::obj([(
                "fallback",
                JValue::from("its model file could not be read"),
            )]));
        }
        let m = read.get("meta");
        let profile = self.deps().presets_get(model);
        let eff = js::spread(&[profile.get("defaults"), profile.get("options")]);
        let native = uint(m.get("contextLength"), f64::from(1u32 << 24));
        let cache_mib = {
            let (cap, hard) = self.deps().cache_ram_limits();
            crate::sizing::tune_cache_mib(&eff, cap, hard)?
        };
        if !cache_mib.is_finite() {
            return Ok(JValue::obj([(
                "fallback",
                JValue::from("its prompt cache is unbounded (cache-ram -1)"),
            )]));
        }
        let free = self.deps().read_memory();
        let u = |key: &str, max: f64| JValue::Num(uint(m.get(key), max));
        let b20 = f64::from(1u32 << 20);
        let b24 = f64::from(1u32 << 24);
        let ubatch = uint(&JValue::Num(js::number(eff.get("ubatch-size"))), b24)
            .max(f64::from(phases::UBATCH[2]));
        let slots = uint(&JValue::Num(js::number(eff.get("parallel"))), 1024.0).max(1.0);
        let p50 = 2f64.powi(50);
        let p30 = 2f64.powi(30);
        let facts = JValue::obj([
            ("nCtxTrain", JValue::Num(native)),
            ("blockCount", u("blockCount", 4096.0)),
            ("headCount", u("headCount", b20)),
            ("headCountKv", layer_list(m.get("headCountKv"))),
            ("embeddingLength", u("embeddingLength", b20)),
            ("keyLength", u("keyLength", b20)),
            ("valueLength", u("valueLength", b20)),
            ("keyLengthSwa", u("keyLengthSwa", b20)),
            ("valueLengthSwa", u("valueLengthSwa", b20)),
            ("slidingWindow", u("slidingWindow", b24)),
            (
                "slidingWindowPattern",
                layer_list(m.get("slidingWindowPattern")),
            ),
            ("sharedKvLayers", u("sharedKvLayers", 4096.0)),
            ("fullAttentionInterval", u("fullAttentionInterval", 4096.0)),
            ("nextnPredictLayers", u("nextnPredictLayers", 4096.0)),
            ("modelBytes", JValue::Num(uint(read.get("modelBytes"), p50))),
            (
                "mmprojBytes",
                JValue::Num(uint(read.get("mmprojBytes"), p50)),
            ),
            ("ubatch", JValue::Num(ubatch)),
            ("slots", JValue::Num(slots)),
        ]);
        let config = self.config();
        let budget = (self.deps().budget_gib()? * 1024.0).floor();
        let memory = JValue::obj([
            ("budgetMib", JValue::Num(uint(&JValue::Num(budget), p30))),
            (
                "memAvailableMib",
                match free {
                    None => JValue::Null,
                    Some(free) => JValue::Num(uint(&JValue::Num((free * 1024.0).floor()), p30)),
                },
            ),
            (
                "reserveMib",
                JValue::Num(uint(&JValue::Num(config.services_reserve_mib as f64), p30)),
            ),
            (
                "floorMib",
                JValue::Num(uint(
                    &JValue::Num(js::round(config.memory_floor_gib * 1024.0)),
                    p30,
                )),
            ),
            (
                "cacheRamMib",
                JValue::Num(uint(&JValue::Num(cache_mib), p30)),
            ),
        ]);
        let ladder = self.deps().ladder(native as u64)?;
        Ok(JValue::obj([
            ("facts", facts),
            ("memory", memory),
            (
                "ladder",
                JValue::Arr(ladder.iter().map(|c| JValue::Num(*c as f64)).collect()),
            ),
            ("kv", JValue::Arr(kvs.to_vec())),
        ]))
    }

    /// `fallBack(j, item, why)`: the standard order, nothing measured yet.
    fn fall_back(&self, mi: usize, why: &str) -> Result<bool, Fail> {
        let item = self.item(mi);
        let fresh = phases::new_model(item.get("model"), false, &self.item_kv(&item));
        self.with_item(mi, |item| {
            js::set(item, "plan", JValue::Null);
            js::set(item, "planFallback", why);
            js::set(item, "phases", fresh.get("phases").clone());
        });
        self.note(&format!(
            "Using the standard tuning order for {}: {why}.",
            string_of(item.get("model"))
        ))?;
        Ok(false)
    }

    /// Opens phase `pi` for `inStage` (its starting text kept unless it is already running).
    fn open_stage(&self, mi: usize, pi: usize) -> Result<(), Fail> {
        if self.phase(mi, pi).get("status").as_str() != Some("running") {
            let (text, _) = self.deps().presets_snapshot();
            let now = self.deps().now();
            self.with_phase(mi, pi, |p| {
                js::set(p, "_beforeText", text);
                js::set(p, "status", "running");
                js::set(p, "startedAt", JValue::Num(now));
            });
        }
        let label = self.phase(mi, pi).get("label").clone();
        self.with_job(|j| js::set(j, "phase", label));
        self.save()
    }

    /// The `inStage` catch: models.ini back to the phase's starting text, the phase closed.
    async fn fail_stage(&self, mi: usize, pi: usize, e: Fail) -> Fail {
        let restored = match self.restore_phase(mi, pi).await {
            Ok(r) => r,
            Err(save) => return save,
        };
        if let Err(save) = self.close_failed_phase(mi, pi, &e) {
            return save;
        }
        if restored {
            e
        } else {
            unsafe_restore(&e)
        }
    }

    /// `closeStage(j, p, value)`.
    fn close_stage(&self, mi: usize, pi: usize, value: JValue) -> Result<(), Fail> {
        let now = self.deps().now();
        let revision = self.job().get("_revision").clone();
        self.with_phase(mi, pi, |p| {
            js::set(p, "status", "passed");
            js::set(p, "finishedAt", JValue::Num(now));
            js::set(p, "value", value);
            js::set(p, "_committedRevision", revision);
            js::del(p, "_beforeText");
        });
        self.save()
    }

    /// `fillCheck(j, step, base, withQuality)`: one fill-and-recall through the calibrator, then
    /// (for a probe) the quality probes at the same profile.
    async fn fill_check(
        &self,
        mi: usize,
        step: &JValue,
        base: JValue,
        with_quality: bool,
    ) -> Result<Fill, Fail> {
        self.check()?;
        self.guard_revision()?;
        self.unload_all(false).await?;
        let mut hooks = self.hooks(None);
        hooks.fill = true;
        let cal = Arc::new(self.deps().context(hooks, true));
        self.st().set_child(Some(Child::Probe(Arc::clone(&cal))));
        let bf16 = base.get("cache-type-k").as_str() == Some("bf16");
        let j = self.job();
        let opts = JValue::obj([
            ("ctx", step.get("ctx").clone()),
            ("fill", step.get("fill").clone()),
            ("base", base),
            ("baseRevision", j.get("_revision").clone()),
            ("promptBudgetSeconds", j.get("promptBudgetSeconds").clone()),
            ("evidence", JValue::Bool(true)),
        ]);
        let model = j.get("model").as_str().unwrap_or("").to_string();
        let probed = cal.probe(&model, opts).await;
        self.st().set_child(None);
        let r = probed?;
        self.check()?;
        if !r.get("passed").truthy() {
            let request = self.plan_of(mi).get("request").clone();
            let has_f16 = js::items(request.get("kv"))
                .iter()
                .any(|k| k.as_str() == Some("f16"));
            if bf16 && request.truthy() && !has_f16 {
                let ev = r.get("evidence");
                if kv::bf16_unsupported(ev.get("text").as_str()) {
                    return Ok(Fill::of("kv_unsupported", kv::BF16_FALLBACK));
                }
                // #1058: a failed router row with no HTTP status and no text (the process exited).
                let silent = r.get("cause").as_str() == Some("load")
                    && ev.truthy()
                    && !ev.get("text").truthy()
                    && ev.get("status").is_nullish();
                if silent && Some(step.get("ctx")) == js::items(request.get("ladder")).first() {
                    return Ok(Fill::of("kv_unsupported", kv::BF16_CRASH_FALLBACK));
                }
                if silent {
                    let reason = r.get("reason");
                    let mut f = Fill::of(
                        "load_failed",
                        if reason.truthy() {
                            reason.clone()
                        } else {
                            JValue::from(FILL_FAILED)
                        },
                    );
                    f.silent = true;
                    return Ok(f);
                }
            }
            let reason = if r.get("reason").truthy() {
                r.get("reason").clone()
            } else {
                JValue::from(FILL_FAILED)
            };
            let outcome = if r.get("cause").as_str() == Some("timeout") {
                "timeout"
            } else {
                plan_cause(r.get("cause")).unwrap_or("load_failed")
            };
            let own = Fill::of(outcome, reason.clone());
            let evidence = if r.get("evidence").truthy() {
                r.get("evidence").clone()
            } else {
                JValue::Null
            };
            let judged = self
                .deps()
                .judge(JValue::obj([
                    ("cause", r.get("cause").clone()),
                    ("evidence", evidence),
                ]))
                .await?;
            self.check()?;
            if !judged.truthy()
                || judged.get("classification").get("source").as_str() == Some("measured")
            {
                return Ok(own);
            }
            let mut f = Fill::of(
                &string_of(judged.get("outcome")),
                format!("{} {}", string_of(&reason), string_of(judged.get("reason"))),
            );
            f.classification = judged.get("classification").clone();
            return Ok(f);
        }
        if with_quality {
            let tried: Result<Option<Fill>, Fail> = async {
                self.load().await?;
                let q = self.quality(&model, self.gate(&model).as_ref()).await?;
                if q.get("passed") != &JValue::Bool(true) {
                    return Ok(Some(Fill::of(
                        "quality_failed",
                        quality::quality_failure(&q),
                    )));
                }
                Ok(None)
            }
            .await;
            match tried {
                Ok(Some(f)) => return Ok(f),
                Ok(None) => {}
                Err(e) => {
                    if e.fatal || e.cancelled || self.cancelled() {
                        return Err(e);
                    }
                    let lower = e.message.to_lowercase();
                    let outcome = if lower.contains("memory") {
                        "oom"
                    } else if lower.contains("timed out") {
                        "timeout"
                    } else {
                        "load_failed"
                    };
                    return Ok(Fill::of(outcome, e.message.as_str()));
                }
            }
        }
        let mut f = Fill::of("passed", JValue::Null);
        f.prompt_seconds = r.get("promptSeconds").clone();
        Ok(f)
    }

    /// `baselineSize(plan)` (#1026): the smallest rung and the most precise type that fits there.
    fn baseline_size(&self, request: &JValue) -> Result<Sized, Fail> {
        let ladder = js::items(request.get("ladder"));
        let ctx = ladder.first().cloned().unwrap_or(JValue::Undefined);
        let fits = |kv: &JValue| {
            let ask = with(
                request,
                vec![
                    ("ladder", JValue::Arr(vec![ctx.clone()])),
                    ("kv", JValue::Arr(vec![kv.clone()])),
                    ("results", JValue::Arr(Vec::new())),
                ],
            );
            self.deps()
                .plan(&ask)
                .is_ok_and(|s| s.get("step").as_str() == Some("probe"))
        };
        let Some(kv) = js::items(request.get("kv"))
            .iter()
            .find(|k| fits(k))
            .cloned()
        else {
            return Err(Fail::public(
                "This model does not fit the inference memory budget even at the smallest context size.",
            ));
        };
        let kv = string_of(&kv);
        Ok(Sized {
            ctx: js::number(&ctx),
            f16: kv::unquantized(&kv),
            kv,
        })
    }

    /// `measureFill(j, item, p, step, kind)`: one step row, its fill-and-recall (once more after a
    /// load timeout), and the planner's record of it.
    async fn measure_fill(
        &self,
        mi: usize,
        pi: usize,
        step: &JValue,
        kind: &str,
    ) -> Result<(), Fail> {
        let results_len = js::items(self.plan_of(mi).get("results")).len();
        let mut id = format!("{kind}-{results_len}");
        while js::items(self.phase(mi, pi).get("steps"))
            .iter()
            .any(|r| r.get("id").as_str() == Some(id.as_str()))
        {
            id.push('b');
        }
        let ctx = step.get("ctx");
        let ctx_text = match ctx.as_num() {
            Some(n) if n.fract() == 0.0 && (0.0..9e15).contains(&n) => js::locale_int(n as u64),
            _ => string_of(ctx),
        };
        let row = JValue::obj([
            ("id", JValue::from(id.as_str())),
            (
                "label",
                JValue::from(format!(
                    "{}{ctx_text} · {} KV cache",
                    if kind == "probe" {
                        "Fill "
                    } else {
                        "Final fill "
                    },
                    string_of(step.get("kv"))
                )),
            ),
            ("status", JValue::from("running")),
            ("ctx", ctx.clone()),
            ("kv", step.get("kv").clone()),
            ("estimateMib", step.get("estimateMib").clone()),
            ("startedAt", JValue::Num(self.deps().now())),
        ]);
        let row_index = self
            .with_phase(mi, pi, |p| {
                js::get_mut(p, "steps")
                    .and_then(js::items_mut)
                    .map(|steps| {
                        steps.push(row);
                        steps.len() - 1
                    })
            })
            .flatten()
            .unwrap_or(0);
        self.save()?;
        self.note(&format!(
            "{}{} tokens with {} KV cache (estimate {} MiB).",
            if kind == "probe" {
                "Filling "
            } else {
                "Final check: filling "
            },
            string_of(ctx),
            string_of(step.get("kv")),
            string_of(step.get("estimateMib"))
        ))?;
        let run = || {
            if kind == "probe" {
                let mut base = JValue::obj([
                    ("cache-type-k", step.get("kv").clone()),
                    ("cache-type-v", step.get("kv").clone()),
                ]);
                for (k, v) in js::entries(&spec_off()).into_iter().flatten() {
                    js::set(&mut base, k, v.clone());
                }
                self.fill_check(mi, step, base, true)
            } else {
                self.fill_check(mi, step, JValue::Obj(Vec::new()), false)
            }
        };
        let mut r = run().await?;
        if r.outcome == "timeout" {
            self.note("Loading timed out; trying the same setting once more.")?;
            r = run().await?;
        }
        let set_row = |me: &Self, pairs: Vec<(&str, JValue)>| {
            me.with_phase(mi, pi, |p| {
                if let Some(row) = js::get_mut(p, "steps")
                    .and_then(js::items_mut)
                    .and_then(|s| s.get_mut(row_index))
                {
                    for (k, v) in pairs {
                        js::set(row, k, v);
                    }
                }
            });
        };
        if r.classification.truthy() {
            set_row(self, vec![("classification", r.classification.clone())]);
            let advisor = r.classification.get("advisor");
            if advisor.as_str() != Some("not_asked") {
                self.note(&format!(
                    "Decision service advice for this failure: {}.",
                    string_of(advisor)
                ))?;
            }
        }
        for (outcome, stop) in [("timeout", LOAD_TIMEOUT), ("template", TEMPLATE_STOP)] {
            if r.outcome == outcome {
                let now = self.deps().now();
                set_row(
                    self,
                    vec![
                        ("status", JValue::from("failed")),
                        ("reason", JValue::from(stop)),
                        ("finishedAt", JValue::Num(now)),
                    ],
                );
                return Err(Fail::public(stop));
            }
        }
        if r.outcome == "kv_unsupported" {
            // #1057: f16 takes bf16's place; the bf16 failures before it are dropped, not carried.
            let now = self.deps().now();
            set_row(
                self,
                vec![
                    ("status", JValue::from("failed")),
                    ("reason", r.reason.clone()),
                    ("finishedAt", JValue::Num(now)),
                ],
            );
            let swap = |k: &JValue| {
                if k.as_str() == Some("bf16") {
                    JValue::from("f16")
                } else {
                    k.clone()
                }
            };
            let item_kv: Vec<JValue> = self.item_kv(&self.item(mi)).iter().map(swap).collect();
            self.with_plan(mi, |plan| {
                if let Some(request) = js::get_mut(plan, "request") {
                    let kvs: Vec<JValue> = js::items(request.get("kv")).iter().map(swap).collect();
                    js::set(request, "kv", JValue::Arr(kvs));
                }
                let results: Vec<JValue> = js::items(plan.get("results"))
                    .iter()
                    .filter(|e| {
                        !(e.get("step").as_str() == Some("probe")
                            && e.get("kv").as_str() == Some("bf16")
                            && e.get("outcome").as_str() != Some("passed"))
                    })
                    .map(|e| {
                        if e.get("kv").truthy() {
                            with(e, vec![("kv", swap(e.get("kv")))])
                        } else {
                            e.clone()
                        }
                    })
                    .collect();
                js::set(plan, "results", JValue::Arr(results));
            });
            self.with_item(mi, |item| js::set(item, "kv", JValue::Arr(item_kv)));
            self.with_plan(mi, |plan| {
                js::set(
                    plan,
                    "kvFallback",
                    JValue::obj([
                        ("from", JValue::from("bf16")),
                        ("to", JValue::from("f16")),
                        ("at", step.get("ctx").clone()),
                    ]),
                );
            });
            self.note(&string_of(&r.reason))?;
            return self.save();
        }
        if kind == "probe" && step.get("kv").as_str() == Some("bf16") {
            self.with_plan(mi, |plan| {
                let prior = match plan.get("bf16Silent") {
                    JValue::Undefined | JValue::Null => true,
                    v => v.truthy(),
                };
                js::set(plan, "bf16Silent", prior && r.silent);
            });
        }
        let now = self.deps().now();
        let mut pairs = vec![(
            "status",
            JValue::from(if r.outcome == "passed" {
                "passed"
            } else {
                "failed"
            }),
        )];
        if r.reason.truthy() {
            pairs.push(("reason", r.reason.clone()));
        }
        if !r.prompt_seconds.is_nullish() {
            pairs.push(("promptSeconds", r.prompt_seconds.clone()));
        }
        pairs.push(("finishedAt", JValue::Num(now)));
        set_row(self, pairs);
        let entry = JValue::obj([
            ("step", JValue::from(kind)),
            ("ctx", step.get("ctx").clone()),
            ("kv", step.get("kv").clone()),
            ("outcome", JValue::from(r.outcome.as_str())),
        ]);
        self.with_plan(mi, |plan| {
            if let Some(results) = js::get_mut(plan, "results").and_then(js::items_mut) {
                results.push(entry);
            }
        });
        self.save()
    }

    /// `runPlanned(j, item, release)`: true when the model finished; false when it fell back to the
    /// standard order (nothing measured yet).
    pub(crate) async fn run_planned(&self, mi: usize, release: &D::Release) -> Result<bool, Fail> {
        if !self.plan_of(mi).get("request").truthy() {
            self.unload_all(false).await?;
            let item = self.item(mi);
            let model = string_of(item.get("model"));
            let request = match self.plan_request(&model, &self.item_kv(&item)).await {
                Ok(r) => r,
                Err(e) if e.code.as_deref() == Some("autoconfig_impl") => return Err(e),
                Err(_) => {
                    JValue::obj([("fallback", JValue::from("its model file could not be read"))])
                }
            };
            if request.get("fallback").truthy() {
                return self.fall_back(mi, &string_of(request.get("fallback")));
            }
            self.with_plan(mi, |plan| js::set(plan, "request", request));
            self.save()?;
        }
        match self.plan_steps(mi, release).await {
            Ok(done) => Ok(done),
            Err(e) => {
                // A stop between steps still puts back what an open stage changed.
                let mut restored = true;
                let count = js::items(self.item(mi).get("phases")).len();
                for pi in 0..count {
                    if self.phase(mi, pi).get("_beforeText").is_nullish() {
                        continue;
                    }
                    restored = self.restore_phase(mi, pi).await? && restored;
                    let interrupted = self.cancelled() || e.cancelled;
                    let now = self.deps().now();
                    self.with_phase(mi, pi, |p| {
                        js::set(
                            p,
                            "status",
                            if interrupted { "interrupted" } else { "failed" },
                        );
                        js::set(p, "reason", e.message.as_str());
                        js::set(p, "finishedAt", JValue::Num(now));
                    });
                }
                self.save()?;
                if !restored && !e.unsafe_ {
                    return Err(unsafe_restore(&e));
                }
                Err(e)
            }
        }
    }

    async fn plan_steps(&self, mi: usize, release: &D::Release) -> Result<bool, Fail> {
        let mut fin: Option<JValue> = None;
        let model = string_of(self.item(mi).get("model"));
        for _ in 0..PLAN_STEP_LIMIT {
            self.check()?;
            let plan = self.plan_of(mi);
            let request = plan.get("request");
            let ask = with(request, vec![("results", plan.get("results").clone())]);
            let mut step = match self.deps().plan(&ask) {
                Ok(s) => s,
                Err(_) => {
                    if js::items(plan.get("results")).is_empty() {
                        return self.fall_back(mi, "the planner is unavailable");
                    }
                    return Err(Fail::public("The auto-tune planner is unavailable.").with_fatal());
                }
            };
            // #1058: about to leave bf16 after it only ever died silently: one bf16 probe at the
            // smallest rung first, where memory is no excuse.
            let kvs = js::items(request.get("kv"));
            let smallest = js::items(request.get("ladder"))
                .first()
                .cloned()
                .unwrap_or(JValue::Undefined);
            let has = |k: &str| kvs.iter().any(|x| x.as_str() == Some(k));
            let kind = step.get("step").as_str().unwrap_or("").to_string();
            if (kind == "probe" || kind == "fail")
                && step.get("kv").as_str() != Some("bf16")
                && has("bf16")
                && !has("f16")
                && plan.get("bf16Silent") == &JValue::Bool(true)
                && !js::items(plan.get("results")).iter().any(|r| {
                    r.get("step").as_str() == Some("probe")
                        && r.get("kv").as_str() == Some("bf16")
                        && (r.get("ctx") == &smallest
                            || r.get("outcome").as_str() == Some("passed"))
                })
            {
                let probe_at = self.deps().plan(&with(
                    request,
                    vec![
                        ("ladder", JValue::Arr(vec![smallest.clone()])),
                        ("kv", JValue::Arr(vec![JValue::from("bf16")])),
                        ("results", JValue::Arr(Vec::new())),
                    ],
                ));
                if let Ok(p) = probe_at {
                    if p.get("step").as_str() == Some("probe")
                        && p.get("kv").as_str() == Some("bf16")
                        && p.get("ctx") == &smallest
                    {
                        step = p;
                    }
                }
            }
            let kind = step.get("step").as_str().unwrap_or("").to_string();
            if kind == "fail" {
                if step.get("code").as_str() == Some("unsizeable")
                    && js::items(plan.get("results")).is_empty()
                {
                    return self
                        .fall_back(mi, "its KV cache cannot be sized from the model's metadata");
                }
                return Err(Fail::public(string_of(step.get("message"))));
            }
            let context = self.phase_index(mi, "context").ok_or_else(|| {
                Fail::new("Cannot read properties of undefined (reading 'status')")
            })?;
            if kind == "probe" {
                release.set_reason(&format!(
                    "Chat is paused while noevia tunes {model}: context and KV cache."
                ));
                self.open_stage(mi, context)?;
                let staged: Result<(), Fail> = async {
                    if !self.plan_of(mi).get("before").truthy() {
                        let before = keys_of(self.deps().presets_get(&model).get("options"));
                        self.with_plan(mi, |plan| js::set(plan, "before", before));
                        self.save()?;
                    }
                    // Node evaluates `presets.get(j.model).options` before `baselineSize(plan)`.
                    let options = self.deps().presets_get(&model).get("options").clone();
                    let sized = self.baseline_size(self.plan_of(mi).get("request"))?;
                    self.ensure_baseline(
                        &options,
                        BaselineOpts {
                            sized: Some(sized),
                            ..BaselineOpts::default()
                        },
                    )
                    .await?;
                    self.measure_fill(mi, context, &step, "probe").await
                }
                .await;
                if let Err(e) = staged {
                    return Err(self.fail_stage(mi, context, e).await);
                }
                continue;
            }
            if self.phase(mi, context).get("status").as_str() != Some("passed") {
                // The search is over: the chosen context and KV cache type, drafting as it was.
                self.open_stage(mi, context)?;
                let before = self.plan_of(mi).get("before").clone();
                let mut options = JValue::obj([
                    ("ctx-size", JValue::from(string_of(step.get("ctx")))),
                    ("cache-type-k", step.get("kv").clone()),
                    ("cache-type-v", step.get("kv").clone()),
                ]);
                for k in SPEC_KEYS {
                    let v = before.get(k);
                    js::set(
                        &mut options,
                        k,
                        if v.truthy() {
                            v.clone()
                        } else {
                            JValue::from("")
                        },
                    );
                }
                if let Err(e) = self.write(options).await {
                    return Err(self.fail_stage(mi, context, e).await);
                }
                let probes = js::items(self.plan_of(mi).get("results"))
                    .iter()
                    .filter(|r| r.get("step").as_str() == Some("probe"))
                    .count();
                self.close_stage(
                    mi,
                    context,
                    JValue::obj([
                        ("context", step.get("ctx").clone()),
                        ("kv", step.get("kv").clone()),
                        ("probes", JValue::Num(probes as f64)),
                    ]),
                )?;
                self.note(&format!(
                    "Chose context {} with {} KV cache after the fill-and-recall test.",
                    string_of(step.get("ctx")),
                    string_of(step.get("kv"))
                ))?;
            }
            if kind == "phase" {
                let id = string_of(step.get("id"));
                let pi = self.phase_index(mi, &id).ok_or_else(|| {
                    Fail::new("Cannot read properties of undefined (reading 'label')")
                })?;
                let label = string_of(self.phase(mi, pi).get("label"));
                release.set_reason(&format!(
                    "Chat is paused while noevia tunes {model}: {}.",
                    label.to_lowercase()
                ));
                self.run_phase(mi, pi).await?;
                let value = self.phase(mi, pi).get("value").clone();
                let outcome = if value.get("failed").truthy() {
                    "failed"
                } else if value.get("skipped").truthy() {
                    "skipped"
                } else {
                    "passed"
                };
                let entry = JValue::obj([
                    ("step", JValue::from("phase")),
                    ("id", step.get("id").clone()),
                    ("outcome", JValue::from(outcome)),
                ]);
                self.with_plan(mi, |plan| {
                    if let Some(results) = js::get_mut(plan, "results").and_then(js::items_mut) {
                        results.push(entry);
                    }
                });
                self.save()?;
                continue;
            }
            let verify = self.phase_index(mi, "verify").ok_or_else(|| {
                Fail::new("Cannot read properties of undefined (reading 'status')")
            })?;
            if kind == "verify" {
                release.set_reason(&format!(
                    "Chat is paused while noevia tunes {model}: final fill check."
                ));
                self.open_stage(mi, verify)?;
                if let Err(e) = self.measure_fill(mi, verify, &step, "verify").await {
                    return Err(self.fail_stage(mi, verify, e).await);
                }
                continue;
            }
            if self.phase(mi, verify).get("status").as_str() != Some("passed") {
                self.close_stage(
                    mi,
                    verify,
                    JValue::obj([("context", step.get("ctx").clone())]),
                )?;
            }
            self.guard_revision()?;
            if fin.is_none() {
                self.with_job(|j| js::set(j, "phase", "Verifying saved profile"));
                self.save()?;
                self.unload_all(false).await?;
                self.load().await?;
                fin = Some(self.validate(&model, None).await?);
                self.check()?;
            }
            if kind == "serving" {
                self.with_job(|j| js::set(j, "phase", "Checking a realistic chat request"));
                self.save()?;
                let serving = self.serving_check(&model).await?;
                let outcome = if serving.truthy() {
                    "passed"
                } else {
                    "skipped"
                };
                self.with_plan(mi, |plan| {
                    js::set(plan, "serving", serving);
                    if let Some(results) = js::get_mut(plan, "results").and_then(js::items_mut) {
                        results.push(JValue::obj([
                            ("step", JValue::from("serving")),
                            ("outcome", JValue::from(outcome)),
                        ]));
                    }
                });
                self.save()?;
                continue;
            }
            let mut done = fin.take().unwrap_or(JValue::Undefined);
            let serving = self.plan_of(mi).get("serving").clone();
            js::set(
                &mut done,
                "serving",
                if serving.truthy() {
                    serving
                } else {
                    JValue::Null
                },
            );
            self.finish_model(
                mi,
                &done,
                Some((step.get("kv").clone(), step.get("ctx").clone())),
            )
            .await?;
            return Ok(true);
        }
        Err(Fail::public(
            "Auto-tune did not finish within its step limit.",
        ))
    }
}
