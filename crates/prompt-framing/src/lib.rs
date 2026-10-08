//! noevia-core's prompt-injection boundary in Rust, byte-identical to the JS references over
//! UTF-16 code units (a JS string may hold lone surrogates; they pass through):
//!
//! - [`framing`]: `server/prompt-framing.cjs` (`frameUntrusted`, `escapeClosing`).
//! - [`provenance`]: `server/provenance-policy.cjs` (noevia#769): the taint store, `checkWrite`.
//! - [`packet`]: `server/task-packet.cjs` (noevia#740): `parsePacket`, `validatePacket`,
//!   `renderPacket`.
//!
//! The functions at the crate root are what `dav-parse.wasm` exposes: [`frame`] and [`escape`]
//! take UTF-16LE frames and reply with UTF-16LE units; [`provenance_call`] and [`packet_call`] take
//! a UTF-8 JSON request and reply with UTF-8 JSON (lone surrogates as `\udxxx`, as
//! `JSON.stringify` writes them). A refusal is [`Error`]; its code never carries input.
//!
//! Unicode tables: the casing comes from Rust's `std`, NFKC from `icu_normalizer` and IDNA from
//! `idna`. The JS follows its runtime's ICU (Unicode 17 in Node 22.23); a code point whose
//! mappings differ between the two versions would normalise differently. Each side only ever
//! compares text it normalised itself, so a difference changes which approval cards appear for
//! such text, never whether framing holds.

pub mod framing;
pub mod js;
pub mod json;
pub mod packet;
pub mod provenance;

use json::{push_ascii, push_str, Value};
use provenance::{Index, Store};

/// A refused request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The request does not have the expected shape.
    Input,
    /// The request is over its cap.
    TooLarge,
}

impl Error {
    /// The fixed public code.
    pub fn code(self) -> &'static str {
        match self {
            Error::Input => "input",
            Error::TooLarge => "too_large",
        }
    }

    /// `{"error":"code"}`.
    pub fn json(self) -> Vec<u8> {
        format!("{{\"error\":\"{}\"}}", self.code()).into_bytes()
    }
}

/// The longest text [`frame`] and [`escape`] take, in code units.
pub const MAX_TEXT_UNITS: usize = 8 * 1024 * 1024;
/// The longest kind or label [`frame`] takes, in code units.
pub const MAX_LABEL_UNITS: usize = 1024 * 1024;
/// The largest [`provenance_call`] request, in bytes.
pub const MAX_PROVENANCE_BYTES: usize = 24 * 1024 * 1024;
/// The largest [`packet_call`] request, in bytes.
pub const MAX_PACKET_BYTES: usize = 1024 * 1024;

fn u32_at(b: &[u8], at: usize) -> Option<usize> {
    let h: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(h) as usize)
}

fn units_le(b: &[u8]) -> Option<Vec<u16>> {
    let (pairs, rest) = b.as_chunks::<2>();
    rest.is_empty()
        .then(|| pairs.iter().map(|&p| u16::from_le_bytes(p)).collect())
}

/// `n` units at byte `at`, then where they end.
fn take_units(b: &[u8], at: usize, n: usize) -> Option<(Vec<u16>, usize)> {
    let end = at.checked_add(n.checked_mul(2)?)?;
    Some((units_le(b.get(at..end)?)?, end))
}

