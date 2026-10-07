//! mcp.cjs `resolveSchemaRefs(schema)` / `inlineRefs`: local `$ref` inlining with the same
//! limits (node depth 64, ref depth 8, 20 000 nodes, 262 144 characters), the same walk order and
//! the same JS semantics, including the corners:
//!
//! - `defs` is `{ ...(schema.$defs || {}), ...(schema.definitions || {}) }`: a non-empty string
//!   spreads into one entry per UTF-16 unit, an array into index keys; a key missing from `defs`
//!   still finds `Object.prototype`'s members (`toString`, `__proto__`, ...), which inline as `{}`.
//! - A ref target is spread (`{ ...resolved, ...siblings }`): a string target becomes index keys,
//!   numbers and booleans nothing.
//! - Copying a walked object assigns `out[k] = v`, so a `__proto__` member sets the copy's
//!   prototype (object or null) or is dropped (primitive) instead of becoming a key.
//! - Siblings of a `$ref` are charged `JSON.stringify(siblings).length` characters.
//!
//! **Input**: the schema as `JSON.stringify` writes it, except that `-0` is written `-0` and
//! `±Infinity` as `1e400`/`-1e400` (the host's replacer), so every value crosses unchanged.
//! **Output** (see [`resolve_schema_refs`]): the resolved tree as JSON where every object key `k`
//! is written `"=k"` and an object whose prototype was set carries `"^": proto` first; keys come
//! in JS property order (array indices ascending, then insertion order). The host rebuilds the
//! objects with `defineProperty` and `setPrototypeOf`.

use crate::json::{self, Scalar, Sink};
use std::collections::HashMap;

/// mcp.cjs MAX_REF_DEPTH.
pub const MAX_REF_DEPTH: usize = 8;
/// mcp.cjs MAX_NODE_DEPTH.
pub const MAX_NODE_DEPTH: u32 = 64;
/// mcp.cjs MAX_SCHEMA_NODES.
pub const MAX_SCHEMA_NODES: u64 = 20_000;
/// mcp.cjs MAX_SCHEMA_CHARS.
pub const MAX_SCHEMA_CHARS: u64 = 256 * 1024;
/// Input cap in UTF-16 units (wasm only; the JS has none). Real tool schemas are a few KB; the
/// whole 160-tool reference server is ~430 K characters.
pub const MAX_SCHEMA_UNITS: usize = 2 * 1024 * 1024;

/// `Object.getOwnPropertyNames(Object.prototype)`: what `defs[key]` finds when `defs` has no own
/// `key`. All are truthy and all inline as `{}` (functions and `Object.prototype` have no own
/// enumerable properties).
pub const OBJECT_PROTOTYPE_NAMES: [&str; 12] = [
    "constructor",
    "__defineGetter__",
    "__defineSetter__",
    "hasOwnProperty",
    "__lookupGetter__",
    "__lookupSetter__",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toString",
    "valueOf",
    "__proto__",
    "toLocaleString",
];

/// Why the JS throws; the host words the message exactly as mcp.cjs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaError {
    /// 'schema nests deeper than we will walk'
    Nest,
    /// 'refs expand deeper than we will inline'
    RefDepth,
    /// `schema expands past ${MAX_SCHEMA_NODES} nodes`
    Nodes,
    /// `schema expands past ${MAX_SCHEMA_CHARS} characters`
    Chars,
    /// `cannot resolve non-local ref ${ref}`
    NonLocal(Vec<u16>),
    /// `circular ref ${ref}`
    Circular(Vec<u16>),
    /// `ref ${ref} points at a definition that is not present`
    Missing(Vec<u16>),
}

impl SchemaError {
    /// `{"code":"…"}` or `{"code":"…","ref":"…"}` (the ref written as JSON.stringify would).
    pub fn json(&self) -> Vec<u8> {
        let (code, r) = match self {
            Self::Nest => ("nest", None),
            Self::RefDepth => ("ref_depth", None),
            Self::Nodes => ("nodes", None),
            Self::Chars => ("chars", None),
            Self::NonLocal(r) => ("non_local", Some(r)),
            Self::Circular(r) => ("circular", Some(r)),
            Self::Missing(r) => ("missing", Some(r)),
        };
        let mut out = format!("{{\"code\":\"{code}\"").into_bytes();
        if let Some(r) = r {
            out.extend_from_slice(b",\"ref\":");
            json::push_json_string(&mut out, r);
        }
        out.push(b'}');
        out
    }
}

