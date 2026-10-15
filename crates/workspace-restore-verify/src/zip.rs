//! The ZIP container, adapted from docx-text's reader (noevia#981): the end-of-central-directory
//! search and central directory parse that Python's `zipfile` makes when opening, then (stricter
//! than Python, see the crate docs) a full agreement check of every local header with the central
//! directory and an exact read of each member (size, DEFLATE end, CRC-32).
//!
//! Every failure here is `Reject::Invalid`, the class Python's `verify` gives every `zipfile` and
//! `zlib` error ("Invalid workspace archive or manifest").

use crate::Reject;

const EOCD_SIG: &[u8; 4] = b"PK\x05\x06";
const ZIP64_LOCATOR_SIG: &[u8; 4] = b"PK\x06\x07";
const CENTRAL_SIG: &[u8; 4] = b"PK\x01\x02";
const LOCAL_SIG: &[u8; 4] = b"PK\x03\x04";
const DESCRIPTOR_SIG: &[u8; 4] = b"PK\x07\x08";
/// zipfile's search window: the largest comment plus the record.
const EOCD_WINDOW: usize = 65_535 + 22;
/// zipfile.MAX_EXTRACT_VERSION.
const MAX_EXTRACT_VERSION: u8 = 63;
pub(crate) const FLAG_ENCRYPTED: u16 = 0x0001;
const FLAG_DESCRIPTOR: u16 = 0x0008;
const FLAG_UTF8: u16 = 0x0800;
/// Flag bits a member may carry: encryption (refused by the entry checks, with Python's
/// message), the two DEFLATE option bits, data descriptor, UTF-8 names. Anything else (patched
/// data, strong encryption, masked headers, reserved bits) is refused.
const FLAGS_ALLOWED: u16 = FLAG_ENCRYPTED | 0x0002 | 0x0004 | FLAG_DESCRIPTOR | FLAG_UTF8;
pub(crate) const METHOD_STORED: u16 = 0;
pub(crate) const METHOD_DEFLATED: u16 = 8;
/// More members than this can never match the 16-bit EOCD count; parsing stops there so a
/// huge central directory cannot make us allocate per-entry records without bound.
const MAX_PARSED_MEMBERS: usize = 65_536;

/// Python's cp437 codec for bytes 0x80..=0xFF (0x00..=0x7F map to themselves).
const CP437_HIGH: &str = "\u{c7}\u{fc}\u{e9}\u{e2}\u{e4}\u{e0}\u{e5}\u{e7}\u{ea}\u{eb}\u{e8}\u{ef}\u{ee}\u{ec}\u{c4}\u{c5}\u{c9}\u{e6}\u{c6}\u{f4}\u{f6}\u{f2}\u{fb}\u{f9}\u{ff}\u{d6}\u{dc}\u{a2}\u{a3}\u{a5}\u{20a7}\u{192}\u{e1}\u{ed}\u{f3}\u{fa}\u{f1}\u{d1}\u{aa}\u{ba}\u{bf}\u{2310}\u{ac}\u{bd}\u{bc}\u{a1}\u{ab}\u{bb}\u{2591}\u{2592}\u{2593}\u{2502}\u{2524}\u{2561}\u{2562}\u{2556}\u{2555}\u{2563}\u{2551}\u{2557}\u{255d}\u{255c}\u{255b}\u{2510}\u{2514}\u{2534}\u{252c}\u{251c}\u{2500}\u{253c}\u{255e}\u{255f}\u{255a}\u{2554}\u{2569}\u{2566}\u{2560}\u{2550}\u{256c}\u{2567}\u{2568}\u{2564}\u{2565}\u{2559}\u{2558}\u{2552}\u{2553}\u{256b}\u{256a}\u{2518}\u{250c}\u{2588}\u{2584}\u{258c}\u{2590}\u{2580}\u{3b1}\u{df}\u{393}\u{3c0}\u{3a3}\u{3c3}\u{b5}\u{3c4}\u{3a6}\u{398}\u{3a9}\u{3b4}\u{221e}\u{3c6}\u{3b5}\u{2229}\u{2261}\u{b1}\u{2265}\u{2264}\u{2320}\u{2321}\u{f7}\u{2248}\u{b0}\u{2219}\u{b7}\u{221a}\u{207f}\u{b2}\u{25a0}\u{a0}";

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    let b = d.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([*b.first()?, *b.get(1)?]))
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    let b = d.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([
        *b.first()?,
        *b.get(1)?,
        *b.get(2)?,
        *b.get(3)?,
    ]))
}

