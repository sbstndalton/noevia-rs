//! `mtp-artifact.cjs` `inspect(buffer)`: the GGUF v2/v3 header and tensor directory read from a
//! byte prefix of the file, bounded independently of the weights' size. Every limit and every
//! error message is core's.

use js_json::JValue;

/// How much of each file is fetched and inspected (`LIMIT`, 16 MiB).
pub const LIMIT: usize = 16 * 1024 * 1024;

const MAX_SAFE: u64 = (1 << 53) - 1;

pub const TRUNCATED: &str = "GGUF header exceeds inspection limit or is truncated.";
pub const UNSAFE_COUNT: &str = "GGUF count exceeds safe limits.";
pub const NESTED: &str = "Unsupported nested GGUF metadata.";
pub const ARRAY_LIMIT: &str = "GGUF array exceeds inspection limit.";
pub const VALUE_TYPE: &str = "Unsupported GGUF value type.";
pub const NOT_GGUF: &str = "Not a supported little-endian GGUF file.";
pub const VERSION: &str = "Unsupported GGUF version.";
pub const DIRECTORY_LIMIT: &str = "GGUF directory exceeds inspection limit.";
pub const DIMENSIONS: &str = "Unsupported GGUF tensor dimensions.";

/// At most this many matching tensor names are kept.
pub const TENSOR_NAMES: usize = 8;

/// What `inspect` returns.
#[derive(Debug, Clone, PartialEq)]
pub struct Inspection {
    /// `general.architecture`'s value as read ('' without one; a number or `null` when the file
    /// gives it another type).
    pub architecture: JValue,
    /// The last `*.nextn_predict_layers` value (`null` without one).
    pub declared_layers: JValue,
    /// `split.count`'s value (1 without one).
    pub split_count: JValue,
    pub tensor_count: u64,
    /// The first eight tensor names that look like an MTP/NextN head.
    pub tensors: Vec<String>,
}

impl Inspection {
    /// `{ file, ...inspect(buffer) }`, in core's key order.
    pub fn evidence(&self, file: &str) -> JValue {
        JValue::obj([
            ("file", JValue::from(file)),
            ("architecture", self.architecture.clone()),
            ("declaredLayers", self.declared_layers.clone()),
            ("splitCount", self.split_count.clone()),
            ("tensorCount", JValue::Num(self.tensor_count as f64)),
            (
                "tensors",
                JValue::Arr(self.tensors.iter().map(|t| JValue::from(t.as_str())).collect()),
            ),
        ])
    }
}

/// `/(?:^|\.)(?:nextn|mtp)(?:\.|_)/i.test(name)`.
pub fn is_mtp_tensor(name: &str) -> bool {
    let b = name.as_bytes();
    (0..b.len()).any(|i| {
        let at_boundary = i == 0 || b.get(i - 1) == Some(&b'.');
        let rest = b.get(i..).unwrap_or(&[]);
        let then = |n: usize| matches!(rest.get(n), Some(b'.' | b'_'));
        at_boundary
            && ((crate::js::starts_with_ci(rest, "nextn") && then(5))
                || (crate::js::starts_with_ci(rest, "mtp") && then(3)))
    })
}

struct Reader<'a> {
    b: &'a [u8],
    at: u64,
}

