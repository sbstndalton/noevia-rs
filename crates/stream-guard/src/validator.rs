//! stream-guard.cjs `IncrementalValidator`, step for step, over UTF-16 code units (`text[i]` in
//! the JS). Every message is the JS's, unit for unit.

use crate::schema::{
    units, EnumVal, NodeId, Schema, EMPTY, T_ARRAY, T_BOOLEAN, T_NULL, T_NUMBER, T_OBJECT, T_STRING,
};
use crate::Error;

/// stream-guard.cjs DEFAULT_MAX_DEPTH.
pub const DEFAULT_MAX_DEPTH: i64 = 64;
/// stream-guard.cjs DEFAULT_MAX_BYTES (2 MiB).
pub const DEFAULT_MAX_BYTES: i64 = 2 * 1024 * 1024;
/// Largest `maxBytes` this port takes (every caller uses at most the default). A larger one is
/// refused ([`Error::Options`]), which bounds the state.
pub const MAX_GUARD_BYTES: i64 = DEFAULT_MAX_BYTES;
/// Largest `maxDepth` this port takes.
pub const MAX_DEPTH_CAP: i64 = 1024;

/// Which rule a violation broke. The JS has only the message; this names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    MaxBytes,
    UnexpectedChar,
    TypeMismatch,
    MaxDepth,
    ExpectedKey,
    ExpectedColon,
    ExpectedCommaOrBrace,
    ExpectedCommaOrBracket,
    UnknownProperty,
    MissingRequired,
    MaxItems,
    InvalidUnicodeEscape,
    InvalidEscape,
    MaxLength,
    EnumPrefix,
    EnumString,
    InvalidNumber,
    NotInteger,
    EnumNumber,
    InvalidLiteral,
    EnumLiteral,
    UnterminatedString,
    UnterminatedLiteral,
    UnterminatedContainer,
    NoValue,
}

pub(crate) const REASONS: [Reason; 25] = [
    Reason::MaxBytes,
    Reason::UnexpectedChar,
    Reason::TypeMismatch,
    Reason::MaxDepth,
    Reason::ExpectedKey,
    Reason::ExpectedColon,
    Reason::ExpectedCommaOrBrace,
    Reason::ExpectedCommaOrBracket,
    Reason::UnknownProperty,
    Reason::MissingRequired,
    Reason::MaxItems,
    Reason::InvalidUnicodeEscape,
    Reason::InvalidEscape,
    Reason::MaxLength,
    Reason::EnumPrefix,
    Reason::EnumString,
    Reason::InvalidNumber,
    Reason::NotInteger,
    Reason::EnumNumber,
    Reason::InvalidLiteral,
    Reason::EnumLiteral,
    Reason::UnterminatedString,
    Reason::UnterminatedLiteral,
    Reason::UnterminatedContainer,
    Reason::NoValue,
];

impl Reason {
    pub fn code(self) -> &'static str {
        match self {
            Reason::MaxBytes => "max_bytes",
            Reason::UnexpectedChar => "unexpected_char",
            Reason::TypeMismatch => "type_mismatch",
            Reason::MaxDepth => "max_depth",
            Reason::ExpectedKey => "expected_key",
            Reason::ExpectedColon => "expected_colon",
            Reason::ExpectedCommaOrBrace => "expected_comma_or_brace",
            Reason::ExpectedCommaOrBracket => "expected_comma_or_bracket",
            Reason::UnknownProperty => "unknown_property",
            Reason::MissingRequired => "missing_required",
            Reason::MaxItems => "max_items",
            Reason::InvalidUnicodeEscape => "invalid_unicode_escape",
            Reason::InvalidEscape => "invalid_escape",
            Reason::MaxLength => "max_length",
            Reason::EnumPrefix => "enum_prefix",
            Reason::EnumString => "enum_string",
            Reason::InvalidNumber => "invalid_number",
            Reason::NotInteger => "not_integer",
            Reason::EnumNumber => "enum_number",
            Reason::InvalidLiteral => "invalid_literal",
            Reason::EnumLiteral => "enum_literal",
            Reason::UnterminatedString => "unterminated_string",
            Reason::UnterminatedLiteral => "unterminated_literal",
            Reason::UnterminatedContainer => "unterminated_container",
            Reason::NoValue => "no_value",
        }
    }

    pub(crate) fn index(self) -> u8 {
        REASONS
            .iter()
            .position(|r| *r == self)
            .and_then(|i| u8::try_from(i).ok())
            .unwrap_or(0)
    }
}

