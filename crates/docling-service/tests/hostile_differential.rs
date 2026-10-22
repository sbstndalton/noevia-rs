#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! sbstndalton/noevia#1071: the native server is the only Docling HTTP owner, so this compares it
//! with the retired Python server (the pinned reference, `NOEVIA_DOCLING_REFERENCE`, loaded by the
//! fixed oracle loader) over a seeded, generated corpus of hostile requests: odd sizes and
//! spellings of Content-Length, Latin-1 and boundary-length names, duplicate and re-cased
//! headers, page ranges, the figures header, paths, methods and versions, crashing and failing
//! engines. Well-framed requests must get the same status, content type and decoded body from
//! both. Malformed framing (a bad request line, a header line without a colon, a folded header)
//! is answered by the native server with its own 400, a documented difference: those cases only
//! check that the native server refuses. The engine is the synthetic converter; nothing real is read.
use std::{
    io::{Read, Write},
    net::TcpStream,
    path::PathBuf,
    time::Duration,
};
#[allow(dead_code)] // the fixture generator's half of the oracle is not used here
#[path = "support/oracle.rs"]
mod oracle;

/// Generated well-framed cases, and malformed-framing cases, per run. Fixed seed: the corpus is
/// the same on every run, so a difference is reproducible from its case number.
const STRICT: usize = 1500;
const FRAMING: usize = 150;
const SEED: u64 = 0x1071_d0c1_1e9e_5eed;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

/// Python's `int()` over a Latin-1 header value: Python whitespace stripped, one sign, ASCII
/// digits with single underscores between digits. None when Python raises.
fn python_int(value: &[u8]) -> Option<i128> {
    let space = |b: &u8| {
        matches!(
            b,
            b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | 0x1c..=0x1f | b' ' | 0x85 | 0xa0
        )
    };
    let start = value.iter().position(|b| !space(b))?;
    let end = value.iter().rposition(|b| !space(b))?;
    let mut text = &value[start..=end];
    let negative = text.first() == Some(&b'-');
    if matches!(text.first(), Some(b'+' | b'-')) {
        text = &text[1..];
    }
    if text.is_empty() || !text[0].is_ascii_digit() || !text[text.len() - 1].is_ascii_digit() {
        return None;
    }
    let mut out: i128 = 0;
    let mut previous = b'0';
    for &b in text {
        if b == b'_' {
            if previous == b'_' {
                return None;
            }
        } else if b.is_ascii_digit() {
            out = out.checked_mul(10)?.checked_add(i128::from(b - b'0'))?;
        } else {
            return None;
        }
        previous = b;
    }
    Some(if negative { -out } else { out })
}

fn name_pool(rng: &mut Rng) -> Vec<u8> {
    let suffixes: &[&[u8]] = &[
        b".pdf",
        b".PDF",
        b".PdF",
        b".docx",
        b".DOCX",
        b".xlsx",
        b".pptx",
        b".html",
        b".htm",
        b".csv",
        b".epub",
        b".odt",
        b".ods",
        b".odp",
        b".png",
        b".jpg",
        b".jpeg",
        b".tif",
        b".tiff",
        b".TIFF",
        b".bmp",
        b".md",
        b".txt",
        b".zip",
        b".pdf.",
        b".pdf ",
        b".pdf\t",
        b".pdf\x1f",
        b".pdf\xa0",
        b".tar.gz",
        b".",
        b"",
        b"..pdf",
        b".p\xe9f",
        b".\xc9PUB",
        b".pdf\x85",
    ];
    let stems: &[&[u8]] = &[
        b"x",
        b"report",
        b"",
        b" lead",
        b"\tlead",
        b"caf\xe9",
        b"\xc3\xa9t\xc3\xa9",
        b"a b",
        b"a%2Fb",
        b"..",
        b".hidden",
        b"\x7f",
        b"semi;colon",
        b"quote\"d",
        b"\xff\xfe",
        b"UPPER",
        b"x\x1fy",
    ];
    let mut name = Vec::new();
    match rng.below(10) {
        0 => {
            // Boundary lengths, counted in Latin-1 characters.
            let total = *rng.pick(&[199usize, 200, 201, 202]);
            let suffix: &[u8] = rng.pick(&[b".pdf".as_slice(), b".docx", b".zip"]);
            let fill = if rng.chance(50) { b'x' } else { 0xe9 };
            name.resize(total.saturating_sub(suffix.len()), fill);
            name.extend_from_slice(suffix);
        }
        2..=5 => {
            // Well-formed: a plain stem and a suffix the engine reads.
            name.extend_from_slice(rng.pick(&[b"x".as_slice(), b"report", b"Scan 01"]));
            name.extend_from_slice(rng.pick(&[
                b".pdf".as_slice(),
                b".PDF",
                b".docx",
                b".xlsx",
                b".html",
                b".csv",
                b".png",
                b".tiff",
                b".odt",
            ]));
        }
        1 => name.extend_from_slice(rng.pick(&[
            b"x/y.pdf".as_slice(),
            b"x\\y.pdf",
            b"/x.pdf",
            b"x.pdf/",
            b"\\",
            b"/",
        ])),
        _ => {
            name.extend_from_slice(rng.pick(stems));
            name.extend_from_slice(rng.pick(suffixes));
        }
    }
    name
}