/// The input could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputError {
    /// Over [`MAX_SCHEMA_UNITS`].
    TooLarge,
    /// Not one JSON text.
    NotJson,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum K {
    Null,
    True,
    False,
    Number,
    String,
    Array,
    Object,
}

#[derive(Clone, Copy, Debug)]
struct Node {
    kind: K,
    start: u32,
    end: u32,
    /// `JSON.stringify` writes this subtree `corr` units shorter than the input does (`-0` → `0`,
    /// `1e400` → `null`, `-1e400` → `null`).
    corr: u32,
    first: u32,
    /// Children: elements, or key/value pairs (2 per member) for an object.
    len: u32,
}

struct Arena<'a> {
    s: &'a [u16],
    nodes: Vec<Node>,
    kids: Vec<u32>,
    pending: Vec<u32>,
    frames: Vec<(u32, usize)>,
    root: Option<u32>,
}

impl Arena<'_> {
    fn push(&mut self, kind: K, start: usize, end: usize, corr: u32) -> u32 {
        let id = self.nodes.len() as u32;
        self.nodes.push(Node {
            kind,
            start: start as u32,
            end: end as u32,
            corr,
            first: 0,
            len: 0,
        });
        if self.frames.is_empty() {
            self.root = Some(id);
        } else {
            self.pending.push(id);
        }
        id
    }
}

fn units_eq(s: &[u16], lit: &str) -> bool {
    s.iter().copied().eq(lit.encode_utf16())
}

impl Sink for Arena<'_> {
    fn begin(&mut self, array: bool, at: usize) {
        let id = self.push(if array { K::Array } else { K::Object }, at, at, 0);
        self.frames.push((id, self.pending.len()));
    }
    fn key(&mut self, start: usize, end: usize) {
        self.push(K::String, start, end, 0);
    }
    fn scalar(&mut self, kind: Scalar, start: usize, end: usize) {
        let (kind, corr) = match kind {
            Scalar::Null => (K::Null, 0),
            Scalar::True => (K::True, 0),
            Scalar::False => (K::False, 0),
            Scalar::String => (K::String, 0),
            Scalar::Number => {
                let lex = self.s.get(start..end).unwrap_or(&[]);
                let corr = if units_eq(lex, "-0") || units_eq(lex, "1e400") {
                    1
                } else if units_eq(lex, "-1e400") {
                    2
                } else {
                    0
                };
                (K::Number, corr)
            }
        };
        self.push(kind, start, end, corr);
    }
    fn end(&mut self, end: usize) {
        let Some((id, from)) = self.frames.pop() else {
            return;
        };
        let first = self.kids.len() as u32;
        let mut corr = 0u32;
        for k in self.pending.drain(from..) {
            corr = corr.saturating_add(self.nodes.get(k as usize).map_or(0, |n| n.corr));
            self.kids.push(k);
        }
        let count = self.kids.len() as u32 - first;
        if let Some(n) = self.nodes.get_mut(id as usize) {
            n.end = end as u32;
            n.corr = corr;
            n.first = first;
            n.len = if n.kind == K::Object {
                count / 2
            } else {
                count
            };
        }
    }
}

#[derive(Clone, Copy)]
enum Def {
    Node(u32),
    Unit(u16),
    /// A member of `Object.prototype`.
    Proto,
}

/// A walked value.
enum Out {
    /// An input value, unchanged (scalars, and the siblings of a `$ref`).
    Src(u32),
    /// A one-unit string (from spreading a string).
    Unit(u16),
    Arr(Vec<Out>),
    Obj(Obj),
}

#[derive(Default)]
struct Obj {
    entries: Vec<(Vec<u16>, Out)>,
    proto: Option<Box<Out>>,
}

struct Walk<'a> {
    s: &'a [u16],
    nodes: &'a [Node],
    kids: &'a [u32],
    defs: HashMap<Vec<u16>, Def>,
    nodes_spent: u64,
    chars: u64,
}

