//! `server/task-packet.cjs`, schema 1 (noevia#740): the hand-off contract between the gatherer and
//! the answer model. Strict: unknown keys anywhere, a wrong type, an empty goal, a control
//! character or any bound exceeded rejects the whole packet with the JS's `path: rule` message
//! (the path names the offending key, as the JS does; never a value). A rendered packet is one
//! [`frame_untrusted`](crate::framing::frame_untrusted) block.

use crate::framing::frame_untrusted;
use crate::js::{trim, units};
use crate::json::{self, push_ascii, push_str, Value};

/// The schema number.
pub const PACKET_SCHEMA: u32 = 1;
/// The source kinds, in the JS's order.
pub const SOURCE_KINDS: [&str; 5] = ["tool", "web", "file", "project", "chat"];
/// `LIMITS.goalChars`
pub const GOAL_CHARS: usize = 400;
/// `LIMITS.facts`
pub const FACTS: usize = 24;
/// `LIMITS.factChars`
pub const FACT_CHARS: usize = 600;
/// `LIMITS.quoteChars`
pub const QUOTE_CHARS: usize = 400;
/// `LIMITS.refChars`
pub const REF_CHARS: usize = 300;
/// `LIMITS.constraints`
pub const CONSTRAINTS: usize = 12;
/// `LIMITS.open_questions`
pub const OPEN_QUESTIONS: usize = 12;
/// `LIMITS.itemChars`
pub const ITEM_CHARS: usize = 300;
/// `LIMITS.packetBytes`: the serialized packet, UTF-8.
pub const PACKET_BYTES: usize = 16384;
/// `LIMITS.inputChars`: the raw model output `parsePacket` looks at.
pub const INPUT_CHARS: usize = 32768;
/// The depth a packet is parsed to: the deepest value read is a source's kind or ref (depth 4).
const CAP: usize = 6;

/// One fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fact {
    /// The fact.
    pub text: Vec<u16>,
    /// One of [`SOURCE_KINDS`].
    pub kind: &'static str,
    /// Where it came from.
    pub reference: Vec<u16>,
    /// A supporting quote, when not empty.
    pub quote: Option<Vec<u16>>,
}

/// A validated packet (trimmed copies).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// The goal.
    pub goal: Vec<u16>,
    /// The facts.
    pub facts: Vec<Fact>,
    /// The constraints.
    pub constraints: Vec<Vec<u16>>,
    /// The open questions.
    pub open_questions: Vec<Vec<u16>>,
}

/// Why `parsePacket` refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `reason: 'invalid-json'` with its fixed error.
    InvalidJson(&'static str),
    /// `reason: 'schema'` with `path: rule`.
    Schema(Vec<u16>),
}

fn err(path: &[u16], rule: &str) -> Vec<u16> {
    let mut e = path.to_vec();
    e.extend(units(": "));
    e.extend(units(rule));
    e
}

fn join(path: &[u16], tail: &str) -> Vec<u16> {
    let mut p = path.to_vec();
    p.extend(units(tail));
    p
}

/// `C0 controls except tab and newline, DEL, C1, bidi overrides/isolates, zero-width, BOM`.
const fn bad_char(c: u16) -> bool {
    matches!(c, 0x00..=0x08 | 0x0b..=0x1f | 0x7f..=0x9f | 0x200b..=0x200f | 0x202a..=0x202e | 0x2066..=0x2069 | 0xfeff)
}

fn text(v: Option<&Value>, path: &[u16], max: usize, required: bool) -> Result<Vec<u16>, Vec<u16>> {
    let Some(Value::Str(s)) = v else {
        return Err(err(path, "must be a string"));
    };
    let t = trim(s);
    if required && t.is_empty() {
        return Err(err(path, "must not be empty"));
    }
    if t.len() > max {
        return Err(err(path, &format!("longer than {max} characters")));
    }
    if t.iter().any(|&c| bad_char(c)) {
        return Err(err(path, "contains a control or formatting character"));
    }
    Ok(t.to_vec())
}

fn only_keys<'a>(
    v: Option<&'a Value>,
    allowed: &[&str],
    path: &[u16],
) -> Result<&'a [(Vec<u16>, Value)], Vec<u16>> {
    let Some(Value::Obj(m)) = v else {
        return Err(err(path, "must be an object"));
    };
    for (k, _) in m {
        if !allowed
            .iter()
            .any(|a| k.iter().copied().eq(a.encode_utf16()))
        {
            let mut p = path.to_vec();
            p.push(0x2e);
            p.extend_from_slice(k);
            return Err(err(&p, "is not part of the schema"));
        }
    }
    Ok(m)
}