struct Built {
    raw: Vec<u8>,
    strict: bool,
}

fn header(raw: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    raw.extend_from_slice(key);
    raw.extend_from_slice(b":");
    raw.extend_from_slice(value);
    raw.extend_from_slice(b"\r\n");
}

fn replace_request_line(raw: &[u8], line: &[u8]) -> Vec<u8> {
    let rest = raw.iter().position(|b| *b == b'\n').unwrap() + 1;
    [line, &raw[rest..]].concat()
}

fn build(rng: &mut Rng, production: bool, malformed: bool) -> Built {
    let mut methods: Vec<&[u8]> = vec![b"POST", b"POST", b"POST", b"POST", b"GET"];
    if production {
        // Unsupported-method HTML depends on the interpreter's template: the CI (3.12) one only.
        methods.extend_from_slice(&[b"PUT", b"HEAD", b"OPTIONS", b"post", b"DELETE"]);
    }
    let method = *rng.pick(&methods);
    let paths: &[&[u8]] = if method == b"GET" {
        &[
            b"/health",
            b"//health",
            b"///health",
            b"/health/",
            b"/Health",
            b"/health?x=1",
            b"/health#x",
            b"/extract",
            b"/",
        ]
    } else {
        &[
            b"/extract",
            b"/extract",
            b"/extract",
            b"/extract",
            b"//extract",
            b"/extract?",
            b"/extract/",
            b"/Extract",
            b"/extract%00",
            b"/%65xtract",
        ]
    };
    let path = *rng.pick(paths);
    let version_close = rng.chance(25);
    let mut raw = Vec::new();
    raw.extend_from_slice(method);
    raw.push(b' ');
    raw.extend_from_slice(path);
    raw.extend_from_slice(if version_close {
        b" HTTP/1.1\r\n"
    } else {
        b" HTTP/1.0\r\n"
    });
    header(&mut raw, b"Host", b" localhost");
    if version_close {
        header(&mut raw, b"Connection", b" close");
    }

    let name = name_pool(rng);
    let pdf = name.to_ascii_lowercase().ends_with(b".pdf");
    let body: Vec<u8> = match rng.below(12) {
        0 => b"BOOM".to_vec(),
        1 => b"EXIT".to_vec(),
        2 => b"  x  ".to_vec(),
        3 => vec![0u8; 1 + rng.below(40)],
        _ => b"x".to_vec(),
    };
    // Content-Length spellings; the body sent follows what Python's int() reads, so neither side
    // waits for bytes that never come unless the two disagree about the size.
    let n = body.len().to_string();
    let lengths: Vec<Vec<u8>> = vec![
        n.clone().into_bytes(),
        n.clone().into_bytes(),
        format!("+{n}").into_bytes(),
        format!(" {n}").into_bytes(),
        format!("{n} ").into_bytes(),
        format!("\t{n}\t").into_bytes(),
        format!("0{n}").into_bytes(),
        [n.as_bytes(), b"\x0b"].concat(),
        [n.as_bytes(), b"\x1f"].concat(),
        [b"\xa0".as_slice(), n.as_bytes()].concat(),
        [n.as_bytes(), b"\x85"].concat(),
        format!("-{n}").into_bytes(),
        format!("{n}.0").into_bytes(),
        b"0".to_vec(),
        b"-0".to_vec(),
        b"".to_vec(),
        b"0x1".to_vec(),
        b"1e0".to_vec(),
        b"1__0".to_vec(),
        b"_1".to_vec(),
        b"1_".to_vec(),
        b"\xd9\xa1".to_vec(),
        b"99999999999999999999999999".to_vec(),
        b"26214401".to_vec(),
        b"100663297".to_vec(),
    ];
    let length = if rng.chance(50) {
        n.clone().into_bytes()
    } else {
        rng.pick(&lengths).clone()
    };
    let send_length = !rng.chance(4);

    let mut fields: Vec<(Vec<u8>, Vec<u8>)> = vec![];
    let key = |rng: &mut Rng, canonical: &[u8]| -> Vec<u8> {
        match rng.below(6) {
            0 => canonical.to_ascii_lowercase(),
            1 => canonical.to_ascii_uppercase(),
            _ => canonical.to_vec(),
        }
    };
    if send_length {
        let k = key(rng, b"Content-Length");
        fields.push((k, [b" ".as_slice(), &length].concat()));
    }
    if !rng.chance(4) {
        let k = key(rng, b"X-Document-Name");
        let lead: &[u8] = rng.pick(&[b" ".as_slice(), b"", b"  ", b"\t"]);
        fields.push((k, [lead, &name].concat()));
    }
    if pdf && rng.chance(45) || rng.chance(8) {
        let pages: &[u8] = rng.pick(&[
            b"1-3".as_slice(),
            b" 2-2 ",
            b"\t4-9\t",
            b"1-200",
            b"1-201",
            b"0-1",
            b"3-2",
            b"999801-999801",
            b"1000000-1000001",
            b"a-b",
            b"7",
            b"-3",
            b"1-2-3",
            b"+1-2",
            b"1 -2",
            b"1- 2",
            b"",
            b" ",
            b"\xb9-\xb2",
            b"01-02",
            b"1-1\x0b",
        ]);
        let k = key(rng, b"X-Document-Pages");
        fields.push((k, [b" ".as_slice(), pages].concat()));
    }
    if rng.chance(35) {
        for _ in 0..1 + rng.below(2) {
            let figures: &[u8] = rng.pick(&[
                b"1".as_slice(),
                b"1",
                b" 1",
                b"1 ",
                b"\t1",
                b"0",
                b"true",
                b"",
                b"01",
                b"1\x0b",
            ]);
            let k = key(rng, b"X-Document-Figures");
            fields.push((k, [b" ".as_slice(), figures].concat()));
        }
    }
    if rng.chance(15) {
        // A second, different name or size after the first: the first one counts.
        let other = name_pool(rng);
        if rng.chance(50) {
            fields.push((
                b"X-Document-Name".to_vec(),
                [b" ".as_slice(), &other].concat(),
            ));
        } else {
            fields.push((b"Content-Length".to_vec(), b" 7".to_vec()));
        }
    }
    if rng.chance(20) {
        fields.push((b"X-Unrelated".to_vec(), b" value".to_vec()));
    }
    if rng.chance(10) {
        fields.push((b"X-Empty".to_vec(), b"".to_vec()));
    }
    // Shuffle the header order a little (Host stays first).
    if fields.len() > 1 && rng.chance(40) {
        let a = rng.below(fields.len());
        let b = rng.below(fields.len());
        fields.swap(a, b);
    }
    let mut no_colon = false;
    if malformed {
        match rng.below(7) {
            0 => no_colon = true,
            1 => fields.push((b"X-Folded".to_vec(), b" a\r\n b".to_vec())),
            2 => raw = replace_request_line(&raw, b"POST /extract\r\n"),
            3 => raw = replace_request_line(&raw, b"POST /extract HTTP/2.0\r\n"),
            4 => raw = replace_request_line(&raw, b"POST /extract http/1.0\r\n"),
            5 => raw = replace_request_line(&raw, b"POST  /extract HTTP/1.0 extra\r\n"),
            _ => fields.push((b" Leading-Space".to_vec(), b" x".to_vec())),
        }
    }
    for (k, v) in &fields {
        header(&mut raw, k, v);
    }
    if no_colon {
        raw.extend_from_slice(b"X-No-Colon-Here\r\n");
    }
    raw.extend_from_slice(b"\r\n");
    // The body Python reads: exactly the size it parsed when that is a small positive number,
    // else nothing (it refuses before reading).
    let first_length = fields
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(b"content-length"));
    let size = match first_length {
        Some((_, v)) => python_int(&v[1..]),
        None => Some(0),
    };
    let posts = raw.starts_with(b"POST ");
    if posts {
        if let Some(size) = size.filter(|s| (1..=64).contains(s)) {
            let size = usize::try_from(size).unwrap();
            let mut data = body.clone();
            data.resize(size, b'x');
            raw.extend_from_slice(&data);
        }
    }
    Built {
        raw,
        strict: !malformed,
    }
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Reply {
        status: u16,
        kind: String,
        body: serde_json::Value,
    },
    Closed,
    Timeout,
}

