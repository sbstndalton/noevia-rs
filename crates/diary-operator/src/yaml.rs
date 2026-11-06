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
            r"^[0-9]{4}-[0-9]{1,2}-[0-9]{1,2}(?:[Tt]|[ \t]+)[0-9]{1,2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9]{1,2}(?::[0-9]{2})?))?$",
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
        "bool" => {
            let value = s.to_lowercase();
            Value::new(Kind::Bool(match value.as_str() {
                "yes" | "true" | "on" => true,
                "no" | "false" | "off" => false,
                _ => return Err(format!("KeyError: {}", repr(&value))),
            }))
        }
        "int" => {
            let s = s.replace('_', "");
            if s.is_empty() {
                return Err("IndexError: string index out of range".into());
            }
            let (sign, digits) = if let Some(s) = s.strip_prefix('-') {
                ("-", s)
            } else {
                ("", s.strip_prefix('+').unwrap_or(&s))
            };
            if digits.is_empty() {
                return Err("IndexError: string index out of range".into());
            }
            if digits.contains(':') {
                let mut total = BigInt::from(0);
                for part in digits.split(':') {
                    let parsed = integer(part.trim(), 10).map_err(|e| {
                        if e.starts_with("ValueError: Exceeds") {
                            e
                        } else {
                            format!(
                                "ValueError: invalid literal for int() with base 10: {}",
                                repr(part)
                            )
                        }
                    })?;
                    let Kind::Int(parsed) = parsed.kind() else {
                        return Err("integer construction failed".into());
                    };
                    let value = BigInt::parse_bytes(parsed.as_bytes(), 10)
                        .ok_or("integer construction failed")?;
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
                                + part.trim().parse::<f64>().map_err(|_| {
                                    format!(
                                        "ValueError: could not convert string to float: {}",
                                        repr(part)
                                    )
                                })?;
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
        "timestamp" => timestamp(s)?,
        _ => return Err(format!("unknown tag {tag}")),
    })
}
fn timestamp(s: &str) -> Result<Value> {
    let regex=Regex::new(r"^(?P<year>[0-9]{4})-(?P<month>[0-9]{1,2})-(?P<day>[0-9]{1,2})(?:(?:[Tt]|[ \t]+)(?P<hour>[0-9]{1,2}):(?P<minute>[0-9]{2}):(?P<second>[0-9]{2})(?:\.[0-9]*)?(?:[ \t]*(?:Z|(?P<tzsign>[-+])(?P<tzhour>[0-9]{1,2})(?::(?P<tzminute>[0-9]{2}))?))?)?$").map_err(|_|"timestamp schema")?;
    let parts = regex
        .captures(s)
        .ok_or("AttributeError: 'NoneType' object has no attribute 'groupdict'")?;
    let n = |key: &str| {
        parts
            .name(key)
            .and_then(|m| m.as_str().parse::<u32>().ok())
            .unwrap_or(0)
    };
    let year = n("year");
    let month = n("month");
    let day = n("day");
    if year == 0 {
        return Err("ValueError: year 0 is out of range".into());
    }
    if !(1..=12).contains(&month) {
        return Err("ValueError: month must be in 1..12".into());
    }
    let max = match month {
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day == 0 || day > max {
        return Err("ValueError: day is out of range for month".into());
    }
    for (key, max) in [("hour", 23), ("minute", 59), ("second", 59)] {
        if n(key) > max {
            return Err(format!("ValueError: {key} must be in 0..{max}"));
        }
    }
    if parts.name("tzsign").is_some() {
        let sign = if parts.name("tzsign").is_some_and(|m| m.as_str() == "-") {
            -1i64
        } else {
            1
        };
        let minutes = sign * (i64::from(n("tzhour")) * 60 + i64::from(n("tzminute")));
        if minutes.abs() >= 1440 {
            let days = minutes.div_euclid(1440);
            let seconds = minutes.rem_euclid(1440) * 60;
            let detail = if seconds == 0 {
                format!("days={days}")
            } else {
                format!("days={days}, seconds={seconds}")
            };
            return Err(format!("ValueError: offset must be a timedelta strictly between -timedelta(hours=24) and timedelta(hours=24), not datetime.timedelta({detail})."));
        }
    }
    Ok(Value::new(Kind::Unsupported(
        if parts.name("hour").is_some() {
            "datetime"
        } else {
            "date"
        },
    )))
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
        let mut col = col;
        let chars = line.chars().collect::<Vec<_>>();
        while col > 0 && chars.get(col - 1) == Some(&'!') {
            col -= 1;
        }
        self.mark_at(mark.line(), col)
    }
    fn shape(&self, tag: &str, actual: &str, mark: Marker) -> Result<()> {
        let expected = match tag {
            "seq" | "omap" | "pairs" => "sequence",
            "map" | "set" => "mapping",
            _ => "scalar",
        };
        if actual != expected {
            return Err(format!("yaml.constructor.ConstructorError: expected a {expected} node, but found {actual}{}",self.tag_mark(mark)));
        }
        Ok(())
    }
    fn binary(&self, s: &str, mark: Marker) -> Result<()> {
        let chars = s.chars().collect::<Vec<_>>();
        if let Some(at) = chars.iter().position(|c| !c.is_ascii()) {
            let len = chars.iter().skip(at).take_while(|c| !c.is_ascii()).count();
            let detail = if len == 1 {
                let c = chars.get(at).copied().unwrap_or('\0') as u32;
                let escaped = if c < 256 {
                    format!("\\x{c:02x}")
                } else if c < 65536 {
                    format!("\\u{c:04x}")
                } else {
                    format!("\\U{c:08x}")
                };
                format!("character '{escaped}' in position {at}")
            } else {
                format!("characters in position {at}-{}", at + len - 1)
            };
            return Err(format!("yaml.constructor.ConstructorError: failed to convert base64 data into ascii: 'ascii' codec can't encode {detail}: ordinal not in range(128){}",self.tag_mark(mark)));
        }
        let data = s
            .bytes()
            .filter(|c| c.is_ascii_alphanumeric() || b"+/=".contains(c))
            .collect::<Vec<_>>();
        // Match binascii's non-strict quantum and padding validation.
        let mut position = 0usize;
        let mut pads = 0usize;
        let mut count = 0usize;
        let mut complete = false;
        for c in data {
            if c == b'=' {
                pads += 1;
                if position >= 2 && position + pads >= 4 {
                    complete = true;
                    break;
                }
            } else {
                pads = 0;
                count += 1;
                position = (position + 1) % 4;
            }
        }
        let error = if complete || position == 0 {
            None
        } else if position == 1 {
            Some(format!("Invalid base64-encoded string: number of data characters ({count}) cannot be 1 more than a multiple of 4"))
        } else {
            Some("Incorrect padding".into())
        };
        if let Some(e) = error {
            return Err(format!(
                "yaml.constructor.ConstructorError: failed to decode base64 data: {e}{}",
                self.tag_mark(mark)
            ));
        }
        Ok(())
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
                self.shape(&tag, "scalar", mark)?;
                if tag == "binary" {
                    self.binary(&s, mark)?;
                }
                (id, scalar(&s, &tag)?)
            }
            Event::SequenceStart(id, tag) => {
                let tag = self.tag(tag, "seq", mark)?;
                self.shape(&tag, "sequence", mark)?;
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
                // SafeConstructor's scalar exception: an unquoted '=' entry in
                // a mapping supplies its raw scalar value (for !!str, !!int, ...).
                let scalar_value = match (
                    self.events.get(self.at),
                    self.events.get(self.at + 1),
                    self.events.get(self.at + 2),
                ) {
                    (
                        Some((Event::Scalar(k, TScalarStyle::Plain, _, None), _)),
                        Some((Event::Scalar(v, _, _, _), _)),
                        Some((Event::MappingEnd, _)),
                    ) if k == "=" => Some(v.clone()),
                    _ => None,
                };
                if !["map", "set", "seq", "omap", "pairs"].contains(&tag.as_str()) {
                    if let Some(value) = scalar_value {
                        self.at += 3;
                        let node = scalar(&value, &tag)?;
                        if id > 0 {
                            self.anchors.insert(id, node.clone());
                        }
                        return Ok(node);
                    }
                }
                self.shape(&tag, "mapping", mark)?;
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
fn source_mark(text: &str, line: usize, col: usize) -> String {
    let snippet = text.split('\n').nth(line.saturating_sub(1)).unwrap_or("");
    format!(
        "\n  in \"<unicode string>\", line {line}, column {}:\n    {snippet}\n    {}^",
        col + 1,
        " ".repeat(col)
    )
}
fn syntax_error(
    text: &str,
    events: &[(Event, Marker)],
    error: yaml_rust2::scanner::ScanError,
) -> String {
    let info = error.info();
    let m = error.marker();
    let at = source_mark(text, m.line(), m.col());
    if info.contains("found unknown anchor") {
        let line = text
            .split('\n')
            .nth(m.line().saturating_sub(1))
            .unwrap_or("");
        let name = line
            .chars()
            .skip(m.col() + 1)
            .take_while(|c| !c.is_whitespace() && !",]}{".contains(*c))
            .collect::<String>();
        return format!(
            "yaml.composer.ComposerError: found undefined alias {}{at}",
            repr(&name)
        );
    }
    if info.contains("while scanning a quoted scalar") && info.contains("unexpected end of stream")
    {
        let line = text.split('\n').count();
        let col = text.rsplit('\n').next().unwrap_or("").chars().count();
        return format!("yaml.scanner.ScannerError: while scanning a quoted scalar{at}\nfound unexpected end of stream{}",source_mark(text,line,col));
    }
    if info.contains("while parsing a flow sequence") {
        let context = events
            .iter()
            .rev()
            .find_map(|(e, m)| {
                matches!(e, Event::SequenceStart(..)).then_some(source_mark(
                    text,
                    m.line(),
                    m.col(),
                ))
            })
            .unwrap_or_default();
        let token = text
            .split('\n')
            .nth(m.line().saturating_sub(1))
            .and_then(|s| s.chars().nth(m.col()))
            .map(|c| c.to_string())
            .unwrap_or("<stream end>".into());
        return format!("yaml.parser.ParserError: while parsing a flow sequence{context}\nexpected ',' or ']', but got '{token}'{at}");
    }
    if info.contains("did not find expected node content") {
        let token = text
            .split('\n')
            .nth(m.line().saturating_sub(1))
            .and_then(|s| s.chars().nth(m.col()))
            .map(|c| c.to_string())
            .unwrap_or("<stream end>".into());
        return format!("yaml.parser.ParserError: while parsing a flow node\nexpected the node content, but found '{token}'{at}");
    }
    format!("yaml.scanner.ScannerError: {error}")
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
            .map_err(|e| syntax_error(text, &events, e))?;
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