fn list<'a>(v: Option<&'a Value>, path: &[u16], max: usize) -> Result<&'a [Value], Vec<u16>> {
    let Some(Value::Arr(items)) = v else {
        return Err(err(path, "must be an array"));
    };
    if items.len() > max {
        return Err(err(path, &format!("has more than {max} items")));
    }
    Ok(items)
}

fn has(m: &[(Vec<u16>, Value)], key: &str) -> bool {
    m.iter()
        .any(|(k, _)| k.iter().copied().eq(key.encode_utf16()))
}

fn validate_inner(raw: &Value) -> Result<Packet, Vec<u16>> {
    let root = units("$");
    let top = only_keys(
        Some(raw),
        &[
            "packet_schema",
            "goal",
            "facts",
            "constraints",
            "open_questions",
        ],
        &root,
    )?;
    if raw.get("packet_schema") != Some(&Value::Num(f64::from(PACKET_SCHEMA))) {
        return Err(err(&units("$.packet_schema"), "must be 1"));
    }
    for k in ["goal", "facts", "constraints", "open_questions"] {
        if !has(top, k) {
            return Err(err(&units(&format!("$.{k}")), "is required"));
        }
    }
    let goal = text(raw.get("goal"), &units("$.goal"), GOAL_CHARS, true)?;
    let mut facts = Vec::new();
    for (i, f) in list(raw.get("facts"), &units("$.facts"), FACTS)?
        .iter()
        .enumerate()
    {
        let p = units(&format!("$.facts[{i}]"));
        let fm = only_keys(Some(f), &["text", "source", "quote"], &p)?;
        let sp = join(&p, ".source");
        only_keys(f.get("source"), &["kind", "ref"], &sp)?;
        let source = f.get("source");
        let kind = source
            .and_then(|s| s.get("kind"))
            .and_then(Value::as_str)
            .and_then(|k| {
                SOURCE_KINDS
                    .iter()
                    .find(|n| k.iter().copied().eq(n.encode_utf16()))
            })
            .ok_or_else(|| {
                err(
                    &join(&sp, ".kind"),
                    &format!("must be one of {}", SOURCE_KINDS.join(", ")),
                )
            })?;
        let t = text(f.get("text"), &join(&p, ".text"), FACT_CHARS, true)?;
        let reference = text(
            source.and_then(|s| s.get("ref")),
            &join(&sp, ".ref"),
            REF_CHARS,
            true,
        )?;
        let quote = if has(fm, "quote") {
            Some(text(
                f.get("quote"),
                &join(&p, ".quote"),
                QUOTE_CHARS,
                false,
            )?)
            .filter(|q| !q.is_empty())
        } else {
            None
        };
        facts.push(Fact {
            text: t,
            kind,
            reference,
            quote,
        });
    }
    let items = |key: &str, max: usize| -> Result<Vec<Vec<u16>>, Vec<u16>> {
        let p = units(&format!("$.{key}"));
        list(raw.get(key), &p, max)?
            .iter()
            .enumerate()
            .map(|(i, c)| text(Some(c), &join(&p, &format!("[{i}]")), ITEM_CHARS, true))
            .collect()
    };
    let constraints = items("constraints", CONSTRAINTS)?;
    let open_questions = items("open_questions", OPEN_QUESTIONS)?;
    let packet = Packet {
        goal,
        facts,
        constraints,
        open_questions,
    };
    if packet_json(&packet).len() > PACKET_BYTES {
        return Err(err(&root, &format!("larger than {PACKET_BYTES} bytes")));
    }
    Ok(packet)
}

/// `validatePacket(raw)` on a decoded value: the packet, or the `path: rule` error.
pub fn validate(raw: &Value) -> Result<Packet, Vec<u16>> {
    validate_inner(raw)
}

