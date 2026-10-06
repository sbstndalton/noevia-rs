//! Bounded GGUF metadata parser. Mirrors `_read_raw_stream` in noevia's
//! `services/model-manager/app/gguf_meta.py` (hardened in #877).
//!
//! Every length, count and skip in a GGUF header is attacker-controlled. The parser never
//! trusts one without comparing it with what the stream can still supply, and never
//! allocates for a length before that check.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use crate::pyfmt::py_bytes_repr;

/// Elements kept from an array before it is summarised as `{_array, count, sample}`.
pub const MAX_ARRAY_ELEMENTS_KEPT: u64 = 8;
/// String values longer than this keep their head and gain a `…[truncated]` suffix.
pub const MAX_STRING_LEN: u64 = 200_000;
/// Keys are short identifiers; the GGUF spec caps them at 65535.
pub const MAX_KEY_LEN: u64 = 65_535;
/// A header declaring more KV pairs than this is refused before any pair is read.
pub const MAX_KV_COUNT: u64 = 100_000;
/// Total string characters kept across the whole header.
pub const MAX_RETAINED_CHARS: u64 = 16_000_000;
/// One array nested directly inside another is accepted; deeper is rejected.
pub const MAX_ARRAY_DEPTH: u32 = 1;
/// Total bytes actually read (skips excluded). Not in gguf_meta.py, whose other caps bound
/// reads only implicitly; real headers read a few MB at most.
pub const MAX_BYTES_READ: u64 = 256 * 1024 * 1024;
/// Total values kept across the whole header (every scalar, string, list and array summary,
/// nested ones included). Not in gguf_meta.py: without it ~100,000 KV pairs each holding
/// eight lists of eight bytes build several hundred MB of values. Real headers keep a few
/// thousand.
pub const MAX_RETAINED_VALUES: u64 = 500_000;

const BUF_CAPACITY: usize = 64 * 1024;

const T_UINT8: u32 = 0;
const T_INT8: u32 = 1;
const T_UINT16: u32 = 2;
const T_INT16: u32 = 3;
const T_UINT32: u32 = 4;
const T_INT32: u32 = 5;
const T_FLOAT32: u32 = 6;
const T_BOOL: u32 = 7;
const T_STRING: u32 = 8;
const T_ARRAY: u32 = 9;
const T_UINT64: u32 = 10;
const T_INT64: u32 = 11;
const T_FLOAT64: u32 = 12;

const TRUNCATED: &str = "header is truncated or declares a length past the end of the data";

/// One metadata value, typed as Python's `struct` would hand it back.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i128),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    /// An array with more than [`MAX_ARRAY_ELEMENTS_KEPT`] elements: its declared count and
    /// the first few elements (`{"_array": true, "count": …, "sample": […]}` in Python).
    ArraySummary {
        count: u64,
        sample: Vec<Value>,
    },
    /// Python `None`. The parser never produces it; [`crate::summarize`] maps every NaN/±Inf
    /// float to it first, as gguf_meta.py's `_finite` does (sbstndalton/noevia#901).
    None,
}

/// Raw metadata: every KV pair plus `_gguf_version`, `_tensor_count`, `_kv_count` and, when
/// parsing stopped early, `_error`. Later duplicate keys win, as in a Python dict.
pub type Raw = HashMap<String, Value>;

/// The file is not a GGUF, its fixed header is cut short, or it could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GgufError(pub String);

impl std::fmt::Display for GgufError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GgufError {}

enum Fault {
    /// Python's GgufMetaError / OSError inside the KV loop.
    Meta(String),
    /// [`MAX_BYTES_READ`] reached.
    ReadLimit,
    /// The underlying reader failed (Python's OSError); never mistaken for running out of
    /// data while skipping an array.
    Io(String),
}

impl Fault {
    fn message(&self) -> String {
        match self {
            Fault::Meta(m) | Fault::Io(m) => m.clone(),
            Fault::ReadLimit => format!("header read limit of {MAX_BYTES_READ} bytes exceeded"),
        }
    }
}

fn truncated() -> Fault {
    Fault::Meta(TRUNCATED.to_string())
}

struct Source<R: Read + Seek> {
    r: BufReader<R>,
    pos: u64,
    size: u64,
    retained: u64,
    values: u64,
    ran_out: bool,
    bytes_read: u64,
}

impl<R: Read + Seek> Source<R> {
    fn new(mut inner: R) -> std::io::Result<Self> {
        let start = inner.stream_position()?;
        let size = inner.seek(SeekFrom::End(0))?;
        inner.seek(SeekFrom::Start(start))?;
        Ok(Source {
            r: BufReader::with_capacity(BUF_CAPACITY, inner),
            pos: start,
            size,
            retained: 0,
            values: 0,
            ran_out: false,
            bytes_read: 0,
        })
    }

    fn remaining(&self) -> u64 {
        self.size.saturating_sub(self.pos)
    }

    fn account(&mut self, n: u64) -> Result<(), Fault> {
        if n > self.remaining() {
            return Err(truncated());
        }
        match self.bytes_read.checked_add(n) {
            Some(total) if total <= MAX_BYTES_READ => {
                self.bytes_read = total;
                Ok(())
            }
            _ => Err(Fault::ReadLimit),
        }
    }

