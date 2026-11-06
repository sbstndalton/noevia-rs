//! PyYAML SafeLoader schema on top of a native YAML event parser. The event API
//! preserves quoting, tags, duplicate keys and aliases that a JSON projection loses.
use crate::value::{decode_utf8, integer, repr, Kind, Result, Value};
use num_bigint_dig::BigInt;
use regex::Regex;
use std::collections::BTreeMap;
use yaml_rust2::{
    parser::{Event, Parser, Tag},
    scanner::{Marker, TScalarStyle},
};
fn matches(pattern: &str, s: &str) -> bool {
    Regex::new(pattern).is_ok_and(|r| r.is_match(s))
}
fn implicit(s: &str) -> &'static str {
    if [
        "yes", "Yes", "YES", "no", "No", "NO", "true", "True", "TRUE", "false", "False", "FALSE",
        "on", "On", "ON", "off", "Off", "OFF",
    ]
    .contains(&s)
    {
        "bool"
    } else if ["", "~", "null", "Null", "NULL"].contains(&s) {
        "null"
    } else if matches(
        r"^(?:[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))$",
        s,
    ) {
        "float"
    } else if matches(
        r"^(?:[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+)$",
        s,
    ) {
        "int"
    } else if matches(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$", s)
        || matches(
            r"^[0-9]{4}-[0-9]{1,2}-[0-9]{1,2}(?:[Tt]|[ \t]+)[0-9]{1,2}:[0-9]{2}:[0-9]{2}",
            s,
        )
    {
        "timestamp"
    } else {
        "str"
    }
}
fn scalar(s: &str, tag: &str) -> Result<Value> {
    Ok(match tag {
        "str" | "value" => Value::string(s),
        "null" => Value::null(),
        "bool" => Value::new(Kind::Bool(
            ["yes", "true", "on"].contains(&s.to_lowercase().as_str()),
        )),
        "int" => {
            let s = s.replace('_', "");
            let (sign, digits) = if let Some(s) = s.strip_prefix('-') {
                ("-", s)
            } else {
                ("", s.strip_prefix('+').unwrap_or(&s))
            };
            if digits.contains(':') {
                let mut total = BigInt::from(0);
                for part in digits.split(':') {
                    let value = BigInt::parse_bytes(part.as_bytes(), 10)
                        .ok_or("ValueError: invalid integer")?;
                    total = total * 60 + value;
                }
                if sign == "-" {
                    total = -total;
                }
                Value::new(Kind::Int(total.to_string()))
            } else if let Some(s) = digits.strip_prefix("0b") {
                integer(&format!("{sign}{s}"), 2)?
            } else if let Some(s) = digits.strip_prefix("0x") {
                integer(&format!("{sign}{s}"), 16)?
            } else if digits.starts_with('0') && digits.len() > 1 {
                integer(&format!("{sign}{digits}"), 8)?
            } else {
                integer(&format!("{sign}{digits}"), 10)?
            }
        }
        "float" => {
            let s = s.replace('_', "").to_lowercase();
            let n = match s.as_str() {
                ".inf" | "+.inf" => f64::INFINITY,
                "-.inf" => f64::NEG_INFINITY,
                ".nan" => f64::NAN,
                _ => {
                    if s.contains(':') {
                        let mut n = 0.0;
                        let minus = s.starts_with('-');
                        for part in s.trim_start_matches(['-', '+']).split(':') {
                            n = n * 60.0
                                + part
                                    .parse::<f64>()
                                    .map_err(|_| "ValueError: invalid float")?;
                        }
                        if minus {
                            -n
                        } else {
                            n
                        }
                    } else {
                        s.parse().map_err(|_| {
                            format!(
                                "ValueError: could not convert string to float: {}",
                                repr(&s)
                            )
                        })?
                    }
                }
            };
            Value::new(Kind::Float(n))
        }
        "binary" => Value::new(Kind::Unsupported("bytes")),
        "timestamp" => Value::new(Kind::Unsupported(if s.len() == 10 {
            "date"
        } else {
            "datetime"
        })),
        _ => return Err(format!("unknown tag {tag}")),
    })
}
struct Reader<'a> {
    text: &'a str,
    events: Vec<(Event, Marker)>,
    at: usize,
    anchors: BTreeMap<usize, Value>,
}
impl Reader<'_> {
    fn next(&mut self) -> Result<(Event, Marker)> {
        let event = self
            .events
            .get(self.at)
            .cloned()
            .ok_or("YAML stream ended")?;
        self.at += 1;
        Ok(event)
    }
    fn mark(&self, m: Marker) -> String {
        self.mark_at(m.line(), m.col())
    }
    fn mark_at(&self, line: usize, col: usize) -> String {
        let snippet = self.text.lines().nth(line.saturating_sub(1)).unwrap_or("");
        if snippet.is_empty() {
            return String::new();
        }
        format!(
            "\n  in \"<unicode string>\", line {line}, column {}:\n    {snippet}\n    {}^",
            col + 1,
            " ".repeat(col)
        )
    }
    fn tag_mark(&self, mark: Marker) -> String {
        let line = self
            .text
            .lines()
            .nth(mark.line().saturating_sub(1))
            .unwrap_or("");
        let col = line
            .chars()
            .take(mark.col())
            .collect::<String>()
            .rfind('!')
            .map(|byte| line.get(..byte).unwrap_or("").chars().count())
            .unwrap_or(mark.col());
        self.mark_at(mark.line(), col)
    }
    fn tag(&self, tag: Option<Tag>, default: &str, mark: Marker) -> Result<String> {
        let Some(tag) = tag else {
            return Ok(default.into());
        };
        let tag = if tag.handle == "!!" || tag.handle == "tag:yaml.org,2002:" {
            tag.suffix
        } else if tag.handle.is_empty() && tag.suffix.starts_with("tag:yaml.org,2002:") {
            tag.suffix.trim_start_matches("tag:yaml.org,2002:").into()
        } else {
            return Err(format!(
                "yaml.constructor.ConstructorError: could not determine a constructor for the tag '{}'{}",
                format_args!("{}{}", tag.handle, tag.suffix),
                self.tag_mark(mark)
            ));
        };
        if [
            "str",
            "int",
            "float",
            "bool",
            "null",
            "binary",
            "timestamp",
            "seq",
            "map",
            "set",
            "omap",
            "pairs",
        ]
        .contains(&tag.as_str())
        {
            Ok(tag)
        } else {
            Err(format!(
                "yaml.constructor.ConstructorError: could not determine a constructor for the tag 'tag:yaml.org,2002:{tag}'{}",
                self.mark(mark)
            ))
        }
    }
    fn node(&mut self, depth: usize) -> Result<Value> {
        if depth > 500 {
            return Err("RecursionError: maximum recursion depth exceeded".into());
        }
        let (event, mark) = self.next()?;
        let (id, node) = match event {
            Event::Alias(id) => {
                return self
                    .anchors
                    .get(&id)
                    .cloned()
                    .ok_or("yaml.composer.ComposerError: found undefined alias".into());
            }
            Event::Scalar(s, style, id, tag) => {
                let default = if style == TScalarStyle::Plain {
                    implicit(&s)
                } else {
                    "str"
                };
                let tag = self.tag(tag, default, mark)?;
                (id, scalar(&s, &tag)?)
            }
            Event::SequenceStart(id, tag) => {
                let tag = self.tag(tag, "seq", mark)?;
                let node = Value::new(Kind::Seq(Vec::new()));
                if id > 0 {
                    self.anchors.insert(id, node.clone());
                }
                let mut values = Vec::new();
                while !matches!(self.events.get(self.at), Some((Event::SequenceEnd, _))) {
                    values.push(self.node(depth + 1)?);
                }
                self.next()?;
                let values = if tag == "omap" || tag == "pairs" {
                    values.into_iter().map(|value|{let rows=value.entries().ok_or("yaml.constructor.ConstructorError: expected a mapping of length 1")?;
                        if rows.len()!=1{return Err("yaml.constructor.ConstructorError: expected a single mapping item".into());}
                        let Some((key,value))=rows.into_iter().next()else{return Err("yaml.constructor.ConstructorError: expected a single mapping item".into());};
                        Ok(Value::new(Kind::Seq(vec![key,value])))
                    }).collect::<Result<Vec<_>>>()?
                } else {
                    values
                };
                *node.0.borrow_mut() = Kind::Seq(values);
                (id, node)
            }
            Event::MappingStart(id, tag) => {
                let tag = self.tag(tag, "map", mark)?;
                let node = Value::map();
                if id > 0 {
                    self.anchors.insert(id, node.clone());
                }
                let mut rows = Vec::new();
                let mut merged = Vec::new();
                while !matches!(self.events.get(self.at), Some((Event::MappingEnd, _))) {
                    let key_mark = self
                        .events
                        .get(self.at)
                        .map(|(_, m)| *m)
                        .ok_or("YAML key missing")?;
                    let merge = matches!(self.events.get(self.at),Some((Event::Scalar(s,TScalarStyle::Plain,_,None),_)) if s=="<<");
                    let key = self.node(depth + 1)?;
                    let value = self.node(depth + 1)?;
                    if merge {
                        if let Some(pairs) = value.entries() {
                            merged.extend(pairs);
                        } else if let Some(seq) = value.as_array() {
                            for value in seq.into_iter().rev() {
                                merged.extend(value.entries().ok_or("yaml.constructor.ConstructorError: expected a mapping for merging")?);
                            }
                        } else {
                            return Err("yaml.constructor.ConstructorError: expected a mapping or list of mappings for merging".into());
                        }
                    } else {
                        if matches!(key.kind(), Kind::Seq(_) | Kind::Map(_)) {
                            return Err(format!(
                                "yaml.constructor.ConstructorError: while constructing a mapping{}\nfound unhashable key{}",
                                self.mark(mark),
                                self.mark(key_mark)
                            ));
                        }
                        rows.push((key, value));
                    }
                }
                self.next()?;
                for (k, v) in merged.into_iter().chain(rows) {
                    node.insert(k, v)?;
                }
                if tag == "set" {
                    *node.0.borrow_mut() = Kind::Unsupported("set");
                }
                (id, node)
            }
            _ => return Err("YAML node missing".into()),
        };
        if id > 0 {
            self.anchors.insert(id, node.clone());
        }
        Ok(node)
    }
}
pub fn load(bytes: &[u8]) -> Result<Value> {
    let text = decode_utf8(bytes)?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut parser = Parser::new_from_str(text);
    let mut events = Vec::new();
    loop {
        let event = parser
            .next_token()
            .map_err(|e| format!("yaml.scanner.ScannerError: {e}"))?;
        let end = matches!(event.0, Event::StreamEnd);
        events.push(event);
        if end {
            break;
        }
    }
    let mut r = Reader {
        text,
        events,
        at: 0,
        anchors: BTreeMap::new(),
    };
    r.next()?;
    if matches!(r.events.get(r.at), Some((Event::StreamEnd, _))) {
        return Ok(Value::null());
    }
    r.next()?;
    let first = r
        .events
        .get(r.at)
        .map(|(_, m)| *m)
        .ok_or("YAML node missing")?;
    let first_empty = matches!(r.events.get(r.at),Some((Event::Scalar(s,_,_,_),_)) if s.is_empty());
    let value = r.node(0)?;
    r.next()?;
    if let Some((Event::DocumentStart, m)) = r.events.get(r.at) {
        return Err(format!(
            "yaml.composer.ComposerError: expected a single document in the stream{}\nbut found another document{}",
            if first_empty {String::new()}else{let line=r.text.lines().nth(first.line().saturating_sub(1)).unwrap_or(""); r.mark_at(first.line(),line.chars().take_while(|c|c.is_whitespace()).count())},
            r.mark(*m)
        ));
    }
    Ok(value)
}
