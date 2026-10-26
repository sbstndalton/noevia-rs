//! Source-only bounded subprocess transport for pure Diary write/edit transforms.
use diary_corpus_write as corpus;
use serde_json::{json, Value};
use std::io::{self, Read, Write};

mod unicode;

const INPUT_MAGIC: &[u8; 4] = b"DCW1";
const OUTPUT_MAGIC: &[u8; 4] = b"DWR1";
const END: &[u8; 4] = b"END1";
const MIB: usize = 1024 * 1024;
const MAX_INPUT_FRAME: usize = 272 * MIB;
const MAX_OUTPUT_FRAME: usize = 512 * MIB;
const MAX_TOKEN_BYTES: usize = 4096;

#[derive(Debug, PartialEq, Eq)]
enum Failure {
    InvalidFrame,
    InvalidRequest,
    Capacity,
    Io,
    Encode,
}

impl Failure {
    fn code(&self) -> &'static str {
        match self {
            Self::InvalidFrame => "invalid-frame",
            Self::InvalidRequest => "invalid-request",
            Self::Capacity => "capacity",
            Self::Io => "io",
            Self::Encode => "encode",
        }
    }
}

struct BoundedVec {
    bytes: Vec<u8>,
    limit: usize,
    overflowed: bool,
}

impl Write for BoundedVec {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.overflowed = true;
            return Err(io::Error::other("response limit"));
        }
        self.bytes
            .try_reserve(data.len())
            .map_err(|_| io::Error::other("allocation refused"))?;
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn py_space(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{001c}'..='\u{0020}' | '\u{0085}' | '\u{00a0}'
        | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}
fn line_break(c: char) -> bool {
    matches!(
        c,
        '\r' | '\n'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{001c}'
            | '\u{001d}'
            | '\u{001e}'
            | '\u{0085}'
            | '\u{2028}'
            | '\u{2029}'
    )
}
fn nonstructural_token(s: &str) -> bool {
    s.len() <= MAX_TOKEN_BYTES && !s.chars().any(line_break)
}

fn render_header(time_token: &str, topic: &str) -> Result<String, Failure> {
    if !nonstructural_token(time_token) {
        return Err(Failure::InvalidRequest);
    }
    let mut sanitized = String::with_capacity(topic.len());
    let mut was_break = false;
    for c in topic.chars() {
        if line_break(c) {
            if !was_break {
                sanitized.push(' ');
            }
            was_break = true;
        } else {
            sanitized.push(c);
            was_break = false;
        }
    }
    let topic = sanitized.trim_matches(py_space);
    if topic.is_empty() {
        Ok(format!("### {time_token}"))
    } else {
        Ok(format!("### {time_token} — {topic}"))
    }
}

fn ascii_letters(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphabetic())
}
fn digits(s: &str, min: usize, max: usize) -> bool {
    let count = s.chars().count();
    (min..=max).contains(&count) && s.chars().all(|c| unicode::decimal(c).is_some())
}
fn day_header_line(line: &str) -> bool {
    let Some(mut rest) = line.strip_prefix("## ") else {
        return false;
    };
    rest = rest.trim_end_matches(py_space);
    if let Some((dow, tail)) = rest.split_once(", ") {
        if ascii_letters(dow) {
            rest = tail;
        }
    }
    let Some((month, tail)) = rest.split_once(' ') else {
        return false;
    };
    if !ascii_letters(month) {
        return false;
    }
    let Some((day, year)) = tail.split_once(", ") else {
        return false;
    };
    digits(day, 1, 2) && digits(year, 4, 4)
}
fn drop_trailing_blanks(lines: &mut Vec<String>) {
    while lines
        .last()
        .is_some_and(|line| line.trim_matches(py_space).is_empty())
    {
        lines.pop();
    }
}

/// Python append_to_month_text with its canonical header supplied by the
/// Python owner. This preserves process-locale strftime output as data.
fn append_with_header(
    current: Option<&str>,
    header: &str,
    sub_header: &str,
    body: &str,
) -> Result<String, Failure> {
    if !header.starts_with("## ") || !nonstructural_token(header) {
        return Err(Failure::InvalidRequest);
    }
    let block = corpus::render_subsection_block(sub_header, body);
    let block_lines: Vec<&str> = block.trim_end_matches('\n').split('\n').collect();
    let Some(current) = current.filter(|text| !text.trim_matches(py_space).is_empty()) else {
        return Ok(format!("{header}\n\n{}\n", block_lines.join("\n")));
    };
    let mut lines: Vec<String> = current
        .trim_end_matches('\n')
        .split('\n')
        .map(str::to_owned)
        .collect();
    if current.contains(header) {
        if let Some(start) = lines
            .iter()
            .rposition(|line| line.trim_matches(py_space) == header.trim_matches(py_space))
        {
            let end = lines
                .iter()
                .enumerate()
                .skip(start + 1)
                .find(|(_, line)| day_header_line(line))
                .map_or(lines.len(), |(i, _)| i);
            let mut tail = lines.split_off(end);
            drop_trailing_blanks(&mut lines);
            lines.push(String::new());
            lines.extend(block_lines.iter().map(|line| (*line).to_owned()));
            lines.append(&mut tail);
            return Ok(format!("{}\n", lines.join("\n").trim_end_matches('\n')));
        }
    }
    drop_trailing_blanks(&mut lines);
    lines.extend([String::new(), header.to_owned(), String::new()]);
    lines.extend(block_lines.iter().map(|line| (*line).to_owned()));
    Ok(format!("{}\n", lines.join("\n").trim_end_matches('\n')))
}