    /// Up to four bytes, unchecked, like `f.read(4)` for the magic.
    fn read_magic(&mut self) -> std::io::Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(4);
        (&mut self.r).take(4).read_to_end(&mut buf)?;
        self.pos = self.pos.saturating_add(buf.len() as u64);
        Ok(buf)
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], Fault> {
        self.account(N as u64)?;
        let mut buf = [0u8; N];
        self.r
            .read_exact(&mut buf)
            .map_err(|e| Fault::Io(e.to_string()))?;
        self.pos = self.pos.saturating_add(N as u64);
        Ok(buf)
    }

    fn read(&mut self, n: u64) -> Result<Vec<u8>, Fault> {
        // Checked against the bytes left before anything is allocated.
        self.account(n)?;
        let len = usize::try_from(n).map_err(|_| truncated())?;
        let mut buf = vec![0u8; len];
        self.r
            .read_exact(&mut buf)
            .map_err(|e| Fault::Io(e.to_string()))?;
        self.pos = self.pos.saturating_add(n);
        Ok(buf)
    }

    fn skip(&mut self, n: u64) -> Result<(), Fault> {
        if n > self.remaining() {
            return Err(truncated());
        }
        let off = i64::try_from(n).map_err(|_| truncated())?;
        self.r
            .seek_relative(off)
            .map_err(|e| Fault::Io(e.to_string()))?;
        self.pos = self.pos.saturating_add(n);
        Ok(())
    }

    fn run_out(&mut self) {
        self.ran_out = true;
        self.pos = self.size;
        // remaining() is now 0, so every later read fails its check before touching the
        // reader; a failed seek here cannot be observed.
        let _ = self.r.seek(SeekFrom::Start(self.size));
    }

    fn u32(&mut self) -> Result<u32, Fault> {
        Ok(u32::from_le_bytes(self.take::<4>()?))
    }

    fn u64(&mut self) -> Result<u64, Fault> {
        Ok(u64::from_le_bytes(self.take::<8>()?))
    }

    fn read_string(&mut self, max_len: u64) -> Result<String, Fault> {
        let n = self.u64()?;
        if n > self.remaining() {
            return Err(Fault::Meta(format!(
                "string length {n} runs past the end of the data"
            )));
        }
        let text = if n > max_len {
            let chunk = self.read(max_len)?;
            self.skip(n - max_len)?;
            let mut s = String::from_utf8_lossy(&chunk).into_owned();
            s.push_str("…[truncated]");
            s
        } else {
            String::from_utf8_lossy(&self.read(n)?).into_owned()
        };
        self.retained = self.retained.saturating_add(text.chars().count() as u64);
        if self.retained > MAX_RETAINED_CHARS {
            return Err(Fault::Meta(
                "header holds implausibly much string data".to_string(),
            ));
        }
        Ok(text)
    }

    fn skip_string(&mut self) -> Result<(), Fault> {
        let n = self.u64()?;
        self.skip(n)
    }

    fn read_value(&mut self, vtype: u32, depth: u32) -> Result<Value, Fault> {
        // Counted before the value is read, so the cap bounds what gets allocated.
        self.values += 1;
        if self.values > MAX_RETAINED_VALUES {
            return Err(Fault::Meta(
                "header holds implausibly many values".to_string(),
            ));
        }
        if let Some(v) = self.read_scalar(vtype)? {
            return Ok(v);
        }
        if vtype == T_STRING {
            return Ok(Value::Str(self.read_string(MAX_STRING_LEN)?));
        }
        if vtype != T_ARRAY {
            return Err(Fault::Meta(format!("unknown value type {vtype}")));
        }
        let subtype = self.u32()?;
        let count = self.u64()?;
        if subtype == T_ARRAY && depth >= MAX_ARRAY_DEPTH {
            return Err(Fault::Meta(
                "arrays nested more than one level deep are not supported".to_string(),
            ));
        }
        let elem_size = scalar_size(subtype);
        if elem_size.is_none() && subtype != T_STRING && subtype != T_ARRAY {
            return Err(Fault::Meta(format!("unknown array element type {subtype}")));
        }
        if count <= MAX_ARRAY_ELEMENTS_KEPT {
            let mut items = Vec::new();
            for _ in 0..count {
                items.push(self.read_value(subtype, depth + 1)?);
            }
            return Ok(Value::List(items));
        }
        if subtype == T_ARRAY {
            return Err(Fault::Meta(format!(
                "unsupported nested array subtype {subtype}"
            )));
        }
        let mut sample = Vec::new();
        for _ in 0..MAX_ARRAY_ELEMENTS_KEPT {
            sample.push(self.read_value(subtype, depth + 1)?);
        }
        let rest = count - MAX_ARRAY_ELEMENTS_KEPT;
        match elem_size {
            None => {
                // Strings: each skip consumes at least 8 bytes, so this ends with the data.
                let mut left = rest;
                while left > 0 {
                    match self.skip_string() {
                        Ok(()) => left -= 1,
                        Err(Fault::Meta(_)) => {
                            self.run_out();
                            break;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            Some(size) => {
                let need = u128::from(size) * u128::from(rest);
                if need > u128::from(self.remaining()) {
                    self.run_out();
                } else {
                    self.skip(size * rest)?;
                }
            }
        }
        Ok(Value::ArraySummary { count, sample })
    }

    fn read_scalar(&mut self, vtype: u32) -> Result<Option<Value>, Fault> {
        let v = match vtype {
            T_UINT8 => Value::Int(i128::from(u8::from_le_bytes(self.take::<1>()?))),
            T_INT8 => Value::Int(i128::from(i8::from_le_bytes(self.take::<1>()?))),
            T_UINT16 => Value::Int(i128::from(u16::from_le_bytes(self.take::<2>()?))),
            T_INT16 => Value::Int(i128::from(i16::from_le_bytes(self.take::<2>()?))),
            T_UINT32 => Value::Int(i128::from(u32::from_le_bytes(self.take::<4>()?))),
            T_INT32 => Value::Int(i128::from(i32::from_le_bytes(self.take::<4>()?))),
            T_UINT64 => Value::Int(i128::from(u64::from_le_bytes(self.take::<8>()?))),
            T_INT64 => Value::Int(i128::from(i64::from_le_bytes(self.take::<8>()?))),
            T_FLOAT32 => Value::Float(f64::from(f32::from_le_bytes(self.take::<4>()?))),
            T_FLOAT64 => Value::Float(f64::from_le_bytes(self.take::<8>()?)),
            T_BOOL => Value::Bool(u8::from_le_bytes(self.take::<1>()?) != 0),
            _ => return Ok(None),
        };
        Ok(Some(v))
    }
}

fn scalar_size(t: u32) -> Option<u64> {
    match t {
        T_UINT8 | T_INT8 | T_BOOL => Some(1),
        T_UINT16 | T_INT16 => Some(2),
        T_UINT32 | T_INT32 | T_FLOAT32 => Some(4),
        T_UINT64 | T_INT64 | T_FLOAT64 => Some(8),
        _ => None,
    }
}

/// Parse GGUF metadata from any seekable stream.
///
/// Errors only when the magic or the fixed header is wrong. Past that, a bad length, count or
/// nesting depth is recorded as `_error` and parsing stops with whatever was read.
pub fn read_raw_stream<R: Read + Seek>(inner: R) -> Result<Raw, GgufError> {
    let mut src = Source::new(inner).map_err(|e| GgufError(e.to_string()))?;
    let magic = src.read_magic().map_err(|e| GgufError(e.to_string()))?;
    if magic != b"GGUF" {
        return Err(GgufError(format!(
            "not a GGUF file (magic={})",
            py_bytes_repr(&magic)
        )));
    }
    let fixed = |src: &mut Source<R>| -> Result<(u32, u64, u64), Fault> {
        Ok((src.u32()?, src.u64()?, src.u64()?))
    };
    let (version, tensor_count, kv_count) = fixed(&mut src).map_err(|f| GgufError(f.message()))?;
    let mut out = Raw::new();
    out.insert("_gguf_version".into(), Value::Int(i128::from(version)));
    out.insert("_tensor_count".into(), Value::Int(i128::from(tensor_count)));
    out.insert("_kv_count".into(), Value::Int(i128::from(kv_count)));
    if kv_count > MAX_KV_COUNT {
        out.insert(
            "_error".into(),
            Value::Str(format!(
                "stopped at KV read: implausible kv_count {kv_count}"
            )),
        );
        return Ok(out);
    }
    for _ in 0..kv_count {
        let kv = (|| -> Result<(String, Value), Fault> {
            let key = src.read_string(MAX_KEY_LEN)?;
            let vtype = src.u32()?;
            let value = src.read_value(vtype, 0)?;
            Ok((key, value))
        })();
        match kv {
            Ok((k, v)) => {
                out.insert(k, v);
            }
            Err(f) => {
                out.insert(
                    "_error".into(),
                    Value::Str(format!("stopped at KV read: {}", f.message())),
                );
                break;
            }
        }
    }
    if src.ran_out && !out.contains_key("_error") {
        out.insert(
            "_error".into(),
            Value::Str("stopped at KV read: array runs past the end of the data".into()),
        );
    }
    Ok(out)
}

/// Parse GGUF metadata from an in-memory buffer (e.g. a range-fetched header).
pub fn read_raw_bytes(buf: &[u8]) -> Result<Raw, GgufError> {
    read_raw_stream(Cursor::new(buf))
}

/// Parse GGUF metadata from a file through a bounded buffered reader. Only the header is read.
pub fn read_raw(path: &Path) -> Result<Raw, GgufError> {
    let file = File::open(path).map_err(|e| GgufError(e.to_string()))?;
    let meta = file.metadata().map_err(|e| GgufError(e.to_string()))?;
    if meta.is_dir() {
        return Err(GgufError("is a directory".into()));
    }
    read_raw_stream(file)
}
