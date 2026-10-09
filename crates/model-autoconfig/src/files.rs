//! Companion files: autoconfig_core.py's `pick_file`: the name rules that decide which
//! projector (mmproj) and which speculative-decoding draft head belong to a model, applied to a
//! directory listing autoconfig.py read (the listing and every stat stay in Python), and the
//! projector resolution analyze() budgets.

use crate::pystr::{lower_tok, strip};
use crate::{Error, Work};
use model_files::json::Value;

/// HEAD_MAX_BYTES: a larger "-MTP-" file is a model build, not a head.
pub const HEAD_MAX_BYTES: i128 = 2 * 1024 * 1024 * 1024;
/// Most entries one listing may hold, and most rule calls one request may hold.
pub const MAX_ENTRIES: usize = 16_384;
pub const MAX_CALLS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Other,
    Error,
}

struct Entry<'a> {
    name: &'a str,
    kind: Kind,
    size: Option<i128>,
}

fn listing<'a>(v: &'a Value, work: &mut Work) -> Result<Option<Vec<Entry<'a>>>, Error> {
    let items = match v {
        Value::Null => return Ok(None),
        Value::Arr(items) if items.len() <= MAX_ENTRIES => items,
        Value::Arr(_) => return Err(Error::OutOfRange("files.listing")),
        _ => return Err(Error::Schema("files.listing")),
    };
    work.charge(items.len())?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Value::Arr(e) = item else {
            return Err(Error::Schema("files.listing entry"));
        };
        let [Value::Str(name), Value::Str(kind), size] = e.as_slice() else {
            return Err(Error::Schema("files.listing entry"));
        };
        let kind = match kind.as_str() {
            "file" => Kind::File,
            "other" => Kind::Other,
            "error" => Kind::Error,
            _ => return Err(Error::Schema("files.listing kind")),
        };
        let size = match size {
            Value::Null => None,
            Value::Int(_) if kind == Kind::File => Some(crate::pyval::big(size, "files.size")?),
            _ => return Err(Error::Schema("files.listing size")),
        };
        if kind != Kind::File && size.is_some() {
            return Err(Error::Schema("files.listing size"));
        }
        out.push(Entry { name, kind, size });
    }
    Ok(Some(out))
}

/// `_looks_like_draft(filename)`.
pub fn looks_like_draft(filename: &str) -> bool {
    let low = lower_tok(filename);
    for neg in [
        "nomtp", "no-mtp", "no_mtp", "nodraft", "no-draft", "no_draft",
    ] {
        if low.contains(neg) {
            return false;
        }
    }
    for tok in [
        "-draft-", "-draft.", "_draft_", ".draft.", "-mtp-", "_mtp_", ".mtp.",
    ] {
        if low.contains(tok) {
            return true;
        }
    }
    // low[:-5]: ".gguf" is five ASCII characters, so five bytes.
    let stem = low.strip_suffix(".gguf").unwrap_or(&low);
    let parts = stem.replace('_', "-");
    let mut split = parts.split('-');
    let first = split.next().unwrap_or("");
    let last = parts.rsplit('-').next().unwrap_or("");
    ["draft", "mtp"].contains(&first) || ["draft", "mtp"].contains(&last)
}

fn is_mmproj(name: &str) -> bool {
    let low = lower_tok(name);
    low.ends_with(".gguf") && low.contains("mmproj")
}

/// `name.lower().replace("-", "").replace("_", "").replace(".", "")`, for a name that is only
/// ever searched for an ASCII key (see [`lower_tok`]).
fn name_key(name: &str) -> String {
    lower_tok(name)
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | '.'))
        .collect()
}

/// The stem key of a section name, which is itself searched for: refused unless ASCII (section
/// names are; ini.py only accepts [A-Za-z0-9._+-]).
fn stem_key(section: &str) -> Result<String, Error> {
    if !section.is_ascii() {
        return Err(Error::Unsupported("a non-ASCII section name"));
    }
    Ok(name_key(section))
}

/// PurePosixPath(name).suffix and .stem (CPython 3.12): split at the last dot unless it is the
/// first or the last character.
fn suffix_stem(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => {
            (name.get(i..).unwrap_or(""), name.get(..i).unwrap_or(name))
        }
        _ => ("", name),
    }
}

