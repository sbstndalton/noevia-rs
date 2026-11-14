//! An in-memory tar reader with the member semantics of CPython 3.12 `tarfile` in read mode ("r:").
//!
//! Followed from the reference: header checksum (unsigned or signed), octal and base-256 numbers parsed like
//! `tarfile.nti` (Python `int(text, 8)`, so signs, `0o` and underscores), the ustar prefix field joined for
//! every non-GNU type, an old-style regular file whose name ends in `/` read as a directory, trailing slashes
//! of directory names dropped, GNU long names and long links (one trailing slash dropped from a directory),
//! pax extended headers (`path` with trailing slashes dropped, `linkpath`, `size`; a bad `size` reads as 0)
//! and pax global headers applied to every later member (their `size` changes the size, not the data
//! offset), data skipped only for regular and unknown types, and the first all-zero block ending the archive.
//!
//! Deliberate differences, all refusals where the reference accepts: a malformed or truncated header after
//! the first member is refused (the reference silently ends the archive there, ignoring what follows), a
//! negative size of an extension header is refused, and chains of more than 512 extension headers are
//! refused (the reference fails with an uncaught recursion error near 1000).

use crate::Refused;
use std::collections::BTreeMap;

const BLOCK: usize = 512;
const MAX_CHAIN: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `plain` is false for the contiguous-file type, which the reference reads as regular but refuses.
    File {
        plain: bool,
    },
    Directory,
    Symlink,
    /// Hard links, devices, FIFOs and unknown types.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// Raw name bytes (the reference decodes them leniently and refuses non-UTF-8 only where it checks).
    pub name: Vec<u8>,
    pub linkname: Vec<u8>,
    /// Header mode as parsed (may be negative from a base-256 field, as in the reference).
    pub mode: i128,
    pub size: i128,
    pub kind: Kind,
    /// GNU sparse type, or a pax header (global or extended) with a `GNU.sparse.*` or `SCHILY.realsize` key.
    pub sparse: bool,
    /// The header's type byte (after the old-style directory rule).
    pub type_byte: u8,
    data_offset: usize,
}

pub struct Reader<'a> {
    data: &'a [u8],
    offset: usize,
    global: BTreeMap<Vec<u8>, Vec<u8>>,
    first: bool,
}

fn invalid() -> Refused {
    Refused::new("invalid source archive")
}

fn block_len(size: i128) -> Result<usize, Refused> {
    let size = usize::try_from(size).map_err(|_| invalid())?;
    size.div_ceil(BLOCK).checked_mul(BLOCK).ok_or_else(invalid)
}

/// Bytes up to the first NUL (`tarfile.nts`).
fn nts(field: &[u8]) -> &[u8] {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    field.get(..end).unwrap_or_default()
}

fn is_py_space(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | 0x1c..=0x1f
    )
}

/// Python `int(text, radix)` for ASCII text with radix 8 or 10: surrounding whitespace, a sign, for radix 8
/// an optional `0o` prefix (an underscore may follow it), digits with single underscores between them.
pub fn py_int(text: &[u8], radix: u32) -> Option<i128> {
    let start = text.iter().position(|byte| !is_py_space(*byte))?;
    let end = text.iter().rposition(|byte| !is_py_space(*byte))?;
    let mut rest = text.get(start..=end)?;
    let mut negative = false;
    if let Some((sign, tail)) = rest.split_first() {
        if *sign == b'+' || *sign == b'-' {
            negative = *sign == b'-';
            rest = tail;
        }
    }
    let mut underscore_ok = false;
    if radix == 8
        && rest.len() >= 2
        && rest.first() == Some(&b'0')
        && matches!(rest.get(1), Some(b'o' | b'O'))
    {
        rest = rest.get(2..)?;
        underscore_ok = true;
    }
    let mut value: i128 = 0;
    let mut digits = 0usize;
    let mut last_underscore = false;
    for byte in rest {
        if *byte == b'_' {
            if last_underscore || (digits == 0 && !underscore_ok) {
                return None;
            }
            last_underscore = true;
            continue;
        }
        let digit = char::from(*byte).to_digit(radix)?;
        value = value
            .checked_mul(i128::from(radix))?
            .checked_add(i128::from(digit))?;
        digits += 1;
        last_underscore = false;
    }
    if digits == 0 || last_underscore {
        return None;
    }
    Some(if negative { -value } else { value })
}