fn get<'a>(request: &'a Value, field: &str) -> Result<&'a str, Failure> {
    request
        .get(field)
        .and_then(Value::as_str)
        .ok_or(Failure::InvalidRequest)
}
fn nullable<'a>(request: &'a Value, field: &str) -> Result<Option<&'a str>, Failure> {
    match request.get(field) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        _ => Err(Failure::InvalidRequest),
    }
}
fn valid_keys(request: &Value, keys: &[&str]) -> bool {
    request.as_object().is_some_and(|fields| {
        fields.len() == keys.len() && fields.keys().all(|key| keys.contains(&key.as_str()))
    })
}

fn transform(request: &Value) -> Result<Value, Failure> {
    if request.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(Failure::InvalidRequest);
    }
    let op = get(request, "op")?;
    match op {
        "prepare_append" => {
            if !valid_keys(
                request,
                &[
                    "version",
                    "op",
                    "time_token",
                    "topic",
                    "me",
                    "assistant",
                    "xid",
                ],
            ) {
                return Err(Failure::InvalidRequest);
            }
            let rendered = render_header(get(request, "time_token")?, get(request, "topic")?)?;
            let sub_header = rendered
                .strip_prefix("### ")
                .ok_or(Failure::InvalidRequest)?;
            let body = corpus::render_exchange(
                get(request, "me")?,
                get(request, "assistant")?,
                get(request, "xid")?,
            );
            Ok(json!({"version":1,"op":op,"sub_header":sub_header,"body":body}))
        }
        "apply_append" => {
            if !valid_keys(
                request,
                &[
                    "version",
                    "op",
                    "current",
                    "day_header",
                    "sub_header",
                    "body",
                    "xid",
                ],
            ) {
                return Err(Failure::InvalidRequest);
            }
            let current = nullable(request, "current")?;
            let xid = get(request, "xid")?;
            let day_header = get(request, "day_header")?;
            let sub_header = get(request, "sub_header")?;
            let body = get(request, "body")?;
            if let Some(text) = current.filter(|text| corpus::has_marker(text, xid)) {
                return Ok(json!({"version":1,"op":op,"changed":false,"text":text}));
            }
            let text = append_with_header(current, day_header, sub_header, body)?;
            Ok(json!({"version":1,"op":op,"changed":true,"text":text}))
        }
        "inspect_edit" => {
            if !valid_keys(
                request,
                &["version", "op", "current", "xid", "new_me", "new_assistant"],
            ) {
                return Err(Failure::InvalidRequest);
            }
            let current = nullable(request, "current")?;
            let xid = get(request, "xid")?;
            let new_me = get(request, "new_me")?;
            let new_assistant = get(request, "new_assistant")?;
            let edited = current
                .and_then(|text| corpus::replace_exchange_text(text, xid, new_me, new_assistant));
            if let Some(edited) = edited {
                let current_text = corpus::exchange_text(current, xid);
                let current_hash = corpus::exchange_hash(current, xid);
                let edited_hash = corpus::exchange_hash(Some(&edited), xid);
                Ok(
                    json!({"version":1,"op":op,"found":true,"current_text":current_text,"current_hash":current_hash,"edited_text":edited,"edited_hash":edited_hash}),
                )
            } else {
                Ok(
                    json!({"version":1,"op":op,"found":false,"current_text":null,"current_hash":null,"edited_text":null,"edited_hash":null}),
                )
            }
        }
        "apply_edit" => {
            if !valid_keys(
                request,
                &["version", "op", "current", "xid", "new_me", "new_assistant"],
            ) {
                return Err(Failure::InvalidRequest);
            }
            let current = nullable(request, "current")?;
            let xid = get(request, "xid")?;
            let new_me = get(request, "new_me")?;
            let new_assistant = get(request, "new_assistant")?;
            let edited = current
                .and_then(|text| corpus::replace_exchange_text(text, xid, new_me, new_assistant));
            if let Some(edited) = edited {
                Ok(
                    json!({"version":1,"op":op,"found":true,"changed":Some(edited.as_str()) != current,"text":edited}),
                )
            } else {
                Ok(json!({"version":1,"op":op,"found":false,"changed":false,"text":null}))
            }
        }
        _ => Err(Failure::InvalidRequest),
    }
}

// Version 1 has at most seven fields and only scalar/string/null values.
// Detect duplicate keys before Value could silently replace an earlier value.
struct Request(Value);