impl<'a> Reader<'a> {
    fn need(&self, n: u64) -> Result<(), &'static str> {
        if n > MAX_SAFE || self.at + n > self.b.len() as u64 {
            return Err(TRUNCATED);
        }
        Ok(())
    }

    fn bytes(&mut self, n: u64) -> Result<&'a [u8], &'static str> {
        self.need(n)?;
        let from = usize::try_from(self.at).map_err(|_| TRUNCATED)?;
        let to = usize::try_from(self.at + n).map_err(|_| TRUNCATED)?;
        self.at += n;
        self.b.get(from..to).ok_or(TRUNCATED)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(N as u64)?);
        Ok(out)
    }

    fn u32(&mut self) -> Result<u32, &'static str> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    /// A count or length: `Number(readBigUInt64LE)`, which must be a safe integer.
    fn u64(&mut self) -> Result<u64, &'static str> {
        let v = u64::from_le_bytes(self.array()?);
        if v > MAX_SAFE {
            return Err(UNSAFE_COUNT);
        }
        Ok(v)
    }

    /// A string: decoded as UTF-8 (U+FFFD where it is not) when it is kept.
    fn str(&mut self, keep: bool) -> Result<Option<String>, &'static str> {
        let n = self.u64()?;
        let raw = self.bytes(n)?;
        Ok(keep.then(|| String::from_utf8_lossy(raw).into_owned()))
    }

    fn value(&mut self, ty: u32, keep: bool, depth: u32) -> Result<JValue, &'static str> {
        if depth > 2 {
            return Err(NESTED);
        }
        if ty == 8 {
            return Ok(self.str(keep)?.map_or(JValue::Null, JValue::Str));
        }
        if ty == 9 {
            let t = self.u32()?;
            let n = self.u64()?;
            if n > 2_000_000 {
                return Err(ARRAY_LIMIT);
            }
            for _ in 0..n {
                self.value(t, false, depth + 1)?;
            }
            return Ok(JValue::Null);
        }
        let size = match ty {
            0 | 1 | 7 => 1,
            2 | 3 => 2,
            4..=6 => 4,
            10..=12 => 8,
            _ => return Err(VALUE_TYPE),
        };
        let raw = self.bytes(size)?;
        let le = |n: usize| -> [u8; 8] {
            let mut out = [0u8; 8];
            for (o, b) in out.iter_mut().zip(raw.iter().take(n)) {
                *o = *b;
            }
            out
        };
        if !keep {
            return Ok(JValue::Null);
        }
        let x = u64::from_le_bytes(le(8));
        Ok(match ty {
            0 => JValue::Num(f64::from(x as u8)),
            2 => JValue::Num(f64::from(x as u16)),
            4 => JValue::Num(f64::from(x as u32)),
            5 => JValue::Num(f64::from(x as u32 as i32)),
            // Not checked for safety: `Number()` rounds as `as` does (to nearest, ties to even).
            10 => JValue::Num(x as f64),
            _ => JValue::Null,
        })
    }
}

/// `inspect(buffer)`: the directory, or core's error message.
pub fn inspect(buffer: &[u8]) -> Result<Inspection, &'static str> {
    let mut r = Reader { b: buffer, at: 0 };
    r.need(24)?;
    // `buffer.toString('ascii', 0, 4)`: Node's 'ascii' decoding clears each byte's high bit.
    let magic = buffer.get(..4).ok_or(TRUNCATED)?;
    if !magic.iter().map(|b| b & 0x7f).eq(*b"GGUF") {
        return Err(NOT_GGUF);
    }
    r.at = 4;
    if !matches!(r.u32()?, 2 | 3) {
        return Err(VERSION);
    }
    let count = r.u64()?;
    let kv = r.u64()?;
    if count > 200_000 || kv > 100_000 {
        return Err(DIRECTORY_LIMIT);
    }
    let mut architecture = JValue::from("");
    let mut declared_layers = JValue::Null;
    let mut split_count = JValue::Num(1.0);
    for _ in 0..kv {
        let key = r.str(true)?.unwrap_or_default();
        let ty = r.u32()?;
        let is_arch = key == "general.architecture";
        let is_layers = key.ends_with(".nextn_predict_layers");
        let is_split = key == "split.count";
        let v = r.value(ty, is_arch || is_layers || is_split, 0)?;
        if is_arch {
            architecture = v.clone();
        }
        if is_layers {
            declared_layers = v.clone();
        }
        if is_split {
            split_count = v;
        }
    }
    let mut tensors = Vec::new();
    for _ in 0..count {
        let name = r.str(true)?.unwrap_or_default();
        let dims = r.u32()?;
        if dims > 8 {
            return Err(DIMENSIONS);
        }
        for _ in 0..dims {
            r.u64()?;
        }
        r.u32()?;
        r.u64()?;
        if is_mtp_tensor(&name) && tensors.len() < TENSOR_NAMES {
            tensors.push(name);
        }
    }
    Ok(Inspection {
        architecture,
        declared_layers,
        split_count,
        tensor_count: count,
        tensors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tensor_name_test_is_core_s() {
        for (name, want) in [
            ("blk.0.nextn.eh_proj.weight", true),
            ("NEXTN_head", true),
            ("mtp.0", true),
            ("blk.mtp_x", true),
            ("blk.xmtp.0", false),
            ("nextn", false),
            ("blk.nextnx.0", false),
            ("mtp", false),
            ("a.MtP.b", true),
        ] {
            assert_eq!(is_mtp_tensor(name), want, "{name}");
        }
    }

    #[test]
    fn short_or_foreign_buffers_are_refused() {
        assert_eq!(inspect(&[0; 23]), Err(TRUNCATED));
        assert_eq!(inspect(&[0; 24]), Err(NOT_GGUF));
    }
}
