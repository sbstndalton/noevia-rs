//! Native chart inputs: helpers._predicted_vram_gb and the private bench-run runtime.
//! These optional estimates never ingest telemetry or change a section.
use super::*;

impl SectionAutoconfig {
    /// None retains compatibility with a Python-owned sampler. With a native sampler this
    /// always answers locally, including optional discovery/file failures (zero/null).
    pub async fn chart_runtime(&self, aliases: &[String]) -> Option<Vec<u8>> {
        let stats = self.stats.get()?;
        let backends = match stats.fallback_reads.run(stats.descriptors()).await {
            Ok(Ok(descriptors)) => inventory::backend_list(&descriptors),
            _ => None,
        };
        let capacity = capacity(backends.as_ref());
        let (settings, open, aliases) = (self.settings.clone(), self.open, aliases.to_vec());
        let names = aliases.clone();
        let predicted = self
            .blocking(move || {
                Ok(aliases
                    .iter()
                    .map(|name| {
                        prediction(&settings, open, name, backends.as_ref()).unwrap_or(Value::Null)
                    })
                    .collect::<Vec<_>>())
            })
            .await
            .unwrap_or_else(|_| vec![Value::Null; names.len()]);
        let value = Value::Obj(vec![
            ("capacity_gb".into(), Value::Float(capacity)),
            (
                "predicted".into(),
                Value::Obj(names.into_iter().zip(predicted).collect()),
            ),
        ]);
        let mut out = String::new();
        render::write(&mut out, &value);
        Some(out.into_bytes())
    }
}

fn capacity(backends: Option<&Value>) -> f64 {
    let Some(Value::Arr(backends)) = backends else {
        return 0.0;
    };
    let mut largest: Option<f64> = None;
    for b in backends {
        let value = b
            .get("vram_gb")
            .and_then(|v| model_autoconfig::pyval::float_of(v, "vram").ok())
            .unwrap_or(0.0);
        if largest.is_none_or(|old| value > old) {
            largest = Some(value);
        }
    }
    model_autoconfig::pyfloat::round_nd(largest.unwrap_or(0.0), 2)
}

fn prediction(
    settings: &Settings,
    open: fn(&Path) -> Result<Root, WalkError>,
    name: &str,
    backends: Option<&Value>,
) -> Option<Value> {
    let backends = backends?;
    if !backends.truthy() {
        return None;
    }
    let files::Part::Located(located) = files::locate_prediction(
        Path::new(&settings.models),
        Path::new(&settings.ini),
        name,
        open,
    )
    .ok()?
    else {
        return None;
    };
    let (current, _) = read_current(Path::new(&settings.ini), name).ok()?;
    let ctx = current
        .as_ref()
        .and_then(|items| items.iter().rev().find(|(k, _)| k == "ctx-size"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let root = open(&located.base).ok()?;
    let subdir = located
        .rel
        .as_deref()
        .and_then(|r| r.rsplit_once('/'))
        .map_or("", |(d, _)| d);
    let summary = analyze::from_summary(&located.summary);
    let plan = analyze::analyze(&analyze::Inputs {
        name,
        summary: &summary,
        file_size: located.file_size,
        backends: backends.clone(),
        model_rel: &located.model_rel,
        current: current.clone(),
        preset: "",
        n_sessions: 1,
        model_subdir: subdir,
        spec_profile: "",
        vision: true,
        prompt_tps: 0.0,
        prompt_budget_s: 120.0,
        verified_ctx: 0,
        mode: "",
        cache_ram_cap_mib: settings.cache_ram_cap_mib,
        files: Some(analyze::Files {
            models_text: &settings.models,
            root: &root,
            base: &located.base,
        }),
        mmproj_gb_override: None,
    })
    .ok()?;
    select(&plan, ctx)
}

fn select(plan: &Value, ctx: &str) -> Option<Value> {
    let Value::Arr(frontier) = plan.get("frontier")? else {
        return None;
    };
    let mut want = match model_autoconfig::pyval::int_str(ctx, "ctx") {
        Ok(n) => n,
        Err(model_autoconfig::Error::Python("ValueError")) => 0,
        Err(_) => return None,
    };
    if want == 0 {
        want = plan
            .get("recommended_total_ctx")
            .filter(|v| v.truthy())
            .or_else(|| plan.get("recommended_ctx"))
            .and_then(|v| model_autoconfig::pyval::int_of(v, "ctx").ok())
            .unwrap_or(0);
    }
    let row = if want == 0 {
        frontier.first()?
    } else {
        frontier.iter().min_by_key(|r| {
            let ctx = r
                .get("ctx")
                .and_then(|v| model_autoconfig::pyval::int_of(v, "ctx").ok())
                .unwrap_or(0);
            ctx.abs_diff(want)
        })?
    };
    let gpu = model_autoconfig::pyval::float_of(row.get("gpu_gb")?, "gpu").ok()?;
    let kv = model_autoconfig::pyval::float_of(row.get("kv_gb")?, "kv").ok()?;
    Some(Value::Float(model_autoconfig::pyfloat::round_nd(
        gpu + kv,
        2,
    )))
}

#[cfg(test)]
mod tests;
