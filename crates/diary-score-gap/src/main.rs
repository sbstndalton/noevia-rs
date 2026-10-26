//! Single bounded record on stdin; no text-bearing fields or diagnostic echoes.
use diary_score_gap::cut;
use std::io::{self, Read, Write};

const VERSION: &str = "diary-score-gap-v1";
const MAX_INPUT: u64 = 131_072;
const MAX_SCORES: usize = 4096;

fn number(token: &str) -> Result<Option<f64>, ()> {
    if token == "null" {
        return Ok(None);
    }
    // Only ASCII decimal floats, never whitespace, NaN, infinity or Rust spellings.
    if token.is_empty()
        || !token
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'))
        || !token.bytes().any(|b| b.is_ascii_digit())
    {
        return Err(());
    }
    let n = token.parse::<f64>().map_err(|_| ())?;
    if n.is_finite() {
        Ok(Some(n))
    } else {
        Err(())
    }
}

fn reply(input: &[u8]) -> Result<String, ()> {
    let record = std::str::from_utf8(input).map_err(|_| ())?;
    let record = record.strip_suffix("\n").ok_or(())?;
    let fields: Vec<_> = record.split("\t").collect();
    let [version, gap, scores] = fields.as_slice() else {
        return Err(());
    };
    if *version != VERSION {
        return Err(());
    }
    let gap = number(gap)?;
    if gap.is_some_and(|v| !(0.0 < v && v <= 1.0)) {
        return Err(());
    }
    let mut values = Vec::new();
    if !scores.is_empty() {
        for token in scores.split(",") {
            if values.len() == MAX_SCORES {
                return Err(());
            }
            values.push(number(token)?);
        }
    }
    let result = cut(&values, gap);
    let indices = |positions: &[usize]| {
        positions
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    let best = result
        .best
        .map_or_else(|| "null".to_owned(), |v| v.to_string());
    Ok(format!(
        "{VERSION}\t{best}\t{}\t{}\n",
        indices(&result.kept),
        indices(&result.removed)
    ))
}

fn run() -> Result<(), u8> {
    if std::env::args_os().count() != 1 {
        return Err(2);
    }
    exchange(io::stdin(), io::stdout().lock())
}

fn exchange(reader: impl Read, mut stdout: impl Write) -> Result<(), u8> {
    // Read one byte beyond the bound to reject oversize without unbounded allocation.
    // EOF is mandatory. The host must apply a wall-clock child-process timeout.
    let mut input = Vec::new();
    reader
        .take(MAX_INPUT + 1)
        .read_to_end(&mut input)
        .map_err(|_| 3)?;
    if input.len() as u64 > MAX_INPUT {
        return Err(2);
    }
    let output = reply(&input).map_err(|_| 2)?;
    stdout.write_all(output.as_bytes()).map_err(|_| 3)?;
    stdout.flush().map_err(|_| 3)?;
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(code) => {
            // Fixed diagnostics: never echo scores or unknown input.
            let _ = io::stderr().write_all(if code == 2 {
                b"invalid request\n"
            } else {
                b"io failure\n"
            });
            std::process::ExitCode::from(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("synthetic read failure"))
        }
    }
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("synthetic write failure"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn io_failures_have_separate_status() {
        let mut output = Vec::new();
        assert_eq!(exchange(Broken, &mut output), Err(3));
        assert!(output.is_empty());
        assert_eq!(
            exchange(b"diary-score-gap-v1\tnull\t1\n".as_slice(), Broken),
            Err(3)
        );
    }
}