/// `bytes.rfind(needle, start)`.
fn rfind(hay: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    let last = hay.len().checked_sub(needle.len())?;
    (start..=last)
        .rev()
        .find(|&i| hay.get(i..i + needle.len()) == Some(needle))
}

/// A member name as Python decodes it (UTF-8 when flagged, else cp437), cut at the first NUL
/// like `ZipInfo.__init__` does. None when a flagged name is not UTF-8 (Python raises).
fn python_name(raw: &[u8], flags: u16) -> Option<String> {
    let full: String = if flags & FLAG_UTF8 != 0 {
        std::str::from_utf8(raw).ok()?.to_owned()
    } else {
        raw.iter()
            .map(|&b| {
                if b < 0x80 {
                    Some(char::from(b))
                } else {
                    CP437_HIGH.chars().nth(usize::from(b - 0x80))
                }
            })
            .collect::<Option<String>>()?
    };
    Some(match full.find('\0') {
        Some(cut) => full.get(..cut).unwrap_or_default().to_owned(),
        None => full,
    })
}

/// `ZipInfo._decodeExtra`: a block running past the end is corrupt.
fn python_extra_ok(extra: &[u8]) -> bool {
    let mut rest = extra;
    while rest.len() >= 4 {
        let Some(len) = u16_at(rest, 2) else {
            return false;
        };
        let end = 4 + usize::from(len);
        if end > rest.len() {
            return false;
        }
        rest = rest.get(end..).unwrap_or_default();
    }
    true
}

/// Stricter than Python: blocks must tile the field exactly, and the zip64 (0x0001) and
/// Info-ZIP Unicode Path (0x7075, which renames the member in Python) blocks are refused.
fn strict_extra_ok(extra: &[u8]) -> bool {
    let mut rest = extra;
    while !rest.is_empty() {
        let (Some(id), Some(len)) = (u16_at(rest, 0), u16_at(rest, 2)) else {
            return false;
        };
        if id == 0x0001 || id == 0x7075 {
            return false;
        }
        let Some(next) = rest.get(4 + usize::from(len)..) else {
            return false;
        };
        rest = next;
    }
    true
}

pub(crate) struct Member<'a> {
    raw_name: &'a [u8],
    /// The name as Python sees it (`ZipInfo.filename`).
    pub(crate) name: String,
    pub(crate) flags: u16,
    pub(crate) method: u16,
    crc: u32,
    csize: u32,
    /// `ZipInfo.file_size`.
    pub(crate) usize: u32,
    disk_start: u16,
    offset: u32,
    extra: &'a [u8],
    /// `ZipInfo.external_attr`, whatever the "made by" system says (Python ignores it too).
    pub(crate) external_attr: u32,
}

/// Where a member's bytes live once its local header has been checked.
pub(crate) struct Located {
    data_start: usize,
    data_end: usize,
}

