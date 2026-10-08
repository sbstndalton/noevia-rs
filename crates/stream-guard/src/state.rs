//! The validator's state between chunks, as bytes the caller holds (noevia-core keeps them in a
//! `Uint8Array` and passes them back with the next chunk; nothing stays in the module between
//! calls, so a dropped or re-instantiated module loses nothing).
//!
//! Version 1, little-endian, every length checked against what is left before anything is
//! allocated. A decoded state is checked against the schema it is used with (node ids, the
//! number of `required` flags, enum candidates) and against the validator's own invariants; any
//! mismatch is [`Error::State`], never a panic.

use crate::schema::{NodeId, Schema};
use crate::validator::{
    is_number_char, ArrAwait, Frame, ObjAwait, State, Token, Violation, LITERALS, MAX_DEPTH_CAP,
    MAX_GUARD_BYTES, REASONS,
};
use crate::Error;

const MAGIC: &[u8; 4] = b"SG1\0";

/// Largest encoded state accepted. Every unit a state holds comes from at most
/// [`MAX_GUARD_BYTES`] units of input, held at most four times over (an open key as a path
/// segment and as the current key, again in a violation's message and path), plus per-frame
/// overhead.
pub const MAX_STATE_BYTES: usize = 20 * 1024 * 1024;

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn len(&mut self, n: usize) {
        self.u32(u32::try_from(n).unwrap_or(u32::MAX));
    }
    fn units(&mut self, u: &[u16]) {
        self.len(u.len());
        for &c in u {
            self.0.extend_from_slice(&c.to_le_bytes());
        }
    }
    fn opt_units(&mut self, u: Option<&Vec<u16>>) {
        match u {
            Some(u) => {
                self.u8(1);
                self.units(u);
            }
            None => self.u8(0),
        }
    }
}

struct R<'a> {
    b: &'a [u8],
}

impl R<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Error> {
        let (head, rest) = self.b.split_at_checked(n).ok_or(Error::State)?;
        self.b = rest;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        self.take(1)?.first().copied().ok_or(Error::State)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        <[u8; N]>::try_from(self.take(N)?).map_err(|_| Error::State)
    }
    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.arr()?))
    }
    fn i64(&mut self) -> Result<i64, Error> {
        Ok(i64::from_le_bytes(self.arr()?))
    }
    fn flag(&mut self) -> Result<bool, Error> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::State),
        }
    }
    /// A count whose items take at least `each` bytes, checked against what is left.
    fn count(&mut self, each: usize) -> Result<usize, Error> {
        let n = self.u32()? as usize;
        if n.checked_mul(each).is_none_or(|need| need > self.b.len()) {
            return Err(Error::State);
        }
        Ok(n)
    }
    fn units(&mut self) -> Result<Vec<u16>, Error> {
        let n = self.count(2)?;
        let raw = self.take(n * 2)?;
        Ok(raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&p| u16::from_le_bytes(p))
            .collect())
    }
    fn opt_units(&mut self) -> Result<Option<Vec<u16>>, Error> {
        Ok(if self.flag()? {
            Some(self.units()?)
        } else {
            None
        })
    }
    fn node(&mut self, schema: &Schema) -> Result<NodeId, Error> {
        let id = self.u32()?;
        if (id as usize) < schema.len() {
            Ok(id)
        } else {
            Err(Error::State)
        }
    }
}

fn obj_await(a: ObjAwait) -> u8 {
    match a {
        ObjAwait::KeyOrClose => 0,
        ObjAwait::Key => 1,
        ObjAwait::Colon => 2,
        ObjAwait::Value => 3,
        ObjAwait::CommaOrClose => 4,
    }
}

fn arr_await(a: ArrAwait) -> u8 {
    match a {
        ArrAwait::ValueOrClose => 0,
        ArrAwait::Value => 1,
        ArrAwait::CommaOrClose => 2,
    }
}

