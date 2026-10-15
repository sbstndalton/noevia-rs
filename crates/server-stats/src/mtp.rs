//! core `server/mtp.cjs` `acceptance` (with `specType` and `capability`), the `mtp` rows of the
//! non-llama.cpp kinds, evaluated in the JS's order so that it throws where the JS throws.
//!
//! The JS falls back to the last chat response's draft counts (`record`, kept in Node's memory per
//! user and model) when the backend total gives no rate. The front cannot see those samples, so
//! this port always reports the backend total; noevia-server therefore refuses
//! NOEVIA_RUST_STATS with the Lemonade kind (config.rs) rather than drop the fallback.

use crate::js::{self, get, opt, strict_eq, string_to_number, to_string, truthy, Fault};
use crate::metrics::strip_ascii;
use prompt_framing::json::{self as pjson, Value};

/// One acceptance row: `{ model, rate, drafted, accepted, source: 'backend total' }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Acceptance {
    pub model: Option<Value>,
    pub rate: Option<f64>,
    pub drafted: Option<f64>,
    pub accepted: Option<f64>,
}

struct Token {
    value: Vec<u16>,
}

/// mtp.cjs `tokens(args)` over `String(args)` (a shell-like split: quotes, backslash escapes).
fn tokens(input: &[u16]) -> Vec<Token> {
    const BACKSLASH: u16 = b'\\' as u16;
    const DQ: u16 = b'"' as u16;
    const SQ: u16 = b'\'' as u16;
    let mut result = Vec::new();
    let mut i = 0;
    while let Some(&c) = input.get(i) {
        if js::is_space(c) {
            i += 1;
            continue;
        }
        let mut value = Vec::new();
        let mut quote: Option<u16> = None;
        while let Some(&c) = input.get(i) {
            if quote.is_none() && js::is_space(c) {
                break;
            }
            if c == BACKSLASH && quote != Some(SQ) && i + 1 < input.len() {
                if let Some(&next) = input.get(i + 1) {
                    value.push(next);
                }
                i += 2;
                continue;
            }
            if Some(c) == quote {
                quote = None;
                i += 1;
                continue;
            }
            if quote.is_none() && (c == DQ || c == SQ) {
                quote = Some(c);
                i += 1;
                continue;
            }
            value.push(c);
            i += 1;
        }
        result.push(Token { value });
    }
    result
}

/// mtp.cjs `specType(args)`: the last `--spec-type` value (`None` is JS `null`). `args` is the
/// raw value; `undefined` is the default `''`.
pub fn spec_type(args: Option<&Value>) -> Result<Option<Vec<u16>>, Fault> {
    let text = match args {
        None => Vec::new(),
        Some(v) => to_string(Some(v))?,
    };
    let parts = tokens(&text);
    let flag: Vec<u16> = "--spec-type".encode_utf16().collect();
    let mut value: Option<Vec<u16>> = None;
    let mut i = 0;
    while let Some(part) = parts.get(i) {
        if part.value == flag {
            i += 1;
            // `parts[++i]?.value || null`: a missing or empty value is null.
            value = parts
                .get(i)
                .map(|p| p.value.clone())
                .filter(|v| !v.is_empty());
        } else if let Some(rest) = strip_ascii(&part.value, "--spec-type=") {
            value = Some(rest.to_vec());
        }
        i += 1;
    }
    Ok(value)
}

/// `type.split(',').includes('draft-mtp')`.
fn names_draft_mtp(t: &[u16]) -> bool {
    t.split(|&c| c == u16::from(b','))
        .any(|p| p.iter().copied().eq("draft-mtp".encode_utf16()))
}

/// `labels?.includes('mtp')`: a string searches its text, an array its elements; `None` is
/// `undefined`; anything else has no `includes` (TypeError).
fn includes_mtp(labels: Option<&Value>) -> Result<Option<bool>, Fault> {
    let needle: Vec<u16> = "mtp".encode_utf16().collect();
    match labels {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Str(s)) => Ok(Some(
            s.windows(needle.len()).any(|w| w == needle.as_slice()),
        )),
        Some(Value::Arr(items)) => Ok(Some(
            items
                .iter()
                .any(|v| matches!(v, Value::Str(s) if *s == needle)),
        )),
        Some(Value::Deep) => Err(Fault::Unsupported),
        Some(_) => Err(Fault::Throws),
    }
}

/// mtp.cjs `capability(model).enabled` for a found (truthy) registry row.
fn capability_enabled(model: &Value) -> Result<bool, Fault> {
    let m = Some(model);
    let recipe = opt(m, "recipe")?;
    let llamacpp =
        matches!(recipe, Some(Value::Str(s)) if s.iter().copied().eq("llamacpp".encode_utf16()));
    let supported = llamacpp && includes_mtp(opt(m, "labels")?)? == Some(true);
    // Evaluated whether or not the model is supported, so a throw here is the JS's throw too.
    let t = spec_type(opt(opt(m, "recipe_options")?, "llamacpp_args")?)?;
    Ok(supported && t.as_deref().is_none_or(names_draft_mtp))
}