/// The first violation: the JS SchemaViolation's `message` and `path`, as UTF-16 code units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub message: Vec<u16>,
    pub path: Vec<u16>,
    pub reason: Reason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjAwait {
    KeyOrClose,
    Key,
    Colon,
    Value,
    CommaOrClose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArrAwait {
    ValueOrClose,
    Value,
    CommaOrClose,
}

/// An open container. `seg` is this frame's part of its path: `$` for the root, `.key` or `[n]`
/// below; the full path is the concatenation down the stack (so the state stays linear).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Frame {
    Obj {
        node: NodeId,
        seg: Vec<u16>,
        awaiting: ObjAwait,
        current_key: Option<Vec<u16>>,
        /// One flag per `required` name: seen as a completed member.
        seen: Vec<bool>,
    },
    Arr {
        node: NodeId,
        seg: Vec<u16>,
        count: u64,
        awaiting: ArrAwait,
    },
}

/// The active token. `seg` is the token's path past the stack's path (empty for a key, whose
/// path is its object's).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Token {
    Str {
        key: bool,
        node: NodeId,
        seg: Vec<u16>,
        escape: bool,
        /// `\u` digits still expected and the value so far.
        unicode: Option<(u8, u16)>,
        /// The text so far, kept for a key or an enum string (where a message quotes it).
        chars: Option<Vec<u16>>,
        /// The text's length in units (`t.chars.length`).
        len: u64,
        /// Indices into the node's string enum members still matching the prefix.
        candidates: Option<Vec<u32>>,
    },
    Num {
        node: NodeId,
        seg: Vec<u16>,
        text: Vec<u8>,
    },
    Lit {
        node: NodeId,
        seg: Vec<u16>,
        /// 0 true, 1 false, 2 null.
        which: u8,
        matched: u8,
    },
}

pub(crate) const LITERALS: [&str; 3] = ["true", "false", "null"];

/// The validator's whole state between chunks.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub(crate) max_depth: i64,
    pub(crate) max_bytes: i64,
    pub(crate) bytes_seen: u64,
    pub(crate) done: bool,
    pub(crate) violation: Option<Violation>,
    pub(crate) token: Option<Token>,
    pub(crate) stack: Vec<Frame>,
}

/// A validator over one schema: `new IncrementalValidator(schema, { maxDepth, maxBytes })`.
pub struct Validator<'s> {
    schema: &'s Schema,
    pub(crate) st: State,
}

/// `/\s/` on one UTF-16 unit (the whitespace the JS skips between tokens).
pub fn is_ws(c: u16) -> bool {
    matches!(
        c,
        0x09..=0x0d
            | 0x20
            | 0xa0
            | 0x1680
            | 0x2000..=0x200a
            | 0x2028
            | 0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    )
}

/// `/[0-9eE+\-.]/`.
pub(crate) fn is_number_char(c: u16) -> bool {
    matches!(c, 0x30..=0x39 | 0x65 | 0x45 | 0x2b | 0x2d | 0x2e)
}

fn is_hex(c: u16) -> bool {
    matches!(c, 0x30..=0x39 | 0x41..=0x46 | 0x61..=0x66)
}

/// `Buffer.byteLength(text, 'utf8')`: a lone surrogate counts as U+FFFD (3 bytes).
pub fn utf8_len(text: &[u16]) -> u64 {
    let mut n: u64 = 0;
    let mut i = 0;
    while let Some(&c) = text.get(i) {
        let next = text.get(i + 1).copied();
        let (bytes, step) = match c {
            0..=0x7f => (1, 1),
            0x80..=0x7ff => (2, 1),
            0xd800..=0xdbff if matches!(next, Some(0xdc00..=0xdfff)) => (4, 2),
            _ => (3, 1),
        };
        n = n.saturating_add(bytes);
        i += step;
    }
    n
}

