//! The standard order's stages and what every stage shares (core llamacpp-full-autotune.cjs lines
//! 552-1151): `write`, `validate`, `record`, `measure`, the calibrator stage, the quality baseline,
//! the five phases, the serving check, `finishModel`, and the restore of a phase's starting text.

use crate::deps::{Context as _, Deps, Fail, Hooks};
use crate::engine::{encode_uri_component, string_of};
use crate::runner::{lock, unsafe_restore, Child, Runner, StageHooks, VERSION};
use crate::{js, kv, phases, quality};
use js_json::{wtf16, JValue};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

const BASELINE_KEYS: [&str; 5] = [
    "cache-type-k",
    "cache-type-v",
    "spec-type",
    "spec-draft-n-max",
    "spec-draft-p-min",
];
pub(crate) const SPEC_KEYS: [&str; 3] = ["spec-type", "spec-draft-n-max", "spec-draft-p-min"];
const PAD: &str =
    "The garden committee reviewed irrigation, seed orders, volunteer rotas and pump maintenance. ";
/// sampling-recommendation.cjs `INI_KEY_LIST`.
const INI_KEY_LIST: [&str; 5] = ["temp", "top-p", "top-k", "min-p", "repeat-penalty"];

/// `{ 'spec-type': 'none', 'spec-draft-n-max': '', 'spec-draft-p-min': '' }`.
pub(crate) fn spec_off() -> JValue {
    JValue::obj([
        ("spec-type", JValue::from("none")),
        ("spec-draft-n-max", JValue::from("")),
        ("spec-draft-p-min", JValue::from("")),
    ])
}

/// `options[k] || ''` as the string the profile holds.
fn or_empty(options: &JValue, key: &str) -> JValue {
    let v = options.get(key);
    if v.truthy() {
        v.clone()
    } else {
        JValue::from("")
    }
}

/// `keysOf(options)`: the KV and drafting keys of a profile.
pub(crate) fn keys_of(options: &JValue) -> JValue {
    JValue::obj(BASELINE_KEYS.iter().map(|k| (*k, or_empty(options, k))))
}

fn reference(kv: &str) -> JValue {
    let mut r = JValue::obj([
        ("cache-type-k", JValue::from(kv)),
        ("cache-type-v", JValue::from(kv)),
    ]);
    for (k, v) in js::entries(&spec_off()).into_iter().flatten() {
        js::set(&mut r, k, v.clone());
    }
    r
}

/// `/failed to load|could not be loaded/i`.
pub(crate) fn load_failure(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("failed to load") || m.contains("could not be loaded")
}

/// sampling-recommendation.cjs `toIniOptions(values)`: the valid keys as models.ini text.
fn to_ini_options(values: &JValue) -> JValue {
    let mut out = JValue::Obj(Vec::new());
    for (key, ini) in [
        ("temperature", "temp"),
        ("top_p", "top-p"),
        ("top_k", "top-k"),
        ("min_p", "min-p"),
        ("repeat_penalty", "repeat-penalty"),
    ] {
        let Some(v) = values.get(key).as_num().filter(|v| v.is_finite()) else {
            continue;
        };
        let valid = match key {
            "min_p" => (0.0..=1.0).contains(&v),
            "temperature" => (0.0..=2.0).contains(&v),
            "top_p" => v > 0.0 && v <= 1.0,
            "top_k" => v.fract() == 0.0 && v >= 0.0,
            _ => (0.0..=3.0).contains(&v),
        };
        if valid {
            js::set(&mut out, ini, js::num(js::round(v * 1000.0) / 1000.0));
        }
    }
    out
}

/// `uint(v, max)`.
pub(crate) fn uint(v: &JValue, max: f64) -> f64 {
    match v.as_num() {
        Some(n)
            if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 && n >= 0.0 && n <= max =>
        {
            n
        }
        _ => 0.0,
    }
}

type Evaluate<'a> = Pin<Box<dyn Future<Output = Result<JValue, Fail>> + Send + 'a>>;

impl<D: Deps> Runner<D> {
    /// `writeOptions(j)` (#1003): one recovery copy of models.ini per run.
    pub(crate) fn write_options(&self) -> JValue {
        if self.job().get("_backedUp").truthy() {
            JValue::obj([("backup", JValue::Bool(false))])
        } else {
            JValue::Obj(Vec::new())
        }
    }

    /// `wrote(j)`.
    pub(crate) fn wrote(&self) -> Result<(), Fail> {
        if !self.job().get("_backedUp").truthy() {
            self.with_job(|j| js::set(j, "_backedUp", true));
            self.save()?;
        }
        Ok(())
    }

    /// `write(j, options)`: every model unloaded, then the job's own model's options written over
    /// the revision it last wrote.
    pub(crate) async fn write(&self, options: JValue) -> Result<(), Fail> {
        self.check()?;
        self.guard_revision()?;
        self.unload_all(false).await?;
        let j = self.job();
        let body = JValue::obj([
            ("model", j.get("model").clone()),
            ("baseRevision", j.get("_revision").clone()),
            ("options", options),
        ]);
        let r = self
            .deps()
            .apply_unlocked(body, self.write_options())
            .await?;
        if r.get("ok").truthy() {
            self.wrote()?;
        } else {
            let error = r.get("body").get("error");
            let message = if error.truthy() {
                string_of(error)
            } else {
                "Could not save settings; the profile may have changed.".to_string()
            };
            return Err(Fail {
                fatal: r.get("status").as_num() == Some(409.0),
                ..Fail::new(message)
            });
        }
        let (_, revision) = self.deps().presets_snapshot();
        self.with_job(|j| js::set(j, "_revision", revision));
        self.save()
    }

    /// `gate(model)`: the model's baseline once it has a discriminative probe.
    pub(crate) fn gate(&self, model: &str) -> Option<JValue> {
        let j = self.job();
        let b = js::items(j.get("models"))
            .iter()
            .find(|m| m.get("model").as_str() == Some(model))?
            .get("baseline")
            .clone();
        (!js::items(b.get("probes")).is_empty()).then_some(b)
    }

    /// `validate(model, measured)`: the quality gate, then the three workloads.
    pub(crate) async fn validate(
        &self,
        model: &str,
        measured: Option<JValue>,
    ) -> Result<JValue, Fail> {
        let quality = match measured {
            Some(q) => q,
            None => self.quality(model, self.gate(model).as_ref()).await?,
        };
        if quality.get("passed") != &JValue::Bool(true) {
            return Err(Fail::new(quality::quality_failure(&quality)));
        }
        let harmony = self.deps().quirks(model).harmony;
        let mut workloads = Vec::new();
        let (mut drafted, mut accepted) = (0.0, 0.0);
        let mut gens = Vec::new();
        for w in phases::WORKLOADS {
            let max = if harmony { w.max.max(512) } else { w.max };
            let r = self.chat(model, w.prompt, max, None).await?;
            let gen = r.get("gen").as_num().filter(|g| g.is_finite() && *g > 0.0);
            let Some(gen) = gen else {
                return Err(Fail::new("Missing throughput measurements."));
            };
            if r.get("text")
                .as_str()
                .is_none_or(|t| js::trim(t).is_empty())
            {
                return Err(Fail::new("Workload did not produce a final answer."));
            }
            drafted += r.get("drafted").as_num().unwrap_or(0.0);
            accepted += r.get("accepted").as_num().unwrap_or(0.0);
            gens.push(gen);
            workloads.push(JValue::obj([
                ("workload", JValue::from(w.id)),
                ("gen", JValue::Num(gen)),
                ("drafted", r.get("drafted").clone()),
                ("accepted", r.get("accepted").clone()),
                ("text", r.get("text").clone()),
            ]));
        }
        Ok(JValue::obj([
            ("quality", quality),
            ("workloads", JValue::Arr(workloads)),
            (
                "generation",
                JValue::Num(js::round(phases::geomean(&gens) * 10.0) / 10.0),
            ),
            (
                "acceptance",
                if drafted != 0.0 {
                    JValue::Num(js::round(100.0 * accepted / drafted))
                } else {
                    JValue::Null
                },
            ),
        ]))
    }