/// The elements `for (const m of models || [])` visits: an array's elements, a string's code
/// points; any other truthy value is not iterable (TypeError).
fn iterate(models: Option<&Value>) -> Result<Vec<Value>, Fault> {
    if !truthy(models) {
        return Ok(Vec::new());
    }
    match models {
        Some(Value::Arr(items)) => Ok(items.clone()),
        Some(Value::Str(s)) => {
            let mut out = Vec::new();
            let mut i = 0;
            while let Some(&c) = s.get(i) {
                let pair = (0xd800..0xdc00).contains(&c)
                    && s.get(i + 1).is_some_and(|d| (0xdc00..0xe000).contains(d));
                let n = if pair { 2 } else { 1 };
                out.push(Value::Str(s.get(i..i + n).unwrap_or(&[]).to_vec()));
                i += n;
            }
            Ok(out)
        }
        Some(Value::Deep) => Err(Fault::Unsupported),
        _ => Err(Fault::Throws),
    }
}

/// `/^lemonade_llamacpp_spec_decode_num_(draft|accepted)_tokens_total\{([^}]*)\}\s+([^\s]+)/`:
/// (draft?, labels, value).
fn counter_line(line: &[u16]) -> Option<(bool, &[u16], &[u16])> {
    let rest = strip_ascii(line, "lemonade_llamacpp_spec_decode_num_")?;
    let (draft, rest) = match strip_ascii(rest, "draft") {
        Some(r) => (true, r),
        None => (false, strip_ascii(rest, "accepted")?),
    };
    let rest = strip_ascii(rest, "_tokens_total{")?;
    let close = rest.iter().position(|&c| c == u16::from(b'}'))?;
    let labels = rest.get(..close)?;
    let rest = rest.get(close + 1..)?;
    let spaces = rest
        .iter()
        .position(|&c| !js::is_space(c))
        .unwrap_or(rest.len());
    if spaces == 0 {
        return None;
    }
    let rest = rest.get(spaces..)?;
    let len = rest
        .iter()
        .position(|&c| js::is_space(c))
        .unwrap_or(rest.len());
    if len == 0 {
        return None;
    }
    Some((draft, labels, rest.get(..len)?))
}

fn line_terminator(c: u16) -> bool {
    matches!(c, 0x0a | 0x0d | 0x2028 | 0x2029)
}

/// `/(?:^|,)model_name=("(?:[^"\\]|\\.)*")/.exec(labels)?.[1]`.
fn model_name_label(labels: &[u16]) -> Option<&[u16]> {
    let starts = std::iter::once(0).chain(
        labels
            .iter()
            .enumerate()
            .filter(|(_, &c)| c == u16::from(b','))
            .map(|(k, _)| k + 1),
    );
    for q in starts {
        let Some(rest) = labels.get(q..).and_then(|r| strip_ascii(r, "model_name=")) else {
            continue;
        };
        if rest.first() != Some(&u16::from(b'"')) {
            continue;
        }
        let mut i = 1;
        let end = loop {
            match rest.get(i) {
                None => break None,
                Some(&c) if c == u16::from(b'"') => break Some(i + 1),
                Some(&c) if c == u16::from(b'\\') => match rest.get(i + 1) {
                    Some(&n) if !line_terminator(n) => i += 2,
                    _ => break None,
                },
                Some(_) => i += 1,
            }
        };
        if let Some(end) = end {
            return rest.get(..end);
        }
    }
    None
}