fn index_key(i: usize) -> Vec<u16> {
    i.to_string().encode_utf16().collect()
}

impl Walk<'_> {
    fn node(&self, id: u32) -> Node {
        self.nodes.get(id as usize).copied().unwrap_or(Node {
            kind: K::Null,
            start: 0,
            end: 0,
            corr: 0,
            first: 0,
            len: 0,
        })
    }
    fn kid(&self, n: Node, i: u32) -> u32 {
        self.kids
            .get((n.first + i) as usize)
            .copied()
            .unwrap_or(u32::MAX)
    }
    /// `(key node, value node)` of member `i`.
    fn member(&self, n: Node, i: u32) -> (u32, u32) {
        (self.kid(n, 2 * i), self.kid(n, 2 * i + 1))
    }
    fn decode(&self, id: u32) -> Vec<u16> {
        let n = self.node(id);
        json::decode_string(self.s, n.start as usize, n.end as usize)
    }
    fn key_is(&self, id: u32, lit: &str) -> bool {
        let n = self.node(id);
        json::string_is(self.s, n.start as usize, n.end as usize, lit)
    }
    /// The last member named `lit`, if any.
    fn get(&self, obj: Node, lit: &str) -> Option<u32> {
        (0..obj.len)
            .rev()
            .map(|i| self.member(obj, i))
            .find(|&(k, _)| self.key_is(k, lit))
            .map(|(_, v)| v)
    }
    fn truthy_node(&self, id: u32) -> bool {
        let n = self.node(id);
        match n.kind {
            K::Null | K::False => false,
            K::Number => {
                let x = json::number_value(self.s, n.start as usize, n.end as usize);
                x != 0.0 && !x.is_nan()
            }
            K::String => n.end - n.start > 2,
            K::True | K::Array | K::Object => true,
        }
    }
    fn spend(&mut self, nodes: u64, chars: u64) -> Result<(), SchemaError> {
        self.nodes_spent += nodes;
        self.chars += chars;
        if self.nodes_spent > MAX_SCHEMA_NODES {
            return Err(SchemaError::Nodes);
        }
        if self.chars > MAX_SCHEMA_CHARS {
            return Err(SchemaError::Chars);
        }
        Ok(())
    }

    /// `{ ...(schema.$defs || {}), ...(schema.definitions || {}) }`
    fn build_defs(&mut self, root: u32) {
        let top = self.node(root);
        if top.kind != K::Object {
            return;
        }
        for name in ["$defs", "definitions"] {
            let Some(v) = self.get(top, name) else {
                continue;
            };
            if !self.truthy_node(v) {
                continue;
            }
            let n = self.node(v);
            match n.kind {
                K::Object => {
                    for i in 0..n.len {
                        let (k, val) = self.member(n, i);
                        let key = self.decode(k);
                        self.defs.insert(key, Def::Node(val));
                    }
                }
                K::Array => {
                    for i in 0..n.len {
                        let c = self.kid(n, i);
                        self.defs.insert(index_key(i as usize), Def::Node(c));
                    }
                }
                K::String => {
                    for (i, u) in self.decode(v).into_iter().enumerate() {
                        self.defs.insert(index_key(i), Def::Unit(u));
                    }
                }
                _ => {}
            }
        }
    }

    fn lookup(&self, key: &[u16]) -> Option<Def> {
        if let Some(d) = self.defs.get(key) {
            return Some(*d);
        }
        OBJECT_PROTOTYPE_NAMES
            .iter()
            .any(|n| units_eq(key, n))
            .then_some(Def::Proto)
    }

    /// `JSON.stringify(siblings).length` for the members of `obj` other than `$ref`, or `None`
    /// when there are none.
    fn siblings_len(&self, obj: Node) -> Option<u64> {
        let mut n = 0u64;
        let mut len = 2u64;
        for i in 0..obj.len {
            let (k, v) = self.member(obj, i);
            if self.key_is(k, "$ref") {
                continue;
            }
            let kn = self.node(k);
            let vn = self.node(v);
            len += u64::from(kn.end - kn.start) + 1 + u64::from(vn.end - vn.start)
                - u64::from(vn.corr);
            n += 1;
        }
        (n > 0).then_some(len + n - 1)
    }

    /// `{ ...v }`: the own enumerable entries of `v`.
    fn spread(&self, v: Out) -> Vec<(Vec<u16>, Out)> {
        match v {
            Out::Src(id) => {
                if self.node(id).kind == K::String {
                    self.decode(id)
                        .into_iter()
                        .enumerate()
                        .map(|(i, u)| (index_key(i), Out::Unit(u)))
                        .collect()
                } else {
                    Vec::new()
                }
            }
            Out::Unit(u) => vec![(index_key(0), Out::Unit(u))],
            Out::Arr(items) => items
                .into_iter()
                .enumerate()
                .map(|(i, o)| (index_key(i), o))
                .collect(),
            Out::Obj(o) => o.entries,
        }
    }

    fn walk(&mut self, v: Def, stack: &mut Vec<Vec<u16>>, depth: u32) -> Result<Out, SchemaError> {
        if depth > MAX_NODE_DEPTH {
            return Err(SchemaError::Nest);
        }
        if stack.len() > MAX_REF_DEPTH {
            return Err(SchemaError::RefDepth);
        }
        let id = match v {
            Def::Unit(u) => {
                self.spend(1, 1)?;
                return Ok(Out::Unit(u));
            }
            Def::Proto => {
                self.spend(1, 0)?;
                return Ok(Out::Obj(Obj::default()));
            }
            Def::Node(id) => id,
        };
        let n = self.node(id);
        let strlen = if n.kind == K::String {
            json::string_units(self.s, n.start as usize, n.end as usize) as u64
        } else {
            0
        };
        self.spend(1, strlen)?;
        match n.kind {
            K::Array => {
                let mut out = Vec::with_capacity(n.len as usize);
                for i in 0..n.len {
                    out.push(self.walk(Def::Node(self.kid(n, i)), stack, depth + 1)?);
                }
                Ok(Out::Arr(out))
            }
            K::Object => {
                if let Some(r) = self
                    .get(n, "$ref")
                    .filter(|&r| self.node(r).kind == K::String)
                {
                    return self.inline_ref(n, r, stack, depth);
                }
                let mut out = Obj::default();
                for i in 0..n.len {
                    let (k, val) = self.member(n, i);
                    let key = self.decode(k);
                    if units_eq(&key, "$defs") || units_eq(&key, "definitions") {
                        continue;
                    }
                    self.spend(0, key.len() as u64)?;
                    let w = self.walk(Def::Node(val), stack, depth + 1)?;
                    if units_eq(&key, "__proto__") {
                        // `out.__proto__ = w`: an object (or null) becomes the prototype; a
                        // primitive is ignored. Never an own key.
                        let sets = match &w {
                            Out::Arr(_) | Out::Obj(_) => true,
                            Out::Src(s) => self.node(*s).kind == K::Null,
                            Out::Unit(_) => false,
                        };
                        if sets {
                            out.proto = Some(Box::new(w));
                        }
                    } else {
                        out.entries.push((key, w));
                    }
                }
                Ok(Out::Obj(out))
            }
            _ => Ok(Out::Src(id)),
        }
    }

    fn inline_ref(
        &mut self,
        n: Node,
        r: u32,
        stack: &mut Vec<Vec<u16>>,
        depth: u32,
    ) -> Result<Out, SchemaError> {
        let reference = self.decode(r);
        let Some(key) = local_key(&reference) else {
            return Err(SchemaError::NonLocal(reference));
        };
        if stack.contains(&key) {
            return Err(SchemaError::Circular(reference));
        }
        let target = match self.lookup(&key) {
            Some(Def::Node(t)) if !self.truthy_node(t) => None,
            other => other,
        };
        let Some(target) = target else {
            return Err(SchemaError::Missing(reference));
        };
        if let Some(len) = self.siblings_len(n) {
            self.spend(0, len)?;
        }
        stack.push(key);
        let resolved = self.walk(target, stack, depth + 1);
        stack.pop();
        let mut entries = self.spread(resolved?);
        let mut at: HashMap<Vec<u16>, usize> = entries
            .iter()
            .enumerate()
            .map(|(i, (k, _))| (k.clone(), i))
            .collect();
        for i in 0..n.len {
            let (k, v) = self.member(n, i);
            if self.key_is(k, "$ref") {
                continue;
            }
            let key = self.decode(k);
            match at.get(&key) {
                Some(&j) => {
                    if let Some(e) = entries.get_mut(j) {
                        e.1 = Out::Src(v);
                    }
                }
                None => {
                    at.insert(key.clone(), entries.len());
                    entries.push((key, Out::Src(v)));
                }
            }
        }
        Ok(Out::Obj(Obj {
            entries,
            proto: None,
        }))
    }
}

