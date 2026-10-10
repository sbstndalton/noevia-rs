//! @levischuck/tiny-cbor 0.2.11, as @simplewebauthn v14 uses it (`isoCBOR.decodeFirst` and
//! `isoCBOR.encode`). The stored `passkeys.public_key` is `encode(decodeFirst(...))` of the key in
//! the authenticator data, so registering a passkey must re-encode exactly as tiny-cbor does, and
//! the authenticator-data walk advances by the re-encoded length, as Node's does.
//!
//! Decoding, as tiny-cbor: definite lengths only; an argument of 24/25/26/27 must carry a value of
//! at least 24 (smaller ones are "not well formed") and a 64-bit one at most 2^53 - 1; a byte or
//! text string whose length runs past the data is cut short (JS `ArrayBuffer#slice`), while the
//! position still moves by the claimed length; text is decoded with replacement and a leading BOM
//! dropped; map keys are strings or numbers, never repeated; half floats only as ±Infinity/NaN;
//! simple values 20..23. Every number is a JS number.
//!
//! Encoding, as tiny-cbor: safe integers as (shortest) integers, other numbers as float32 when
//! exact, else float64; text strings with the UTF-16 length as their header (tiny-cbor's own
//! quirk; equal to the byte length for ASCII); maps in their order.

/// A decoded CBOR item, as a JS value.
#[derive(Debug, Clone, PartialEq)]
pub enum Cbor {
    Num(f64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    /// Keys are [`Cbor::Num`] or [`Cbor::Text`], in order.
    Map(Vec<(Cbor, Cbor)>),
    Tag(f64, Box<Cbor>),
    Bool(bool),
    Null,
    Undefined,
}

impl Cbor {
    /// `map.get(key)` for a numeric key; `None` when absent or not a map.
    pub fn get_num(&self, key: f64) -> Option<&Cbor> {
        match self {
            Cbor::Map(items) => items
                .iter()
                .find(|(k, _)| matches!(k, Cbor::Num(n) if *n == key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// `map.get(key)` for a string key.
    pub fn get_text(&self, key: &str) -> Option<&Cbor> {
        match self {
            Cbor::Map(items) => items
                .iter()
                .find(|(k, _)| matches!(k, Cbor::Text(t) if t == key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// JS truthiness of the decoded value (byte strings are Uint8Arrays: always truthy).
    pub fn truthy(&self) -> bool {
        match self {
            Cbor::Num(n) => *n != 0.0 && !n.is_nan(),
            Cbor::Text(t) => !t.is_empty(),
            Cbor::Bool(b) => *b,
            Cbor::Null | Cbor::Undefined => false,
            Cbor::Bytes(_) | Cbor::Array(_) | Cbor::Map(_) | Cbor::Tag(..) => true,
        }
    }

    /// `String(value)` for messages: numbers as JS prints them, strings as is.
    pub fn display(&self) -> String {
        match self {
            Cbor::Num(n) => js_json::number_to_string(*n),
            Cbor::Text(t) => t.clone(),
            Cbor::Bool(b) => b.to_string(),
            Cbor::Null => "null".into(),
            Cbor::Undefined => "undefined".into(),
            Cbor::Bytes(b) => b
                .iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(","),
            Cbor::Array(_) | Cbor::Map(_) | Cbor::Tag(..) => "[object Object]".into(),
        }
    }
}

/// tiny-cbor's thrown errors (the message is not reproduced; callers wrap or replace it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CborError;

const MAX_DEPTH: usize = 256;

fn decode_length(data: &[u8], argument: u8, index: usize) -> Result<(f64, usize), CborError> {
    if argument < 24 {
        return Ok((f64::from(argument), 1));
    }
    let rest = data.get(index + 1..).unwrap_or(&[]);
    let (value, bytes) = match argument {
        24 => (rest.first().map(|b| u64::from(*b)), 2),
        25 => (
            rest.get(..2)
                .and_then(|b| <[u8; 2]>::try_from(b).ok())
                .map(|b| u64::from(u16::from_be_bytes(b))),
            3,
        ),
        26 => (
            rest.get(..4).and_then(|b| <[u8; 4]>::try_from(b).ok()).map(|b| u64::from(u32::from_be_bytes(b))),
            5,
        ),
        27 => {
            let v = rest
                .get(..8)
                .and_then(|b| <[u8; 8]>::try_from(b).ok())
                .map(u64::from_be_bytes);
            match v {
                Some(v) if (24..=9_007_199_254_740_991).contains(&v) => return Ok((v as f64, 9)),
                _ => return Err(CborError),
            }
        }
        _ => (None, 0),
    };
    match value {
        Some(v) if v >= 24 => Ok((v as f64, bytes)),
        _ => Err(CborError),
    }
}

fn as_len(n: f64) -> usize {
    if n >= usize::MAX as f64 {
        usize::MAX
    } else {
        n as usize
    }
}

fn slice_clamped(data: &[u8], start: usize, len: usize) -> &[u8] {
    let end = start.saturating_add(len).min(data.len());
    data.get(start.min(end)..end).unwrap_or(&[])
}

fn decode_next(data: &[u8], index: usize, depth: usize) -> Result<(Cbor, usize), CborError> {
    if depth > MAX_DEPTH {
        return Err(CborError);
    }
    let byte = *data.get(index).ok_or(CborError)?;
    let major = byte >> 5;
    let argument = byte & 0x1f;
    match major {
        0 => decode_length(data, argument, index).map(|(v, n)| (Cbor::Num(v), n)),
        1 => decode_length(data, argument, index).map(|(v, n)| (Cbor::Num(-v - 1.0), n)),
        2 | 3 => {
            let (len, consumed) = decode_length(data, argument, index)?;
            let len = as_len(len);
            let bytes = slice_clamped(data, index + consumed, len);
            let total = consumed.checked_add(len).ok_or(CborError)?;
            if major == 2 {
                Ok((Cbor::Bytes(bytes.to_vec()), total))
            } else {
                Ok((Cbor::Text(crate::b64::utf8_decode(bytes)), total))
            }
        }
        4 => {
            if argument == 0 {
                return Ok((Cbor::Array(Vec::new()), 1));
            }
            let (len, mut consumed) = decode_length(data, argument, index)?;
            let mut items = Vec::new();
            let mut i = 0.0;
            while i < len {
                if data.len() <= index + consumed {
                    return Err(CborError);
                }
                let (v, n) = decode_next(data, index + consumed, depth + 1)?;
                items.push(v);
                consumed = consumed.checked_add(n).ok_or(CborError)?;
                i += 1.0;
            }
            Ok((Cbor::Array(items), consumed))
        }
        5 => {
            if argument == 0 {
                return Ok((Cbor::Map(Vec::new()), 1));
            }
            let (len, mut consumed) = decode_length(data, argument, index)?;
            let mut items: Vec<(Cbor, Cbor)> = Vec::new();
            let mut i = 0.0;
            while i < len {
                if data.len() <= index + consumed {
                    return Err(CborError);
                }
                let (key, kn) = decode_next(data, index + consumed, depth + 1)?;
                consumed = consumed.checked_add(kn).ok_or(CborError)?;
                if data.len() <= index + consumed {
                    return Err(CborError);
                }
                let dup = match &key {
                    // Map keys compare with SameValueZero: NaN equals NaN, -0 equals 0.
                    Cbor::Num(n) => items.iter().any(|(k, _)| {
                        matches!(k, Cbor::Num(m) if m == n || (m.is_nan() && n.is_nan()))
                    }),
                    Cbor::Text(t) => items
                        .iter()
                        .any(|(k, _)| matches!(k, Cbor::Text(u) if u == t)),
                    _ => return Err(CborError),
                };
                if dup {
                    return Err(CborError);
                }
                let (value, vn) = decode_next(data, index + consumed, depth + 1)?;
                consumed = consumed.checked_add(vn).ok_or(CborError)?;
                items.push((key, value));
                i += 1.0;
            }
            Ok((Cbor::Map(items), consumed))
        }
        6 => {
            let (tag, tn) = decode_length(data, argument, index)?;
            let (v, vn) = decode_next(data, index + tn, depth + 1)?;
            Ok((Cbor::Tag(tag, Box::new(v)), tn.checked_add(vn).ok_or(CborError)?))
        }
        _ => match argument {
            20 => Ok((Cbor::Bool(false), 1)),
            21 => Ok((Cbor::Bool(true), 1)),
            22 => Ok((Cbor::Null, 1)),
            23 => Ok((Cbor::Undefined, 1)),
            25 => {
                let b = data.get(index + 1..index + 3).ok_or(CborError)?;
                match (b.first(), b.get(1)) {
                    (Some(0x7c), Some(0x00)) => Ok((Cbor::Num(f64::INFINITY), 3)),
                    (Some(0x7e), Some(0x00)) => Ok((Cbor::Num(f64::NAN), 3)),
                    (Some(0xfc), Some(0x00)) => Ok((Cbor::Num(f64::NEG_INFINITY), 3)),
                    _ => Err(CborError),
                }
            }
            26 => {
                let b: [u8; 4] = data
                    .get(index + 1..index + 5)
                    .and_then(|b| b.try_into().ok())
                    .ok_or(CborError)?;
                Ok((Cbor::Num(f64::from(f32::from_be_bytes(b))), 5))
            }
            27 => {
                let b: [u8; 8] = data
                    .get(index + 1..index + 9)
                    .and_then(|b| b.try_into().ok())
                    .ok_or(CborError)?;
                Ok((Cbor::Num(f64::from_be_bytes(b)), 9))
            }
            _ => Err(CborError),
        },
    }
}

/// `isoCBOR.decodeFirst(data)` with the bytes it consumed (tiny-cbor `decodePartialCBOR`).
pub fn decode_first(data: &[u8]) -> Result<(Cbor, usize), CborError> {
    if data.is_empty() {
        return Err(CborError);
    }
    decode_next(data, 0, 0)
}

fn header(out: &mut Vec<u8>, major: u8, arg: u64) {
    let m = major << 5;
    if arg <= 23 {
        out.push(m | arg as u8);
    } else if arg <= 0xff {
        out.extend([m | 24, arg as u8]);
    } else if arg <= 0xffff {
        out.push(m | 25);
        out.extend((arg as u16).to_be_bytes());
    } else if arg <= 0xffff_ffff {
        out.push(m | 26);
        out.extend((arg as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend(arg.to_be_bytes());
    }
}

const MAX_SAFE: f64 = 9_007_199_254_740_991.0;

fn encode_num(out: &mut Vec<u8>, n: f64) {
    if n.fract() == 0.0 && n.abs() <= MAX_SAFE {
        if n < 0.0 {
            header(out, 1, (-n) as u64 - 1);
        } else {
            header(out, 0, n as u64);
        }
        return;
    }
    let f = n as f32;
    if f64::from(f) == n || !n.is_finite() {
        out.push(0xfa);
        out.extend(f.to_be_bytes());
    } else {
        out.push(0xfb);
        out.extend(n.to_be_bytes());
    }
}

fn encode_into(v: &Cbor, out: &mut Vec<u8>) {
    match v {
        Cbor::Num(n) => encode_num(out, *n),
        Cbor::Bytes(b) => {
            header(out, 2, b.len() as u64);
            out.extend_from_slice(b);
        }
        Cbor::Text(t) => {
            header(out, 3, js_json::js_len(t) as u64);
            out.extend_from_slice(t.as_bytes());
        }
        Cbor::Array(items) => {
            header(out, 4, items.len() as u64);
            for i in items {
                encode_into(i, out);
            }
        }
        Cbor::Map(items) => {
            header(out, 5, items.len() as u64);
            for (k, val) in items {
                encode_into(k, out);
                encode_into(val, out);
            }
        }
        Cbor::Tag(t, val) => {
            header(out, 6, *t as u64);
            encode_into(val, out);
        }
        Cbor::Bool(false) => out.push(0xf4),
        Cbor::Bool(true) => out.push(0xf5),
        Cbor::Null => out.push(0xf6),
        Cbor::Undefined => out.push(0xf7),
    }
}

/// `isoCBOR.encode(value)`.
pub fn encode(v: &Cbor) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(v, &mut out);
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn canonical_keys_round_trip() {
        // A P-256 COSE key as authenticators send it: {1:2, 3:-7, -1:1, -2:x, -3:y}.
        let mut key = vec![0xa5, 0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21, 0x58, 0x20];
        key.extend([7u8; 32]);
        key.extend([0x22, 0x58, 0x20]);
        key.extend([9u8; 32]);
        let (v, n) = decode_first(&key).unwrap();
        assert_eq!(n, key.len());
        assert_eq!(encode(&v), key);
        assert_eq!(v.get_num(3.0), Some(&Cbor::Num(-7.0)));
        assert_eq!(v.get_num(-1.0), Some(&Cbor::Num(1.0)));
        // Trailing bytes are not read.
        let mut more = key.clone();
        more.extend([0xff, 0xff]);
        assert_eq!(decode_first(&more).unwrap().1, key.len());
    }

    #[test]
    fn tiny_cbor_rules() {
        // A length-24 argument carrying 5 is not well formed; 25 carrying 30 re-encodes shorter.
        assert!(decode_first(&[0x18, 0x05]).is_err());
        let (v, n) = decode_first(&[0x19, 0x00, 0x1e]).unwrap();
        assert_eq!((v.clone(), n), (Cbor::Num(30.0), 3));
        assert_eq!(encode(&v), vec![0x18, 0x1e]);
        // Indefinite lengths, unknown simple values, duplicate and non-scalar map keys.
        for bad in [
            &[0x9f, 0x01, 0xff][..],
            &[0xf8, 0x20],
            &[0xa2, 0x01, 0x01, 0x01, 0x02],
            &[0xa1, 0x80, 0x01],
            &[0xf9, 0x3c, 0x00],
            &[0x82, 0x01],
            &[],
        ] {
            assert!(decode_first(bad).is_err(), "{bad:?}");
        }
        // A byte string longer than the data is cut, the position still moves by its length.
        let (v, n) = decode_first(&[0x45, 1, 2]).unwrap();
        assert_eq!((v, n), (Cbor::Bytes(vec![1, 2]), 6));
        // Floats: exact float32 stays float32; -0 is the integer 0.
        assert_eq!(encode(&Cbor::Num(1.5)), vec![0xfa, 0x3f, 0xc0, 0, 0]);
        assert_eq!(encode(&Cbor::Num(-0.0)), vec![0x00]);
        assert_eq!(encode(&Cbor::Num(0.1)).len(), 9);
        assert_eq!(encode(&Cbor::Num(-1.0)), vec![0x20]);
        // Text headers count UTF-16 units (tiny-cbor), not bytes.
        assert_eq!(encode(&Cbor::Text("é".into())), vec![0x61, 0xc3, 0xa9]);
        let (v, _) = decode_first(&[0xf9, 0x7e, 0x00]).unwrap();
        assert!(matches!(v, Cbor::Num(n) if n.is_nan()));
    }
}