/// `acceptance(metrics, models, installed, owner)` with no recorded samples. `metrics` is the
/// /metrics body (or `''`), `models` health's `all_models_loaded`, `installed` the model list's
/// `data` (`None` is `undefined`, the default `[]`).
pub fn acceptance(
    metrics: &Value,
    models: Option<&Value>,
    installed: Option<&Value>,
) -> Result<Vec<Acceptance>, Fault> {
    let empty = Value::Arr(Vec::new());
    let installed = installed.unwrap_or(&empty);
    let mut rows = Vec::new();
    let mut text: Option<Vec<u16>> = None;
    for m in iterate(models)? {
        let m = Some(&m);
        // installed.find(row => (row.id || row.model_name) === m.model_name)
        let registered = match installed {
            Value::Arr(list) => {
                let mut found = None;
                for row in list {
                    let id = get(Some(row), "id")?;
                    let key = if truthy(id) {
                        id
                    } else {
                        get(Some(row), "model_name")?
                    };
                    if strict_eq(key, get(m, "model_name")?) {
                        found = Some(row);
                        break;
                    }
                }
                found
            }
            Value::Deep => return Err(Fault::Unsupported),
            _ => return Err(Fault::Throws),
        };
        let t = spec_type(opt(get(m, "recipe_options")?, "llamacpp_args")?)?;
        let enabled = match t {
            None => match registered {
                Some(r) if truthy(Some(r)) => capability_enabled(r)?,
                _ => false,
            },
            Some(t) => names_draft_mtp(&t),
        };
        let llm = matches!(get(m, "type")?, Some(Value::Str(s)) if s.iter().copied().eq("llm".encode_utf16()));
        if !truthy(get(m, "loaded")?) || !llm || !enabled {
            continue;
        }
        let name = get(m, "model_name")?;
        if text.is_none() {
            text = Some(if truthy(Some(metrics)) {
                to_string(Some(metrics))?
            } else {
                Vec::new()
            });
        }
        let mut drafted: Option<f64> = None;
        let mut accepted: Option<f64> = None;
        for line in text
            .as_deref()
            .unwrap_or(&[])
            .split(|&c| c == u16::from(b'\n'))
        {
            let Some((draft, labels, value)) = counter_line(line) else {
                continue;
            };
            let Some(label) = model_name_label(labels) else {
                continue;
            };
            // JSON.parse(name[1]): the regex admits text JSON does not (`\x`, a raw CR).
            let parsed = pjson::parse(label, js::DEPTH_CAP).ok_or(Fault::Throws)?;
            if !strict_eq(Some(&parsed), name) {
                continue;
            }
            let n = string_to_number(value);
            if !n.is_finite() || n < 0.0 {
                continue;
            }
            let slot = if draft { &mut drafted } else { &mut accepted };
            *slot = Some(slot.unwrap_or(0.0) + n);
        }
        let rate = match (drafted, accepted) {
            (Some(d), Some(a)) if d.is_finite() && d > 0.0 && a.is_finite() && a <= d => {
                Some(a / d)
            }
            _ => None,
        };
        // The last-response lookup's key, `${owner}:${m.model_name}`, converts the name to a
        // string even when nothing was recorded.
        to_string(name)?;
        rows.push(Acceptance {
            model: name.cloned(),
            rate,
            drafted,
            accepted,
        });
    }
    Ok(rows)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn v(s: &str) -> Value {
        pjson::parse(&s.encode_utf16().collect::<Vec<_>>(), js::DEPTH_CAP).unwrap()
    }

    #[test]
    fn spec_type_takes_the_last_flag() {
        let st = |s: &str| {
            spec_type(Some(&Value::Str(s.encode_utf16().collect())))
                .unwrap()
                .map(|v| String::from_utf16(&v).unwrap())
        };
        assert_eq!(st("--spec-type draft-mtp"), Some("draft-mtp".into()));
        assert_eq!(st("--spec-type=none --spec-type x"), Some("x".into()));
        assert_eq!(st("--spec-type"), None);
        assert_eq!(st("--spec-type ''"), None);
        assert_eq!(st("--spec-type="), Some(String::new()));
        assert_eq!(st("'--spec-type' \"a b\""), Some("a b".into()));
        assert_eq!(spec_type(None).unwrap(), None);
        assert_eq!(spec_type(Some(&v(r#"{"toString":1}"#))), Err(Fault::Throws));
    }

    #[test]
    fn counts_the_named_model_only() {
        let metrics = Value::Str(
            "lemonade_llamacpp_spec_decode_num_draft_tokens_total{model_name=\"m\"} 10\nlemonade_llamacpp_spec_decode_num_accepted_tokens_total{a=\"1\",model_name=\"m\"} 4 x\nlemonade_llamacpp_spec_decode_num_accepted_tokens_total{model_name=\"other\"} 4"
                .encode_utf16()
                .collect(),
        );
        let models = v(
            r#"[{"model_name":"m","loaded":true,"type":"llm","recipe_options":{"llamacpp_args":"--spec-type draft-mtp"}}]"#,
        );
        let rows = acceptance(&metrics, Some(&models), None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].rate, Some(0.4));
        // A label the regex admits but JSON.parse refuses throws, as in the JS.
        let bad = Value::Str(
            "lemonade_llamacpp_spec_decode_num_draft_tokens_total{model_name=\"\\x\"} 1"
                .encode_utf16()
                .collect(),
        );
        assert_eq!(acceptance(&bad, Some(&models), None), Err(Fault::Throws));
        // Not iterable.
        assert_eq!(
            acceptance(&metrics, Some(&v("{}")), None),
            Err(Fault::Throws)
        );
        assert_eq!(acceptance(&metrics, Some(&v("0")), None), Ok(vec![]));
    }
}