/// `/^#\/(\$defs|definitions)\/(.+)$/` (`.` is any code unit but a line terminator).
fn local_key(r: &[u16]) -> Option<Vec<u16>> {
    let rest = ["#/$defs/", "#/definitions/"].iter().find_map(|p| {
        let p: Vec<u16> = p.encode_utf16().collect();
        r.strip_prefix(p.as_slice())
    })?;
    if rest.is_empty()
        || rest
            .iter()
            .any(|&c| matches!(c, 0x0a | 0x0d | 0x2028 | 0x2029))
    {
        return None;
    }
    Some(rest.to_vec())
}

/// A canonical array index ("0", or digits without a leading zero, at most 2^32 - 2).
fn array_index(k: &[u16]) -> Option<u64> {
    if k.is_empty() || k.len() > 10 || (k.len() > 1 && k.first() == Some(&0x30)) {
        return None;
    }
    let mut v = 0u64;
    for &c in k {
        if !(0x30..=0x39).contains(&c) {
            return None;
        }
        v = v * 10 + u64::from(c - 0x30);
    }
    (v <= 4_294_967_294).then_some(v)
}

struct Emit<'a> {
    w: &'a Walk<'a>,
    out: Vec<u8>,
}

impl Emit<'_> {
    fn lexeme(&mut self, n: Node) {
        let w = self.w;
        json::push_utf8_escaped(
            &mut self.out,
            w.s.get(n.start as usize..n.end as usize).unwrap_or(&[]),
        );
    }

    /// An input subtree, keys prefixed with `=`; iterative (siblings may nest deeply).
    fn src(&mut self, root: u32) {
        let w = self.w;
        let mut stack: Vec<(u32, u32)> = vec![(root, 0)];
        while let Some((id, i)) = stack.pop() {
            let n = w.node(id);
            match n.kind {
                K::Array | K::Object => {
                    let open = n.kind == K::Array;
                    if i == 0 {
                        self.out.push(if open { b'[' } else { b'{' });
                    }
                    if i == n.len {
                        self.out.push(if open { b']' } else { b'}' });
                        continue;
                    }
                    if i > 0 {
                        self.out.push(b',');
                    }
                    stack.push((id, i + 1));
                    let child = if open {
                        w.kid(n, i)
                    } else {
                        let (k, v) = w.member(n, i);
                        let kn = w.node(k);
                        self.out.extend_from_slice(b"\"=");
                        json::push_utf8_escaped(
                            &mut self.out,
                            w.s.get(kn.start as usize + 1..kn.end as usize)
                                .unwrap_or(&[]),
                        );
                        self.out.push(b':');
                        v
                    };
                    stack.push((child, 0));
                }
                _ => self.lexeme(n),
            }
        }
    }

    fn value(&mut self, o: &Out) {
        match o {
            Out::Src(id) => self.src(*id),
            Out::Unit(u) => json::push_json_string(&mut self.out, &[*u]),
            Out::Arr(items) => {
                self.out.push(b'[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.push(b',');
                    }
                    self.value(it);
                }
                self.out.push(b']');
            }
            Out::Obj(obj) => {
                self.out.push(b'{');
                let mut first = true;
                if let Some(p) = &obj.proto {
                    self.out.extend_from_slice(b"\"^\":");
                    self.value(p);
                    first = false;
                }
                let mut idx: Vec<(u64, usize)> = Vec::new();
                let mut rest: Vec<usize> = Vec::new();
                for (i, (k, _)) in obj.entries.iter().enumerate() {
                    match array_index(k) {
                        Some(v) => idx.push((v, i)),
                        None => rest.push(i),
                    }
                }
                idx.sort_unstable();
                for i in idx.into_iter().map(|(_, i)| i).chain(rest) {
                    let Some((k, v)) = obj.entries.get(i) else {
                        continue;
                    };
                    if !first {
                        self.out.push(b',');
                    }
                    first = false;
                    let mut key = vec![u16::from(b'=')];
                    key.extend_from_slice(k);
                    json::push_json_string(&mut self.out, &key);
                    self.out.push(b':');
                    self.value(v);
                }
                self.out.push(b'}');
            }
        }
    }
}