fn is_gguf_suffix(name: &str) -> bool {
    let (suffix, _) = suffix_stem(name);
    lower_tok(suffix) == ".gguf"
}

fn string<'a>(v: Option<&'a Value>, what: &'static str) -> Result<&'a str, Error> {
    match v {
        Some(Value::Str(s)) => Ok(s),
        _ => Err(Error::Schema(what)),
    }
}

/// The answer to one rule call: a path, or the projector resolution.
pub enum Answer {
    Path(String),
    Projector {
        mmproj_rel: String,
        mmproj_gb: Value,
        available: String,
    },
}

/// `pick_file(inp)`.
pub fn pick(inp: &Value, work: &mut Work) -> Result<Answer, Error> {
    let Value::Obj(_) = inp else {
        return Err(Error::Schema("files call"));
    };
    let rule = string(inp.get("rule"), "files.rule")?;
    let allowed: &[&str] = match rule {
        "projector" => &[
            "rule", "files", "current", "found", "stat_gb", "vision", "override",
        ],
        "mmproj_subdir" => &["rule", "listing", "subdir"],
        "mmproj_flat" | "mtp_flat" => &["rule", "listing", "section"],
        "mtp_folder" => &["rule", "listing", "prefix", "section"],
        _ => return Err(Error::Schema("files.rule")),
    };
    if let Value::Obj(pairs) = inp {
        if pairs.iter().any(|(k, _)| !allowed.contains(&k.as_str())) || pairs.len() != allowed.len()
        {
            return Err(Error::Schema("files call"));
        }
    }
    if rule == "projector" {
        return projector(inp);
    }
    let entries = listing(inp.get("listing").unwrap_or(&Value::Null), work)?;
    let path = match rule {
        "mmproj_subdir" => {
            let subdir = string(inp.get("subdir"), "files.subdir")?;
            let mut best: Option<(i128, &str)> = None;
            for e in entries.iter().flatten() {
                work.charge(e.name.len())?;
                if e.kind == Kind::Error {
                    break;
                }
                if e.kind == Kind::File && is_mmproj(e.name) {
                    let Some(size) = e.size else { break };
                    if best.is_none_or(|(b, _)| size < b) {
                        best = Some((size, e.name));
                    }
                }
            }
            best.map_or_else(String::new, |(_, n)| format!("/models/{subdir}/{n}"))
        }
        "mmproj_flat" => {
            let key = stem_key(string(inp.get("section"), "files.section")?)?;
            let mut found = String::new();
            if !key.is_empty() {
                for e in entries.iter().flatten() {
                    work.charge(e.name.len().saturating_mul(key.len().max(1)))?;
                    if e.kind == Kind::Error {
                        break;
                    }
                    if e.kind == Kind::File && is_mmproj(e.name) && name_key(e.name).contains(&key)
                    {
                        found = format!("/models/{}", e.name);
                        break;
                    }
                }
            }
            found
        }
        "mtp_folder" => {
            let prefix = string(inp.get("prefix"), "files.prefix")?;
            let section = string(inp.get("section"), "files.section")?;
            let mut best: Option<(i128, &str)> = None;
            let mut failed = false;
            for e in entries.iter().flatten() {
                work.charge(e.name.len())?;
                if e.kind == Kind::Error {
                    failed = true;
                    break;
                }
                if e.kind != Kind::File || !is_gguf_suffix(e.name) {
                    continue;
                }
                if lower_tok(e.name).contains("mmproj") || !looks_like_draft(e.name) {
                    continue;
                }
                let (_, stem) = suffix_stem(e.name);
                let Some(size) = e.size else { continue };
                if stem == section || size > HEAD_MAX_BYTES {
                    continue;
                }
                // min() keeps the first of equal sizes.
                if best.is_none_or(|(b, _)| size < b) {
                    best = Some((size, e.name));
                }
            }
            match best {
                Some((_, n)) if !failed => format!("{prefix}{n}"),
                _ => String::new(),
            }
        }
        _ => {
            // mtp_flat
            let key = stem_key(string(inp.get("section"), "files.section")?)?;
            let mut best: Option<(Option<i128>, &str)> = None;
            let mut failed = false;
            let mut no_size = false;
            if !key.is_empty() {
                for e in entries.iter().flatten() {
                    work.charge(e.name.len().saturating_mul(key.len().max(1)))?;
                    if e.kind == Kind::Error {
                        failed = true;
                        break;
                    }
                    if e.kind == Kind::File
                        && is_gguf_suffix(e.name)
                        && looks_like_draft(e.name)
                        && !lower_tok(e.name).contains("mmproj")
                        && name_key(e.name).contains(&key)
                    {
                        match (e.size, best) {
                            (None, _) => no_size = true,
                            (Some(s), Some((Some(b), _))) if s >= b => {}
                            (Some(s), _) => best = Some((Some(s), e.name)),
                        }
                    }
                }
            }
            if failed || key.is_empty() {
                String::new()
            } else if no_size {
                // min(..., key=stat) raises when a candidate cannot be sized.
                return Err(Error::Python("OSError"));
            } else {
                best.map_or_else(String::new, |(_, n)| format!("/models/{n}"))
            }
        }
    };
    Ok(Answer::Path(path))
}