    /// `record(p, id, status, details)`: `Object.assign(row, details, { status })`, then a save.
    pub(crate) fn record(
        &self,
        mi: usize,
        pi: usize,
        id: &JValue,
        status: &str,
        details: JValue,
    ) -> Result<(), Fail> {
        self.with_phase(mi, pi, |p| {
            if let Some(row) = js::get_mut(p, "steps")
                .and_then(js::items_mut)
                .and_then(|steps| steps.iter_mut().find(|s| s.get("id") == id))
            {
                for (k, v) in js::entries(&details).into_iter().flatten() {
                    js::set(row, k, v.clone());
                }
                js::set(row, "status", status);
            }
        });
        self.save()
    }

    /// `measure(j, p, id, options, evaluate)`: write, load, evaluate; a failure is recorded and
    /// skipped unless it must stop the job.
    pub(crate) async fn measure<'a>(
        &'a self,
        mi: usize,
        pi: usize,
        id: &JValue,
        options: JValue,
        evaluate: impl FnOnce() -> Evaluate<'a>,
    ) -> Result<Option<JValue>, Fail> {
        self.check()?;
        let now = self.deps().now();
        self.record(
            mi,
            pi,
            id,
            "running",
            JValue::obj([("startedAt", JValue::Num(now))]),
        )?;
        let attempt: Result<JValue, Fail> = async {
            self.write(options.clone()).await?;
            self.load().await?;
            evaluate().await
        }
        .await;
        match attempt {
            Ok(result) => {
                let mut details = compact(&result);
                js::set(&mut details, "value", options);
                js::set(&mut details, "finishedAt", JValue::Num(self.deps().now()));
                self.record(mi, pi, id, "passed", details)?;
                Ok(Some(result))
            }
            Err(e) => {
                let status = if self.cancelled() {
                    "interrupted"
                } else {
                    "failed"
                };
                let now = self.deps().now();
                self.record(
                    mi,
                    pi,
                    id,
                    status,
                    JValue::obj([
                        ("reason", JValue::from(e.message.as_str())),
                        ("finishedAt", JValue::Num(now)),
                    ]),
                )?;
                if e.fatal || e.cancelled || self.cancelled() || self.revision_moved() {
                    return Err(e);
                }
                Ok(None)
            }
        }
    }

    /// `guardedApply(j)(body)`: the calibrator's writes, only over the revision this job last
    /// wrote, models unloaded first.
    pub(crate) async fn guarded_apply(&self, body: JValue) -> Result<JValue, Fail> {
        if body.get("baseRevision") != self.job().get("_revision") {
            return Err(Fail::outside_edit());
        }
        self.unload_all(false).await?;
        let r = self
            .deps()
            .apply_unlocked(body, self.write_options())
            .await?;
        if r.get("ok").truthy() {
            self.wrote()?;
        }
        Ok(r)
    }

    pub(crate) fn hooks(&self, phase: Option<(usize, usize, String, usize)>) -> StageHooks<D> {
        let st = self.st();
        StageHooks {
            runner: self.clone(),
            run: st.run,
            job_object: st.job_object,
            model: st
                .state
                .get("job")
                .get("model")
                .as_str()
                .unwrap_or("")
                .to_string(),
            phase,
            foreign_stop: Arc::new(Mutex::new(None)),
            fill: false,
        }
    }

    /// `contextStage(j, p, prefix)`: the calibrator's context search, its steps mirrored into
    /// phase `pi` after the first `offset` (the prefix's) steps.
    pub(crate) async fn context_stage(
        &self,
        mi: usize,
        pi: usize,
        prefix: &str,
    ) -> Result<JValue, Fail> {
        self.check()?;
        self.guard_revision()?;
        // `offset` is fixed now, before the calibrator reports anything.
        let offset = if prefix.is_empty() {
            0
        } else {
            js::items(self.phase(mi, pi).get("steps")).len()
        };
        let hooks = self.hooks(Some((mi, pi, prefix.to_string(), offset)));
        let foreign_stop = Arc::clone(&hooks.foreign_stop);
        let child = Arc::new(self.deps().context(hooks, false));
        self.st().set_child(Some(Child::Stage(Arc::clone(&child))));
        let outcome = async {
            let j = self.job();
            let model = j.get("model").as_str().unwrap_or("").to_string();
            let started = child
                .start(&model, js::number(j.get("promptBudgetSeconds")))
                .await?;
            if !started.get("ok").truthy() {
                let error = started.get("body").get("error");
                return Err(Fail::new(if error.truthy() {
                    string_of(error)
                } else {
                    "Context measurement could not start.".to_string()
                }));
            }
            if self.cancelled() {
                child.abort();
            }
            child.completion().await?;
            self.check()?;
            let result = child.status_job(&model);
            let passed = result.get("status").as_str() == Some("passed");
            if let Some(stop) = lock(&foreign_stop).clone() {
                if !passed {
                    return Err(stop);
                }
            }
            if !passed {
                let error = result.get("error");
                return Err(Fail {
                    fatal: result.get("restored") == &JValue::Bool(false),
                    ..Fail::new(if error.truthy() {
                        string_of(error)
                    } else {
                        "Context measurement failed.".to_string()
                    })
                });
            }
            Ok(result.get("result").clone())
        }
        .await;
        self.st().set_child(None);
        outcome
    }

    /// `verifyContext(j, p)`: the committed context still passes with the phase's setting.
    async fn verify_context(&self, mi: usize, pi: usize) -> Result<(), Fail> {
        let committed = self
            .phase_index(mi, "context")
            .map(|ci| js::number(self.phase(mi, ci).get("value").get("context")))
            .unwrap_or(f64::NAN);
        let measured = self.context_stage(mi, pi, "Context check · ").await?;
        let applied = js::number(measured.get("appliedCtx"));
        if applied < committed {
            return Err(Fail::new(
                "The saved context no longer passes with this setting.",
            ));
        }
        if applied != committed {
            self.write(JValue::obj([(
                "ctx-size",
                JValue::from(js::num(committed)),
            )]))
            .await?;
            self.load().await?;
            let model = self.job().get("model").as_str().unwrap_or("").to_string();
            let fin = self.quality(&model, self.gate(&model).as_ref()).await?;
            if fin.get("passed") != &JValue::Bool(true) {
                return Err(Fail::new(quality::quality_failure(&fin)));
            }
        }
        Ok(())
    }

    /// `setBaseline(j, reference, quality)`: recorded on the queue item; no passing probe stops the
    /// job (fatal on purpose, so the optional sampling phase cannot swallow it).
    pub(crate) fn set_baseline(
        &self,
        reference: &str,
        quality_result: &JValue,
    ) -> Result<JValue, Fail> {
        let baseline = quality::baseline_of(reference, quality_result);
        if let Some(mi) = self.current_item() {
            self.with_item(mi, |item| js::set(item, "baseline", baseline.clone()));
        }
        self.save()?;
        self.note(&quality::baseline_summary(&baseline))?;
        if js::items(baseline.get("probes")).is_empty() {
            return Err(Fail {
                fatal: true,
                public_message: Some(quality::NO_PROBE_PASSED.to_string()),
                ..Fail::new(format!(
                    "{} {}",
                    quality::NO_PROBE_PASSED,
                    quality::quality_failure(quality_result)
                ))
            });
        }
        Ok(baseline)
    }

    /// `ensureBaseline(j, original, opts)` (#328, #1078, #1026).
    pub(crate) async fn ensure_baseline(
        &self,
        original: &JValue,
        opts: BaselineOpts,
    ) -> Result<(), Fail> {
        let model = self.job().get("model").as_str().unwrap_or("").to_string();
        if self.gate(&model).is_some() {
            return Ok(());
        }
        let restore = keys_of(original);
        let mut try_bf16 = !opts.skip_bf16;
        let mut refused = opts.bf16_refused;
        let mut original = original.clone();
        if let Some(sized) = &opts.sized {
            try_bf16 = try_bf16 && sized.f16;
            original = js::spread(&[&original]);
            js::set(&mut original, "cache-type-k", sized.kv.as_str());
            js::set(&mut original, "cache-type-v", sized.kv.as_str());
        }
        let at = |options: JValue| match &opts.sized {
            Some(sized) => {
                let mut o = js::spread(&[&options]);
                js::set(&mut o, "ctx-size", js::num(sized.ctx));
                o
            }
            None => options,
        };
        let mut chosen = "bf16";
        let mut result = None;
        if try_bf16 {
            let (q, was_refused) = self
                .baseline_attempt(&model, "bf16", at(reference("bf16")))
                .await?;
            result = q;
            refused = refused || was_refused;
        }
        let sized_f16 = opts.sized.as_ref().is_none_or(|s| s.f16);
        if result.is_none() && refused && opts.f16_standin && sized_f16 {
            chosen = "f16";
            result = self
                .baseline_attempt(&model, "f16", at(reference("f16")))
                .await?
                .0;
        }
        if result.is_none() {
            chosen = "current";
            let mut current = keys_of(&original);
            for (k, v) in js::entries(&spec_off()).into_iter().flatten() {
                js::set(&mut current, k, v.clone());
            }
            result = self
                .baseline_attempt(&model, "current", at(current))
                .await?
                .0;
        }
        self.write(restore).await?;
        let Some(q) = result else {
            return Err(Fail::fatal(
                "The quality baseline could not be measured at the reference or current settings.",
            ));
        };
        self.set_baseline(chosen, &q)?;
        Ok(())
    }

    /// One baseline measurement (`attempt` in `ensureBaseline`): the probes without a gate, or
    /// `None` with the reason noted. The flag says a bf16 reference was refused by the engine.
    async fn baseline_attempt(
        &self,
        model: &str,
        reference: &str,
        options: JValue,
    ) -> Result<(Option<JValue>, bool), Fail> {
        self.check()?;
        self.note(&format!(
            "Measuring the quality baseline at {}.",
            quality::reference_label(reference)
        ))?;
        let tried: Result<JValue, Fail> = async {
            self.write(options).await?;
            self.load().await?;
            let q = self.quality(model, None).await?;
            if quality::engine_failed(&q) {
                return Err(Fail::new(quality::quality_failure(&q)));
            }
            Ok(q)
        }
        .await;
        match tried {
            Ok(q) => Ok((Some(q), false)),
            Err(e) => {
                if e.fatal || e.cancelled || self.cancelled() {
                    return Err(e);
                }
                let refused = reference == "bf16"
                    && (kv::bf16_unsupported(Some(&e.message)) || load_failure(&e.message));
                self.note(&format!(
                    "The baseline at {} could not be measured: {}",
                    quality::reference_label(reference),
                    e.message
                ))?;
                Ok((None, refused))
            }
        }
    }

    /// `runSampling(j, p)` (#308): the recommendation table decides; the engine is asked once to
    /// prove it accepts it. An operator's own sampling keys are never overwritten.
    async fn run_sampling(&self, mi: usize, pi: usize) -> Result<(), Fail> {
        let model = self.job().get("model").as_str().unwrap_or("").to_string();
        let rec = self.deps().sampling_for(&model).await?;
        let options = if rec.truthy() {
            to_ini_options(rec.get("values"))
        } else {
            JValue::Obj(Vec::new())
        };
        let profile = self.deps().presets_get(&model);
        let own: Vec<&str> = INI_KEY_LIST
            .iter()
            .copied()
            .filter(|k| {
                profile.get("options").get(k).truthy() || profile.get("defaults").get(k).truthy()
            })
            .collect();
        let apply = JValue::from("apply");
        let skip = |reason: String| -> Result<(), Fail> {
            self.record(
                mi,
                pi,
                &apply,
                "skipped",
                JValue::obj([("reason", JValue::from(reason.as_str()))]),
            )?;
            let mut value = JValue::obj([
                ("skipped", JValue::Bool(true)),
                ("reason", JValue::from(reason.as_str())),
            ]);
            if rec.truthy() {
                js::set(&mut value, "tier", rec.get("tier").clone());
                js::set(&mut value, "source", rec.get("source").clone());
            }
            self.with_phase(mi, pi, |p| js::set(p, "value", value));
            self.note(&format!("Sampling left unchanged: {reason}"))
        };
        if js::entries(&options).is_none_or(Vec::is_empty) {
            return skip(
                "No recommended sampling values; the engine defaults stay in force.".to_string(),
            );
        }
        if !own.is_empty() {
            return skip(format!(
                "Sampling is already set in models.ini ({}); left as configured.",
                own.join(", ")
            ));
        }
        self.ensure_baseline(profile.get("options"), BaselineOpts::default())
            .await?;
        self.check()?;
        self.note(&format!(
            "Applying recommended sampling ({}).",
            string_of(rec.get("source"))
        ))?;
        let now = self.deps().now();
        self.record(
            mi,
            pi,
            &apply,
            "running",
            JValue::obj([("startedAt", JValue::Num(now))]),
        )?;
        self.write(options.clone()).await?;
        self.load().await?;
        let fin = self.validate(&model, None).await?;
        let now = self.deps().now();
        self.record(
            mi,
            pi,
            &apply,
            "passed",
            JValue::obj([("value", options.clone()), ("finishedAt", JValue::Num(now))]),
        )?;
        let value = JValue::obj([
            ("applied", JValue::Bool(true)),
            ("tier", rec.get("tier").clone()),
            ("source", rec.get("source").clone()),
            ("family", rec.get("familyId").clone()),
            ("values", options.clone()),
            ("quality", fin.get("quality").clone()),
        ]);
        self.with_phase(mi, pi, |p| js::set(p, "value", value));
        let listed = js::entries(&options)
            .into_iter()
            .flatten()
            .map(|(k, v)| format!("{k} {}", string_of(v)))
            .collect::<Vec<_>>()
            .join(", ");
        self.note(&format!("Committed recommended sampling: {listed}."))
    }

    /// `tuneCacheMib(eff)` through the dependency, `kvCeilings(j, model, kvs)` (#1057, #1133): per
    /// type the largest ladder rung whose estimate fits; `None` when the model cannot be sized
    /// (the reason goes to the job log).
    async fn kv_ceilings(&self, model: &str, kvs: &[JValue]) -> Result<Option<JValue>, Fail> {
        let unplanned = |why: &str| {
            self.note(&format!(
                "KV ceilings could not be planned ({why}), so the precision order alone decides."
            ))
        };
        let attempt: Result<Option<JValue>, Fail> = async {
            let read = self.deps().plan_facts(model).await?;
            if !read.get("meta").truthy() {
                unplanned("the model's sizing facts are unavailable")?;
                return Ok(None);
            }
            let profile = self.deps().presets_get(model);
            let eff = js::spread(&[profile.get("defaults"), profile.get("options")]);
            let cache_mib = self.deps().tune_cache_mib(&eff)?;
            if !cache_mib.is_finite() {
                unplanned("the cache size is not a number")?;
                return Ok(None);
            }
            let free = self.deps().read_memory();
            let config = self.config();
            let budget = self.deps().budget_gib()? * 1024.0;
            let usable = budget.min(match free {
                None => f64::INFINITY,
                Some(free) => {
                    free * 1024.0
                        - config.services_reserve_mib as f64
                        - config.memory_floor_gib * 1024.0
                }
            });
            let native = uint(read.get("meta").get("contextLength"), f64::from(1u32 << 24));
            let rungs = self.deps().ladder(native as u64)?;
            let ubatch = uint(
                &JValue::Num(js::number(eff.get("ubatch-size"))),
                f64::from(1u32 << 24),
            )
            .max(f64::from(phases::UBATCH[2]));
            let mut out = JValue::Obj(Vec::new());
            for k in kvs {
                let mut fit = None;
                for c in &rungs {
                    let mut options = eff.clone();
                    js::set(&mut options, "ctx-size", c.to_string());
                    js::set(&mut options, "cache-type-k", k.clone());
                    js::set(&mut options, "cache-type-v", k.clone());
                    js::set(&mut options, "cache-ram", js::num(cache_mib));
                    js::set(&mut options, "ubatch-size", js::num(ubatch));
                    let mmproj = read.get("mmprojBytes");
                    let e = self.deps().estimate_footprint(JValue::obj([
                        ("meta", read.get("meta").clone()),
                        ("modelBytes", read.get("modelBytes").clone()),
                        (
                            "mmprojBytes",
                            if mmproj.truthy() {
                                mmproj.clone()
                            } else {
                                JValue::Num(0.0)
                            },
                        ),
                        ("model", JValue::from(model)),
                        ("options", options),
                    ]))?;
                    let total = e.get("totalGib");
                    if e.get("sizeable").truthy()
                        && !total.is_nullish()
                        && js::number(total) * 1024.0 <= usable
                    {
                        fit = Some(*c);
                    }
                }
                js::set(
                    &mut out,
                    &string_of(k),
                    fit.map_or(JValue::Null, |c| JValue::Num(c as f64)),
                );
            }
            if js::entries(&out)
                .into_iter()
                .flatten()
                .any(|(_, v)| !v.is_nullish())
            {
                return Ok(Some(out));
            }
            unplanned("no context rung fits the memory budget")?;
            Ok(None)
        }
        .await;
        match attempt {
            Err(e) if e.code.as_deref() == Some("autoconfig_impl") => Err(e),
            Err(_) => {
                unplanned("the sizing check failed")?;
                Ok(None)
            }
            ok => ok,
        }
    }

    /// `runKv(j, p)` (#1057, #1058): every candidate with drafting off, precision first.
    async fn run_kv(&self, mi: usize, pi: usize) -> Result<(), Fail> {
        let model = self.job().get("model").as_str().unwrap_or("").to_string();
        let kvs = self.item_kv(&self.item(mi));
        let before = js::spread(&[self.deps().presets_get(&model).get("options")]);
        self.unload_all(false).await?;
        let mut ceilings = match self.kv_ceilings(&model, &kvs).await {
            Ok(c) => c,
            Err(e) => {
                // Only models were unloaded; a sizing fault stops before any proposal, including
                // an unchanged restore write.
                self.with_phase(mi, pi, |p| js::del(p, "_beforeText"));
                return Err(e);
            }
        };
        let mut order = kvs.clone();
        let mut kv_fallback: Option<JValue> = None;
        let mut results: Vec<(JValue, JValue)> = Vec::new();
        let mut i = 0;
        while let Some(k) = order.get(i).cloned() {
            let k_text = string_of(&k);
            self.check()?;
            self.note(&format!("Testing {k_text} KV cache with drafting off."))?;
            self.unload_all(false).await?;
            let reuse = kv::unquantized(&k_text) && self.gate(&model).is_none();
            let mut options =
                JValue::obj([("cache-type-k", k.clone()), ("cache-type-v", k.clone())]);
            for (key, v) in js::entries(&spec_off()).into_iter().flatten() {
                js::set(&mut options, key, v.clone());
            }
            let me = self;
            let model_ref = model.as_str();
            let k_ref = k_text.clone();
            let measured = self
                .measure(mi, pi, &k, options, move || {
                    Box::pin(async move {
                        if !reuse {
                            return me.validate(model_ref, None).await;
                        }
                        let q = me.quality(model_ref, None).await?;
                        if quality::engine_failed(&q) {
                            return Err(Fail::new(quality::quality_failure(&q)));
                        }
                        let b = me.set_baseline(&k_ref, &q)?;
                        let probes = js::items(b.get("probes"));
                        let checks: Vec<JValue> = js::items(q.get("checks"))
                            .iter()
                            .filter(|c| probes.contains(c.get("id")))
                            .cloned()
                            .collect();
                        let mut gate = JValue::obj([
                            ("passed", JValue::Bool(true)),
                            ("checks", JValue::Arr(checks)),
                        ]);
                        let skipped = js::items(b.get("skipped"));
                        if !skipped.is_empty() {
                            js::set(
                                &mut gate,
                                "skipped",
                                JValue::Arr(skipped.iter().map(|s| s.get("id").clone()).collect()),
                            );
                        }
                        me.validate(model_ref, Some(gate)).await
                    })
                })
                .await?;
            if let Some(m) = &measured {
                results.push((k.clone(), m.clone()));
            }
            let failure = {
                let reason = js::items(self.phase(mi, pi).get("steps"))
                    .iter()
                    .find(|s| s.get("id") == &k)
                    .map(|s| s.get("reason").clone())
                    .unwrap_or(JValue::Undefined);
                if reason.truthy() {
                    string_of(&reason)
                } else {
                    String::new()
                }
            };
            let profile = self.deps().presets_get(&model);
            let eff = js::spread(&[profile.get("defaults"), profile.get("options")]);
            let profile_ctx = {
                let n = js::number(eff.get("ctx-size"));
                if n.is_nan() {
                    0.0
                } else {
                    n
                }
            };
            let fits_bf16 = match &ceilings {
                None => true,
                Some(c) => match c.get("bf16").as_num() {
                    Some(cap) => profile_ctx == 0.0 || cap >= profile_ctx,
                    None => false,
                },
            };
            let has_f16 = order.iter().any(|o| o.as_str() == Some("f16"));
            if measured.is_none()
                && k_text == "bf16"
                && fits_bf16
                && !has_f16
                && load_failure(&failure)
            {
                order.insert(i + 1, JValue::from("f16"));
                self.with_phase(mi, pi, |p| {
                    if let Some(steps) = js::get_mut(p, "steps").and_then(js::items_mut) {
                        let at = steps
                            .iter()
                            .position(|s| s.get("id") == &k)
                            .map_or(0, |n| n + 1);
                        steps.insert(at, phases::step("f16", "f16 KV cache"));
                    }
                });
                kv_fallback = Some(JValue::obj([
                    ("from", JValue::from("bf16")),
                    ("to", JValue::from("f16")),
                ]));
                if let Some(c) = ceilings.as_mut() {
                    let bf16 = c.get("bf16").clone();
                    js::set(c, "f16", bf16);
                }
                self.note(
                    "bf16 KV cache did not load; trying f16 (also unquantized) in its place.",
                )?;
                self.save()?;
                i += 1;
                continue;
            }
            if kv::unquantized(&k_text) && self.gate(&model).is_none() {
                self.ensure_baseline(
                    &before,
                    BaselineOpts {
                        skip_bf16: k_text == "bf16",
                        bf16_refused: k_text == "bf16"
                            && (kv::bf16_unsupported(Some(&failure)) || load_failure(&failure)),
                        f16_standin: fits_bf16,
                        sized: None,
                    },
                )
                .await?;
            }
            i += 1;
        }
        let passed: Vec<String> = results.iter().map(|(k, _)| string_of(k)).collect();
        let chosen = kv::prefer(&passed, ceilings.as_ref());
        let Some((best_kv, best)) = results
            .iter()
            .find(|(k, _)| chosen.as_deref() == Some(string_of(k).as_str()))
            .cloned()
        else {
            return Err(Fail::new(
                "No KV cache type passed quality and throughput checks.",
            ));
        };
        let mut options = JValue::obj([
            ("cache-type-k", best_kv.clone()),
            ("cache-type-v", best_kv.clone()),
        ]);
        for key in SPEC_KEYS {
            js::set(&mut options, key, or_empty(&before, key));
        }
        self.write(options).await?;
        self.load().await?;
        let fin = self.validate(&model, None).await?;
        let mut value = JValue::obj([("kv", best_kv.clone())]);
        if let Some(fb) = kv_fallback.filter(|_| best_kv.as_str() == Some("f16")) {
            js::set(&mut value, "kvFallback", fb);
        }
        js::set(&mut value, "generation", best.get("generation").clone());
        js::set(&mut value, "quality", fin.get("quality").clone());
        let candidates = results
            .iter()
            .map(|(k, m)| {
                let mut row = JValue::obj([
                    ("kv", k.clone()),
                    ("generation", m.get("generation").clone()),
                ]);
                if let Some(c) = &ceilings {
                    let cap = c.get(&string_of(k));
                    js::set(
                        &mut row,
                        "ceiling",
                        if cap.is_nullish() {
                            JValue::Null
                        } else {
                            cap.clone()
                        },
                    );
                }
                row
            })
            .collect();
        js::set(&mut value, "candidates", JValue::Arr(candidates));
        self.with_phase(mi, pi, |p| js::set(p, "value", value));
        let first = results
            .first()
            .map(|(k, _)| k.clone())
            .unwrap_or(JValue::Undefined);
        let tail = match &ceilings {
            Some(c) if best_kv != first => format!(
                " (it fits {} tokens, at least twice what {} fits).",
                string_of(c.get(&string_of(&best_kv))),
                string_of(&first)
            ),
            _ => ".".to_string(),
        };
        self.note(&format!(
            "Committed {} KV cache after quality checks{tail}",
            string_of(&best_kv)
        ))
    }

    /// `runContext(j, p)`.
    async fn run_context(&self, mi: usize, pi: usize) -> Result<(), Fail> {
        let result = self.context_stage(mi, pi, "").await?;
        let value = JValue::obj([
            ("context", result.get("appliedCtx").clone()),
            ("verifiedCtx", result.get("verifiedCtx").clone()),
            ("loadCtx", result.get("loadCtx").clone()),
        ]);
        self.with_phase(mi, pi, |p| js::set(p, "value", value));
        self.note(&format!(
            "Committed context {} after long-prompt recall.",
            string_of(result.get("appliedCtx"))
        ))
    }

    /// `runDrafting(j, p)`: every speculative profile; one that drafts nothing or changes the
    /// deterministic list answer fails.
    async fn run_drafting(&self, mi: usize, pi: usize) -> Result<(), Fail> {
        let model = self.job().get("model").as_str().unwrap_or("").to_string();
        let mut results: Vec<(usize, JValue)> = Vec::new();
        let mut reference: JValue = JValue::Null;
        let list_text = |m: &JValue| {
            js::items(m.get("workloads"))
                .iter()
                .find(|w| w.get("workload").as_str() == Some("list"))
                .map(|w| w.get("text").clone())
                .unwrap_or(JValue::Undefined)
        };
        for (ci, candidate) in phases::SPEC_CANDIDATES.iter().enumerate() {
            self.check()?;
            self.note(&format!("Testing drafting: {}", candidate.label))?;
            self.unload_all(false).await?;
            let model_ref = model.as_str();
            let measured = self
                .measure(
                    mi,
                    pi,
                    &JValue::from(candidate.id),
                    candidate.options_json(),
                    move || Box::pin(self.validate(model_ref, None)),
                )
                .await?;
            let Some(m) = measured else { continue };
            if candidate.id == "off" {
                reference = list_text(&m);
            }
            if candidate.id != "off"
                && (!js::items(m.get("workloads"))
                    .iter()
                    .any(|w| w.get("drafted").as_num().is_some_and(|d| d > 0.0))
                    || list_text(&m) != reference)
            {
                self.record(
                    mi,
                    pi,
                    &JValue::from(candidate.id),
                    "failed",
                    JValue::obj([(
                        "reason",
                        JValue::from(
                            "No active drafting or the deterministic list answer changed.",
                        ),
                    )]),
                )?;
                continue;
            }
            results.push((ci, m));
        }
        let Some((_, off)) = results.iter().find(|(ci, _)| *ci == 0).cloned() else {
            return Err(Fail::new("The drafting-off baseline failed."));
        };
        let off_entry = |results: &[(usize, JValue)]| {
            results.first().cloned().unwrap_or((0, JValue::Undefined))
        };
        let gen = |m: &JValue| m.get("generation").as_num().unwrap_or(f64::NAN);
        // A stable sort by generation, highest first: the earliest of the fastest (`off` is in
        // `results`, so there is one).
        let mut best = &off_entry(&results);
        for r in &results {
            if gen(&r.1) > gen(&best.1) {
                best = r;
            }
        }
        let Some(candidate) = phases::SPEC_CANDIDATES.get(best.0).copied() else {
            return Err(Fail::new("The drafting-off baseline failed."));
        };
        self.write(candidate.options_json()).await?;
        self.load().await?;
        let fin = self.validate(&model, None).await?;
        if !self.item(mi).get("plan").truthy() {
            self.verify_context(mi, pi).await?;
        }
        let value = JValue::obj([
            ("spec", JValue::from(candidate.id)),
            ("specLabel", JValue::from(candidate.label)),
            ("generation", fin.get("generation").clone()),
            ("acceptance", fin.get("acceptance").clone()),
            ("quality", fin.get("quality").clone()),
            ("baseline", off.get("generation").clone()),
        ]);
        self.with_phase(mi, pi, |p| js::set(p, "value", value));
        self.note(&format!("Committed drafting: {}.", candidate.label))
    }

    /// `runBatch(j, p)`: each micro-batch by prompt throughput on a ~3,000-token prompt.
    async fn run_batch(&self, mi: usize, pi: usize) -> Result<(), Fail> {
        let model = self.job().get("model").as_str().unwrap_or("").to_string();
        let prompt = format!(
            "{}\nIn one sentence, what did the committee review?",
            PAD.repeat(167)
        );
        let mut results: Vec<(u32, JValue)> = Vec::new();
        for ub in phases::UBATCH {
            self.check()?;
            self.note(&format!("Testing micro-batch {ub}"))?;
            self.unload_all(false).await?;
            let options = JValue::obj([
                ("ubatch-size", JValue::from(ub.to_string())),
                ("batch-size", JValue::from(ub.max(2048).to_string())),
            ]);
            let model_ref = model.as_str();
            let prompt_ref = prompt.as_str();
            let measured = self
                .measure(mi, pi, &JValue::from(ub.to_string()), options, move || {
                    Box::pin(async move {
                        let q = self
                            .quality(model_ref, self.gate(model_ref).as_ref())
                            .await?;
                        if q.get("passed") != &JValue::Bool(true) {
                            return Err(Fail::new(quality::quality_failure(&q)));
                        }
                        let r = self.chat(model_ref, prompt_ref, 16, None).await?;
                        let Some(p) = r
                            .get("prompt")
                            .as_num()
                            .filter(|p| p.is_finite() && *p > 0.0)
                        else {
                            return Err(Fail::new("Missing prompt throughput measurement."));
                        };
                        Ok(JValue::obj([
                            ("promptPerSecond", JValue::Num(js::round(p))),
                            ("quality", q),
                        ]))
                    })
                })
                .await?;
            if let Some(m) = measured {
                results.push((ub, m));
            }
        }
        let pps = |m: &JValue| m.get("promptPerSecond").as_num().unwrap_or(f64::NAN);
        let mut best: Option<&(u32, JValue)> = None;
        for r in &results {
            // `b.promptPerSecond - a.promptPerSecond || a.ub - b.ub`, stable: highest throughput,
            // then the smaller micro-batch.
            if best.is_none_or(|b| pps(&r.1) > pps(&b.1) || (pps(&r.1) == pps(&b.1) && r.0 < b.0)) {
                best = Some(r);
            }
        }
        let Some((ub, best)) = best.cloned() else {
            return Err(Fail::new(
                "No micro-batch setting passed quality and prompt throughput checks.",
            ));
        };
        self.write(JValue::obj([
            ("ubatch-size", JValue::from(ub.to_string())),
            ("batch-size", JValue::from(ub.max(2048).to_string())),
        ]))
        .await?;
        self.load().await?;
        let fin = self.validate(&model, None).await?;
        if !self.item(mi).get("plan").truthy() {
            self.verify_context(mi, pi).await?;
        }
        let value = JValue::obj([
            ("ubatch", JValue::Num(f64::from(ub))),
            ("batch", JValue::Num(f64::from(ub.max(2048)))),
            ("promptPerSecond", best.get("promptPerSecond").clone()),
            ("generation", fin.get("generation").clone()),
            ("acceptance", fin.get("acceptance").clone()),
            ("quality", fin.get("quality").clone()),
        ]);
        self.with_phase(mi, pi, |p| js::set(p, "value", value));
        self.note(&format!("Committed micro-batch {ub}."))
    }

    /// `runPhase(j, item, p)`: one phase of the standard order, restored to its starting text on
    /// any failure; a sampling failure after a confirmed restore does not fail the tune.
    pub(crate) async fn run_phase(&self, mi: usize, pi: usize) -> Result<(), Fail> {
        self.check()?;
        self.guard_revision()?;
        let (text, _) = self.deps().presets_snapshot();
        let now = self.deps().now();
        let p = self.phase(mi, pi);
        let id = p.get("id").as_str().unwrap_or("").to_string();
        let steps = phases::steps_for(&id, &self.item_kv(&self.item(mi)));
        self.with_phase(mi, pi, |p| {
            js::set(p, "_beforeText", text.as_str());
            js::set(p, "status", "running");
            js::set(p, "startedAt", JValue::Num(now));
            js::set(p, "steps", steps);
        });
        let label = p.get("label").clone();
        self.with_job(|j| js::set(j, "phase", label));
        self.save()?;
        let outcome: Result<(), Fail> = async {
            match id.as_str() {
                "sampling" => self.run_sampling(mi, pi).await?,
                "kv" => self.run_kv(mi, pi).await?,
                "context" => self.run_context(mi, pi).await?,
                "drafting" => self.run_drafting(mi, pi).await?,
                "batch" => self.run_batch(mi, pi).await?,
                // `phaseRuns[p.id]` is undefined: calling it throws a TypeError.
                other => {
                    return Err(Fail::new(format!(
                        "phaseRuns[p.id] is not a function ({other})"
                    )))
                }
            }
            self.check()?;
            self.guard_revision()
        }
        .await;
        match outcome {
            Ok(()) => {
                let now = self.deps().now();
                let revision = self.job().get("_revision").clone();
                self.with_phase(mi, pi, |p| {
                    js::set(p, "status", "passed");
                    js::set(p, "finishedAt", JValue::Num(now));
                    js::set(p, "_committedRevision", revision);
                    js::del(p, "_beforeText");
                });
                self.save()
            }
            Err(e) => {
                let restored = self.restore_phase(mi, pi).await?;
                if id == "sampling" && restored && !self.cancelled() && !e.cancelled && !e.fatal {
                    let now = self.deps().now();
                    self.with_phase(mi, pi, |p| {
                        if let Some(steps) = js::get_mut(p, "steps").and_then(js::items_mut) {
                            for s in steps.iter_mut() {
                                if s.get("status").as_str() != Some("skipped") {
                                    js::set(s, "status", "failed");
                                    js::set(s, "reason", e.message.as_str());
                                }
                            }
                        }
                        js::set(p, "status", "passed");
                        js::set(p, "reason", e.message.as_str());
                        js::set(
                            p,
                            "value",
                            JValue::obj([
                                ("skipped", JValue::Bool(true)),
                                ("failed", JValue::Bool(true)),
                                ("reason", JValue::from(e.message.as_str())),
                            ]),
                        );
                        js::set(p, "finishedAt", JValue::Num(now));
                    });
                    self.note(&format!(
                        "Recommended sampling was not applied and models.ini was restored: {}",
                        e.message
                    ))?;
                    return Ok(());
                }
                self.close_failed_phase(mi, pi, &e)?;
                if !restored {
                    return Err(unsafe_restore(&e));
                }
                Err(e)
            }
        }
    }

    /// The failed or interrupted phase's record (`runPhase`/`inStage` catch blocks).
    pub(crate) fn close_failed_phase(&self, mi: usize, pi: usize, e: &Fail) -> Result<(), Fail> {
        let interrupted = self.cancelled() || e.cancelled;
        let now = self.deps().now();
        self.with_phase(mi, pi, |p| {
            js::set(
                p,
                "status",
                if interrupted { "interrupted" } else { "failed" },
            );
            js::set(p, "reason", e.message.as_str());
            if let Some(steps) = js::get_mut(p, "steps").and_then(js::items_mut) {
                for s in steps.iter_mut() {
                    if s.get("status").as_str() == Some("running") {
                        js::set(s, "status", "interrupted");
                        js::set(s, "reason", e.message.as_str());
                    }
                }
            }
            js::set(p, "finishedAt", JValue::Num(now));
        });
        self.save()
    }

    /// `servingCheck(j, model)` (#1003): one chat-shaped request (system turn, tools where chat
    /// would send them) must be served. `Null` when the checker itself is unavailable.
    pub(crate) async fn serving_check(&self, model: &str) -> Result<JValue, Fail> {
        let Some(encoded) = encode_uri_component(model) else {
            return Err(Fail::new("URI malformed"));
        };
        let props = self
            .request(
                crate::Call::get(format!("/props?model={encoded}&autoload=false"), 8000),
                None,
            )
            .await
            .ok();
        let template = props.filter(|p| p.get("ok").truthy()).and_then(|p| {
            p.get("body")
                .get("chat_template")
                .as_str()
                .map(str::to_string)
        });
        let mut send_tools = JValue::Bool(true);
        if let Some(t) = &template {
            send_tools = match self.deps().template_caps(t) {
                Ok(caps) => caps.get("sendTools").clone(),
                Err(_) => JValue::Bool(true),
            };
        }
        let ask = |tools: bool| {
            let mut extra = JValue::obj([("messages", serving_messages())]);
            if tools {
                js::set(&mut extra, "tools", serving_tools());
            }
            extra
        };
        let extra = ask(send_tools.truthy());
        let mut r = self.chat(model, "", 64, Some(&extra)).await?;
        let verdict_of = |r: &JValue| {
            self.deps().serving_verdict(
                js::number(r.get("status")),
                r.get("bodyText").as_str().unwrap_or(""),
            )
        };
        let mut verdict = match verdict_of(&r) {
            Ok(v) => v,
            Err(_) => {
                self.note("The realistic chat check was skipped: its checker is unavailable.")?;
                return Ok(JValue::Null);
            }
        };
        let mut retried = false;
        if !verdict.get("passed").truthy()
            && send_tools.truthy()
            && verdict.get("kind").as_str() == Some("template_or_tools_unsupported")
        {
            self.note("The engine refused tools for this chat template; checking again without tools, as chat would.")?;
            retried = true;
            send_tools = JValue::Bool(false);
            let extra = ask(false);
            r = self.chat(model, "", 64, Some(&extra)).await?;
            verdict = match verdict_of(&r) {
                Ok(v) => v,
                Err(_) => return Ok(JValue::Null),
            };
        }
        if !verdict.get("passed").truthy() {
            let reason = verdict.get("reason");
            let why = if reason.truthy() {
                string_of(reason)
            } else {
                string_of(verdict.get("kind")).replace('_', " ")
            };
            return Err(Fail::public(format!(
                "The saved profile cannot serve a realistic chat request{}: {why}.",
                if send_tools.truthy() {
                    " with the app's tools"
                } else {
                    ""
                }
            )));
        }
        Ok(JValue::obj([
            ("passed", JValue::Bool(true)),
            ("toolsSent", send_tools),
            ("templateKnown", JValue::Bool(template.is_some())),
            ("retriedWithoutTools", JValue::Bool(retried)),
        ]))
    }

    /// `signature(model, identity, kvs)`: the scan policy's signature of the profile tuned.
    pub(crate) fn signature(
        &self,
        model: &str,
        identity: &JValue,
        kvs: &[JValue],
    ) -> Result<JValue, Fail> {
        let allow_q5 = kvs
            .iter()
            .any(|k| matches!(k.as_str(), Some("q5_1" | "q5_0")));
        let r = self.deps().scan(JValue::obj([
            ("version", JValue::Num(1.0)),
            ("op", JValue::from("signature")),
            (
                "row",
                JValue::obj([
                    ("identity", identity.clone()),
                    ("profile", self.scan_profile(model)),
                    ("allowQ5Kv", JValue::Bool(allow_q5)),
                ]),
            ),
        ]))?;
        Ok(r.get("signature").clone())
    }

    /// `finishModel(j, item, final, planned)`: the item's result and history entry.
    pub(crate) async fn finish_model(
        &self,
        mi: usize,
        fin: &JValue,
        planned: Option<(JValue, JValue)>,
    ) -> Result<(), Fail> {
        let item = self.item(mi);
        let model = item.get("model").as_str().unwrap_or("").to_string();
        let identity = self.deps().identity_for(&model).await?;
        let identity = match identity.filter(JValue::truthy) {
            Some(id)
                if item.get("_identity").as_str() == Some(crate::stable_identity(&id).as_str()) =>
            {
                id
            }
            _ => return Err(Fail::fatal("Model identity changed since tuning began.")),
        };
        let phase_value = |id: &str| {
            js::items(item.get("phases"))
                .iter()
                .find(|p| p.get("id").as_str() == Some(id))
                .map(|p| p.get("value").clone())
                .unwrap_or(JValue::Undefined)
        };
        let (kv_value, context) = match &planned {
            Some((kv, ctx)) => (kv.clone(), ctx.clone()),
            None => (
                phase_value("kv").get("kv").clone(),
                phase_value("context").get("context").clone(),
            ),
        };
        let draft = phase_value("drafting");
        let batch = phase_value("batch");
        let mut result = JValue::obj([("kv", kv_value), ("context", context)]);
        if planned.is_some() {
            js::set(&mut result, "plan", "wasm");
            let probes = js::items(item.get("plan").get("results"))
                .iter()
                .filter(|r| r.get("step").as_str() == Some("probe"))
                .count();
            js::set(&mut result, "probes", JValue::Num(probes as f64));
        }
        let plan_fallback = item.get("plan").get("kvFallback");
        let kv_fallback = if plan_fallback.truthy() {
            plan_fallback.clone()
        } else {
            phase_value("kv").get("kvFallback").clone()
        };
        if kv_fallback.truthy() {
            js::set(&mut result, "kvFallback", kv_fallback);
        }
        let sampling = phase_value("sampling");
        for (k, v) in [
            ("spec", draft.get("spec").clone()),
            ("specLabel", draft.get("specLabel").clone()),
            ("generation", fin.get("generation").clone()),
            ("acceptance", fin.get("acceptance").clone()),
            ("ubatch", batch.get("ubatch").clone()),
            ("promptPerSecond", batch.get("promptPerSecond").clone()),
            ("quality", fin.get("quality").clone()),
            ("extensions", JValue::Arr(Vec::new())),
            (
                "sampling",
                if sampling.truthy() {
                    sampling
                } else {
                    JValue::Null
                },
            ),
        ] {
            js::set(&mut result, k, v);
        }
        if item.get("baseline").truthy() {
            js::set(&mut result, "baseline", item.get("baseline").clone());
        }
        if fin.get("serving").truthy() {
            js::set(&mut result, "serving", fin.get("serving").clone());
        }
        js::set(&mut result, "loaded", true);
        js::set(&mut result, "version", JValue::Num(f64::from(VERSION)));
        let signature = self.signature(&model, &identity, &self.item_kv(&item))?;
        js::set(&mut result, "signature", signature);
        self.guard_revision()?;
        self.with_item(mi, |item| {
            js::set(item, "result", result.clone());
            js::set(item, "status", "passed");
        });
        let now = self.deps().now();
        {
            let mut st = self.st();
            let mut entry = JValue::obj([("at", JValue::Num(now))]);
            for (k, v) in js::entries(&result).into_iter().flatten() {
                js::set(&mut entry, k, v.clone());
            }
            let mut list = vec![entry];
            let old = st.state.get("history").get(&model);
            if old.truthy() {
                list.extend(js::items(old).iter().cloned());
            }
            list.truncate(10);
            if let Some(history) = js::get_mut(&mut st.state, "history") {
                js::set(history, &model, JValue::Arr(list));
            }
        }
        self.save()?;
        if self.deps().on_result(&model, &result).await.is_err() {
            self.note("Settings saved, but qualification evidence could not be recorded.")?;
        }
        Ok(())
    }

    /// `restorePhase(j, p)`: models.ini back to the phase's starting text over this job's own
    /// revision (an operator's later edit stands); a lost lease leaves it to the recovery.
    pub(crate) async fn restore_phase(&self, mi: usize, pi: usize) -> Result<bool, Fail> {
        let before = self.phase(mi, pi).get("_beforeText").clone();
        if before.is_nullish() {
            return Ok(true);
        }
        if self.revision_moved() {
            self.with_phase(mi, pi, |p| js::set(p, "restored", false));
            return Ok(false);
        }
        let park = |me: &Self| -> Result<bool, Fail> {
            let rec = {
                let j = me.job();
                JValue::obj([
                    ("revision", j.get("_revision").clone()),
                    ("text", before.clone()),
                    ("backedUp", JValue::Bool(j.get("_backedUp").truthy())),
                ])
            };
            me.with_job(|j| {
                if !j.get("_recovery").truthy() {
                    js::set(j, "_recovery", rec);
                }
            });
            me.with_phase(mi, pi, |p| js::set(p, "restored", false));
            me.save()?;
            me.require_trial_recovery();
            Ok(false)
        };
        if self.deps().recoverable() && self.deps().coverage_lost() {
            return park(self);
        }
        let attempt: Result<(), Fail> = async {
            self.unload_own().await?;
            self.deps().assert_covered()?;
            let j = self.job();
            self.deps()
                .presets_commit(
                    JValue::obj([
                        ("baseRevision", j.get("_revision").clone()),
                        ("text", before.clone()),
                    ]),
                    if j.get("_backedUp").truthy() {
                        JValue::obj([("backup", JValue::Bool(false))])
                    } else {
                        JValue::Undefined
                    },
                )
                .await?;
            let (_, revision) = self.deps().presets_snapshot();
            self.with_job(|j| js::set(j, "_revision", revision));
            self.save()?;
            self.reread_after_restore().await?;
            self.with_phase(mi, pi, |p| {
                js::set(p, "restored", true);
                js::del(p, "_beforeText");
            });
            self.save()
        }
        .await;
        match attempt {
            Ok(()) => Ok(true),
            Err(e) => {
                if self.deps().recoverable() && (e.coverage_lost || self.deps().coverage_lost()) {
                    return park(self);
                }
                self.with_phase(mi, pi, |p| js::set(p, "restored", false));
                self.save()?;
                Ok(false)
            }
        }
    }

    /// `requireTrialRecovery(j)`: the gate keeps chat and new maintenance closed until
    /// [`Runner::recover_trial`] has restored the text under a fresh lease.
    pub(crate) fn require_trial_recovery(&self) {
        let j = self.job();
        if !j.get("_recovery").truthy() || !self.deps().recoverable() {
            return;
        }
        self.deps().require_recovery(
            &format!("autotune:{}", string_of(j.get("id"))),
            "Chat is paused while noevia restores the settings an interrupted tune left behind.",
        );
    }

    /// `recoverTrial(j)`: the repair the gate runs with the engine idle under a fresh lease.
    pub async fn recover_trial(&self) -> Result<(), Fail> {
        let rec = self.job().get("_recovery").clone();
        if !rec.truthy() {
            return Ok(());
        }
        let (_, revision) = self.deps().presets_snapshot();
        if rec.get("revision").as_str() == Some(revision.as_str()) {
            self.deps().assert_covered()?;
            self.deps()
                .presets_commit(
                    JValue::obj([
                        ("baseRevision", rec.get("revision").clone()),
                        ("text", rec.get("text").clone()),
                    ]),
                    if rec.get("backedUp").truthy() {
                        JValue::obj([("backup", JValue::Bool(false))])
                    } else {
                        JValue::Undefined
                    },
                )
                .await?;
            let (_, revision) = self.deps().presets_snapshot();
            self.with_job(|j| js::set(j, "_revision", revision));
        }
        self.with_job(|j| js::set(j, "_reloadPending", true));
        self.save()?;
        self.deps().assert_covered()?;
        if !self.deps().invalidate_baseline("autotune").await {
            return Err(Fail::new(
                "The preset reload baseline did not acknowledge the restore.",
            ));
        }
        let reload = self.reread().await?;
        if !reload.get("ok").truthy() {
            return Err(Fail::new("The router did not confirm restored settings."));
        }
        self.deps().assert_covered()?;
        self.with_job(|j| {
            if let Some(items) = js::get_mut(j, "models").and_then(js::items_mut) {
                for item in items.iter_mut() {
                    if let Some(phases) = js::get_mut(item, "phases").and_then(js::items_mut) {
                        for p in phases.iter_mut() {
                            if !p.get("_beforeText").is_nullish() {
                                js::del(p, "_beforeText");
                                js::set(p, "restored", true);
                            }
                        }
                    }
                }
            }
            js::del(j, "originalText");
            js::del(j, "_recovery");
            js::set(j, "_reloadPending", false);
            js::set(j, "_unsafe", false);
            js::set(j, "restored", true);
        });
        self.note("Settings an interrupted tune left behind were restored, and the model server re-read them.")?;
        self.save()
    }
}

