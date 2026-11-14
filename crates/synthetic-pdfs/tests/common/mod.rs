//! A reader for exactly the PDFs this crate writes (and the reference writes): objects in file
//! order, stream data located by `/Length`, the xref table and the trailer.

#![allow(dead_code)]

use std::io::Read;

#[derive(Debug, PartialEq, Eq)]
pub struct Object {
    pub number: u32,
    pub offset: usize,
    /// The dictionary (or whole body) with a FlateDecode stream's `/Length n` replaced by
    /// `/Length <inflated>`, so two encoders compare equal.
    pub head: Vec<u8>,
    /// Stream data, inflated when the dictionary says `/FlateDecode`.
    pub data: Option<Vec<u8>>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Parsed {
    pub preamble: Vec<u8>,
    pub objects: Vec<Object>,
    /// `trailer\n<< ... >>` with the numbers that depend on stream sizes left out.
    pub trailer: Vec<u8>,
    pub xref: Vec<(u32, usize)>,
    pub startxref: usize,
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

fn digits_at(hay: &[u8], at: usize) -> Option<(usize, usize)> {
    let len = hay
        .get(at..)?
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    let text = std::str::from_utf8(hay.get(at..at + len)?).ok()?;
    Some((text.parse().ok()?, at + len))
}

pub fn inflate(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .unwrap_or_else(|e| unreachable!("inflate: {e}"));
    out
}

pub fn parse(data: &[u8]) -> Option<Parsed> {
    let startxref_at = find(data, b"startxref\n", 0)?;
    let (startxref, _) = digits_at(data, startxref_at + b"startxref\n".len())?;
    let mut objects = Vec::new();
    let first = find(data, b"\n1 0 obj\n", 0).map(|p| p + 1)?;
    let preamble = data.get(..first)?.to_vec();
    let mut at = first;
    while at < startxref {
        let (number, after) = digits_at(data, at)?;
        let body_at = after + b" 0 obj\n".len();
        if data.get(after..body_at)? != b" 0 obj\n" {
            return None;
        }
        let number = u32::try_from(number).ok()?;
        let stream_at = find(data, b">>\nstream\n", body_at);
        let endobj_at = find(data, b"\nendobj\n", body_at)?;
        let (head, payload, end) = match stream_at {
            Some(s) if s < endobj_at => {
                let dict = data.get(body_at..s + 2)?;
                let length_at = find(dict, b"/Length ", 0)? + b"/Length ".len();
                let (length, length_end) = digits_at(dict, length_at)?;
                let start = s + b">>\nstream\n".len();
                let raw = data.get(start..start + length)?;
                if data.get(start + length..start + length + b"\nendstream\nendobj\n".len())?
                    != b"\nendstream\nendobj\n"
                {
                    return None;
                }
                let flate = find(dict, b"/FlateDecode", 0).is_some();
                let mut head = dict.get(..length_at)?.to_vec();
                head.extend_from_slice(if flate {
                    b"<inflated>"
                } else {
                    dict.get(length_at..length_end)?
                });
                head.extend_from_slice(dict.get(length_end..)?);
                let payload = if flate { inflate(raw) } else { raw.to_vec() };
                (
                    head,
                    Some(payload),
                    start + length + b"\nendstream\nendobj\n".len(),
                )
            }
            _ => (
                data.get(body_at..endobj_at)?.to_vec(),
                None,
                endobj_at + b"\nendobj\n".len(),
            ),
        };
        objects.push(Object {
            number,
            offset: at,
            head,
            data: payload,
        });
        at = end;
    }
    if at != startxref {
        return None;
    }
    let table = std::str::from_utf8(data.get(startxref..startxref_at)?).ok()?;
    let mut lines = table.lines();
    if lines.next()? != "xref" {
        return None;
    }
    let count: u32 = lines.next()?.strip_prefix("0 ")?.parse().ok()?;
    if lines.next()? != "0000000000 65535 f " {
        return None;
    }
    let mut xref = Vec::new();
    for n in 1..count {
        let line = lines.next()?;
        let offset = line.strip_suffix(" 00000 n ")?.parse().ok()?;
        xref.push((n, offset));
    }
    let trailer = lines.collect::<Vec<_>>().join("\n").into_bytes();
    if data.get(startxref_at + b"startxref\n".len()..)?
        != format!("{startxref}\n%%EOF\n").as_bytes()
    {
        return None;
    }
    Some(Parsed {
        preamble,
        objects,
        trailer,
        xref,
        startxref,
    })
}

/// Every xref entry points at `n 0 obj\n` of its own number (the reference test's check).
pub fn xref_points_at_objects(data: &[u8]) -> bool {
    let Some(parsed) = parse(data) else {
        return false;
    };
    parsed.xref.iter().all(|(n, offset)| {
        data.get(*offset..)
            .is_some_and(|rest| rest.starts_with(format!("{n} 0 obj\n").as_bytes()))
    })
}

/// Preamble, (number, head, data) per object, trailer, xref entry count.
pub type Semantic = (Vec<u8>, Vec<(u32, Vec<u8>, Option<Vec<u8>>)>, Vec<u8>, usize);

/// The view two encoders must agree on: everything except deflate bytes, their lengths and the
/// offsets those lengths move.
pub fn semantic(data: &[u8]) -> Option<Semantic> {
    let parsed = parse(data)?;
    let objects = parsed
        .objects
        .into_iter()
        .map(|o| (o.number, o.head, o.data))
        .collect::<Vec<_>>();
    Some((parsed.preamble, objects, parsed.trailer, parsed.xref.len()))
}