fn projector(inp: &Value) -> Result<Answer, Error> {
    let get = |k: &str| inp.get(k).ok_or(Error::Schema("files.projector"));
    let files = match get("files")? {
        Value::Bool(b) => *b,
        _ => return Err(Error::Schema("files.files")),
    };
    let vision = match get("vision")? {
        Value::Bool(b) => *b,
        _ => return Err(Error::Schema("files.vision")),
    };
    let found = string(inp.get("found"), "files.found")?;
    let stat_gb = match get("stat_gb")? {
        Value::Float(x) => *x,
        _ => return Err(Error::Schema("files.stat_gb")),
    };
    let mut rel = String::new();
    let mut gb = Value::Float(0.0);
    if files {
        let cur = match get("current")? {
            Value::Str(s) => strip(s),
            _ => return Err(Error::Python("AttributeError")),
        };
        rel = if cur.is_empty() { found } else { cur }.to_owned();
        if !rel.is_empty() {
            gb = Value::Float(stat_gb);
        }
    }
    let available = rel.clone();
    if !vision {
        rel = String::new();
        gb = Value::Float(0.0);
    }
    let over = get("override")?;
    if vision && *over != Value::Null && positive(over)? {
        gb = over.clone();
        if rel.is_empty() {
            rel = "(remote projector)".to_owned();
        }
    }
    Ok(Answer::Projector {
        mmproj_rel: rel,
        mmproj_gb: gb,
        available,
    })
}

/// `v > 0` for a number (bool included); anything else raises TypeError, as in Python.
fn positive(v: &Value) -> Result<bool, Error> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::Float(x) => Ok(*x > 0.0),
        Value::Int(i) => Ok(!i.is_zero() && !i.to_string().starts_with('-')),
        _ => Err(Error::Python("TypeError")),
    }
}

/// The answers as `[pick_file(c) for c in calls]` returns them.
pub fn answers_json(answers: &[Answer]) -> Result<String, Error> {
    let mut out = String::from("[");
    for (i, a) in answers.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match a {
            Answer::Path(p) => crate::json_str(&mut out, p),
            Answer::Projector {
                mmproj_rel,
                mmproj_gb,
                available,
            } => {
                out.push_str("{\"mmproj_rel\":");
                crate::json_str(&mut out, mmproj_rel);
                out.push_str(",\"mmproj_gb\":");
                crate::json_value(&mut out, mmproj_gb)?;
                out.push_str(",\"available\":");
                crate::json_str(&mut out, available);
                out.push('}');
            }
        }
    }
    out.push(']');
    Ok(out)
}

/// The `files` part: a list of rule calls.
pub fn pick_all(v: &Value, work: &mut Work) -> Result<Vec<Answer>, Error> {
    match v {
        Value::Arr(calls) if calls.len() <= MAX_CALLS => {
            calls.iter().map(|c| pick(c, work)).collect()
        }
        Value::Arr(_) => Err(Error::OutOfRange("files")),
        _ => Err(Error::Schema("files")),
    }
}