/// `tarfile.nti`: a base-256 field (first byte 0o200 or 0o377) or NUL-terminated ASCII octal.
fn nti(field: &[u8]) -> Result<i128, Refused> {
    match field.split_first() {
        Some((lead @ (0o200 | 0o377), rest)) => {
            let mut value: i128 = 0;
            for byte in rest {
                value = value
                    .checked_mul(256)
                    .and_then(|v| v.checked_add(i128::from(*byte)))
                    .ok_or_else(invalid)?;
            }
            if *lead == 0o377 {
                let span = 1i128
                    .checked_shl(8 * u32::try_from(rest.len()).map_err(|_| invalid())?)
                    .ok_or_else(invalid)?;
                value = -(span - value);
            }
            Ok(value)
        }
        _ => {
            let text = nts(field);
            if !text.is_ascii() {
                return Err(invalid());
            }
            if text.iter().all(|byte| is_py_space(*byte)) {
                return Ok(0);
            }
            py_int(text, 8).ok_or_else(invalid)
        }
    }
}

fn field(header: &[u8], from: usize, to: usize) -> Result<&[u8], Refused> {
    header.get(from..to).ok_or_else(invalid)
}

/// Parse pax records (`<len> <key>=<value>\n`) the way CPython 3.12 does, into `into`.
fn pax_records(buf: &[u8], into: &mut BTreeMap<Vec<u8>, Vec<u8>>) -> Result<(), Refused> {
    let mut pos = 0usize;
    while let Some(&byte) = buf.get(pos) {
        if byte == 0 {
            break;
        }
        let digits = buf
            .get(pos..)
            .unwrap_or_default()
            .iter()
            .take(21)
            .take_while(|b| b.is_ascii_digit())
            .count();
        if digits == 0 || digits > 20 || buf.get(pos + digits) != Some(&b' ') {
            return Err(invalid());
        }
        let length: usize = std::str::from_utf8(buf.get(pos..pos + digits).ok_or_else(invalid)?)
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or_else(invalid)?;
        let end = pos.checked_add(length).ok_or_else(invalid)?;
        if length < 5 || end > buf.len() {
            return Err(invalid());
        }
        let last = end - 1;
        let body = buf.get(pos + digits + 1..last).unwrap_or_default();
        let equals = body.iter().position(|b| *b == b'=');
        let (key, value) = match equals {
            Some(at) => (
                body.get(..at).unwrap_or_default(),
                body.get(at + 1..).unwrap_or_default(),
            ),
            None => return Err(invalid()),
        };
        if key.is_empty() || buf.get(last) != Some(&b'\n') {
            return Err(invalid());
        }
        into.insert(key.to_vec(), value.to_vec());
        pos = end;
    }
    Ok(())
}

fn has_sparse_key(headers: &BTreeMap<Vec<u8>, Vec<u8>>) -> bool {
    headers
        .keys()
        .any(|key| key.starts_with(b"GNU.sparse.") || key.as_slice() == b"SCHILY.realsize")
}

fn rstrip_slashes(name: &mut Vec<u8>) {
    while name.last() == Some(&b'/') {
        name.pop();
    }
}

impl Member {
    fn is_reg(&self) -> bool {
        matches!(self.kind, Kind::File { .. })
    }