/// `JSON.stringify(packet)` as UTF-8.
pub fn packet_json(p: &Packet) -> Vec<u8> {
    let mut o = Vec::new();
    o.extend_from_slice(b"{\"packet_schema\":1,\"goal\":");
    push_str(&mut o, &p.goal);
    o.extend_from_slice(b",\"facts\":[");
    for (i, f) in p.facts.iter().enumerate() {
        if i > 0 {
            o.push(b',');
        }
        o.extend_from_slice(b"{\"text\":");
        push_str(&mut o, &f.text);
        o.extend_from_slice(b",\"source\":{\"kind\":");
        push_ascii(&mut o, f.kind);
        o.extend_from_slice(b",\"ref\":");
        push_str(&mut o, &f.reference);
        o.push(b'}');
        if let Some(q) = &f.quote {
            o.extend_from_slice(b",\"quote\":");
            push_str(&mut o, q);
        }
        o.push(b'}');
    }
    o.extend_from_slice(b"],\"constraints\":");
    push_list(&mut o, &p.constraints);
    o.extend_from_slice(b",\"open_questions\":");
    push_list(&mut o, &p.open_questions);
    o.push(b'}');
    o
}

fn push_list(o: &mut Vec<u8>, items: &[Vec<u16>]) {
    o.push(b'[');
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            o.push(b',');
        }
        push_str(o, s);
    }
    o.push(b']');
}

/// The body of ```` ```json ... ``` ```` when `s` is exactly one fence (the JS's regex
/// `/^```(?:json)?\s*\n([\s\S]*?)\n?```$/`).
fn fenced(s: &[u16]) -> Option<&[u16]> {
    let tick = units("```");
    if s.get(..3) != Some(&tick[..]) {
        return None;
    }
    let n = s.len();
    let mut starts = Vec::new();
    if s.get(3..7) == Some(&units("json")[..]) {
        starts.push(7);
    }
    starts.push(3);
    for q in starts {
        let r = q + s
            .get(q..)
            .unwrap_or(&[])
            .iter()
            .take_while(|&&c| crate::js::is_js_space(c))
            .count();
        for k in (q..r).rev() {
            if s.get(k) != Some(&0x0a) {
                continue;
            }
            let p = k + 1;
            let end = if n >= 4 && n - 4 >= p && s.get(n - 4..) == Some(&units("\n```")[..]) {
                Some(n - 4)
            } else if n >= 3 && n - 3 >= p && s.get(n - 3..) == Some(&tick[..]) {
                Some(n - 3)
            } else {
                None
            };
            if let Some(e) = end {
                return s.get(p..e);
            }
        }
    }
    None
}

/// `parsePacket(output)`.
pub fn parse(output: &[u16]) -> Result<Packet, Refusal> {
    if trim(output).is_empty() || output.len() > INPUT_CHARS {
        return Err(Refusal::InvalidJson("$: no JSON"));
    }
    let mut s = trim(output);
    if let Some(body) = fenced(s) {
        s = trim(body);
    }
    let raw = json::parse(s, CAP).ok_or(Refusal::InvalidJson("$: not JSON"))?;
    validate(&raw).map_err(Refusal::Schema)
}

/// `renderPacket(packet, label)`: the packet as one untrusted-data block.
pub fn render(p: &Packet, label: &[u16]) -> Vec<u16> {
    let mut lines: Vec<Vec<u16>> = vec![
        units(&format!(
            "Task packet (schema {PACKET_SCHEMA}), condensed from the tool result."
        )),
        join(&units("Goal: "), "")
            .into_iter()
            .chain(p.goal.iter().copied())
            .collect(),
    ];
    if p.facts.is_empty() {
        lines.push(units("Facts: none found."));
    } else {
        lines.push(units("Facts:"));
        for (i, f) in p.facts.iter().enumerate() {
            let mut l = units(&format!("{}. ", i + 1));
            l.extend_from_slice(&f.text);
            l.extend(units(&format!(" [{}: ", f.kind)));
            l.extend_from_slice(&f.reference);
            l.push(0x5d);
            lines.push(l);
            if let Some(q) = f.quote.as_ref().filter(|q| !q.is_empty()) {
                let mut l = units("   Quote: \"");
                l.extend_from_slice(q);
                l.push(0x22);
                lines.push(l);
            }
        }
    }
    for (title, items) in [
        ("Constraints:", &p.constraints),
        ("Open questions:", &p.open_questions),
    ] {
        if !items.is_empty() {
            lines.push(units(title));
            for c in items {
                let mut l = units("- ");
                l.extend_from_slice(c);
                lines.push(l);
            }
        }
    }
    let body = lines.join(&0x0a);
    frame_untrusted(&units("task packet"), label, &body)
}