/// `/^-?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?$/`; with `integer`, `/^-?(0|[1-9]\d*)$/`.
fn number_ok(t: &[u8], integer: bool) -> bool {
    let mut i = 0;
    let at = |i: usize| t.get(i).copied();
    if at(i) == Some(b'-') {
        i += 1;
    }
    match at(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            while matches!(at(i), Some(b'0'..=b'9')) {
                i += 1;
            }
        }
        _ => return false,
    }
    if integer {
        return i == t.len();
    }
    if at(i) == Some(b'.') {
        i += 1;
        let s = i;
        while matches!(at(i), Some(b'0'..=b'9')) {
            i += 1;
        }
        if i == s {
            return false;
        }
    }
    if matches!(at(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(at(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let s = i;
        while matches!(at(i), Some(b'0'..=b'9')) {
            i += 1;
        }
        if i == s {
            return false;
        }
    }
    i == t.len()
}

/// The decimal digits of `n`.
fn num_units(n: impl std::fmt::Display) -> Vec<u16> {
    units(&n.to_string())
}

struct Msg(Vec<u16>);

impl Msg {
    fn new() -> Self {
        Msg(Vec::new())
    }
    fn s(mut self, s: &str) -> Self {
        self.0.extend(s.encode_utf16());
        self
    }
    fn u(mut self, u: &[u16]) -> Self {
        self.0.extend_from_slice(u);
        self
    }
}

impl State {
    /// A fresh state; `max_depth` and `max_bytes` as the JS options (integers; see the caps).
    pub fn new(max_depth: i64, max_bytes: i64) -> Result<State, Error> {
        if !(-MAX_DEPTH_CAP..=MAX_DEPTH_CAP).contains(&max_depth)
            || !(-MAX_GUARD_BYTES..=MAX_GUARD_BYTES).contains(&max_bytes)
        {
            return Err(Error::Options);
        }
        Ok(State {
            max_depth,
            max_bytes,
            bytes_seen: 0,
            done: false,
            violation: None,
            token: None,
            stack: Vec::new(),
        })
    }

    pub fn violation(&self) -> Option<&Violation> {
        self.violation.as_ref()
    }

    pub fn is_done(&self) -> bool {
        self.done
    }
}

impl<'s> Validator<'s> {
    pub fn new(schema: &'s Schema, state: State) -> Self {
        Validator { schema, st: state }
    }

    pub fn state(&self) -> &State {
        &self.st
    }

    pub fn into_state(self) -> State {
        self.st
    }

    pub fn violation(&self) -> Option<&Violation> {
        self.st.violation.as_ref()
    }

    fn fail(&mut self, message: Msg, path: Vec<u16>, reason: Reason) {
        if self.st.violation.is_none() {
            self.st.violation = Some(Violation {
                message: message.0,
                path,
                reason,
            });
        }
    }

    /// The path of the innermost open container (`$` with none).
    fn stack_path(&self) -> Vec<u16> {
        if self.st.stack.is_empty() {
            return units("$");
        }
        let mut p = Vec::new();
        for f in &self.st.stack {
            let (Frame::Obj { seg, .. } | Frame::Arr { seg, .. }) = f;
            p.extend_from_slice(seg);
        }
        p
    }

    fn token_path(&self, seg: &[u16]) -> Vec<u16> {
        if self.st.stack.is_empty() {
            return seg.to_vec();
        }
        let mut p = self.stack_path();
        p.extend_from_slice(seg);
        p
    }

    /// `feed(text)`: the text's UTF-8 size counts against maxBytes before any unit is read.
    pub fn feed(&mut self, text: &[u16]) -> Option<&Violation> {
        self.feed_counted(text, utf8_len(text))
    }

    /// `feed` of a chunk known only to be longer than [`MAX_GUARD_BYTES`] units: it is at least
    /// that many UTF-8 bytes, so (unless a violation came first) it exceeds any accepted maxBytes
    /// before any of it is read, exactly as the JS decides it.
    pub fn feed_oversize(&mut self) -> Option<&Violation> {
        let over = u64::try_from(MAX_GUARD_BYTES)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        self.feed_counted(&[], over)
    }

    fn feed_counted(&mut self, text: &[u16], bytes: u64) -> Option<&Violation> {
        if self.st.violation.is_some() {
            return self.st.violation.as_ref();
        }
        self.st.bytes_seen = self.st.bytes_seen.saturating_add(bytes);
        if i128::from(self.st.bytes_seen) > i128::from(self.st.max_bytes) {
            let m = Msg::new()
                .s("Input exceeds maxBytes ")
                .u(&num_units(self.st.max_bytes));
            self.fail(m, units("$"), Reason::MaxBytes);
            return self.st.violation.as_ref();
        }
        for &c in text {
            self.step(c);
            if self.st.violation.is_some() {
                break;
            }
        }
        self.st.violation.as_ref()
    }

    /// `end()`.
    pub fn end(&mut self) -> Option<&Violation> {
        if self.st.violation.is_some() {
            return self.st.violation.as_ref();
        }
        match self.st.token.take() {
            Some(t @ Token::Num { .. }) => self.close_number(t),
            Some(Token::Str { seg, .. }) => {
                let p = self.token_path(&seg);
                self.fail(
                    Msg::new().s("Unterminated string at end of stream"),
                    p,
                    Reason::UnterminatedString,
                );
            }
            Some(Token::Lit { seg, which, .. }) => {
                let p = self.token_path(&seg);
                let expect = LITERALS.get(usize::from(which)).copied().unwrap_or("null");
                let m = Msg::new()
                    .s("Unterminated literal (expected '")
                    .s(expect)
                    .s("') at end of stream");
                self.fail(m, p, Reason::UnterminatedLiteral);
            }
            None => {}
        }
        if self.st.violation.is_some() {
            return self.st.violation.as_ref();
        }
        if !self.st.stack.is_empty() {
            let p = self.stack_path();
            self.fail(
                Msg::new().s("Unexpected end of stream: unterminated object or array"),
                p,
                Reason::UnterminatedContainer,
            );
            return self.st.violation.as_ref();
        }
        if !self.st.done {
            self.fail(
                Msg::new().s("Unexpected end of stream: no JSON value was produced"),
                units("$"),
                Reason::NoValue,
            );
        }
        self.st.violation.as_ref()
    }

    // ---- character dispatch ----------------------------------------------------------------

    fn step(&mut self, c: u16) {
        if self.st.violation.is_some() {
            return;
        }
        if let Some(t) = self.st.token.take() {
            self.feed_token(t, c);
            return;
        }
        self.dispatch(c);
    }

    /// `_step` with no active token.
    fn dispatch(&mut self, c: u16) {
        match self.st.stack.last() {
            None => {
                if self.st.done || is_ws(c) {
                    return; // trailing data after a complete document is ignored
                }
                self.begin_value(self.schema.root(), units("$"), c);
            }
            Some(Frame::Obj { .. }) => self.step_object(c),
            Some(Frame::Arr { .. }) => self.step_array(c),
        }
    }

    /// `_beginValue(schema, path, ch)`; `seg` is the value's path past the stack's.
    fn begin_value(&mut self, node_id: NodeId, seg: Vec<u16>, c: u16) {
        let schema = self.schema;
        let node = schema.node(node_id);
        let (bit, type_name) = match c {
            0x22 => (T_STRING, "string"),
            0x7b => (T_OBJECT, "object"),
            0x5b => (T_ARRAY, "array"),
            0x74 | 0x66 => (T_BOOLEAN, "boolean"),
            0x6e => (T_NULL, "null"),
            0x2d | 0x30..=0x39 => (T_NUMBER, "number"),
            _ => {
                let p = self.token_path(&seg);
                let m = Msg::new()
                    .s("Unexpected character '")
                    .u(&[c])
                    .s("' while expecting a value at ")
                    .u(&p);
                self.fail(m, p, Reason::UnexpectedChar);
                return;
            }
        };
        if let Some(allowed) = node.allowed {
            if allowed & bit == 0 {
                let p = self.token_path(&seg);
                let m = Msg::new()
                    .s("Type mismatch at ")
                    .u(&p)
                    .s(": expected ")
                    .u(&node.label)
                    .s(", got ")
                    .s(type_name);
                self.fail(m, p, Reason::TypeMismatch);
                return;
            }
        }
        match bit {
            T_STRING => {
                let has_enum = node.enum_vals.is_some();
                let candidates = has_enum.then(|| {
                    (0..node.enum_strings.len())
                        .filter_map(|i| u32::try_from(i).ok())
                        .collect()
                });
                self.st.token = Some(Token::Str {
                    key: false,
                    node: node_id,
                    seg,
                    escape: false,
                    unicode: None,
                    chars: has_enum.then(Vec::new),
                    len: 0,
                    candidates,
                });
            }
            T_NUMBER => {
                self.st.token = Some(Token::Num {
                    node: node_id,
                    seg,
                    text: vec![c as u8],
                })
            }
            T_BOOLEAN | T_NULL => {
                let which = match c {
                    0x74 => 0,
                    0x66 => 1,
                    _ => 2,
                };
                self.st.token = Some(Token::Lit {
                    node: node_id,
                    seg,
                    which,
                    matched: 1,
                });
            }
            _ => {
                let depth = i64::try_from(self.st.stack.len()).unwrap_or(i64::MAX);
                if depth.saturating_add(1) > self.st.max_depth {
                    let p = self.token_path(&seg);
                    let m = Msg::new()
                        .s("Nesting exceeds maxDepth ")
                        .u(&num_units(self.st.max_depth))
                        .s(" at ")
                        .u(&p);
                    self.fail(m, p, Reason::MaxDepth);
                    return;
                }
                if bit == T_OBJECT {
                    let seen = vec![false; node.required.as_ref().map_or(0, Vec::len)];
                    self.st.stack.push(Frame::Obj {
                        node: node_id,
                        seg,
                        awaiting: ObjAwait::KeyOrClose,
                        current_key: None,
                        seen,
                    });
                } else {
                    self.st.stack.push(Frame::Arr {
                        node: node_id,
                        seg,
                        count: 0,
                        awaiting: ArrAwait::ValueOrClose,
                    });
                }
            }
        }
    }

    fn value_consumed(&mut self) {
        let schema = self.schema;
        match self.st.stack.last_mut() {
            None => self.st.done = true,
            Some(Frame::Obj {
                node,
                awaiting,
                current_key,
                seen,
                ..
            }) => {
                if let (Some(key), Some(req)) = (current_key.take(), &schema.node(*node).required) {
                    for (flag, name) in seen.iter_mut().zip(req) {
                        if *name == key {
                            *flag = true;
                        }
                    }
                }
                *awaiting = ObjAwait::CommaOrClose;
            }
            Some(Frame::Arr {
                count, awaiting, ..
            }) => {
                *count = count.saturating_add(1);
                *awaiting = ArrAwait::CommaOrClose;
            }
        }
    }

    // ---- object --------------------------------------------------------------------------

    fn step_object(&mut self, c: u16) {
        let Some(Frame::Obj {
            node,
            awaiting,
            current_key,
            ..
        }) = self.st.stack.last()
        else {
            return;
        };
        let (node, awaiting) = (*node, *awaiting);
        match awaiting {
            ObjAwait::KeyOrClose | ObjAwait::Key => {
                if is_ws(c) {
                    return;
                }
                if c == 0x7d && awaiting == ObjAwait::KeyOrClose {
                    self.close_object();
                    return;
                }
                if c == 0x22 {
                    self.st.token = Some(Token::Str {
                        key: true,
                        node: EMPTY,
                        seg: Vec::new(),
                        escape: false,
                        unicode: None,
                        chars: Some(Vec::new()),
                        len: 0,
                        candidates: None,
                    });
                    return;
                }
                let p = self.stack_path();
                let m = Msg::new()
                    .s("Expected a property name")
                    .s(if awaiting == ObjAwait::KeyOrClose {
                        " or '}'"
                    } else {
                        ""
                    })
                    .s(" at ")
                    .u(&p);
                self.fail(m, p, Reason::ExpectedKey);
            }
            ObjAwait::Colon => {
                if is_ws(c) {
                    return;
                }
                if c == 0x3a {
                    self.set_obj_await(ObjAwait::Value);
                    return;
                }
                let p = self.stack_path();
                let m = Msg::new().s("Expected ':' after property name at ").u(&p);
                self.fail(m, p, Reason::ExpectedColon);
            }
            ObjAwait::Value => {
                if is_ws(c) {
                    return;
                }
                let key = current_key.clone().unwrap_or_default();
                let child = self.property_schema(node, &key);
                let seg = Msg::new().s(".").u(&key).0;
                self.begin_value(child, seg, c);
            }
            ObjAwait::CommaOrClose => {
                if is_ws(c) {
                    return;
                }
                if c == 0x2c {
                    self.set_obj_await(ObjAwait::Key);
                    return;
                }
                if c == 0x7d {
                    self.close_object();
                    return;
                }
                let p = self.stack_path();
                let m = Msg::new().s("Expected ',' or '}' at ").u(&p);
                self.fail(m, p, Reason::ExpectedCommaOrBrace);
            }
        }
    }

    fn set_obj_await(&mut self, to: ObjAwait) {
        if let Some(Frame::Obj { awaiting, .. }) = self.st.stack.last_mut() {
            *awaiting = to;
        }
    }

    fn property_schema(&self, node: NodeId, key: &[u16]) -> NodeId {
        self.schema
            .node(node)
            .properties
            .as_ref()
            .and_then(|props| props.iter().find(|(k, _)| k == key))
            .map_or(EMPTY, |(_, id)| *id)
    }

    fn on_key_complete(&mut self, key: Vec<u16>) {
        let Some(Frame::Obj { node, .. }) = self.st.stack.last() else {
            return;
        };
        let n = self.schema.node(*node);
        if let Some(props) = &n.properties {
            if n.additional_false && !props.iter().any(|(k, _)| *k == key) {
                let p = self.stack_path();
                let m = Msg::new().s("Unknown property '").u(&key).s("' at ").u(&p);
                let mut vp = p;
                vp.push(0x2e);
                vp.extend_from_slice(&key);
                self.fail(m, vp, Reason::UnknownProperty);
                return;
            }
        }
        if let Some(Frame::Obj {
            current_key,
            awaiting,
            ..
        }) = self.st.stack.last_mut()
        {
            *current_key = Some(key);
            *awaiting = ObjAwait::Colon;
        }
    }

    fn close_object(&mut self) {
        let Some(Frame::Obj { node, seen, .. }) = self.st.stack.last() else {
            return;
        };
        if let Some(req) = &self.schema.node(*node).required {
            let missing: Vec<&Vec<u16>> = req
                .iter()
                .zip(seen.iter().chain(std::iter::repeat(&false)))
                .filter(|(_, s)| !**s)
                .map(|(k, _)| k)
                .collect();
            if !missing.is_empty() {
                let p = self.stack_path();
                let mut m = Msg::new()
                    .s("Missing required propert")
                    .s(if missing.len() > 1 { "ies" } else { "y" })
                    .s(" ");
                for (i, k) in missing.iter().enumerate() {
                    if i > 0 {
                        m = m.s(", ");
                    }
                    m = m.u(k);
                }
                m = m.s(" at ").u(&p);
                self.fail(m, p, Reason::MissingRequired);
                return;
            }
        }
        self.st.stack.pop();
        self.value_consumed();
    }

    // ---- array ---------------------------------------------------------------------------

    fn step_array(&mut self, c: u16) {
        let Some(Frame::Arr { awaiting, .. }) = self.st.stack.last() else {
            return;
        };
        match *awaiting {
            ArrAwait::ValueOrClose | ArrAwait::Value => {
                if is_ws(c) {
                    return;
                }
                if c == 0x5d && *awaiting == ArrAwait::ValueOrClose {
                    self.close_array();
                    return;
                }
                self.begin_array_value(c);
            }
            ArrAwait::CommaOrClose => {
                if is_ws(c) {
                    return;
                }
                if c == 0x2c {
                    if let Some(Frame::Arr { awaiting, .. }) = self.st.stack.last_mut() {
                        *awaiting = ArrAwait::Value;
                    }
                    return;
                }
                if c == 0x5d {
                    self.close_array();
                    return;
                }
                let p = self.stack_path();
                let m = Msg::new().s("Expected ',' or ']' at ").u(&p);
                self.fail(m, p, Reason::ExpectedCommaOrBracket);
            }
        }
    }

    fn begin_array_value(&mut self, c: u16) {
        let Some(Frame::Arr { node, count, .. }) = self.st.stack.last() else {
            return;
        };
        let (node, count) = (*node, *count);
        let n = self.schema.node(node);
        if let Some(max) = n.max_items {
            if i128::from(count) >= i128::from(max) {
                let p = self.stack_path();
                let m = Msg::new()
                    .s("Array at ")
                    .u(&p)
                    .s(" exceeds maxItems ")
                    .u(&num_units(max));
                self.fail(m, p, Reason::MaxItems);
                return;
            }
        }
        let seg = Msg::new().s("[").u(&num_units(count)).s("]").0;
        self.begin_value(n.items, seg, c);
    }

    fn close_array(&mut self) {
        self.st.stack.pop();
        self.value_consumed();
    }

    // ---- tokens --------------------------------------------------------------------------

    /// `_feedToken`; `t` was taken out of the state and is put back unless it finished.
    fn feed_token(&mut self, t: Token, c: u16) {
        match t {
            Token::Str { .. } => self.feed_string(t, c),
            Token::Num {
                node,
                seg,
                mut text,
            } => {
                if is_number_char(c) {
                    text.push(c as u8);
                    self.st.token = Some(Token::Num { node, seg, text });
                    return;
                }
                self.close_number(Token::Num { node, seg, text });
                if self.st.violation.is_none() {
                    self.dispatch(c); // re-dispatch the delimiter just consumed
                }
            }
            Token::Lit {
                node,
                seg,
                which,
                matched,
            } => {
                let expect = LITERALS.get(usize::from(which)).copied().unwrap_or("null");
                let e: Vec<u16> = units(expect);
                if e.get(usize::from(matched)) != Some(&c) {
                    let p = self.token_path(&seg);
                    let m = Msg::new()
                        .s("Invalid literal at ")
                        .u(&p)
                        .s(": expected '")
                        .s(expect)
                        .s("'");
                    self.fail(m, p, Reason::InvalidLiteral);
                    return;
                }
                let matched = matched.saturating_add(1);
                if usize::from(matched) < e.len() {
                    self.st.token = Some(Token::Lit {
                        node,
                        seg,
                        which,
                        matched,
                    });
                    return;
                }
                if let Some(vals) = &self.schema.node(node).enum_vals {
                    let hit = vals.iter().any(|v| {
                        matches!(
                            (v, which),
                            (EnumVal::Bool(true), 0)
                                | (EnumVal::Bool(false), 1)
                                | (EnumVal::Null, 2)
                        )
                    });
                    if !hit {
                        let p = self.token_path(&seg);
                        let m = Msg::new()
                            .s("Literal ")
                            .s(expect)
                            .s(" at ")
                            .u(&p)
                            .s(" is not one of the allowed enum values");
                        self.fail(m, p, Reason::EnumLiteral);
                        return;
                    }
                }
                self.value_consumed();
            }
        }
    }

    fn feed_string(&mut self, mut t: Token, c: u16) {
        let Token::Str {
            seg,
            escape,
            unicode,
            ..
        } = &mut t
        else {
            return;
        };
        if let Some((remaining, acc)) = *unicode {
            if !is_hex(c) {
                let p = self.token_path(seg);
                let m = Msg::new().s("Invalid unicode escape in string at ").u(&p);
                self.fail(m, p, Reason::InvalidUnicodeEscape);
                return;
            }
            let digit = char::from_u32(u32::from(c))
                .and_then(|ch| ch.to_digit(16))
                .and_then(|d| u16::try_from(d).ok())
                .unwrap_or(0);
            let acc = acc.wrapping_mul(16).wrapping_add(digit);
            let remaining = remaining.saturating_sub(1);
            if remaining == 0 {
                *unicode = None;
                self.append(t, acc);
            } else {
                *unicode = Some((remaining, acc));
                self.st.token = Some(t);
            }
            return;
        }
        if *escape {
            *escape = false;
            let mapped = match c {
                0x22 => Some(0x22),
                0x5c => Some(0x5c),
                0x2f => Some(0x2f),
                0x62 => Some(0x08),
                0x66 => Some(0x0c),
                0x6e => Some(0x0a),
                0x72 => Some(0x0d),
                0x74 => Some(0x09),
                _ => None,
            };
            if c == 0x75 {
                *unicode = Some((4, 0));
                self.st.token = Some(t);
                return;
            }
            match mapped {
                Some(u) => self.append(t, u),
                None => {
                    let p = self.token_path(seg);
                    let m = Msg::new()
                        .s("Invalid escape sequence '\\")
                        .u(&[c])
                        .s("' in string at ")
                        .u(&p);
                    self.fail(m, p, Reason::InvalidEscape);
                }
            }
            return;
        }
        if c == 0x5c {
            *escape = true;
            self.st.token = Some(t);
            return;
        }
        if c == 0x22 {
            self.finish_string(t);
            return;
        }
        self.append(t, c);
    }

    /// `_appendStringChar`.
    fn append(&mut self, mut t: Token, c: u16) {
        let Token::Str {
            key,
            node,
            seg,
            chars,
            len,
            candidates,
            ..
        } = &mut t
        else {
            return;
        };
        if let Some(ch) = chars {
            ch.push(c);
        }
        *len = len.saturating_add(1);
        if !*key {
            let n = self.schema.node(*node);
            if let Some(max) = n.max_length {
                if i128::from(*len) > i128::from(max) {
                    let p = self.token_path(seg);
                    let m = Msg::new()
                        .s("String at ")
                        .u(&p)
                        .s(" exceeds maxLength ")
                        .u(&num_units(max));
                    self.fail(m, p, Reason::MaxLength);
                    return;
                }
            }
            if let (Some(cands), Some(ch)) = (candidates, chars.as_ref()) {
                cands.retain(|&i| {
                    n.enum_strings
                        .get(i as usize)
                        .is_some_and(|v| v.starts_with(ch))
                });
                if cands.is_empty() {
                    let p = self.token_path(seg);
                    let m = Msg::new()
                        .s("String at ")
                        .u(&p)
                        .s(" cannot match any allowed value (prefix '")
                        .u(ch)
                        .s("' is impossible)");
                    self.fail(m, p, Reason::EnumPrefix);
                    return;
                }
            }
        }
        self.st.token = Some(t);
    }

    fn finish_string(&mut self, t: Token) {
        let Token::Str {
            key,
            node,
            seg,
            chars,
            ..
        } = t
        else {
            return;
        };
        let chars = chars.unwrap_or_default();
        if key {
            self.on_key_complete(chars);
            return;
        }
        if let Some(vals) = &self.schema.node(node).enum_vals {
            if !vals
                .iter()
                .any(|v| matches!(v, EnumVal::Str(s) if *s == chars))
            {
                let p = self.token_path(&seg);
                let m = Msg::new()
                    .s("String '")
                    .u(&chars)
                    .s("' at ")
                    .u(&p)
                    .s(" is not one of the allowed enum values");
                self.fail(m, p, Reason::EnumString);
                return;
            }
        }
        self.value_consumed();
    }

    /// `_closeNumber`.
    fn close_number(&mut self, t: Token) {
        let Token::Num { node, seg, text } = t else {
            return;
        };
        let text_units: Vec<u16> = text.iter().map(|&b| u16::from(b)).collect();
        if !number_ok(&text, false) {
            let p = self.token_path(&seg);
            let m = Msg::new()
                .s("Invalid number literal '")
                .u(&text_units)
                .s("' at ")
                .u(&p);
            self.fail(m, p, Reason::InvalidNumber);
            return;
        }
        let n = self.schema.node(node);
        if n.integer_only && !number_ok(&text, true) {
            let p = self.token_path(&seg);
            let m = Msg::new()
                .s("Expected an integer at ")
                .u(&p)
                .s(", got ")
                .u(&text_units);
            self.fail(m, p, Reason::NotInteger);
            return;
        }
        if let Some(vals) = &n.enum_vals {
            // `Number(text)`: correctly rounded, overflowing to ±Infinity, as std's parser does.
            let num = std::str::from_utf8(&text)
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(f64::NAN);
            if !vals
                .iter()
                .any(|v| matches!(v, EnumVal::Num(x) if *x == num))
            {
                let p = self.token_path(&seg);
                let m = Msg::new()
                    .s("Number ")
                    .u(&text_units)
                    .s(" at ")
                    .u(&p)
                    .s(" is not one of the allowed enum values");
                self.fail(m, p, Reason::EnumNumber);
                return;
            }
        }
        self.value_consumed();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn run(schema: &str, chunks: &[&str]) -> Option<(String, String)> {
        let s = Schema::parse(schema.as_bytes()).unwrap();
        let mut v = Validator::new(
            &s,
            State::new(DEFAULT_MAX_DEPTH, DEFAULT_MAX_BYTES).unwrap(),
        );
        for c in chunks {
            let u: Vec<u16> = c.encode_utf16().collect();
            v.feed(&u);
        }
        v.end().map(|x| {
            (
                String::from_utf16_lossy(&x.message),
                String::from_utf16_lossy(&x.path),
            )
        })
    }

    #[test]
    fn number_grammar() {
        for ok in ["0", "-0", "1", "10", "0.5", "1e5", "1E+5", "-1.25e-3"] {
            assert!(number_ok(ok.as_bytes(), false), "{ok}");
        }
        for bad in [
            "01", "-", "1.", ".5", "1e", "1e+", "--1", "1-2", "+1", "1.2.3", "",
        ] {
            assert!(!number_ok(bad.as_bytes(), false), "{bad}");
        }
        assert!(number_ok(b"-12", true) && !number_ok(b"1.0", true) && !number_ok(b"1e2", true));
        assert_eq!("1e+5".parse::<f64>().ok(), Some(1e5));
    }

    #[test]
    fn byte_length_is_buffer_bytelength() {
        let u = |s: &[u16]| utf8_len(s);
        assert_eq!(u(&[0x61, 0xe9, 0x20ac]), 6);
        assert_eq!(u(&[0xd83d, 0xde00]), 4);
        assert_eq!(u(&[0xde00, 0xd83d]), 6);
        assert_eq!(u(&[0xd800]), 3);
    }

    #[test]
    fn the_first_violation_and_its_path() {
        let schema = r#"{"type":"object","required":["kind"],"additionalProperties":false,"properties":{"kind":{"type":"string","enum":["read","edit"]},"n":{"type":"integer"},"xs":{"type":"array","maxItems":1}}}"#;
        assert_eq!(run(schema, &[r#"{"kind":"read"}"#]), None);
        assert_eq!(
            run(schema, &[r#"{"kind":"rx"#]),
            Some((
                "String at $.kind cannot match any allowed value (prefix 'rx' is impossible)"
                    .into(),
                "$.kind".into()
            ))
        );
        assert_eq!(
            run(schema, &[r#"{"zz""#]),
            Some(("Unknown property 'zz' at $".into(), "$.zz".into()))
        );
        assert_eq!(
            run(schema, &[r#"{"n":1.5,"kind":"edit"}"#]),
            Some(("Expected an integer at $.n, got 1.5".into(), "$.n".into()))
        );
        assert_eq!(
            run(schema, &[r#"{"xs":[1,2]}"#]),
            Some(("Array at $.xs exceeds maxItems 1".into(), "$.xs".into()))
        );
        assert_eq!(
            run(schema, &["{}"]),
            Some(("Missing required property kind at $".into(), "$".into()))
        );
        assert_eq!(
            run(r#"{"type":"number"}"#, &["1", "2"]),
            None,
            "a number spanning chunks closes at end()"
        );
        assert_eq!(
            run("{}", &["[[1,", "tru"]),
            Some((
                "Unterminated literal (expected 'true') at end of stream".into(),
                "$[0][1]".into()
            ))
        );
    }
}