fn send(port: u16, raw: &[u8]) -> Outcome {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(8)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(8)))
        .unwrap();
    if socket.write_all(raw).is_err() {
        return Outcome::Closed;
    }
    let mut reply = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        match socket.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => reply.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Outcome::Timeout
            }
            Err(e) => unreachable!("{e}"),
        }
    }
    let Some(split) = reply.windows(4).position(|b| b == b"\r\n\r\n") else {
        return Outcome::Closed;
    };
    let headers = String::from_utf8_lossy(&reply[..split]).into_owned();
    let status = headers
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let field = |key: &str| {
        headers.lines().find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.trim().to_owned())
        })
    };
    let kind = field("content-type").unwrap_or_default();
    let body = &reply[split + 4..];
    if let Some(size) = field("content-length") {
        let size = size.parse::<usize>().unwrap();
        let head = raw.starts_with(b"HEAD ");
        assert_eq!(
            body.len(),
            if head { 0 } else { size },
            "complete HTTP reply required"
        );
    }
    let body = if raw.starts_with(b"HEAD ") {
        serde_json::Value::Null
    } else if kind == "application/json" {
        serde_json::from_slice(body).unwrap()
    } else {
        serde_json::Value::String(String::from_utf8_lossy(body).into_owned())
    };
    Outcome::Reply { status, kind, body }
}