    /// `TarInfo._apply_pax_info` for the fields that reach a verdict.
    fn apply_pax(&mut self, headers: &BTreeMap<Vec<u8>, Vec<u8>>) {
        for (key, value) in headers {
            match key.as_slice() {
                b"path" | b"GNU.sparse.name" => {
                    let mut name = value.clone();
                    if key.as_slice() == b"path" {
                        rstrip_slashes(&mut name);
                    }
                    self.name = name;
                }
                b"linkpath" => self.linkname = value.clone(),
                b"size" => self.size = py_int(value, 10).unwrap_or(0),
                _ => {}
            }
        }
        if has_sparse_key(headers) {
            self.sparse = true;
        }
    }
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader {
            data,
            offset: 0,
            global: BTreeMap::new(),
            first: true,
        }
    }

    /// The next member, or `None` at the first all-zero block or at the end of the data on a block boundary.
    pub fn next_member(&mut self) -> Result<Option<Member>, Refused> {
        let first = std::mem::replace(&mut self.first, false);
        let Some(rest) = self.data.get(self.offset..) else {
            return Err(invalid());
        };
        if rest.is_empty() {
            return if first {
                Err(Refused::new("empty file"))
            } else {
                Ok(None)
            };
        }
        match rest.get(..BLOCK) {
            None => Err(invalid()),
            Some(block) if block.iter().all(|byte| *byte == 0) => Ok(None),
            Some(_) => self.member(0).map(Some),
        }
    }

    /// The content of a regular member: `size` bytes at its data offset.
    pub fn content(&self, member: &Member) -> Result<&'a [u8], Refused> {
        let size = usize::try_from(member.size).map_err(|_| Refused::new("source file size"))?;
        member
            .data_offset
            .checked_add(size)
            .and_then(|end| self.data.get(member.data_offset..end))
            .ok_or_else(|| Refused::new("truncated source file"))
    }

    fn header(&mut self) -> Result<&'a [u8], Refused> {
        let header = self
            .data
            .get(self.offset..)
            .and_then(|rest| rest.get(..BLOCK))
            .ok_or_else(invalid)?;
        if header.iter().all(|byte| *byte == 0) {
            return Err(invalid());
        }
        let checksum = nti(field(header, 148, 156)?)?;
        let mut unsigned: i128 = 256;
        let mut signed: i128 = 256;
        for (index, byte) in header.iter().enumerate() {
            if !(148..156).contains(&index) {
                unsigned += i128::from(*byte);
                signed += i128::from(*byte as i8);
            }
        }
        if checksum != unsigned && checksum != signed {
            return Err(invalid());
        }
        self.offset += BLOCK;
        Ok(header)
    }

    /// One member at the current offset, following extension headers (`depth` counts them).
    fn member(&mut self, depth: usize) -> Result<Member, Refused> {
        if depth > MAX_CHAIN {
            return Err(invalid());
        }
        let header = self.header()?;
        let mut name = nts(field(header, 0, 100)?).to_vec();
        let mode = nti(field(header, 100, 108)?)?;
        for (from, to) in [(108, 116), (116, 124), (136, 148), (329, 337), (337, 345)] {
            nti(field(header, from, to)?)?;
        }
        let size = nti(field(header, 124, 136)?)?;
        let mut kind_byte = *header.get(156).ok_or_else(invalid)?;
        let linkname = nts(field(header, 157, 257)?).to_vec();
        let prefix = nts(field(header, 345, 500)?).to_vec();
        if kind_byte == 0 && name.last() == Some(&b'/') {
            kind_byte = b'5';
        }
        let gnu = matches!(kind_byte, b'L' | b'K' | b'S');
        if kind_byte == b'5' {
            rstrip_slashes(&mut name);
        }
        if !prefix.is_empty() && !gnu {
            let mut joined = prefix;
            joined.push(b'/');
            joined.extend_from_slice(&name);
            name = joined;
        }
        match kind_byte {
            b'L' | b'K' => {
                let length = block_len(size)?;
                let buf = self.extension_data(size, length)?;
                let value = nts(buf).to_vec();
                let mut next = self.member(depth + 1)?;
                if kind_byte == b'L' {
                    next.name = value;
                    if next.kind == Kind::Directory && next.name.last() == Some(&b'/') {
                        next.name.pop();
                    }
                } else {
                    next.linkname = value;
                }
                Ok(next)
            }
            b'x' | b'X' | b'g' => {
                let length = block_len(size)?;
                let buf = self.extension_data(size, length)?;
                let headers = if kind_byte == b'g' {
                    let mut global = std::mem::take(&mut self.global);
                    let parsed = pax_records(buf, &mut global);
                    self.global = global;
                    parsed?;
                    self.global.clone()
                } else {
                    let mut local = self.global.clone();
                    pax_records(buf, &mut local)?;
                    local
                };
                let mut next = self.member(depth + 1)?;
                if kind_byte == b'g' {
                    if has_sparse_key(&headers) {
                        next.sparse = true;
                    }
                    return Ok(next);
                }
                next.apply_pax(&headers);
                if headers.contains_key(b"size".as_slice()) {
                    let mut offset = next.data_offset;
                    if next.is_reg() || !known_type(next.type_byte) {
                        offset = offset
                            .checked_add(block_len(next.size)?)
                            .ok_or_else(invalid)?;
                    }
                    self.offset = offset;
                }
                Ok(next)
            }
            b'S' => {
                // Refused by the caller once its name is checked; its data layout never matters.
                Ok(Member {
                    name,
                    linkname,
                    mode,
                    size,
                    kind: Kind::File { plain: false },
                    sparse: true,
                    type_byte: kind_byte,
                    data_offset: self.offset,
                })
            }
            _ => {
                let kind = match kind_byte {
                    b'0' | 0 => Kind::File { plain: true },
                    b'7' => Kind::File { plain: false },
                    b'5' => Kind::Directory,
                    b'2' => Kind::Symlink,
                    _ => Kind::Other,
                };
                let data_offset = self.offset;
                if matches!(kind, Kind::File { .. }) || !known_type(kind_byte) {
                    self.offset = self
                        .offset
                        .checked_add(block_len(size)?)
                        .ok_or_else(invalid)?;
                }
                let mut member = Member {
                    name,
                    linkname,
                    mode,
                    size,
                    kind,
                    sparse: false,
                    type_byte: kind_byte,
                    data_offset,
                };
                let global = self.global.clone();
                member.apply_pax(&global);
                if member.kind == Kind::Directory {
                    rstrip_slashes(&mut member.name);
                }
                Ok(member)
            }
        }
    }

    fn extension_data(&mut self, size: i128, length: usize) -> Result<&'a [u8], Refused> {
        if size < 0 {
            return Err(invalid());
        }
        let start = self.offset;
        let end = start.checked_add(length).ok_or_else(invalid)?;
        // A short read is what the reference gets too; the following header read then fails.
        let buf = self
            .data
            .get(start..end.min(self.data.len()))
            .ok_or_else(invalid)?;
        self.offset = end.min(self.data.len());
        Ok(buf)
    }
}

