//! The job record's shape (core llamacpp-full-autotune.cjs `PHASES`, `PLANNED_PHASES`, `stepsFor`,
//! `newModel`, `stoppedAt`, `publicJob`) and the fixed inputs the tuner measures with
//! (llamacpp-tune-spec.cjs `WORKLOADS`, `SPEC_CANDIDATES`, `geomean`).

use crate::js;
use js_json::JValue;

/// The standard order: what a model falls back to when it cannot be planned.
pub const PHASES: [(&str, &str); 5] = [
    ("sampling", "Sampling"),
    ("kv", "KV cache"),
    ("context", "Context size"),
    ("drafting", "Drafting"),
    ("batch", "Batch and micro-batch"),
];

/// The planned order (#1003): context and KV together, then sampling, drafting, batch, and one
/// final fill check.
pub const PLANNED_PHASES: [(&str, &str); 5] = [
    ("context", "Context and KV cache"),
    ("sampling", "Sampling"),
    ("drafting", "Drafting"),
    ("batch", "Batch and micro-batch"),
    ("verify", "Final fill check"),
];

pub const UBATCH: [u32; 3] = [512, 1024, 2048];

/// One deterministic workload: `max_tokens` and the prompt.
#[derive(Debug, Clone, Copy)]
pub struct Workload {
    pub id: &'static str,
    pub max: u32,
    pub prompt: &'static str,
}

pub const WORKLOADS: [Workload; 3] = [
    Workload {
        id: "list",
        max: 160,
        prompt: "List the whole numbers from 1 to 60, separated by commas, and nothing else.",
    },
    Workload {
        id: "prose",
        max: 160,
        prompt: "Write one paragraph explaining why community gardens matter to a neighbourhood.",
    },
    Workload {
        id: "code",
        max: 200,
        prompt: "Write a JavaScript function median(values) that returns the median of an array without modifying it. Code only.",
    },
];

/// One speculative-decoding profile the drafting phase tries.
#[derive(Debug, Clone, Copy)]
pub struct SpecCandidate {
    pub id: &'static str,
    pub label: &'static str,
    /// `spec-type`, `spec-draft-n-max`, `spec-draft-p-min`.
    pub options: [(&'static str, &'static str); 3],
}

impl SpecCandidate {
    /// The candidate's options as the models.ini write body.
    pub fn options_json(&self) -> JValue {
        JValue::obj(self.options.iter().map(|(k, v)| (*k, JValue::from(*v))))
    }
}

pub const SPEC_CANDIDATES: [SpecCandidate; 5] = [
    SpecCandidate {
        id: "off",
        label: "Off",
        options: [
            ("spec-type", "none"),
            ("spec-draft-n-max", ""),
            ("spec-draft-p-min", ""),
        ],
    },
    SpecCandidate {
        id: "mtp",
        label: "MTP (engine defaults)",
        options: [
            ("spec-type", "draft-mtp"),
            ("spec-draft-n-max", ""),
            ("spec-draft-p-min", ""),
        ],
    },
    SpecCandidate {
        id: "mtp-deep",
        label: "MTP deep drafts",
        options: [
            ("spec-type", "draft-mtp"),
            ("spec-draft-n-max", "8"),
            ("spec-draft-p-min", "0.05"),
        ],
    },
    SpecCandidate {
        id: "mtp-shallow",
        label: "MTP shallow drafts",
        options: [
            ("spec-type", "draft-mtp"),
            ("spec-draft-n-max", "2"),
            ("spec-draft-p-min", "0.6"),
        ],
    },
    SpecCandidate {
        id: "ngram",
        label: "N-gram",
        options: [
            ("spec-type", "ngram-simple"),
            ("spec-draft-n-max", ""),
            ("spec-draft-p-min", ""),
        ],
    },
];

/// `geomean(values)` over the positive values (0 when none).
pub fn geomean(values: &[f64]) -> f64 {
    let v: Vec<f64> = values.iter().copied().filter(|x| *x > 0.0).collect();
    if v.is_empty() {
        return 0.0;
    }
    (v.iter().map(|x| x.ln()).sum::<f64>() / v.len() as f64).exp()
}

/// `step(id, label)`.
pub fn step(id: impl Into<JValue>, label: impl Into<JValue>) -> JValue {
    JValue::obj([
        ("id", id.into()),
        ("label", label.into()),
        ("status", JValue::from("pending")),
    ])
}

fn string_of(v: &JValue) -> String {
    js_json::to_js_string(v).unwrap_or_default()
}

