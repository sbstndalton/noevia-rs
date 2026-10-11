//! CPython json.loads byte encoding detection, bounded to the native request/text budget.
use super::Error;
use std::borrow::Cow;
pub(super) fn decode(bytes: &[u8]) -> Result<Cow<'_, str>, Error> {
    if bytes.len() > 1_048_576 {
        return Err(Error::Input);
    }
    enum Encoding {
        Utf8,
        Utf16(bool),
        Utf32(bool),
    }
    let (encoding, bytes) = if let Some(rest) = bytes.strip_prefix(&[0, 0, 0xfe, 0xff]) {
        (Encoding::Utf32(false), rest)
    } else if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe, 0, 0]) {
        (Encoding::Utf32(true), rest)
    } else if let Some(rest) = bytes.strip_prefix(&[0xfe, 0xff]) {
        (Encoding::Utf16(false), rest)
    } else if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe]) {
        (Encoding::Utf16(true), rest)
    } else if let Some(rest) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        (Encoding::Utf8, rest)
    } else {
        (
            match bytes {
                [0, b, _, _, ..] if *b != 0 => Encoding::Utf16(false),
                [0, _, _, _, ..] => Encoding::Utf32(false),
                [_, 0, c, d, ..] if *c != 0 || *d != 0 => Encoding::Utf16(true),
                [_, 0, _, _, ..] => Encoding::Utf32(true),
                [0, _] => Encoding::Utf16(false),
                [_, 0] => Encoding::Utf16(true),
                _ => Encoding::Utf8,
            },
            bytes,
        )
    };
    if matches!(encoding, Encoding::Utf8) {
        return std::str::from_utf8(bytes)
            .map(Cow::Borrowed)
            .map_err(|_| Error::Body);
    }
    let mut out = String::new();
    out.try_reserve(bytes.len()).map_err(|_| Error::Limit)?;
    let mut add = |character: char| -> Result<(), Error> {
        if out.len().saturating_add(character.len_utf8()) > 1_048_576 {
            return Err(Error::Text);
        }
        out.try_reserve(character.len_utf8())
            .map_err(|_| Error::Limit)?;
        out.push(character);
        Ok(())
    };
    match encoding {
        Encoding::Utf16(little) => {
            let (chunks, remainder) = bytes.as_chunks::<2>();
            if !remainder.is_empty() {
                return Err(Error::Body);
            }
            let units = chunks.iter().map(|a| {
                if little {
                    u16::from_le_bytes(*a)
                } else {
                    u16::from_be_bytes(*a)
                }
            });
            for character in char::decode_utf16(units) {
                add(character.map_err(|_| Error::Body)?)?;
            }
        }
        Encoding::Utf32(little) => {
            let (chunks, remainder) = bytes.as_chunks::<4>();
            if !remainder.is_empty() {
                return Err(Error::Body);
            }
            for a in chunks {
                let cp = if little {
                    u32::from_le_bytes(*a)
                } else {
                    u32::from_be_bytes(*a)
                };
                add(char::from_u32(cp).ok_or(Error::Body)?)?;
            }
        }
        Encoding::Utf8 => return Err(Error::Body),
    }
    Ok(Cow::Owned(out))
}