impl State {
    /// The state as bytes (see the module docs).
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(MAGIC.to_vec());
        w.i64(self.max_depth);
        w.i64(self.max_bytes);
        w.u64(self.bytes_seen);
        w.u8(u8::from(self.done));
        match &self.violation {
            Some(v) => {
                w.u8(1);
                w.units(&v.message);
                w.units(&v.path);
                w.u8(v.reason.index());
            }
            None => w.u8(0),
        }
        match &self.token {
            None => w.u8(0),
            Some(Token::Str {
                key,
                node,
                seg,
                escape,
                unicode,
                chars,
                len,
                candidates,
            }) => {
                w.u8(1);
                w.u8(u8::from(*key));
                w.u32(*node);
                w.units(seg);
                w.u8(u8::from(*escape));
                match unicode {
                    Some((rem, acc)) => {
                        w.u8(*rem);
                        w.0.extend_from_slice(&acc.to_le_bytes());
                    }
                    None => w.u8(0),
                }
                w.opt_units(chars.as_ref());
                w.u64(*len);
                match candidates {
                    Some(c) => {
                        w.u8(1);
                        w.len(c.len());
                        for &i in c {
                            w.u32(i);
                        }
                    }
                    None => w.u8(0),
                }
            }
            Some(Token::Num { node, seg, text }) => {
                w.u8(2);
                w.u32(*node);
                w.units(seg);
                w.len(text.len());
                w.0.extend_from_slice(text);
            }
            Some(Token::Lit {
                node,
                seg,
                which,
                matched,
            }) => {
                w.u8(3);
                w.u32(*node);
                w.units(seg);
                w.u8(*which);
                w.u8(*matched);
            }
        }
        w.len(self.stack.len());
        for f in &self.stack {
            match f {
                Frame::Obj {
                    node,
                    seg,
                    awaiting,
                    current_key,
                    seen,
                } => {
                    w.u8(0);
                    w.u32(*node);
                    w.units(seg);
                    w.u8(obj_await(*awaiting));
                    w.opt_units(current_key.as_ref());
                    w.len(seen.len());
                    w.0.extend(seen.iter().map(|&s| u8::from(s)));
                }
                Frame::Arr {
                    node,
                    seg,
                    count,
                    awaiting,
                } => {
                    w.u8(1);
                    w.u32(*node);
                    w.units(seg);
                    w.u64(*count);
                    w.u8(arr_await(*awaiting));
                }
            }
        }
        w.0
    }

    /// Decode a state made by [`State::encode`] for the same schema.
    pub fn decode(bytes: &[u8], schema: &Schema) -> Result<State, Error> {
        if bytes.len() > MAX_STATE_BYTES {
            return Err(Error::TooLarge);
        }
        let mut r = R { b: bytes };
        if r.take(4)? != MAGIC {
            return Err(Error::State);
        }
        let max_depth = r.i64()?;
        let max_bytes = r.i64()?;
        if !(-MAX_DEPTH_CAP..=MAX_DEPTH_CAP).contains(&max_depth)
            || !(-MAX_GUARD_BYTES..=MAX_GUARD_BYTES).contains(&max_bytes)
        {
            return Err(Error::State);
        }
        let bytes_seen = r.u64()?;
        let done = r.flag()?;
        let violation = if r.flag()? {
            let message = r.units()?;
            let path = r.units()?;
            let reason = *REASONS.get(usize::from(r.u8()?)).ok_or(Error::State)?;
            Some(Violation {
                message,
                path,
                reason,
            })
        } else {
            None
        };
        let token = match r.u8()? {
            0 => None,
            1 => {
                let key = r.flag()?;
                let node = r.node(schema)?;
                let seg = r.units()?;
                let escape = r.flag()?;
                let unicode = match r.u8()? {
                    0 => None,
                    rem @ 1..=4 => Some((rem, u16::from_le_bytes(r.arr()?))),
                    _ => return Err(Error::State),
                };
                let chars = r.opt_units()?;
                let len = r.u64()?;
                let candidates = if r.flag()? {
                    let n = r.count(4)?;
                    let mut c = Vec::with_capacity(n);
                    for _ in 0..n {
                        c.push(r.u32()?);
                    }
                    Some(c)
                } else {
                    None
                };
                let n = schema.node(node);
                let enumed = n.enum_vals.is_some() && !key;
                if escape && unicode.is_some()
                    || chars.is_some() != (key || enumed)
                    || candidates.is_some() != enumed
                    || candidates
                        .as_ref()
                        .is_some_and(|c| c.iter().any(|&i| i as usize >= n.enum_strings.len()))
                    || chars.as_ref().is_some_and(|c| c.len() as u64 != len)
                    || key && !seg.is_empty()
                {
                    return Err(Error::State);
                }
                Some(Token::Str {
                    key,
                    node,
                    seg,
                    escape,
                    unicode,
                    chars,
                    len,
                    candidates,
                })
            }
            2 => {
                let node = r.node(schema)?;
                let seg = r.units()?;
                let n = r.count(1)?;
                let text = r.take(n)?.to_vec();
                if text.is_empty() || !text.iter().all(|&b| is_number_char(u16::from(b))) {
                    return Err(Error::State);
                }
                Some(Token::Num { node, seg, text })
            }
            3 => {
                let node = r.node(schema)?;
                let seg = r.units()?;
                let which = r.u8()?;
                let matched = r.u8()?;
                let full = LITERALS.get(usize::from(which)).ok_or(Error::State)?.len();
                if matched == 0 || usize::from(matched) >= full {
                    return Err(Error::State);
                }
                Some(Token::Lit {
                    node,
                    seg,
                    which,
                    matched,
                })
            }
            _ => return Err(Error::State),
        };
        let frames = r.count(1)?;
        if i64::try_from(frames).map_or(true, |f| f > max_depth.max(0)) {
            return Err(Error::State);
        }
        let mut stack = Vec::with_capacity(frames);
        for _ in 0..frames {
            stack.push(match r.u8()? {
                0 => {
                    let node = r.node(schema)?;
                    let seg = r.units()?;
                    let awaiting = match r.u8()? {
                        0 => ObjAwait::KeyOrClose,
                        1 => ObjAwait::Key,
                        2 => ObjAwait::Colon,
                        3 => ObjAwait::Value,
                        4 => ObjAwait::CommaOrClose,
                        _ => return Err(Error::State),
                    };
                    let current_key = r.opt_units()?;
                    let n = r.count(1)?;
                    let mut seen = Vec::with_capacity(n);
                    for _ in 0..n {
                        seen.push(match r.u8()? {
                            0 => false,
                            1 => true,
                            _ => return Err(Error::State),
                        });
                    }
                    let required = schema.node(node).required.as_ref().map_or(0, Vec::len);
                    let keyed = matches!(awaiting, ObjAwait::Colon | ObjAwait::Value);
                    if seen.len() != required || current_key.is_some() != keyed {
                        return Err(Error::State);
                    }
                    Frame::Obj {
                        node,
                        seg,
                        awaiting,
                        current_key,
                        seen,
                    }
                }
                1 => {
                    let node = r.node(schema)?;
                    let seg = r.units()?;
                    let count = r.u64()?;
                    let awaiting = match r.u8()? {
                        0 => ArrAwait::ValueOrClose,
                        1 => ArrAwait::Value,
                        2 => ArrAwait::CommaOrClose,
                        _ => return Err(Error::State),
                    };
                    Frame::Arr {
                        node,
                        seg,
                        count,
                        awaiting,
                    }
                }
                _ => return Err(Error::State),
            });
        }
        if !r.b.is_empty() {
            return Err(Error::State);
        }
        // Where a token can be: a key only directly in an object awaiting one; a value at the
        // root only before the document is done, and inside a container awaiting a value.
        let token_ok = match (&token, stack.last()) {
            (None, _) => true,
            (Some(Token::Str { key: true, .. }), Some(Frame::Obj { awaiting, .. })) => {
                matches!(awaiting, ObjAwait::KeyOrClose | ObjAwait::Key)
            }
            (Some(Token::Str { key: true, .. }), _) => false,
            (Some(_), None) => !done,
            (Some(_), Some(Frame::Obj { awaiting, .. })) => *awaiting == ObjAwait::Value,
            (Some(_), Some(Frame::Arr { awaiting, .. })) => {
                matches!(awaiting, ArrAwait::ValueOrClose | ArrAwait::Value)
            }
        };
        if !token_ok || done && (!stack.is_empty() || token.is_some()) {
            return Err(Error::State);
        }
        Ok(State {
            max_depth,
            max_bytes,
            bytes_seen,
            done,
            violation,
            token,
            stack,
        })
    }
}