impl<'de> serde::Deserialize<'de> for Request {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Fields;
        impl<'de> serde::de::Visitor<'de> for Fields {
            type Value = Request;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a bounded versioned request object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Request, A::Error> {
                let mut fields = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if fields.len() >= 7 || fields.contains_key(&key) {
                        return Err(serde::de::Error::custom(
                            "duplicate or excessive request field",
                        ));
                    }
                    fields.insert(key, map.next_value::<Value>()?);
                }
                Ok(Request(Value::Object(fields)))
            }
        }
        deserializer.deserialize_map(Fields)
    }
}

fn run<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    input_limit: usize,
    output_limit: usize,
) -> Result<(), Failure> {
    let mut magic = [0u8; 4];
    reader
        .read_exact(&mut magic)
        .map_err(|_| Failure::InvalidFrame)?;
    if &magic != INPUT_MAGIC {
        return Err(Failure::InvalidFrame);
    }
    let mut length = [0u8; 8];
    reader
        .read_exact(&mut length)
        .map_err(|_| Failure::InvalidFrame)?;
    let length = usize::try_from(u64::from_be_bytes(length)).map_err(|_| Failure::Capacity)?;
    if length > input_limit.saturating_sub(16) {
        return Err(Failure::Capacity);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| Failure::Capacity)?;
    bytes.resize(length, 0);
    reader
        .read_exact(&mut bytes)
        .map_err(|_| Failure::InvalidFrame)?;
    reader
        .read_exact(&mut magic)
        .map_err(|_| Failure::InvalidFrame)?;
    if &magic != END || reader.read(&mut magic[..1]).map_err(|_| Failure::Io)? != 0 {
        return Err(Failure::InvalidFrame);
    }
    let request: Request = serde_json::from_slice(&bytes).map_err(|_| Failure::InvalidRequest)?;
    let response = transform(&request.0)?;
    let mut out = BoundedVec {
        bytes: Vec::new(),
        limit: output_limit.checked_sub(16).ok_or(Failure::Capacity)?,
        overflowed: false,
    };
    if serde_json::to_writer(&mut out, &response).is_err() {
        return Err(if out.overflowed {
            Failure::Capacity
        } else {
            Failure::Encode
        });
    }
    writer.write_all(OUTPUT_MAGIC).map_err(|_| Failure::Io)?;
    writer
        .write_all(
            &u64::try_from(out.bytes.len())
                .map_err(|_| Failure::Capacity)?
                .to_be_bytes(),
        )
        .map_err(|_| Failure::Io)?;
    writer.write_all(&out.bytes).map_err(|_| Failure::Io)?;
    writer.write_all(END).map_err(|_| Failure::Io)?;
    writer.flush().map_err(|_| Failure::Io)?;
    Ok(())
}

fn main() -> std::process::ExitCode {
    if std::env::args().skip(1).collect::<Vec<_>>() != ["transform-v1"] {
        eprintln!("invalid-argument");
        return std::process::ExitCode::FAILURE;
    }
    match run(
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        MAX_INPUT_FRAME,
        MAX_OUTPUT_FRAME,
    ) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", error.code());
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn framed(bytes: &[u8]) -> Result<Vec<u8>, Failure> {
        let mut frame = INPUT_MAGIC.to_vec();
        frame.extend_from_slice(
            &u64::try_from(bytes.len())
                .map_err(|_| Failure::Capacity)?
                .to_be_bytes(),
        );
        frame.extend_from_slice(bytes);
        frame.extend_from_slice(END);
        Ok(frame)
    }

    #[test]
    fn duplicate_fields_refuse_without_a_response() -> Result<(), Failure> {
        for bytes in [
            br#"{"version":2,"version":1,"op":"apply_edit","current":null,"xid":"x","new_me":"m","new_assistant":"a"}"#.as_slice(),
            br#"{"version":1,"op":"apply_edit","current":null,"xid":"bad","xid":"x","new_me":"m","new_assistant":"a"}"#.as_slice(),
        ] {
            let mut output = Vec::new();
            assert_eq!(run(&mut Cursor::new(framed(bytes)?), &mut output, MAX_INPUT_FRAME, MAX_OUTPUT_FRAME), Err(Failure::InvalidRequest));
            assert!(output.is_empty());
        }
        Ok(())
    }

    #[test]
    fn capacity_refusals_emit_no_partial_frame() -> Result<(), Failure> {
        let input = framed(br#"{"version":1,"op":"apply_edit","current":null,"xid":"x","new_me":"m","new_assistant":"a"}"#)?;
        let mut output = Vec::new();
        assert_eq!(
            run(
                &mut Cursor::new(&input),
                &mut output,
                input.len().checked_sub(1).ok_or(Failure::Capacity)?,
                MAX_OUTPUT_FRAME
            ),
            Err(Failure::Capacity)
        );
        assert!(output.is_empty());
        assert_eq!(
            run(&mut Cursor::new(&input), &mut output, MAX_INPUT_FRAME, 16),
            Err(Failure::Capacity)
        );
        assert!(output.is_empty());
        Ok(())
    }
}
