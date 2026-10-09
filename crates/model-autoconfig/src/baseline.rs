//! Baseline parsing: autoconfig_core.py's `parse_baseline`: a llama-server container's command
//! line read into the ini keys it already sets (short flags by a fixed table, long ones when
//! they are known ini keys), each with the next argument as its value or "true".

use crate::values::Values;
use crate::{Error, Work};
use model_files::json::Value;

/// Most arguments, known keys and calls one request may hold.
pub const MAX_ARGS: usize = 4096;
pub const MAX_KNOWN: usize = 4096;
pub const MAX_CALLS: usize = 64;

/// _SHORT_TO_KEY.
const SHORT: [(&str, &str); 18] = [
    ("-ngl", "ngl"),
    ("-fa", "flash-attn"),
    ("-ctk", "cache-type-k"),
    ("-ctv", "cache-type-v"),
    ("-np", "parallel"),
    ("-c", "ctx-size"),
    ("-b", "batch-size"),
    ("-ub", "ubatch-size"),
    ("-t", "threads"),
    ("-tb", "threads-batch"),
    ("-sm", "split-mode"),
    ("-mg", "main-gpu"),
    ("-fit", "fit"),
    ("-fitt", "fit-target"),
    ("-fitc", "fit-ctx"),
    ("-cmoe", "cpu-moe"),
    ("-ncmoe", "n-cpu-moe"),
    ("-kvo", "kv-offload"),
];

/// `parse_baseline(args, known)` for one {"args", "known"} request; `known` must be sorted and
/// unique (analyze() sends sorted(ini.ALL_KNOWN_KEYS)).
pub fn parse(inp: &Value, work: &mut Work) -> Result<Values, Error> {
    let Value::Obj(pairs) = inp else {
        return Err(Error::Schema("baseline"));
    };
    if pairs.len() != 2 || pairs.iter().any(|(k, _)| k != "args" && k != "known") {
        return Err(Error::Schema("baseline"));
    }
    let known: Vec<&str> = match inp.get("known") {
        Some(Value::Arr(items)) if items.len() <= MAX_KNOWN => {
            work.charge(items.len())?;
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let Value::Str(s) = item else {
                    return Err(Error::Schema("baseline.known"));
                };
                if out.last().is_some_and(|prev: &&str| *prev >= s.as_str()) {
                    return Err(Error::Schema("baseline.known: not sorted"));
                }
                out.push(s.as_str());
            }
            out
        }
        Some(Value::Arr(_)) => return Err(Error::OutOfRange("baseline.known")),
        _ => return Err(Error::Schema("baseline.known")),
    };
    let args = match inp.get("args") {
        Some(Value::Arr(items)) if items.len() <= MAX_ARGS => items,
        Some(Value::Arr(_)) => return Err(Error::OutOfRange("baseline.args")),
        _ => return Err(Error::Schema("baseline.args")),
    };
    // A binary search per argument, a linear one through the (small) result.
    work.charge(args.len().saturating_mul(32))?;
    let mut out = Values::default();
    for (i, a) in args.iter().enumerate() {
        let key: Option<&str> = match a {
            Value::Str(s) => match SHORT.iter().find(|(f, _)| f == s) {
                Some((_, k)) => Some(k),
                None => s
                    .strip_prefix("--")
                    // `if not key: continue` skips an empty one even if it were known.
                    .filter(|k| !k.is_empty() && known.binary_search(k).is_ok()),
            },
            // `a in dict` of a list or dict: unhashable.
            Value::Arr(_) | Value::Obj(_) => return Err(Error::Python("TypeError")),
            // Not a key of the table, and has no .startswith().
            _ => return Err(Error::Python("AttributeError")),
        };
        let Some(key) = key else { continue };
        work.charge(out.0.len())?;
        let value = match args.get(i + 1) {
            None | Some(Value::Null) => "true".to_owned(),
            Some(Value::Str(n)) if !n.starts_with('-') => n.clone(),
            Some(Value::Str(_)) => "true".to_owned(),
            Some(_) => return Err(Error::Python("AttributeError")),
        };
        out.set(key, value);
    }
    Ok(out)
}

/// The `baseline` part: a list of requests, each answered as [[key, value]...].
pub fn parse_all(v: &Value, work: &mut Work) -> Result<Vec<Values>, Error> {
    match v {
        Value::Arr(calls) if calls.len() <= MAX_CALLS => {
            calls.iter().map(|c| parse(c, work)).collect()
        }
        Value::Arr(_) => Err(Error::OutOfRange("baseline")),
        _ => Err(Error::Schema("baseline")),
    }
}
