//! benchmark-evidence.cjs `throughputRecords(detail, { now })`: the throughput evidence a finished
//! model-manager benchmark run gives. Only a variant that ran the model's saved preset (no
//! argument overrides) of a run that finished within the last 30 minutes counts, from at least two
//! warm, uncontended, successful results.

use crate::js::{iterate, key_count, round, strict_eq, string, to_number};
use crate::Thrown;
use js_json::{wtf16, JValue};

/// `maxAgeMs`'s default.
pub const MAX_AGE_MS: f64 = 30.0 * 60_000.0;

fn median(values: &[f64]) -> f64 {
    let mut s = values.to_vec();
    // The comparator `a - b` over positive numbers (∞ included): ∞ - ∞ is NaN, which sort reads as
    // equal, and equal they are.
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let m = s.len() / 2;
    let at = |i: usize| s.get(i).copied().unwrap_or(f64::NAN);
    if s.len() % 2 == 1 {
        at(m)
    } else {
        (at(m.wrapping_sub(1)) + at(m)) / 2.0
    }
}

fn round1(n: f64) -> f64 {
    round(n * 10.0) / 10.0
}

/// `detail.list || []` read as an array (`for ... of` for variants, `.filter` for results).
fn list_or_empty(v: &JValue, iterable_strings: bool) -> Result<Vec<JValue>, Thrown> {
    if !v.truthy() {
        return Ok(Vec::new());
    }
    match v {
        JValue::Arr(items) => Ok(items.clone()),
        JValue::Str(_) if iterable_strings => iterate(v),
        _ => Err(Thrown::type_error()),
    }
}

/// `throughputRecords(detail, { now })`: `(model, record)` pairs, in order. A throw (a variant that
/// is `null`, results that are not an array, a value with an own `toString`) is the caller's.
pub fn throughput_records(detail: &JValue, now: f64) -> Result<Vec<(JValue, JValue)>, Thrown> {
    let run = detail.get("run");
    if !run.truthy()
        || !strict_eq(run.get("status"), &JValue::from("done"))
        || !to_number(run.get("finished_at"))?.is_finite()
    {
        return Ok(Vec::new());
    }
    if now - to_number(run.get("finished_at"))? * 1000.0 > MAX_AGE_MS {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for v in list_or_empty(detail.get("variants"), true)? {
        if v.is_nullish() {
            return Err(Thrown::type_error());
        }
        let argv_json = v.get("argv_json");
        // JSON.parse(v.argv_json || '{}'): String() first; a throw of either skips the variant.
        let text = if argv_json.truthy() {
            match string(argv_json) {
                Ok(t) => t,
                Err(_) => continue,
            }
        } else {
            "{}".to_string()
        };
        let argv = match wtf16::parse(&text) {
            Ok(a) => a,
            Err(wtf16::Refusal::Invalid) => continue,
            // JSON.parse reads it (a number past the f64 range, deep nesting); the port treats it
            // as an override, which is what such a text is unless it is a bare number.
            Err(wtf16::Refusal::Unsupported) => continue,
        };
        if argv.truthy() && key_count(&argv) > 0 {
            continue;
        }
        let alias = v.get("alias");
        let mut ok = Vec::new();
        for r in list_or_empty(detail.get("results"), false)? {
            if r.is_nullish() {
                return Err(Thrown::type_error());
            }
            if strict_eq(r.get("alias"), alias)
                && !r.get("err").truthy()
                && !r.get("cold").truthy()
                && !r.get("contended").truthy()
                && !r.get("truncated").truthy()
                && to_number(r.get("gen_tps"))? > 0.0
            {
                ok.push(r);
            }
        }
        if ok.len() < 2 {
            continue;
        }
        let mut prompt = Vec::new();
        for r in &ok {
            let n = to_number(r.get("prompt_tps"))?;
            if n > 0.0 {
                prompt.push(n);
            }
        }
        let mut generated = Vec::new();
        for r in &ok {
            generated.push(to_number(r.get("gen_tps"))?);
        }
        let samples = ok.len();
        let record = JValue::obj([
            ("category", JValue::from("throughput")),
            ("result", JValue::from("reported")),
            (
                "value",
                JValue::obj([
                    ("rate", JValue::Num(round1(median(&generated)))),
                    (
                        "promptRate",
                        if prompt.is_empty() {
                            JValue::Null
                        } else {
                            JValue::Num(round1(median(&prompt)))
                        },
                    ),
                    ("samples", JValue::Num(samples as f64)),
                    ("runId", run.get("id").clone()),
                ]),
            ),
            (
                "suite",
                JValue::obj([
                    ("name", JValue::from("model-manager-benchmark")),
                    ("version", JValue::Num(1.0)),
                ]),
            ),
            ("source", JValue::from("benchmark")),
            (
                "limitations",
                JValue::Arr(vec![JValue::Str(format!(
                    "median of {samples} warm, uncontended requests; depends on prompt mix and max tokens"
                ))]),
            ),
        ]);
        out.push((alias.clone(), record));
    }
    Ok(out)
}