/// Code units as UTF-16LE bytes.
pub fn to_le(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// `frameUntrusted(kind, label, text)` on `u32le(n) kind u32le(m) label text` (UTF-16LE units).
pub fn frame(input: &[u8]) -> Result<Vec<u8>, Error> {
    let nk = u32_at(input, 0).ok_or(Error::Input)?;
    if nk > MAX_LABEL_UNITS {
        return Err(Error::TooLarge);
    }
    let (kind, at) = take_units(input, 4, nk).ok_or(Error::Input)?;
    let nl = u32_at(input, at).ok_or(Error::Input)?;
    if nl > MAX_LABEL_UNITS {
        return Err(Error::TooLarge);
    }
    let (label, at) = take_units(input, at + 4, nl).ok_or(Error::Input)?;
    let rest = input.get(at..).ok_or(Error::Input)?;
    if rest.len() / 2 > MAX_TEXT_UNITS {
        return Err(Error::TooLarge);
    }
    let text = units_le(rest).ok_or(Error::Input)?;
    Ok(to_le(&framing::frame_untrusted(&kind, &label, &text)))
}

/// `escapeClosing(text, tag)` on `u32le(n) tag text` (UTF-16LE units); the tag must be a
/// [`framing::valid_tag`].
pub fn escape(input: &[u8]) -> Result<Vec<u8>, Error> {
    let nt = u32_at(input, 0).ok_or(Error::Input)?;
    if nt > framing::MAX_TAG_UNITS {
        return Err(Error::Input);
    }
    let (tag, at) = take_units(input, 4, nt).ok_or(Error::Input)?;
    if !framing::valid_tag(&tag) {
        return Err(Error::Input);
    }
    let rest = input.get(at..).ok_or(Error::Input)?;
    if rest.len() / 2 > MAX_TEXT_UNITS {
        return Err(Error::TooLarge);
    }
    let text = units_le(rest).ok_or(Error::Input)?;
    Ok(to_le(&framing::escape_closing(&text, &tag)))
}

fn count(v: Option<&Value>, max: usize) -> Result<usize, Error> {
    match v {
        Some(Value::Num(n)) if n.fract() == 0.0 && *n >= 0.0 && *n <= max as f64 => Ok(*n as usize),
        _ => Err(Error::Input),
    }
}

fn string(v: Option<&Value>) -> Result<&[u16], Error> {
    v.and_then(Value::as_str).ok_or(Error::Input)
}

fn strings(v: Option<&Value>) -> Result<Vec<Vec<u16>>, Error> {
    let Some(Value::Arr(a)) = v else {
        return Err(Error::Input);
    };
    a.iter()
        .map(|x| x.as_str().map(<[u16]>::to_vec).ok_or(Error::Input))
        .collect()
}

/// A store from its JSON form (`{"maxChars","chars","saturated","sources","texts"}`), checked.
pub fn store_from(v: Option<&Value>) -> Result<Store, Error> {
    let v = v.ok_or(Error::Input)?;
    let Some(Value::Bool(saturated)) = v.get("saturated") else {
        return Err(Error::Input);
    };
    let Some(Value::Arr(texts)) = v.get("texts") else {
        return Err(Error::Input);
    };
    let texts = texts
        .iter()
        .map(|t| match t {
            Value::Arr(pair) => match pair.as_slice() {
                [at, Value::Str(s)] => Ok((count(Some(at), provenance::MAX_SOURCES)?, s.clone())),
                _ => Err(Error::Input),
            },
            _ => Err(Error::Input),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let store = Store {
        max_chars: count(v.get("maxChars"), provenance::MAX_MAX_CHARS)?,
        chars: count(v.get("chars"), provenance::MAX_MAX_CHARS)?,
        saturated: *saturated,
        sources: strings(v.get("sources"))?,
        texts,
    };
    store.check().map_err(|_| Error::Input)?;
    Ok(store)
}

/// A store's JSON form.
pub fn store_json(out: &mut Vec<u8>, s: &Store) {
    out.extend_from_slice(
        format!(
            "{{\"maxChars\":{},\"chars\":{},\"saturated\":{},\"sources\":",
            s.max_chars, s.chars, s.saturated
        )
        .as_bytes(),
    );
    push_list(out, &s.sources);
    out.extend_from_slice(b",\"texts\":[");
    for (i, (at, t)) in s.texts.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(format!("[{at},").as_bytes());
        push_str(out, t);
        out.push(b']');
    }
    out.extend_from_slice(b"]}");
}

fn push_list(out: &mut Vec<u8>, items: &[Vec<u16>]) {
    out.push(b'[');
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        push_str(out, s);
    }
    out.push(b']');
}

fn store_reply(s: &Store) -> Vec<u8> {
    let st = s.stats();
    let mut out = b"{\"state\":".to_vec();
    store_json(&mut out, s);
    out.extend_from_slice(
        format!(
            ",\"stats\":{{\"chars\":{},\"grams\":{},\"sources\":{},\"saturated\":{}}}}}",
            st.chars, st.grams, st.sources, st.saturated
        )
        .as_bytes(),
    );
    out
}

fn op_is(v: &Value, name: &str) -> bool {
    v.get("op")
        .and_then(Value::as_str)
        .is_some_and(|o| o.iter().copied().eq(name.encode_utf16()))
}

/// The request depth: a store's texts are `[[at, "text"]]` under `state` (depth 3).
const REQUEST_CAP: usize = 4;

/// The provenance policy (noevia#769) on one UTF-8 JSON request, by `op`:
///
/// - `ingest` `{state, contents: [text]}` / `add` `{state, source, text}`: the new
///   `{"state":…,"stats":{"chars","grams","sources","saturated"}}`.
/// - `source` `{state, value}`: `{"source":…|null}` (`sourceOf`).
/// - `check` `{state, args, object}`: `{"found":[{"field","source"}]}` or `{"unchecked":true}`
///   (`checkWrite`; `args` is the call's argument text, or with `object` the JSON text of the
///   object a caller passed).
/// - `normalise` `{value}`: `{"value":…}`; `key` `{key}`: `{"sensitive":bool}`;
///   `candidates` `{value}`: `{"candidates":[…]}`; `blocks` `{content}`:
///   `{"blocks":[[kind, label|null, body]]}`.
/// - `new` `{maxChars}`: an empty store's state and stats.
pub fn provenance_call(input: &[u8]) -> Result<Vec<u8>, Error> {
    if input.len() > MAX_PROVENANCE_BYTES {
        return Err(Error::TooLarge);
    }
    let req = json::parse_utf8(input, REQUEST_CAP).ok_or(Error::Input)?;
    let mut out = Vec::new();
    if op_is(&req, "new") {
        let store = Store::new(count(req.get("maxChars"), provenance::MAX_MAX_CHARS)?);
        return Ok(store_reply(&store));
    }
    if op_is(&req, "ingest") {
        let mut store = store_from(req.get("state"))?;
        store.ingest(&strings(req.get("contents"))?);
        return Ok(store_reply(&store));
    }
    if op_is(&req, "add") {
        let mut store = store_from(req.get("state"))?;
        store.add(string(req.get("source"))?, string(req.get("text"))?);
        return Ok(store_reply(&store));
    }
    if op_is(&req, "source") {
        let store = store_from(req.get("state"))?;
        let index: Index<'_> = store.index();
        out.extend_from_slice(b"{\"source\":");
        match index.source_of(string(req.get("value"))?) {
            Some(s) => push_str(&mut out, &s),
            None => out.extend_from_slice(b"null"),
        }
        out.push(b'}');
        return Ok(out);
    }
    if op_is(&req, "check") {
        let store = store_from(req.get("state"))?;
        let Some(Value::Bool(object)) = req.get("object") else {
            return Err(Error::Input);
        };
        let args = string(req.get("args"))?;
        let result = provenance::parse_args(args, *object)
            .and_then(|a| provenance::check_write(&store.index(), &a));
        match result {
            Err(provenance::Unchecked) => out.extend_from_slice(b"{\"unchecked\":true}"),
            Ok(hits) => {
                out.extend_from_slice(b"{\"found\":[");
                for (i, h) in hits.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    out.extend_from_slice(b"{\"field\":");
                    push_str(&mut out, &h.field);
                    out.extend_from_slice(b",\"source\":");
                    push_str(&mut out, &h.source);
                    out.push(b'}');
                }
                out.extend_from_slice(b"]}");
            }
        }
        return Ok(out);
    }
    if op_is(&req, "normalise") {
        out.extend_from_slice(b"{\"value\":");
        push_str(&mut out, &provenance::normalise(string(req.get("value"))?));
        out.push(b'}');
        return Ok(out);
    }
    if op_is(&req, "key") {
        let s = provenance::is_sensitive_key(string(req.get("key"))?);
        return Ok(format!("{{\"sensitive\":{s}}}").into_bytes());
    }
    if op_is(&req, "candidates") {
        out.extend_from_slice(b"{\"candidates\":");
        push_list(&mut out, &provenance::candidates(string(req.get("value"))?));
        out.push(b'}');
        return Ok(out);
    }
    if op_is(&req, "blocks") {
        out.extend_from_slice(b"{\"blocks\":[");
        for (i, (k, l, b)) in provenance::framed_blocks(string(req.get("content"))?)
            .iter()
            .enumerate()
        {
            if i > 0 {
                out.push(b',');
            }
            out.push(b'[');
            push_str(&mut out, k);
            out.push(b',');
            match l {
                Some(l) => push_str(&mut out, l),
                None => out.extend_from_slice(b"null"),
            }
            out.push(b',');
            push_str(&mut out, b);
            out.push(b']');
        }
        out.extend_from_slice(b"]}");
        return Ok(out);
    }
    Err(Error::Input)
}

