//! The restricted JSON-schema subset stream-guard.cjs reads (type, required, properties,
//! additionalProperties: false, enum, items, maxItems, maxLength), compiled to a flat node table.
//!
//! Each keyword is read as the JS reads it. Where the JS would quietly run on a shape no real
//! schema has (a truthy non-object `properties`/`items`, a non-string `type` entry or `required`
//! name, a fractional or unsafe `maxLength`/`maxItems`), this refuses the schema instead
//! ([`crate::Error::Schema`]): the JS's behaviour there rests on prototype lookups and number
//! formatting this port does not reproduce.

use crate::json::{self, J};
use crate::Error;

/// Largest schema text accepted.
pub const MAX_SCHEMA_BYTES: usize = 256 * 1024;
/// Deepest JSON nesting of a schema text.
pub const MAX_SCHEMA_NESTING: usize = 64;
/// Largest `maxLength` / `maxItems` magnitude (Number.MAX_SAFE_INTEGER).
const SAFE: f64 = 9_007_199_254_740_991.0;

/// Index of a node; node 0 is the empty schema `{}`.
pub(crate) type NodeId = u32;
pub(crate) const EMPTY: NodeId = 0;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EnumVal {
    Str(Vec<u16>),
    Num(f64),
    Bool(bool),
    Null,
    /// An array or object: equal to nothing a document produces.
    Other,
}

pub(crate) const T_STRING: u8 = 1;
pub(crate) const T_OBJECT: u8 = 2;
pub(crate) const T_ARRAY: u8 = 4;
pub(crate) const T_BOOLEAN: u8 = 8;
pub(crate) const T_NULL: u8 = 16;
pub(crate) const T_NUMBER: u8 = 32;

#[derive(Debug, Clone, Default)]
pub(crate) struct Node {
    /// `None`: no `type` (any value). Otherwise the T_* bits allowed (integer allows number).
    pub(crate) allowed: Option<u8>,
    /// `types.join(' | ')`, or `any`.
    pub(crate) label: Vec<u16>,
    /// `type` names integer and not number.
    pub(crate) integer_only: bool,
    /// `enum`, when it is an array.
    pub(crate) enum_vals: Option<Vec<EnumVal>>,
    /// The string members of `enum`, in order (the incremental prefix candidates).
    pub(crate) enum_strings: Vec<Vec<u16>>,
    pub(crate) max_length: Option<i64>,
    pub(crate) max_items: Option<i64>,
    pub(crate) required: Option<Vec<Vec<u16>>>,
    /// `properties`, when it is an object: name and node.
    pub(crate) properties: Option<Vec<(Vec<u16>, NodeId)>>,
    pub(crate) additional_false: bool,
    pub(crate) items: NodeId,
}

#[derive(Debug, Clone)]
pub struct Schema {
    pub(crate) nodes: Vec<Node>,
}

impl Schema {
    /// Compile a schema from its JSON text (UTF-8, as `JSON.stringify` writes it). `null` or any
    /// other falsy root is the empty schema, as `schema || {}` makes it.
    pub fn parse(text: &[u8]) -> Result<Schema, Error> {
        if text.len() > MAX_SCHEMA_BYTES {
            return Err(Error::TooLarge);
        }
        let root = json::parse(text, MAX_SCHEMA_NESTING).ok_or(Error::Schema)?;
        let mut s = Schema {
            nodes: vec![Node {
                label: units("any"),
                ..Node::default()
            }],
        };
        if root.truthy() {
            if !matches!(root, J::Obj(_)) {
                return Err(Error::Schema);
            }
            s.add(&root)?;
        } else {
            // The root is node 1 either way.
            s.nodes.push(s.nodes.first().cloned().unwrap_or_default());
        }
        Ok(s)
    }

    pub(crate) fn node(&self, id: NodeId) -> &Node {
        // Ids come from this table or a decoded state checked against it; EMPTY is always there.
        self.nodes
            .get(id as usize)
            .or_else(|| self.nodes.first())
            .unwrap_or(&EMPTY_NODE)
    }

    pub(crate) fn root(&self) -> NodeId {
        1
    }

    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    /// The node of a member `p` of `properties`, or of `items`: an object, or `{}` for a falsy one.
    fn child(&mut self, v: &J) -> Result<NodeId, Error> {
        match v {
            J::Obj(_) => self.add(v),
            other if !other.truthy() => Ok(EMPTY),
            _ => Err(Error::Schema),
        }
    }

