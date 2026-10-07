//! The ZIP half of `extract_docx`: Python's end-of-central-directory pre-check, the checks
//! `zipfile` makes while reading the central directory, the member / size / encryption / ratio
//! limits, then (stricter than Python, see the crate docs) a full agreement check of every
//! local header with the central directory before `word/document.xml` is inflated.

use crate::Refusal;

const EOCD_SIG: &[u8; 4] = b"PK\x05\x06";
const ZIP64_LOCATOR_SIG: &[u8; 4] = b"PK\x06\x07";
const CENTRAL_SIG: &[u8; 4] = b"PK\x01\x02";
const LOCAL_SIG: &[u8; 4] = b"PK\x03\x04";
const DESCRIPTOR_SIG: &[u8; 4] = b"PK\x07\x08";
/// zipfile's `max(0, len - 65557)` search window: the largest comment plus the record.
const EOCD_WINDOW: usize = 65_535 + 22;
/// zipfile.MAX_EXTRACT_VERSION.
const MAX_EXTRACT_VERSION: u8 = 63;
const FLAG_ENCRYPTED: u16 = 0x0001;
const FLAG_DESCRIPTOR: u16 = 0x0008;
const FLAG_UTF8: u16 = 0x0800;
/// Flag bits a member may carry: encryption (refused separately, with Python's message),
/// the two DEFLATE option bits, data descriptor, UTF-8 names. Anything else (patched data,
/// strong encryption, masked headers, reserved bits) is refused.
const FLAGS_ALLOWED: u16 = FLAG_ENCRYPTED | 0x0002 | 0x0004 | FLAG_DESCRIPTOR | FLAG_UTF8;
const METHOD_STORED: u16 = 0;
const METHOD_DEFLATED: u16 = 8;
pub(crate) const DOCUMENT: &str = "word/document.xml";

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

fn u8_at(d: &[u8], at: usize) -> Option<u8> {
    d.get(at).copied()
}

/// `bytes.rfind(needle, start)`: the highest index >= start where needle occurs entirely.
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

/// One extra field, checked like `ZipInfo._decodeExtra`: a block running past the end is
/// corrupt. (Python ignores 1-3 trailing bytes; `strict_extra_ok` refuses them.)
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
        let end = 4 + usize::from(len);
        let Some(next) = rest.get(end..) else {
            return false;
        };
        rest = next;
    }
    true
}

struct Member<'a> {
    raw_name: &'a [u8],
    name: String,
    flags: u16,
    method: u16,
    crc: u32,
    csize: u32,
    usize: u32,
    disk_start: u16,
    offset: u32,
    extra: &'a [u8],
}

/// Where a member's bytes live once its local header has been checked.
struct Located {
    data_start: usize,
    data_end: usize,
}

/// Python's EOCD pre-check, verbatim, then the strict additions. Returns (central directory
/// offset, size, member count, EOCD position).
fn end_record(data: &[u8]) -> Result<(usize, usize, usize, usize), Refusal> {
    let start = data.len().saturating_sub(EOCD_WINDOW);
    let end = rfind(data, EOCD_SIG, start).ok_or(Refusal::Container)?;
    if data.len() < end + 22 {
        return Err(Refusal::Container);
    }
    let f = |o| u16_at(data, end + o).ok_or(Refusal::Container);
    let (disk, central_disk, disk_count, count) = (f(4)?, f(6)?, f(8)?, f(10)?);
    let size = u32_at(data, end + 12).ok_or(Refusal::Container)?;
    let offset = u32_at(data, end + 16).ok_or(Refusal::Container)?;
    let comment = f(20)?;
    if disk != 0
        || central_disk != 0
        || disk_count != count
        || usize::from(count) > crate::MAX_MEMBERS
        || u64::from(size) > crate::MAX_CENTRAL_DIR
        || u64::from(offset) + u64::from(size) > end as u64
        || end + 22 + usize::from(comment) != data.len()
    {
        return Err(Refusal::Limits);
    }
    // Stricter: zipfile would honour a zip64 locator here and swap in 64-bit values; an archive
    // comment can carry a second archive; data before the first member ("concat" in zipfile)
    // shifts every offset. None of these occur in a DOCX writer's output.
    if comment != 0
        || end
            .checked_sub(20)
            .and_then(|at| data.get(at..at + 4))
            .is_some_and(|sig| sig == ZIP64_LOCATOR_SIG)
        || u64::from(offset) + u64::from(size) != end as u64
    {
        return Err(Refusal::Container);
    }
    Ok((offset as usize, size as usize, usize::from(count), end))
}