/// `stepsFor(id, kv)`.
pub fn steps_for(id: &str, kv: &[JValue]) -> JValue {
    JValue::Arr(match id {
        "sampling" => vec![step("apply", "Recommended sampling")],
        "kv" => kv
            .iter()
            .map(|k| step(k.clone(), string_of(k) + " KV cache"))
            .collect(),
        "drafting" => SPEC_CANDIDATES
            .iter()
            .map(|c| step(c.id, c.label))
            .collect(),
        "batch" => UBATCH
            .iter()
            .map(|n| step(n.to_string(), format!("Micro-batch {n}")))
            .collect(),
        _ => vec![step("capacity", "Load and long-prompt recall")],
    })
}

/// `newModel(model, planned, kv)`: a queue item whose KV list is fixed now (#1057).
pub fn new_model(model: &JValue, planned: bool, kv: &[JValue]) -> JValue {
    let phases = |list: &[(&str, &str)], empty: &[&str]| {
        JValue::Arr(
            list.iter()
                .map(|(id, label)| {
                    JValue::obj([
                        ("id", JValue::from(*id)),
                        ("label", JValue::from(*label)),
                        ("status", JValue::from("pending")),
                        (
                            "steps",
                            if empty.contains(id) {
                                JValue::Arr(Vec::new())
                            } else {
                                steps_for(id, kv)
                            },
                        ),
                    ])
                })
                .collect(),
        )
    };
    let kv = JValue::Arr(kv.to_vec());
    if planned {
        JValue::obj([
            ("model", model.clone()),
            ("status", JValue::from("pending")),
            ("kv", kv),
            ("plan", JValue::obj([("results", JValue::Arr(Vec::new()))])),
            ("phases", phases(&PLANNED_PHASES, &["context", "verify"])),
        ])
    } else {
        JValue::obj([
            ("model", model.clone()),
            ("status", JValue::from("pending")),
            ("kv", kv),
            ("phases", phases(&PHASES, &[])),
        ])
    }
}

fn stopped(p: &JValue) -> bool {
    matches!(p.get("status").as_str(), Some("failed" | "interrupted"))
}

fn find_status<'a>(list: &'a JValue, status: &str) -> Option<&'a JValue> {
    js::items(list)
        .iter()
        .find(|p| p.get("status").as_str() == Some(status))
}

/// `stoppedAt(j)` (#1978): the phase and step a run stopped in, by our own labels.
pub fn stopped_at(j: &mut JValue) {
    let label = {
        let models = js::items(j.get("models"));
        let any_stopped = |m: &&JValue| js::items(m.get("phases")).iter().any(stopped);
        let item = models
            .iter()
            .find(|m| m.get("model") == j.get("model") && any_stopped(m))
            .or_else(|| models.iter().find(any_stopped));
        item.and_then(|item| {
            let phases = item.get("phases");
            let ph =
                find_status(phases, "failed").or_else(|| find_status(phases, "interrupted"))?;
            let phase = ph.get("label").as_str()?.to_string();
            let st = find_status(ph.get("steps"), "failed")
                .or_else(|| find_status(ph.get("steps"), "interrupted"))
                .and_then(|st| st.get("label").as_str().map(str::to_string));
            Some((phase, st))
        })
    };
    js::del(j, "failedPhase");
    js::del(j, "failedStep");
    if let Some((phase, step)) = label {
        js::set(j, "failedPhase", phase);
        if let Some(step) = step {
            js::set(j, "failedStep", step);
        }
    }
}

/// A key `publicJob` (and the state views) leaves out at every depth.
pub fn private_key(key: &str) -> bool {
    key.starts_with('_')
        || matches!(
            key,
            "originalText" | "lastRevision" | "originalRevision" | "beforeText"
        )
}

/// `publicJob(job)`: a JSON copy without private keys at any depth (`null` stays `null`).
pub fn public_job(job: &JValue) -> JValue {
    fn strip(v: &JValue) -> JValue {
        match v {
            JValue::Obj(items) => JValue::Obj(
                items
                    .iter()
                    .filter(|(k, x)| !private_key(k) && !matches!(x, JValue::Undefined))
                    .map(|(k, x)| (k.clone(), strip(x)))
                    .collect(),
            ),
            JValue::Arr(items) => JValue::Arr(
                items
                    .iter()
                    .map(|x| match x {
                        JValue::Undefined => JValue::Null,
                        other => strip(other),
                    })
                    .collect(),
            ),
            JValue::Num(n) if !n.is_finite() => JValue::Null,
            other => other.clone(),
        }
    }
    if job.truthy() {
        strip(job)
    } else {
        job.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geomean_of_positive_values() {
        assert_eq!(geomean(&[]), 0.0);
        assert_eq!(geomean(&[0.0, -1.0]), 0.0);
        assert!((geomean(&[20.0, 80.0]) - 40.0).abs() < 1e-9);
    }
}
