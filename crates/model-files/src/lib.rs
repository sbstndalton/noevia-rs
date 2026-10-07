//! Model-files front for noevia's model manager (sbstndalton/noevia#964): the Hugging Face
//! repository tree listing, which arrives from the network, turned into the file entries the
//! service uses (path, size, quant label, shard group). A port of noevia-services
//! `model-manager/app/model_files.py` `files_from_tree_py`, matching it on every input inside
//! the caps below, including its errors.
//!
//! Pure: no network, no filesystem, no clock. Bounded: [`MAX_INPUT_BYTES`], [`MAX_ENTRIES`],
//! and the JSON caps in [`json`] (depth, string and number length). Inputs past a cap are
//! refused with a typed [`Error`]; the Python caller fails closed on any error.

#![forbid(unsafe_code)]

pub mod backups;
mod bigint;
pub mod json;
#[rustfmt::skip]
pub mod pytables;
pub mod rules;

pub use bigint::BigInt;
pub use rules::{infer_quant, is_support_file, lower_ends_with, py_int_str, shard_key};

use json::Value;
use std::fmt;

/// Largest listing accepted, in bytes of JSON.
pub const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
/// Most entries accepted in one listing (the service reads at most 40 pages of 50).
pub const MAX_ENTRIES: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The input is larger than [`MAX_INPUT_BYTES`].
    InputTooLarge,
    /// The input is not UTF-8.
    NotUtf8,
    /// The input is not JSON (as Python's `json.loads` reads it).
    Json(&'static str),
    /// A `\u` escape names a lone surrogate, which only Python strings can hold.
    LoneSurrogate,
    /// Arrays/objects nest deeper than [`json::MAX_DEPTH`].
    TooDeep,
    /// A string or number literal is longer than the caps in [`json`].
    TooLong,
    /// The top level is not an array.
    NotAList,
    /// More than [`MAX_ENTRIES`] entries.
    TooManyEntries,
    /// An entry is not an object (Python: AttributeError).
    EntryNotObject(usize),
    /// A file entry's path is not a string (Python: AttributeError).
    PathNotString(usize),
    /// A truthy `lfs` that is not an object (Python: AttributeError).
    LfsNotObject(usize),
    /// The size is not something `int()` accepts (Python: TypeError, ValueError, OverflowError).
    BadSize(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InputTooLarge => write!(f, "input is larger than {MAX_INPUT_BYTES} bytes"),
            Error::NotUtf8 => write!(f, "input is not UTF-8"),
            Error::Json(m) => write!(f, "input is not JSON: {m}"),
            Error::LoneSurrogate => write!(f, "input holds a lone surrogate escape"),
            Error::TooDeep => write!(f, "input nests deeper than {}", json::MAX_DEPTH),
            Error::TooLong => write!(f, "a string or number in the input is too long"),
            Error::NotAList => write!(f, "input is not a JSON array"),
            Error::TooManyEntries => write!(f, "more than {MAX_ENTRIES} entries"),
            Error::EntryNotObject(i) => write!(f, "entry {i} is not an object"),
            Error::PathNotString(i) => write!(f, "entry {i}: path is not a string"),
            Error::LfsNotObject(i) => write!(f, "entry {i}: lfs is not an object"),
            Error::BadSize(i) => write!(f, "entry {i}: size is not an integer"),
        }
    }
}

impl std::error::Error for Error {}

/// One file of the listing, as `files_from_tree_py` returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HfFile {
    pub path: String,
    pub size: BigInt,
    pub quant: Option<String>,
    pub shard_base: String,
    pub shard_index: Option<u32>,
    pub shard_total: Option<u32>,
}

/// `int(x)` of a JSON value, or None where Python raises.
pub fn py_int(v: &Value) -> Option<BigInt> {
    match v {
        Value::Int(i) => Some(i.clone()),
        Value::Bool(b) => Some(BigInt::from_u64(u64::from(*b))),
        Value::Float(x) => BigInt::from_f64_trunc(*x),
        Value::Str(s) => py_int_str(s),
        Value::Null | Value::Arr(_) | Value::Obj(_) => None,
    }
}

/// The GGUF and support files of a parsed listing, in listing order.
pub fn files_from_tree(entries: &Value) -> Result<Vec<HfFile>, Error> {
    let Value::Arr(entries) = entries else {
        return Err(Error::NotAList);
    };
    if entries.len() > MAX_ENTRIES {
        return Err(Error::TooManyEntries);
    }
    let mut files = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if !matches!(e, Value::Obj(_)) {
            return Err(Error::EntryNotObject(i));
        }
        if !matches!(e.get("type"), Some(Value::Str(t)) if t == "file") {
            continue;
        }
        let path = match e.get("path") {
            None => "",
            Some(Value::Str(p)) => p.as_str(),
            Some(_) => return Err(Error::PathNotString(i)),
        };
        if !lower_ends_with(path, ".gguf") && !is_support_file(path) {
            continue;
        }
        // int((e.get("lfs") or {}).get("size") or e.get("size") or 0)
        let lfs_size = match e.get("lfs") {
            Some(lfs) if lfs.truthy() => match lfs {
                Value::Obj(_) => lfs.get("size"),
                _ => return Err(Error::LfsNotObject(i)),
            },
            _ => None,
        };
        let raw = [lfs_size, e.get("size")]
            .into_iter()
            .flatten()
            .find(|v| v.truthy());
        let size = match raw {
            Some(v) => py_int(v).ok_or(Error::BadSize(i))?,
            None => BigInt::zero(),
        };
        let (shard_base, shard_index, shard_total) = shard_key(path);
        files.push(HfFile {
            path: path.to_owned(),
            size,
            quant: infer_quant(path),
            shard_base,
            shard_index,
            shard_total,
        });
    }
    Ok(files)
}

fn push_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_opt_u32(out: &mut String, v: Option<u32>) {
    match v {
        Some(n) => out.push_str(&n.to_string()),
        None => out.push_str("null"),
    }
}

/// `{"files": [...]}` with the keys `files_from_tree_py` uses.
pub fn files_to_json(files: &[HfFile]) -> String {
    let mut out = String::from("{\"files\":[");
    for (i, f) in files.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"path\":");
        push_json_str(&mut out, &f.path);
        out.push_str(",\"size\":");
        out.push_str(&f.size.to_string());
        out.push_str(",\"quant\":");
        match &f.quant {
            Some(q) => push_json_str(&mut out, q),
            None => out.push_str("null"),
        }
        out.push_str(",\"shard_base\":");
        push_json_str(&mut out, &f.shard_base);
        out.push_str(",\"shard_index\":");
        push_opt_u32(&mut out, f.shard_index);
        out.push_str(",\"shard_total\":");
        push_opt_u32(&mut out, f.shard_total);
        out.push('}');
    }
    out.push_str("]}");
    out
}

/// The whole CLI contract: listing JSON bytes in, `{"files": [...]}` JSON out.
pub fn files_from_tree_json(input: &[u8]) -> Result<String, Error> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(Error::InputTooLarge);
    }
    let text = std::str::from_utf8(input).map_err(|_| Error::NotUtf8)?;
    let value = json::parse(text)?;
    Ok(files_to_json(&files_from_tree(&value)?))
}
