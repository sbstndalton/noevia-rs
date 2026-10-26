//! Framed, source-only Diary parser transport. Python remains the default caller.
use diary_corpus_parse::{chunks_from_days, parse_diary, Chunk, DaySection};
use serde::Serialize;
use std::io::{self, Read, Write};

const INPUT_MAGIC: &[u8; 4] = b"DCP1";
const INPUT_END: &[u8; 4] = b"END1";
const OUTPUT_MAGIC: &[u8; 4] = b"DCR1";
const OUTPUT_END: &[u8; 4] = b"END1";
const MIB: usize = 1024 * 1024;
const MAX_INPUT_BYTES: usize = 256 * MIB;
const MAX_OUTPUT_BYTES: usize = 512 * MIB; // Entire output frame, including 16-byte framing.
const MAX_LINES: usize = 1_000_000;
const MAX_NODES: usize = 200_000;

#[derive(Debug, PartialEq, Eq)]
enum Failure {
    InvalidFrame,
    InvalidUtf8,
    Capacity,
    Io,
    Encode,
}

impl Failure {
    fn code(&self) -> &'static str {
        match self {
            Self::InvalidFrame => "invalid-frame",
            Self::InvalidUtf8 => "invalid-utf8",
            Self::Capacity => "capacity",
            Self::Io => "io",
            Self::Encode => "encode",
        }
    }
}

#[derive(Serialize)]
struct Response<'a> {
    version: u8,
    days: &'a [DaySection],
    chunks: &'a [Chunk],
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
            return Err(io::Error::other("encoded response cap"));
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

fn read_exact_or_frame<R: Read>(reader: &mut R, data: &mut [u8]) -> Result<(), Failure> {
    reader.read_exact(data).map_err(|_| Failure::InvalidFrame)
}

fn structural_preflight(text: &str, max_lines: usize, max_nodes: usize) -> Result<(), Failure> {
    // These are intentionally conservative potential-node counts. They bound
    // splitlines' pointer table and parse tree before either is allocated.
    let mut lines = 1usize;
    let mut nodes = 0usize;
    for c in text.chars() {
        if matches!(
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
        ) {
            lines = lines.checked_add(1).ok_or(Failure::Capacity)?;
            if lines > max_lines {
                return Err(Failure::Capacity);
            }
        }
    }
    for marker in ["## ", "### ", "**Me:**"] {
        nodes = nodes
            .checked_add(text.match_indices(marker).count())
            .ok_or(Failure::Capacity)?;
        if nodes > max_nodes {
            return Err(Failure::Capacity);
        }
    }
    Ok(())
}