/// mcp.cjs `resolveSchemaRefs` on `input` (see the module docs for both encodings). `Ok(Ok(tree))`
/// is the resolved tree, `Ok(Err(e))` the error the JS throws.
pub fn resolve_schema_refs(input: &[u16]) -> Result<Result<Vec<u8>, SchemaError>, InputError> {
    if input.len() > MAX_SCHEMA_UNITS {
        return Err(InputError::TooLarge);
    }
    let mut arena = Arena {
        s: input,
        nodes: Vec::new(),
        kids: Vec::new(),
        pending: Vec::new(),
        frames: Vec::new(),
        root: None,
    };
    if !json::parse(input, &mut arena) {
        return Err(InputError::NotJson);
    }
    let Some(root) = arena.root else {
        return Err(InputError::NotJson);
    };
    let mut w = Walk {
        s: input,
        nodes: &arena.nodes,
        kids: &arena.kids,
        defs: HashMap::new(),
        nodes_spent: 0,
        chars: 0,
    };
    w.build_defs(root);
    let out = match w.walk(Def::Node(root), &mut Vec::new(), 0) {
        Ok(o) => o,
        Err(e) => return Ok(Err(e)),
    };
    let mut e = Emit {
        w: &w,
        out: Vec::new(),
    };
    e.value(&out);
    Ok(Ok(e.out))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn run(t: &str) -> String {
        let u: Vec<u16> = t.encode_utf16().collect();
        match resolve_schema_refs(&u).unwrap() {
            Ok(b) => String::from_utf8(b).unwrap(),
            Err(e) => String::from_utf8(e.json()).unwrap(),
        }
    }

    #[test]
    fn inlines() {
        assert_eq!(
            run(
                r##"{"type":"object","properties":{"a":{"$ref":"#/$defs/A","description":"d"}},"$defs":{"A":{"type":"string"}}}"##
            ),
            r#"{"=type":"object","=properties":{"=a":{"=type":"string","=description":"d"}}}"#
        );
        assert_eq!(
            run(r##"{"a":{"$ref":"#/$defs/A"},"$defs":{"A":{"b":{"$ref":"#/$defs/A"}}}}"##),
            r##"{"code":"circular","ref":"#/$defs/A"}"##
        );
        assert_eq!(
            run(r##"{"$ref":"http://x"}"##),
            r##"{"code":"non_local","ref":"http://x"}"##
        );
        assert_eq!(run(r##"{"$ref":"#/$defs/toString","x":1}"##), r#"{"=x":1}"#);
        assert_eq!(
            run(r##"{"$defs":"ab","$ref":"#/$defs/1"}"##),
            r#"{"=0":"b","=$defs":"ab"}"#
        );
        assert_eq!(
            run(r#"{"__proto__":{"a":1},"b":2}"#),
            r#"{"^":{"=a":1},"=b":2}"#
        );
        assert_eq!(run(r#"{"__proto__":3,"b":-0}"#), r#"{"=b":-0}"#);
        assert_eq!(run(r#"{"b":1,"1":2}"#), r#"{"=1":2,"=b":1}"#);
    }

    #[test]
    fn bomb() {
        let mut defs = String::new();
        for i in 0..6 {
            let props: Vec<String> = (0..20)
                .map(|j| format!("\"p{j}\":{{\"$ref\":\"#/$defs/D{}\"}}", i + 1))
                .collect();
            defs.push_str(&format!(
                "\"D{i}\":{{\"properties\":{{{}}}}},",
                props.join(",")
            ));
        }
        defs.push_str("\"D6\":{\"type\":\"string\"}");
        let t = format!("{{\"$ref\":\"#/$defs/D0\",\"$defs\":{{{defs}}}}}");
        assert_eq!(run(&t), r#"{"code":"nodes"}"#);
    }
}