    fn add(&mut self, v: &J) -> Result<NodeId, Error> {
        let id = NodeId::try_from(self.nodes.len()).map_err(|_| Error::Schema)?;
        self.nodes.push(Node::default());
        let mut n = Node {
            label: units("any"),
            ..Node::default()
        };
        match v.get("type") {
            None | Some(J::Null) => {}
            Some(J::Str(t)) => set_types(&mut n, std::slice::from_ref(t)),
            Some(J::Arr(list)) => {
                let mut names = Vec::with_capacity(list.len());
                for t in list {
                    let J::Str(t) = t else {
                        return Err(Error::Schema);
                    };
                    names.push(t.clone());
                }
                set_types(&mut n, &names);
            }
            Some(_) => return Err(Error::Schema),
        }
        if let Some(J::Arr(list)) = v.get("enum") {
            let vals: Vec<EnumVal> = list
                .iter()
                .map(|e| match e {
                    J::Str(s) => EnumVal::Str(s.clone()),
                    J::Num(x) => EnumVal::Num(*x),
                    J::Bool(b) => EnumVal::Bool(*b),
                    J::Null => EnumVal::Null,
                    J::Arr(_) | J::Obj(_) => EnumVal::Other,
                })
                .collect();
            n.enum_strings = vals
                .iter()
                .filter_map(|e| match e {
                    EnumVal::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .collect();
            n.enum_vals = Some(vals);
        }
        n.max_length = count(v.get("maxLength"))?;
        n.max_items = count(v.get("maxItems"))?;
        if let Some(J::Arr(list)) = v.get("required") {
            let mut names = Vec::with_capacity(list.len());
            for k in list {
                let J::Str(k) = k else {
                    return Err(Error::Schema);
                };
                names.push(k.clone());
            }
            n.required = Some(names);
        }
        match v.get("properties") {
            Some(J::Obj(members)) => {
                let mut props = Vec::with_capacity(members.len());
                for (k, pv) in members {
                    let child = self.child(pv)?;
                    props.push((k.clone(), child));
                }
                n.properties = Some(props);
            }
            Some(other) if other.truthy() => return Err(Error::Schema),
            _ => {}
        }
        n.additional_false = matches!(v.get("additionalProperties"), Some(J::Bool(false)));
        n.items = match v.get("items") {
            Some(items) => self.child(items)?,
            None => EMPTY,
        };
        let slot = self.nodes.get_mut(id as usize).ok_or(Error::Schema)?;
        *slot = n;
        Ok(id)
    }
}

static EMPTY_NODE: Node = Node {
    allowed: None,
    label: Vec::new(),
    integer_only: false,
    enum_vals: None,
    enum_strings: Vec::new(),
    max_length: None,
    max_items: None,
    required: None,
    properties: None,
    additional_false: false,
    items: EMPTY,
};

fn count(v: Option<&J>) -> Result<Option<i64>, Error> {
    match v {
        Some(J::Num(x)) => {
            if x.fract() != 0.0 || x.abs() > SAFE {
                return Err(Error::Schema);
            }
            // Exact: |x| is a safe integer.
            Ok(Some(*x as i64))
        }
        _ => Ok(None),
    }
}

fn set_types(n: &mut Node, names: &[Vec<u16>]) {
    let mut bits = 0u8;
    let (mut integer, mut number) = (false, false);
    let mut label = Vec::new();
    for (i, t) in names.iter().enumerate() {
        if i > 0 {
            label.extend(" | ".encode_utf16());
        }
        label.extend_from_slice(t);
        bits |= match String::from_utf16(t).as_deref() {
            Ok("string") => T_STRING,
            Ok("object") => T_OBJECT,
            Ok("array") => T_ARRAY,
            Ok("boolean") => T_BOOLEAN,
            Ok("null") => T_NULL,
            Ok("number") => {
                number = true;
                T_NUMBER
            }
            Ok("integer") => {
                integer = true;
                T_NUMBER
            }
            _ => 0,
        };
    }
    n.allowed = Some(bits);
    n.label = label;
    n.integer_only = integer && !number;
}

pub(crate) fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn compiles_the_subset() {
        let s = Schema::parse(
            br#"{"type":"object","required":["a"],"additionalProperties":false,"properties":{"a":{"type":["integer","null"]},"b":null,"c":{"type":"array","items":{"enum":["x",1,true,null,[]]},"maxItems":2}}}"#,
        )
        .unwrap();
        let root = s.node(s.root());
        assert_eq!(root.allowed, Some(T_OBJECT));
        assert!(root.additional_false);
        let props = root.properties.as_ref().unwrap();
        assert_eq!(props.len(), 3);
        let a = s.node(props[0].1);
        assert_eq!(a.allowed, Some(T_NUMBER | T_NULL));
        assert!(a.integer_only);
        assert_eq!(String::from_utf16(&a.label).unwrap(), "integer | null");
        assert_eq!(props[1].1, EMPTY);
        let c = s.node(props[2].1);
        assert_eq!(c.max_items, Some(2));
        let items = s.node(c.items);
        assert_eq!(items.enum_strings, vec![units("x")]);
        assert_eq!(items.enum_vals.as_ref().unwrap().len(), 5);
    }

    #[test]
    fn falsy_roots_are_empty_and_odd_shapes_refused() {
        for root in [&b"null"[..], b"false", b"0", b"\"\""] {
            let s = Schema::parse(root).unwrap();
            assert_eq!(s.node(s.root()).allowed, None);
        }
        for bad in [
            &br#"[]"#[..],
            br#""x""#,
            br#"{"type":5}"#,
            br#"{"type":["string",1]}"#,
            br#"{"required":[1]}"#,
            br#"{"properties":true}"#,
            br#"{"properties":[]}"#,
            br#"{"properties":{"a":1}}"#,
            br#"{"items":true}"#,
            br#"{"maxLength":1.5}"#,
            br#"{"maxItems":1e300}"#,
            br#"{"a":"#,
        ] {
            assert_eq!(Schema::parse(bad).err(), Some(Error::Schema), "{bad:?}");
        }
        // What the JS ignores is ignored.
        let s = Schema::parse(br#"{"enum":"x","required":"a","maxLength":"3","properties":0}"#)
            .unwrap();
        let n = s.node(s.root());
        assert!(
            n.enum_vals.is_none()
                && n.required.is_none()
                && n.max_length.is_none()
                && n.properties.is_none()
        );
        assert_eq!(
            Schema::parse(&vec![b' '; MAX_SCHEMA_BYTES + 1]).err(),
            Some(Error::TooLarge)
        );
    }
}