/// `compact(result)`: the measurement without the workloads' answer texts.
fn compact(result: &JValue) -> JValue {
    let mut out = js::spread(&[result]);
    let workloads = match result.get("workloads") {
        JValue::Arr(rows) => JValue::Arr(
            rows.iter()
                .map(|row| {
                    let mut row = js::spread(&[row]);
                    js::del(&mut row, "text");
                    row
                })
                .collect(),
        ),
        _ => JValue::Undefined,
    };
    js::set(&mut out, "workloads", workloads);
    out
}

/// The size `ensureBaseline` measures at in a planned run (`baselineSize`).
#[derive(Debug, Clone)]
pub(crate) struct Sized {
    pub ctx: f64,
    pub kv: String,
    pub f16: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct BaselineOpts {
    pub skip_bf16: bool,
    pub bf16_refused: bool,
    pub f16_standin: bool,
    pub sized: Option<Sized>,
}

impl Default for BaselineOpts {
    fn default() -> Self {
        BaselineOpts {
            skip_bf16: false,
            bf16_refused: false,
            f16_standin: true,
            sized: None,
        }
    }
}

fn serving_messages() -> JValue {
    JValue::Arr(vec![
        JValue::obj([
            ("role", JValue::from("system")),
            ("content", JValue::from("You are a helpful assistant in a project workspace. Use a tool only when it is needed, and answer briefly.")),
        ]),
        JValue::obj([
            ("role", JValue::from("user")),
            ("content", JValue::from("Which seed orders did the garden committee approve last week?")),
        ]),
    ])
}

fn serving_tools() -> JValue {
    #[allow(clippy::expect_used)] // a constant
    wtf16::parse(r#"[{"type":"function","function":{"name":"search_files","description":"Search the files of this project.","parameters":{"type":"object","properties":{"query":{"type":"string","description":"What to look for."}},"required":["query"]}}},{"type":"function","function":{"name":"more_tools","description":"Call this only if none of the offered tools can do the user's task.","parameters":{"type":"object","properties":{}}}}]"#)
        .expect("serving tools")
}

impl<D: Deps> Hooks for StageHooks<D> {
    fn context_current(&self) -> bool {
        if self.fill {
            return false;
        }
        let r = &self.runner;
        let st = r.st();
        st.job_object == self.job_object
            && st.run == self.run
            && st.state.get("job").get("model").as_str() == Some(self.model.as_str())
            && st.state.get("job").get("status").as_str() == Some("running")
            && !st.cancelled
            && !st.starting
    }

    fn apply_unlocked(&self, body: JValue) -> impl Future<Output = Result<JValue, Fail>> + Send {
        self.runner.guarded_apply(body)
    }

    fn foreign_held(&self, rows: JValue) -> impl Future<Output = Result<Vec<JValue>, Fail>> + Send {
        let model = JValue::from(self.model.as_str());
        async move { Ok(self.runner.held_foreign(js::items(&rows), &model).await) }
    }

    async fn foreign_wait(&self, legacy: Fail) -> Result<(), Fail> {
        match self.runner.yield_to_foreign(legacy, false).await {
            Ok(()) => Ok(()),
            Err(e) => {
                if e.foreign_timeout || e.cancelled {
                    *lock(&self.foreign_stop) = Some(e.clone());
                }
                Err(e)
            }
        }
    }

    fn on_write(&self, revision: &str) {
        self.runner.with_job(|j| js::set(j, "_revision", revision));
        if let Err(e) = self.runner.save() {
            self.runner
                .deps()
                .log(&format!("[autotune] save failed: {}", e.message));
        }
    }

    fn on_update(&self, c: &JValue) {
        let Some((mi, pi, prefix, offset)) = &self.phase else {
            return;
        };
        let offset = *offset;
        if !c.truthy() {
            return;
        }
        let r = &self.runner;
        let p = r.phase(*mi, *pi);
        let label = format!(
            "{}: {}",
            string_of(p.get("label")),
            string_of(c.get("phase"))
        );
        r.with_job(|j| js::set(j, "phase", label));
        let mapped: Vec<JValue> = js::items(c.get("steps"))
            .iter()
            .enumerate()
            .map(|(index, s)| {
                let kind = if s.get("kind").as_str() == Some("long") {
                    "Recall "
                } else {
                    "Load "
                };
                let status = s
                    .get("status")
                    .as_str()
                    .filter(|st| matches!(*st, "passed" | "running" | "skipped"))
                    .unwrap_or("failed");
                JValue::obj([
                    ("id", JValue::from(format!("{prefix}{index}"))),
                    (
                        "label",
                        JValue::from(format!("{prefix}{kind}{}", string_of(s.get("ctx")))),
                    ),
                    ("status", JValue::from(status)),
                    ("reason", s.get("reason").clone()),
                    ("ctx", s.get("ctx").clone()),
                    ("promptPerSecond", s.get("promptPerSecond").clone()),
                    ("promptSeconds", s.get("promptSeconds").clone()),
                ])
            })
            .collect();
        r.with_phase(*mi, *pi, |p| {
            let mut steps: Vec<JValue> = js::items(p.get("steps"))
                .iter()
                .take(offset)
                .cloned()
                .collect();
            steps.extend(mapped);
            js::set(p, "steps", JValue::Arr(steps));
        });
        if let Err(e) = r.save() {
            r.deps()
                .log(&format!("[autotune] save failed: {}", e.message));
        }
    }
}