fn run<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    input_limit: usize,
    output_limit: usize,
    max_lines: usize,
    max_nodes: usize,
) -> Result<(), Failure> {
    let mut magic = [0; 4];
    read_exact_or_frame(reader, &mut magic)?;
    if &magic != INPUT_MAGIC {
        return Err(Failure::InvalidFrame);
    }
    let mut length = [0; 8];
    read_exact_or_frame(reader, &mut length)?;
    let length = usize::try_from(u64::from_be_bytes(length)).map_err(|_| Failure::Capacity)?;
    if length > input_limit {
        return Err(Failure::Capacity);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| Failure::Capacity)?;
    bytes.resize(length, 0);
    read_exact_or_frame(reader, &mut bytes)?;
    read_exact_or_frame(reader, &mut magic)?;
    if &magic != INPUT_END {
        return Err(Failure::InvalidFrame);
    }
    if reader.read(&mut magic[..1]).map_err(|_| Failure::Io)? != 0 {
        return Err(Failure::InvalidFrame);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| Failure::InvalidUtf8)?;
    structural_preflight(text, max_lines, max_nodes)?;

    let days = parse_diary(text);
    let chunks = chunks_from_days(&days);
    let mut out = BoundedVec {
        bytes: Vec::new(),
        limit: output_limit.checked_sub(16).ok_or(Failure::Capacity)?,
        overflowed: false,
    };
    if serde_json::to_writer(
        &mut out,
        &Response {
            version: 1,
            days: &days,
            chunks: &chunks,
        },
    )
    .is_err()
    {
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
    writer.write_all(OUTPUT_END).map_err(|_| Failure::Io)?;
    writer.flush().map_err(|_| Failure::Io)?;
    Ok(())
}

fn main() -> std::process::ExitCode {
    if std::env::args().skip(1).collect::<Vec<_>>() != ["parse-v1"] {
        eprintln!("usage");
        return std::process::ExitCode::from(2);
    }
    match run(
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        MAX_INPUT_BYTES,
        MAX_OUTPUT_BYTES,
        MAX_LINES,
        MAX_NODES,
    ) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", error.code());
            std::process::ExitCode::from(3)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(data: &[u8]) -> Vec<u8> {
        let mut request = Vec::new();
        request.extend_from_slice(INPUT_MAGIC);
        request.extend_from_slice(&(data.len() as u64).to_be_bytes());
        request.extend_from_slice(data);
        request.extend_from_slice(INPUT_END);
        request
    }

    fn call(
        data: &[u8],
        input: usize,
        output: usize,
        lines: usize,
        nodes: usize,
    ) -> (Result<(), Failure>, Vec<u8>) {
        let mut reply = Vec::new();
        let result = run(
            &mut Cursor::new(frame(data)),
            &mut reply,
            input,
            output,
            lines,
            nodes,
        );
        (result, reply)
    }

    #[test]
    fn inclusive_input_and_strict_frame() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(call(b"abc", 3, 1024, 100, 100).0, Ok(()));
        let (error, reply) = call(b"abc", 2, 1024, 100, 100);
        assert_eq!(error, Err(Failure::Capacity));
        assert!(reply.is_empty());
        let mut wrong_end = frame(b"abc");
        wrong_end.truncate(wrong_end.len() - 4);
        wrong_end.extend_from_slice(b"BAD!");
        let mut huge_length = Vec::from(INPUT_MAGIC.as_slice());
        huge_length.extend_from_slice(&u64::MAX.to_be_bytes());
        for request in [
            frame(b"abc").get(..14).ok_or("short request")?.to_vec(),
            [frame(b"abc"), b"x".to_vec()].concat(),
            [
                b"BAD!".to_vec(),
                frame(b"abc").get(4..).ok_or("short request")?.to_vec(),
            ]
            .concat(),
            wrong_end,
            huge_length,
        ] {
            let mut reply = Vec::new();
            assert!(run(&mut Cursor::new(request), &mut reply, 3, 1024, 100, 100).is_err());
            assert!(reply.is_empty());
        }
        Ok(())
    }

    #[test]
    fn response_and_structural_caps_are_fail_closed() -> Result<(), Box<dyn std::error::Error>> {
        let sample = b"## Monday, January 1, 2024\n### 09:00 - Topic\n**Me:** hi\n";
        let (result, reply) = call(sample, sample.len(), 4096, 100, 100);
        assert_eq!(result, Ok(()));
        assert_eq!(reply.get(..4), Some(OUTPUT_MAGIC.as_slice()));
        assert_eq!(reply.get(reply.len() - 4..), Some(OUTPUT_END.as_slice()));
        assert_eq!(
            u64::from_be_bytes(reply.get(4..12).ok_or("length")?.try_into()?) as usize + 16,
            reply.len()
        );
        let payload: serde_json::Value =
            serde_json::from_slice(reply.get(12..reply.len() - 4).ok_or("payload")?)?;
        assert_eq!(
            payload.get("version").and_then(serde_json::Value::as_u64),
            Some(1)
        );
        assert_eq!(
            payload
                .get("chunks")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(call(sample, sample.len(), reply.len(), 100, 100).0, Ok(()));
        assert_eq!(
            call(sample, sample.len(), reply.len() - 1, 100, 100).0,
            Err(Failure::Capacity)
        );
        assert_eq!(
            call(sample, sample.len(), 4096, 1, 100).0,
            Err(Failure::Capacity)
        );
        assert_eq!(
            call(sample, sample.len(), 4096, 100, 1).0,
            Err(Failure::Capacity)
        );
        assert_eq!(
            call(b"\xff", 1, 4096, 100, 100).0,
            Err(Failure::InvalidUtf8)
        );
        Ok(())
    }
}
