use prompt_framing::json::{self, Value};
pub const BODY_CAP: usize = 16384;

// json.loads(bytes) detects UTF-8/16/32, including BOMs. Decode to code units
// without replacing lone surrogate escapes: Python len counts Unicode scalars
// and individual unpaired surrogates, rather than UTF-8 bytes or UTF-16 units.
fn units(bytes: &[u8]) -> Option<Vec<u16>> {
    let mut out = Vec::new();
    out.try_reserve_exact(BODY_CAP).ok()?;
    let head = bytes.get(..4);
    let (width, little, skip) = if bytes.starts_with(&[0, 0, 0xfe, 0xff]) {
        (4, false, 4)
    } else if bytes.starts_with(&[0xff, 0xfe, 0, 0]) {
        (4, true, 4)
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        (2, false, 2)
    } else if bytes.starts_with(&[0xff, 0xfe]) {
        (2, true, 2)
    } else if head
        .is_some_and(|s| s.first() == Some(&0) && s.get(1) == Some(&0) && s.get(2) == Some(&0))
    {
        (4, false, 0)
    } else if head
        .is_some_and(|s| s.get(1) == Some(&0) && s.get(2) == Some(&0) && s.get(3) == Some(&0))
    {
        (4, true, 0)
    } else if bytes.first() == Some(&0) {
        (2, false, 0)
    } else if bytes.get(1) == Some(&0) {
        (2, true, 0)
    } else {
        (
            1,
            false,
            if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
                3
            } else {
                0
            },
        )
    };
    let data = bytes.get(skip..)?;
    if width == 1 {
        // json.loads uses UTF-8 surrogatepass, accepting UTF-8 encoded lone
        // surrogate code points while still rejecting overlong/invalid bytes.
        let mut at = 0;
        while let Some(&first) = data.get(at) {
            let (width, min, mut value) = match first {
                0..=127 => (1, 0, u32::from(first)),
                194..=223 => (2, 128, u32::from(first & 31)),
                224..=239 => (3, 2048, u32::from(first & 15)),
                240..=244 => (4, 65536, u32::from(first & 7)),
                _ => return None,
            };
            for byte in data.get(at + 1..at + width)? {
                if byte & 192 != 128 {
                    return None;
                }
                value = value * 64 + u32::from(byte & 63);
            }
            if value < min || value > 0x10ffff {
                return None;
            }
            append(value, &mut out)?;
            at += width;
        }
    } else {
        if data.len() % width != 0 {
            return None;
        }
        for chunk in data.chunks_exact(width) {
            let value = if little {
                chunk
                    .iter()
                    .rev()
                    .fold(0u32, |n, b| n * 256 + u32::from(*b))
            } else {
                chunk.iter().fold(0u32, |n, b| n * 256 + u32::from(*b))
            };
            if width == 2 {
                out.push(u16::try_from(value).ok()?);
            } else {
                append(value, &mut out)?;
            }
        }
        if width == 2 {
            let mut at = 0;
            while let Some(&unit) = out.get(at) {
                if (0xd800..=0xdbff).contains(&unit)
                    && out
                        .get(at + 1)
                        .is_some_and(|next| (0xdc00..=0xdfff).contains(next))
                {
                    at += 2;
                    continue;
                }
                if (0xd800..=0xdfff).contains(&unit) {
                    if let Some(slot) = out.get_mut(at) {
                        *slot = 0xfffd;
                    }
                }
                at += 1;
            }
        }
    }
    Some(out)
}
fn append(value: u32, out: &mut Vec<u16>) -> Option<()> {
    // Raw surrogatepass code points remain individual Python characters; they
    // must not accidentally combine as an escaped JSON surrogate pair. Text
    // content is not projected: the original validated bytes go to the engine.
    if let Ok(value) = u16::try_from(value) {
        out.push(if (0xd800..=0xdfff).contains(&value) {
            0xfffd
        } else {
            value
        });
    } else {
        let c = char::from_u32(value)?;
        let mut encoded = [0u16; 2];
        out.extend_from_slice(c.encode_utf16(&mut encoded));
    }
    Some(())
}
fn len(s: &[u16]) -> usize {
    char::decode_utf16(s.iter().copied()).count()
}
fn exact(value: &Value, keys: &[&str]) -> bool {
    matches!(value,Value::Obj(fields) if fields.len()==keys.len() && keys.iter().all(|k| value.get(k).is_some()))
}
pub fn validate(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > BODY_CAP {
        return false;
    }
    let Some(mut input) = units(bytes) else {
        return false;
    };
    // Python permits these non-standard constants. They can only be rejected
    // typed fields or overwritten duplicate values, so normalize their numeric
    // representation while preserving shape and duplicate-key semantics.
    let mut normalized = Vec::new();
    if normalized.try_reserve_exact(input.len()).is_err() {
        return false;
    }
    let mut string = false;
    let mut escaped = false;
    let mut cursor = 0;
    while let Some(&c) = input.get(cursor) {
        if string {
            normalized.push(c);
            if escaped {
                escaped = false;
            } else if c == 92 {
                escaped = true;
            } else if c == 34 {
                string = false;
            }
            cursor += 1;
            continue;
        }
        if c == 34 {
            string = true;
        }
        let mut replacement = 0;
        for token in ["NaN", "Infinity", "-Infinity"] {
            let before = cursor == 0
                || input
                    .get(cursor - 1)
                    .is_some_and(|c| matches!(*c, 9 | 10 | 13 | 32 | 58 | 91 | 44));
            let after = input
                .get(cursor + token.len())
                .is_none_or(|c| matches!(*c, 9 | 10 | 13 | 32 | 44 | 93 | 125));
            if before
                && after
                && input
                    .get(cursor..cursor + token.len())
                    .is_some_and(|s| s.iter().copied().eq(token.encode_utf16()))
            {
                replacement = token.len();
                break;
            }
        }
        if replacement > 0 {
            normalized.push(48);
            cursor += replacement;
        } else {
            normalized.push(c);
            cursor += 1;
        }
    }
    input.clear();
    let Some(body) = json::parse(&normalized, 4) else {
        return false;
    };
    if !exact(&body, &["state", "question", "options"]) {
        return false;
    }
    let Some(state) = body.get("state").and_then(Value::as_str) else {
        return false;
    };
    let Some(question) = body.get("question").and_then(Value::as_str) else {
        return false;
    };
    if len(state) > 4000 || !(1..=500).contains(&len(question)) {
        return false;
    }
    let Some(Value::Arr(options)) = body.get("options") else {
        return false;
    };
    if !(2..=8).contains(&options.len()) {
        return false;
    }
    let mut ids: Vec<&[u16]> = Vec::new();
    for option in options {
        if !exact(option, &["id", "label"]) {
            return false;
        }
        let Some(id) = option.get("id").and_then(Value::as_str) else {
            return false;
        };
        let Some(label) = option.get("label").and_then(Value::as_str) else {
            return false;
        };
        if id.is_empty()
            || id.len() > 32
            || !id.first().is_some_and(|c| (97..=122).contains(c))
            || !id
                .iter()
                .all(|c| (97..=122).contains(c) || (48..=57).contains(c) || *c == 95)
            || !(1..=120).contains(&len(label))
            || ids.contains(&id)
        {
            return false;
        }
        ids.push(id);
    }
    true
}