/// The central directory as `ZipFile._RealGetContents` reads it.
fn central_directory(data: &[u8], offset: usize, size: usize) -> Result<Vec<Member<'_>>, Refusal> {
    let cd = data.get(offset..offset + size).ok_or(Refusal::Container)?;
    let mut members = Vec::new();
    let mut pos = 0usize;
    while pos < size {
        let e = cd.get(pos..).ok_or(Refusal::Container)?;
        if e.len() < 46 || e.get(..4) != Some(CENTRAL_SIG) {
            return Err(Refusal::Container);
        }
        let g16 = |o| u16_at(e, o).ok_or(Refusal::Container);
        let g32 = |o| u32_at(e, o).ok_or(Refusal::Container);
        let (nlen, elen, clen) = (
            usize::from(g16(28)?),
            usize::from(g16(30)?),
            usize::from(g16(32)?),
        );
        // Stricter: Python reads short (and goes on) when the name, extra or comment run past
        // the directory; here they must fit.
        let raw_name = e.get(46..46 + nlen).ok_or(Refusal::Container)?;
        let extra = e
            .get(46 + nlen..46 + nlen + elen)
            .ok_or(Refusal::Container)?;
        e.get(46 + nlen + elen..46 + nlen + elen + clen)
            .ok_or(Refusal::Container)?;
        let flags = g16(8)?;
        let name = python_name(raw_name, flags).ok_or(Refusal::Container)?;
        let extract_version = u8_at(e, 6).ok_or(Refusal::Container)?;
        if extract_version > MAX_EXTRACT_VERSION || !python_extra_ok(extra) {
            return Err(Refusal::Container);
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
            offset: g32(42)?,
            extra,
        });
        pos += 46 + nlen + elen + clen;
    }
    Ok(members)
}

/// Stricter than Python, which only reads the local header of the member it opens: every
/// member's local header must agree with its central entry, and the members (each with its
/// data descriptor, if any) must tile the file from byte 0 to the central directory with no
/// gaps or overlaps.
fn locate_all(
    data: &[u8],
    members: &[Member<'_>],
    cd_offset: usize,
) -> Result<Vec<Located>, Refusal> {
    let mut order: Vec<usize> = (0..members.len()).collect();
    order.sort_by_key(|&i| members.get(i).map_or(0, |m| m.offset));
    let mut located: Vec<Option<Located>> = (0..members.len()).map(|_| None).collect();
    let mut expected = 0usize;
    for i in order {
        let m = members.get(i).ok_or(Refusal::Container)?;
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
            return Err(Refusal::Container);
        }
        let at = expected;
        let h = data.get(at..at + 30).ok_or(Refusal::Container)?;
        let g16 = |o| u16_at(h, o).ok_or(Refusal::Container);
        let g32 = |o| u32_at(h, o).ok_or(Refusal::Container);
        if h.get(..4) != Some(LOCAL_SIG)
            || u8_at(h, 4).ok_or(Refusal::Container)? > MAX_EXTRACT_VERSION
            || g16(6)? != m.flags
            || g16(8)? != m.method
        {
            return Err(Refusal::Container);
        }
        let (nlen, elen) = (usize::from(g16(26)?), usize::from(g16(28)?));
        let name = data
            .get(at + 30..at + 30 + nlen)
            .ok_or(Refusal::Container)?;
        let extra = data
            .get(at + 30 + nlen..at + 30 + nlen + elen)
            .ok_or(Refusal::Container)?;
        if name != m.raw_name || !strict_extra_ok(extra) {
            return Err(Refusal::Container);
        }
        let (lcrc, lcsize, lusize) = (g32(14)?, g32(18)?, g32(22)?);
        let data_start = at + 30 + nlen + elen;
        let data_end = data_start
            .checked_add(m.csize as usize)
            .filter(|&e| e <= cd_offset)
            .ok_or(Refusal::Container)?;
        let mut next = data_end;
        if m.flags & FLAG_DESCRIPTOR != 0 {
            // Local fields are zero (streamed) or already the final values.
            let zero_or = |v: u32, want: u32| v == 0 || v == want;
            if !(zero_or(lcrc, m.crc) && zero_or(lcsize, m.csize) && zero_or(lusize, m.usize)) {
                return Err(Refusal::Container);
            }
            let d = data.get(data_end..).ok_or(Refusal::Container)?;
            let body = if d.get(..4) == Some(DESCRIPTOR_SIG) {
                next += 16;
                d.get(4..16)
            } else {
                next += 12;
                d.get(..12)
            }
            .ok_or(Refusal::Container)?;
            if u32_at(body, 0) != Some(m.crc)
                || u32_at(body, 4) != Some(m.csize)
                || u32_at(body, 8) != Some(m.usize)
            {
                return Err(Refusal::Container);
            }
        } else if lcrc != m.crc || lcsize != m.csize || lusize != m.usize {
            return Err(Refusal::Container);
        }
        if next > cd_offset {
            return Err(Refusal::Container);
        }
        *located.get_mut(i).ok_or(Refusal::Container)? = Some(Located {
            data_start,
            data_end,
        });
        expected = next;
    }
    if expected != cd_offset {
        return Err(Refusal::Container);
    }
    located
        .into_iter()
        .map(|l| l.ok_or(Refusal::Container))
        .collect()
}