/// A packet as `renderPacket` receives it (the JSON of `parsePacket`'s packet): the shape only,
/// strictly typed; no limit is re-checked. `None` for anything else.
pub fn from_value(v: &Value) -> Option<Packet> {
    let Value::Obj(m) = v else { return None };
    if m.iter().any(|(k, _)| {
        ![
            "packet_schema",
            "goal",
            "facts",
            "constraints",
            "open_questions",
        ]
        .iter()
        .any(|a| k.iter().copied().eq(a.encode_utf16()))
    }) || v.get("packet_schema") != Some(&Value::Num(1.0))
    {
        return None;
    }
    let strs = |v: Option<&Value>| -> Option<Vec<Vec<u16>>> {
        let Some(Value::Arr(a)) = v else { return None };
        a.iter().map(|x| x.as_str().map(<[u16]>::to_vec)).collect()
    };
    let Some(Value::Arr(fs)) = v.get("facts") else {
        return None;
    };
    let mut facts = Vec::new();
    for f in fs {
        let Value::Obj(fm) = f else { return None };
        if fm.iter().any(|(k, _)| {
            !["text", "source", "quote"]
                .iter()
                .any(|a| k.iter().copied().eq(a.encode_utf16()))
        }) {
            return None;
        }
        let src = f.get("source")?;
        let Value::Obj(sm) = src else { return None };
        if sm.len() != 2 {
            return None;
        }
        let kind_units = src.get("kind")?.as_str()?;
        let kind = SOURCE_KINDS
            .iter()
            .find(|n| kind_units.iter().copied().eq(n.encode_utf16()))?;
        let quote = match f.get("quote") {
            None => None,
            Some(q) => Some(q.as_str()?.to_vec()),
        };
        facts.push(Fact {
            text: f.get("text")?.as_str()?.to_vec(),
            kind,
            reference: src.get("ref")?.as_str()?.to_vec(),
            quote,
        });
    }
    Some(Packet {
        goal: v.get("goal")?.as_str()?.to_vec(),
        facts,
        constraints: strs(v.get("constraints"))?,
        open_questions: strs(v.get("open_questions"))?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn p(t: &str) -> Result<Packet, Refusal> {
        parse(&units(t))
    }
    fn schema(t: &str) -> String {
        match p(t) {
            Err(Refusal::Schema(e)) => String::from_utf16(&e).unwrap(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses() {
        let ok = r#"{"packet_schema":1,"goal":" g ","facts":[{"text":"t","source":{"kind":"web","ref":"r"},"quote":" "}],"constraints":["c"],"open_questions":[]}"#;
        let pk = p(ok).unwrap();
        assert_eq!(pk.goal, units("g"));
        assert_eq!(pk.facts[0].quote, None);
        assert_eq!(p(&format!("```json \n{ok}\n```")).unwrap(), pk);
        assert_eq!(p(&format!("```\n{ok}```")).unwrap(), pk);
        assert_eq!(p("  "), Err(Refusal::InvalidJson("$: no JSON")));
        assert_eq!(p("{"), Err(Refusal::InvalidJson("$: not JSON")));
        assert_eq!(schema("[]"), "$: must be an object");
        assert_eq!(
            schema(r#"{"zz":1,"5":2}"#),
            "$.5: is not part of the schema"
        );
        assert_eq!(
            schema(r#"{"packet_schema":2}"#),
            "$.packet_schema: must be 1"
        );
        assert_eq!(schema(r#"{"packet_schema":1.0}"#), "$.goal: is required");
        assert_eq!(
            schema(
                r#"{"packet_schema":1,"goal":"g","facts":[{"text":"t","source":{"kind":"x","ref":"r"}}],"constraints":[],"open_questions":[]}"#
            ),
            "$.facts[0].source.kind: must be one of tool, web, file, project, chat"
        );
        assert_eq!(
            schema(
                r#"{"packet_schema":1,"goal":"a\rb","facts":[],"constraints":[],"open_questions":[]}"#
            ),
            "$.goal: contains a control or formatting character"
        );
        let r = render(&pk, &units("search"));
        assert!(String::from_utf16(&r).unwrap().contains("1. t [web: r]"));
        let back = json::parse_utf8(&packet_json(&pk), 8).unwrap();
        assert_eq!(from_value(&back).unwrap(), pk);
    }
}