/// Types the reference supports without skipping data blocks for (besides the regular ones).
fn known_type(kind_byte: u8) -> bool {
    matches!(
        kind_byte,
        b'0' | 0 | b'1' | b'2' | b'3' | b'4' | b'5' | b'6' | b'7' | b'L' | b'K' | b'S'
    )
}

#[cfg(test)]
mod tests {
    use super::{nti, py_int};

    #[test]
    fn numbers_parse_like_python() {
        assert_eq!(py_int(b"0000644", 8), Some(0o644));
        assert_eq!(py_int(b" 755 ", 8), Some(0o755));
        assert_eq!(py_int(b"0o7_5_5", 8), Some(0o755));
        assert_eq!(py_int(b"0o_7", 8), Some(7));
        assert_eq!(py_int(b"-17", 8), Some(-15));
        assert_eq!(py_int(b"_7", 8), None);
        assert_eq!(py_int(b"7__7", 8), None);
        assert_eq!(py_int(b"7_", 8), None);
        assert_eq!(py_int(b"8", 8), None);
        assert_eq!(py_int(b"12", 10), Some(12));
        assert_eq!(py_int(b"0o12", 10), None);
        assert_eq!(nti(b"\0\0\0\0\0\0\0\0").ok(), Some(0));
        assert_eq!(nti(b"       \0").ok(), Some(0));
        assert_eq!(nti(&[0o200, 0, 0, 0, 0, 0, 1, 0]).ok(), Some(256));
        assert_eq!(
            nti(&[0o377, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).ok(),
            Some(-1)
        );
        assert!(nti(b"12x4567\0").is_err());
    }
}