/// Inflate (or copy) exactly: the stream must end exactly at the compressed size and produce
/// exactly the declared size, with the declared CRC-32.
fn read_member(raw: &[u8], m: &Member<'_>) -> Result<Vec<u8>, Refusal> {
    let out = match m.method {
        METHOD_STORED => {
            if m.csize != m.usize {
                return Err(Refusal::Container);
            }
            raw.to_vec()
        }
        METHOD_DEFLATED => {
            use miniz_oxide::inflate::core::{decompress, inflate_flags, DecompressorOxide};
            use miniz_oxide::inflate::TINFLStatus;
            let mut out = vec![0u8; m.usize as usize];
            let mut state = Box::<DecompressorOxide>::default();
            let (status, consumed, written) = decompress(
                &mut state,
                raw,
                &mut out,
                0,
                inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
            );
            if status != TINFLStatus::Done || consumed != raw.len() || written != out.len() {
                return Err(Refusal::Container);
            }
            out
        }
        _ => return Err(Refusal::Container),
    };
    if crate::crc::crc32(&out) != m.crc {
        return Err(Refusal::Container);
    }
    Ok(out)
}

/// `word/document.xml`'s bytes, after every container check `extract_docx` makes.
pub(crate) fn document_xml(data: &[u8]) -> Result<Vec<u8>, Refusal> {
    let (cd_offset, cd_size, count, _end) = end_record(data)?;
    let members = central_directory(data, cd_offset, cd_size)?;
    let mut names = std::collections::HashSet::new();
    if members.len() > crate::MAX_MEMBERS || !members.iter().all(|m| names.insert(m.name.as_str()))
    {
        return Err(Refusal::Members);
    }
    let total: u64 = members.iter().map(|m| u64::from(m.usize)).sum();
    if total > crate::MAX_TOTAL_UNCOMPRESSED
        || members.iter().any(|m| m.flags & FLAG_ENCRYPTED != 0)
    {
        return Err(Refusal::EncryptedOrOversized);
    }
    let index = members
        .iter()
        .position(|m| m.name == DOCUMENT)
        .ok_or(Refusal::Missing)?;
    let doc = members.get(index).ok_or(Refusal::Missing)?;
    let usize_ = u64::from(doc.usize);
    if usize_ > crate::XML_LIMIT as u64 || usize_ > u64::from(doc.csize.max(1)) * crate::MAX_RATIO {
        return Err(Refusal::Decompression);
    }
    // Stricter: the directory must list exactly the EOCD's member count.
    if members.len() != count {
        return Err(Refusal::Container);
    }
    let located = locate_all(data, &members, cd_offset)?;
    let loc = located.get(index).ok_or(Refusal::Container)?;
    let raw = data
        .get(loc.data_start..loc.data_end)
        .ok_or(Refusal::Container)?;
    let xml = read_member(raw, doc)?;
    if xml.len() > crate::XML_LIMIT {
        return Err(Refusal::XmlLimit);
    }
    Ok(xml)
}