/// The worker PID differs between the two owners' engines; nothing else may.
fn without_pid(outcome: Outcome) -> Outcome {
    match outcome {
        Outcome::Reply {
            status,
            kind,
            mut body,
        } => {
            if let Some(object) = body.as_object_mut() {
                object.remove("pid");
            }
            Outcome::Reply { status, kind, body }
        }
        other => other,
    }
}

fn printable(raw: &[u8]) -> String {
    raw.iter()
        .map(|&b| match b {
            b'\r' => "\\r".to_owned(),
            b'\n' => "\\n".to_owned(),
            0x20..=0x7e => char::from(b).to_string(),
            _ => format!("\\x{b:02x}"),
        })
        .collect()
}

#[test]
fn hostile_generated_corpus_matches_retired_python_http() {
    let source = PathBuf::from(
        std::env::var("NOEVIA_DOCLING_REFERENCE")
            .expect("required pinned Docling source; see README"),
    );
    let reference_dir = tempfile::tempdir().unwrap();
    oracle::prepare(&source, reference_dir.path()).unwrap();
    let native_dir = tempfile::tempdir().unwrap();
    oracle::prepare(&source, native_dir.path()).unwrap();
    let reference = oracle::Server::start(reference_dir.path()).unwrap();
    if std::env::var("CI").is_ok() {
        assert!(
            reference.production,
            "mandatory oracle interpreter must be Python 3.12"
        );
    }
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut config = docling_service::Config::production(native_dir.path().to_owned());
    config.python = "python3".into();
    let server = runtime.spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        docling_service::serve(listener, config).await
    });

    let mut rng = Rng(SEED);
    let mut differences = vec![];
    let (mut strict, mut framing, mut converted) = (0usize, 0usize, 0usize);
    for index in 0..STRICT + FRAMING {
        let built = build(&mut rng, reference.production, index >= STRICT);
        let expected = without_pid(send(reference.port, &built.raw));
        let actual = without_pid(send(port, &built.raw));
        if built.strict {
            strict += 1;
            if matches!(&expected, Outcome::Reply { status: 200, .. }) {
                converted += 1;
            }
            if expected != actual {
                differences.push(format!(
                    "case {index}: {}\n  python: {expected:?}\n  native: {actual:?}",
                    printable(&built.raw)
                ));
            }
        } else {
            framing += 1;
            // The native server refuses every malformed framing with its own JSON 400 (Python
            // answered some of them: a header line without a colon ends its header block).
            // (A HEAD answer has no body to compare: `send` reads it as null.)
            let native_refuses = matches!(&actual, Outcome::Reply { status: 400, kind, body }
                if kind == "application/json"
                    && (body == &serde_json::json!({"error":"Invalid document size or name."})
                        || (built.raw.starts_with(b"HEAD ") && body.is_null())));
            if !native_refuses {
                differences.push(format!(
                    "framing case {index}: {}\n  python: {expected:?}\n  native: {actual:?}",
                    printable(&built.raw)
                ));
            }
        }
    }
    drop(reference);
    server.abort();
    drop(runtime);
    eprintln!(
        "hostile corpus: {strict} well-framed ({converted} converted), {framing} malformed framing, {} differences",
        differences.len()
    );
    assert!(
        differences.is_empty(),
        "{} differences:\n{}",
        differences.len(),
        differences.join("\n")
    );
    assert!(
        converted >= 100,
        "the corpus must reach the engine often enough: {converted}"
    );
}