/// The archive's members as `zipfile.ZipFile(...)` lists them, plus the strict EOCD rules.
/// Returns (members, central directory offset).
pub(crate) fn directory(data: &[u8]) -> Result<(Vec<Member<'_>>, usize), Reject> {
    let start = data.len().saturating_sub(EOCD_WINDOW);
    let end = rfind(data, EOCD_SIG, start).ok_or(Reject::Invalid)?;
    if data.len() < end + 22 {
        return Err(Reject::Invalid);
    }
    let f = |o| u16_at(data, end + o).ok_or(Reject::Invalid);
    let (disk, central_disk, disk_count, count) = (f(4)?, f(6)?, f(8)?, f(10)?);
    let size = u32_at(data, end + 12).ok_or(Reject::Invalid)? as usize;
    let offset = u32_at(data, end + 16).ok_or(Reject::Invalid)? as usize;
    let comment = f(20)?;
    // Stricter: a single disk; no archive comment (it can carry a second archive); no zip64
    // locator (zipfile would swap in 64-bit values); no bytes before the first member or between
    // the central directory and the EOCD (zipfile's "concat" shifting).
    if disk != 0
        || central_disk != 0
        || disk_count != count
        || comment != 0
        || end + 22 != data.len()
        || end
            .checked_sub(20)
            .and_then(|at| data.get(at..at + 4))
            .is_some_and(|sig| sig == ZIP64_LOCATOR_SIG)
        || offset.checked_add(size) != Some(end)
    {
        return Err(Reject::Invalid);
    }
    let cd = data.get(offset..end).ok_or(Reject::Invalid)?;
    let mut members = Vec::new();
    let mut pos = 0usize;
    while pos < size {
        if members.len() >= MAX_PARSED_MEMBERS {
            return Err(Reject::Invalid);
        }
        let e = cd.get(pos..).ok_or(Reject::Invalid)?;
        if e.len() < 46 || e.get(..4) != Some(CENTRAL_SIG) {
            return Err(Reject::Invalid);
        }
        let g16 = |o| u16_at(e, o).ok_or(Reject::Invalid);
        let g32 = |o| u32_at(e, o).ok_or(Reject::Invalid);
        let (nlen, elen, clen) = (
            usize::from(g16(28)?),
            usize::from(g16(30)?),
            usize::from(g16(32)?),
        );
        // Stricter: name, extra and comment must fit inside the directory.
        let raw_name = e.get(46..46 + nlen).ok_or(Reject::Invalid)?;
        let extra = e.get(46 + nlen..46 + nlen + elen).ok_or(Reject::Invalid)?;
        e.get(46 + nlen + elen..46 + nlen + elen + clen)
            .ok_or(Reject::Invalid)?;
        let flags = g16(8)?;
        let name = python_name(raw_name, flags).ok_or(Reject::Invalid)?;
        // Python raises NotImplementedError for a newer extract version (noevia#982: now
        // answered as an invalid archive, not a server error).
        if e.get(6).copied().ok_or(Reject::Invalid)? > MAX_EXTRACT_VERSION
            || !python_extra_ok(extra)
        {
            return Err(Reject::Invalid);
        }
        members.push(Member {
            raw_name,
            name,
            flags,
            method: g16(10)?,
            crc: g32(16)?,
            csize: g32(20)?,
            usize: g32(24)?,
            disk_start: g16(34)?,
            external_attr: g32(38)?,
            offset: g32(42)?,
            extra,
        });
        pos += 46 + nlen + elen + clen;
    }
    // Stricter: the directory lists exactly the EOCD's member count.
    if members.len() != usize::from(count) {
        return Err(Reject::Invalid);
    }
    Ok((members, offset))
}