/// A validated packet's reply: `{"ok":true,"packet":…}`.
fn packet_ok(p: &packet::Packet) -> Vec<u8> {
    let mut out = b"{\"ok\":true,\"packet\":".to_vec();
    out.extend(packet::packet_json(p));
    out.push(b'}');
    out
}

/// The task packet (noevia#740) on one UTF-8 JSON request, by `op`:
///
/// - `parse` `{output}`: `parsePacket`: `{"ok":true,"packet":…}` or
///   `{"ok":false,"reason":"invalid-json"|"schema","error":"…"}`.
/// - `validate` `{packet}`: `validatePacket` on the decoded value: `{"ok":true,"packet":…}` or
///   `{"ok":false,"error":"…"}`.
/// - `render` `{packet, label}`: `renderPacket` of a packet as `parse` replies it:
///   `{"text":"…"}`; anything not of that shape is refused.
pub fn packet_call(input: &[u8]) -> Result<Vec<u8>, Error> {
    if input.len() > MAX_PACKET_BYTES {
        return Err(Error::TooLarge);
    }
    // The packet sits at depth 1; the deepest value validation reads is depth 4 within it.
    let req = json::parse_utf8(input, 8).ok_or(Error::Input)?;
    if op_is(&req, "parse") {
        return Ok(match packet::parse(string(req.get("output"))?) {
            Ok(p) => packet_ok(&p),
            Err(r) => {
                let (reason, error) = match r {
                    packet::Refusal::InvalidJson(e) => ("invalid-json", js::units(e)),
                    packet::Refusal::Schema(e) => ("schema", e),
                };
                let mut out = b"{\"ok\":false,\"reason\":".to_vec();
                push_ascii(&mut out, reason);
                out.extend_from_slice(b",\"error\":");
                push_str(&mut out, &error);
                out.push(b'}');
                out
            }
        });
    }
    if op_is(&req, "validate") {
        let raw = req.get("packet").ok_or(Error::Input)?;
        return Ok(match packet::validate(raw) {
            Ok(p) => packet_ok(&p),
            Err(e) => {
                let mut out = b"{\"ok\":false,\"error\":".to_vec();
                push_str(&mut out, &e);
                out.push(b'}');
                out
            }
        });
    }
    if op_is(&req, "render") {
        let p = req
            .get("packet")
            .and_then(packet::from_value)
            .ok_or(Error::Input)?;
        let label = string(req.get("label"))?;
        let mut out = b"{\"text\":".to_vec();
        push_str(&mut out, &packet::render(&p, label));
        out.push(b'}');
        return Ok(out);
    }
    Err(Error::Input)
}