/// Stricter than Python, which only reads the local header of the members it opens: every
/// member's local header must agree with its central entry (signature, version, flags, method,
/// name bytes, CRC and sizes or its data descriptor), names must be non-empty and NUL-free
/// (Python silently truncates at NUL), sizes and offsets must not be the zip64 marker, and the
/// members must tile the file from byte 0 to the central directory with no gaps or overlaps
/// (no overlapped-entry "quines").
pub(crate) fn locate_all(
    data: &[u8],
    members: &[Member<'_>],
    cd_offset: usize,
) -> Result<Vec<Located>, Reject> {
    let mut order: Vec<usize> = (0..members.len()).collect();
    order.sort_by_key(|&i| members.get(i).map_or(0, |m| m.offset));
    let mut located: Vec<Option<Located>> = (0..members.len()).map(|_| None).collect();
    let mut expected = 0usize;
    for i in order {
        let m = members.get(i).ok_or(Reject::Invalid)?;
        let ok_name = !m.raw_name.is_empty() && !m.raw_name.contains(&0);
        if m.flags & !FLAGS_ALLOWED != 0
            || (m.method != METHOD_STORED && m.method != METHOD_DEFLATED)
            || m.csize == u32::MAX
            || m.usize == u32::MAX
            || m.offset == u32::MAX
            || m.disk_start != 0
            || !ok_name
            || !strict_extra_ok(m.extra)
            || m.offset as usize != expected
        {
            return Err(Reject::Invalid);
        }
        let at = expected;
        let h = data.get(at..at + 30).ok_or(Reject::Invalid)?;
        let g16 = |o| u16_at(h, o).ok_or(Reject::Invalid);
        let g32 = |o| u32_at(h, o).ok_or(Reject::Invalid);
        if h.get(..4) != Some(LOCAL_SIG)
            || h.get(4).copied().ok_or(Reject::Invalid)? > MAX_EXTRACT_VERSION
            || g16(6)? != m.flags
            || g16(8)? != m.method
        {
            return Err(Reject::Invalid);
        }
        let (nlen, elen) = (usize::from(g16(26)?), usize::from(g16(28)?));
        let name = data.get(at + 30..at + 30 + nlen).ok_or(Reject::Invalid)?;
        let extra = data
            .get(at + 30 + nlen..at + 30 + nlen + elen)
            .ok_or(Reject::Invalid)?;
        if name != m.raw_name || !strict_extra_ok(extra) {
            return Err(Reject::Invalid);
        }
        let (lcrc, lcsize, lusize) = (g32(14)?, g32(18)?, g32(22)?);
        let data_start = at + 30 + nlen + elen;
        let data_end = data_start
            .checked_add(m.csize as usize)
            .filter(|&e| e <= cd_offset)
            .ok_or(Reject::Invalid)?;
        let mut next = data_end;
        if m.flags & FLAG_DESCRIPTOR != 0 {
            let zero_or = |v: u32, want: u32| v == 0 || v == want;
            if !(zero_or(lcrc, m.crc) && zero_or(lcsize, m.csize) && zero_or(lusize, m.usize)) {
                return Err(Reject::Invalid);
            }
            let d = data.get(data_end..).ok_or(Reject::Invalid)?;
            let body = if d.get(..4) == Some(DESCRIPTOR_SIG) {
                next += 16;
                d.get(4..16)
            } else {
                next += 12;
                d.get(..12)
            }
            .ok_or(Reject::Invalid)?;
            if u32_at(body, 0) != Some(m.crc)
                || u32_at(body, 4) != Some(m.csize)
                || u32_at(body, 8) != Some(m.usize)
            {
                return Err(Reject::Invalid);
            }
        } else if lcrc != m.crc || lcsize != m.csize || lusize != m.usize {
            return Err(Reject::Invalid);
        }
        if next > cd_offset {
            return Err(Reject::Invalid);
        }
        *located.get_mut(i).ok_or(Reject::Invalid)? = Some(Located {
            data_start,
            data_end,
        });
        expected = next;
    }
    if expected != cd_offset {
        return Err(Reject::Invalid);
    }
    located
        .into_iter()
        .map(|l| l.ok_or(Reject::Invalid))
        .collect()
}

/// A member's bytes, read exactly: a stored member's sizes agree; a DEFLATE stream ends exactly
/// at the compressed size and yields exactly the declared size (never more: the output buffer
/// is the declared size, so a lying header cannot expand past it); the CRC-32 matches.
pub(crate) fn read(data: &[u8], loc: &Located, m: &Member<'_>) -> Result<Vec<u8>, Reject> {
    let raw = data
        .get(loc.data_start..loc.data_end)
        .ok_or(Reject::Invalid)?;
    let out = match m.method {
        METHOD_STORED => {
            if m.csize != m.usize {
                return Err(Reject::Invalid);
            }
            raw.to_vec()
        }
        METHOD_DEFLATED => {
            use miniz_oxide::inflate::core::{decompress, inflate_flags, DecompressorOxide};
            use miniz_oxide::inflate::TINFLStatus;
            // One spare byte: miniz may need room past the last byte before it reads the final
            // end-of-block code; a stream producing more than declared fills it and is refused.
            let want = m.usize as usize;
            let mut out = vec![0u8; want + 1];
            let mut state = Box::<DecompressorOxide>::default();
            let (status, consumed, written) = decompress(
                &mut state,
                raw,
                &mut out,
                0,
                inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
            );
            if status != TINFLStatus::Done || consumed != raw.len() || written != want {
                return Err(Reject::Invalid);
            }
            out.truncate(want);
            out
        }
        _ => return Err(Reject::Invalid),
    };
    if crate::crc::crc32(&out) != m.crc {
        return Err(Reject::Invalid);
    }
    Ok(out)
}
