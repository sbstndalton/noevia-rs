use server_storage_reads::{json, protocol::*, *};
use std::{collections::VecDeque, error::Error, time::Duration};
use zeroize::Zeroizing;

type TestResult = Result<(), Box<dyn Error>>;

fn connection(kind: Kind) -> Connection {
    Connection {
        kind,
        base_url: "https://storage.invalid/dav".into(),
        username: "synthetic".into(),
        secret: Zeroizing::new(b"fixture-only".to_vec()),
        corpus_root: "diary".into(),
        bucket: "bucket".into(),
        region: "us-east-1".into(),
    }
}
fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

struct Fake {
    now: Duration,
    replies: VecDeque<Result<(u16, Vec<u8>), TransportFailure>>,
    requests: Vec<(Method, String, Duration, usize)>,
    delays: Vec<Duration>,
}
impl Fake {
    fn new(replies: Vec<Result<(u16, Vec<u8>), TransportFailure>>) -> Self {
        Self {
            now: Duration::ZERO,
            replies: replies.into(),
            requests: Vec::new(),
            delays: Vec::new(),
        }
    }
}
impl Transport for Fake {
    fn elapsed(&self) -> Duration {
        self.now
    }
    fn amz_date(&self) -> String {
        "20261011T120000Z".into()
    }
    fn attempt(&mut self, request: Request) -> Result<Response, TransportFailure> {
        let cap = request.body_cap;
        self.requests
            .push((request.method, request.url, request.attempt_timeout, cap));
        let (status, bytes) = self.replies.pop_front().ok_or(TransportFailure::Other)??;
        Ok(Response {
            status,
            body: decoded_fixture(&bytes, cap).map_err(|_| TransportFailure::Body)?,
        })
    }
    fn delay(&mut self, delay: Duration) -> Result<(), TransportFailure> {
        self.delays.push(delay);
        self.now += delay;
        Ok(())
    }
}

#[test]
fn scanner_units_overflow_coercion_and_grammar() -> TestResult {
    assert_eq!(
        json::path(br#"{"path":"old","pa\u0074h":"\ud800.txt"}"#)?,
        vec![0xd800, 46, 116, 120, 116]
    );
    assert_eq!(json::path(br#"{"path":1e400}"#)?, units("Infinity"));
    assert_eq!(
        json::path(br#"{"path":[null,false,[1,2],{"valueOf":false}]}"#)?,
        units(",false,1,2,[object Object]")
    );
    assert_eq!(
        json::path(br#"{"path":{"toString":null}}"#)
            .err()
            .map(|e| e.status),
        Some(502)
    );
    for body in [
        "[1,]",
        "{\"path\":01}",
        "{\"path\":1.}",
        "{\"path\":true x}",
        "{\"path\":\"x\n\"}",
        "{,}",
    ] {
        assert!(json::parse(body.as_bytes()).is_err(), "{body}");
    }
    let deep = format!("{}0{}", "[".repeat(JSON_DEPTH), "]".repeat(JSON_DEPTH));
    assert!(json::parse(deep.as_bytes()).is_ok());
    assert!(json::parse(format!("[{deep}]").as_bytes()).is_err());
    assert_eq!(json::path(b"{\"path\":\"\xff.txt\"}")?, units("�.txt"));
    Ok(())
}

#[test]
fn quote_and_preview_preserve_split_pair() -> TestResult {
    let mut out = String::new();
    json::quote_units(&mut out, &[0xd800, 34, 10, 0xd83d, 0xde00]);
    assert_eq!(out, "\"\\ud800\\\"\\n😀\"");
    let text = format!("{}😀", "x".repeat(PREVIEW_UNITS - 1));
    let preview = preview(
        "é.txt",
        Decoded {
            text,
            capped: false,
        },
    );
    assert!(preview.ends_with("\\ud83d\",\"truncated\":true}"));
    assert!(preview.starts_with("{\"name\":\"é.txt\""));
    Ok(())
}

#[test]
fn decoded_cap_is_chunk_independent() -> TestResult {
    for cap in 0..16 {
        let bytes = b"abcdefghijklmno\xf0\x9f\x98\x80";
        let expected = decoded_fixture(bytes, cap)?;
        for split in 0..=bytes.len() {
            let mut prefix = Prefix::new(cap)?;
            prefix.feed(bytes.get(..split).ok_or("bad split")?);
            prefix.feed(bytes.get(split..).ok_or("bad split")?);
            let got = prefix.finish();
            assert_eq!(
                (got.text, got.capped),
                (expected.text.clone(), expected.capped)
            );
        }
    }
    let exact = decoded_fixture(b"abc", 3)?;
    assert!(!exact.capped);
    let overflow = decoded_fixture(b"abcx", 3)?;
    assert!(overflow.capped);
    assert_eq!(overflow.text, "abc");
    assert!(Prefix::new(LIST_BYTES + 1).is_err());
    Ok(())
}

#[test]
fn endpoint_policy_preserves_explicit_lan_and_admin() {
    assert!(endpoint_approved(
        "http://127.0.0.1:80/private",
        false,
        " http://127.0.0.1 "
    ));
    assert!(!endpoint_approved(
        "http://127.0.0.1",
        false,
        "https://127.0.0.1"
    ));
    assert!(endpoint_approved("http://10.1.1.1", true, ""));
    assert!(!endpoint_approved(
        "https://user:secret@storage.invalid",
        true,
        ""
    ));
    assert!(!endpoint_approved("file:///tmp/x", true, ""));
}

#[test]
fn retry_is_typed_preheader_only_and_never_status() -> TestResult {
    let c = connection(Kind::WebDav);
    let mut transport = Fake::new(vec![
        Err(TransportFailure::PreHeaderReset),
        Ok((200, b"ok".to_vec())),
    ]);
    let body = execute(&c, &units("read.txt"), Operation::Read, &mut transport)?;
    assert_eq!(
        body,
        "{\"name\":\"read.txt\",\"content\":\"ok\",\"truncated\":false}"
    );
    assert_eq!(transport.requests.len(), 2);
    assert_eq!(transport.delays, vec![Duration::from_millis(250)]);
    // Not retried (core withRetry): a timeout, or anything after the headers.
    for (failure, status, message) in [
        (
            TransportFailure::Timeout,
            504,
            "the storage server did not answer in time",
        ),
        (TransportFailure::Body, 502, "terminated"),
        (
            TransportFailure::BodyTimeout,
            502,
            "The operation was aborted due to timeout",
        ),
    ] {
        let mut t = Fake::new(vec![Err(failure)]);
        let error = execute(&c, &units("x.txt"), Operation::Read, &mut t)
            .err()
            .ok_or("missing error")?;
        assert_eq!((error.status, error.message.as_str()), (status, message));
        assert_eq!(t.requests.len(), 1);
        assert!(t.delays.is_empty());
    }
    // Every `fetch failed` is retried once after 250 ms, then reported as unreachable.
    for failure in [
        TransportFailure::Tls,
        TransportFailure::Dns,
        TransportFailure::Redirect,
        TransportFailure::Header,
        TransportFailure::Other,
    ] {
        let mut t = Fake::new(vec![Err(failure), Err(failure)]);
        let error = execute(&c, &units("x.txt"), Operation::Read, &mut t)
            .err()
            .ok_or("missing error")?;
        assert_eq!(
            (error.status, error.message.as_str()),
            (502, "the storage server could not be reached")
        );
        assert_eq!(t.requests.len(), 2);
        assert_eq!(t.delays, vec![Duration::from_millis(250)]);
    }
    for status in [301, 401, 403, 404, 500, 502] {
        let mut t = Fake::new(vec![Ok((status, vec![]))]);
        let error = execute(&c, &units("x.txt"), Operation::Read, &mut t)
            .err()
            .ok_or("missing error")?;
        assert_eq!(error.status, 502);
        assert_eq!(t.requests.len(), 1);
    }
    Ok(())
}

#[test]
fn root_asymmetry_and_refusals() -> TestResult {
    for (kind, expected) in [
        (Kind::WebDav, "/dav/x.txt"),
        (Kind::Nextcloud, "/dav/x.txt"),
        (Kind::S3, "/dav/bucket/diary/x.txt"),
    ] {
        let mut t = Fake::new(vec![Ok((200, vec![]))]);
        execute(&connection(kind), &units("x.txt"), Operation::Read, &mut t)?;
        assert_eq!(
            url::Url::parse(&t.requests.first().ok_or("missing request")?.1)?.path(),
            expected
        );
    }
    let mut t = Fake::new(vec![]);
    assert_eq!(
        execute(
            &connection(Kind::WebDav),
            &units("../x"),
            Operation::List,
            &mut t
        )
        .err()
        .map(|e| e.status),
        Some(400)
    );
    let mut lone = vec![0xd800];
    lone.extend(units(".txt"));
    assert_eq!(
        execute(&connection(Kind::WebDav), &lone, Operation::Read, &mut t)
            .err()
            .map(|e| e.message),
        Some("URI malformed".into())
    );
    assert_eq!(
        execute(
            &connection(Kind::WebDav),
            &[0xd800],
            Operation::Read,
            &mut t
        )
        .err()
        .map(|e| e.json()),
        Some("{\"error\":\"\\\"\\ud800\\\" is not a supported text file\"}".into())
    );
    assert!(t.requests.is_empty());
    Ok(())
}

#[test]
fn s3_repeated_token_and_twenty_page_cap() -> TestResult {
    let page = b"<IsTruncated>true</IsTruncated><NextContinuationToken>same</NextContinuationToken><Contents><Key>diary/z.txt</Key><Size>9007199254740993</Size></Contents>".to_vec();
    let mut t = Fake::new(vec![Ok((200, page.clone())), Ok((200, page))]);
    let out = execute(&connection(Kind::S3), &[], Operation::List, &mut t)?;
    assert_eq!(t.requests.len(), 2);
    assert_eq!(out.matches("9007199254740992").count(), 2);
    let responses = (0..21).map(|i| Ok((200, format!("<IsTruncated>true</IsTruncated><NextContinuationToken>{i}</NextContinuationToken>").into_bytes()))).collect();
    let mut t = Fake::new(responses);
    assert_eq!(
        execute(&connection(Kind::S3), &[], Operation::List, &mut t)?,
        "[]"
    );
    assert_eq!(t.requests.len(), MAX_PAGES);
    assert_eq!(t.replies.len(), 1);
    Ok(())
}

#[test]
fn snapshot_field_budget_refusals_bound_response_and_skip_transport() -> TestResult {
    // Named SNAPSHOT_FIELD_BUDGET qualification: private input contracts, not
    // a zero-difference claim for oversized legacy settings or stored row state.
    let mutations: [fn(&mut Connection); 6] = [
        |c| c.base_url = "x".repeat(65537),
        |c| c.username = "x".repeat(65537),
        |c| c.corpus_root = "x".repeat(65537),
        |c| c.bucket = "x".repeat(65537),
        |c| c.region = "x".repeat(1024 * 1024 + 1),
        |c| c.secret = Zeroizing::new(vec![b'x'; 8 * 1024 * 1024 + 1]),
    ];
    for mutate in mutations {
        let mut c = connection(Kind::S3);
        mutate(&mut c);
        let mut t = Fake::new(vec![]);
        let error = execute(&c, &units("x.txt"), Operation::Read, &mut t)
            .err()
            .ok_or("missing snapshot refusal")?;
        assert_eq!(error.status, 502);
        assert_eq!(error.json(), "{\"error\":\"fetch failed\"}");
        assert!(error.json().len() < 128);
        assert!(t.requests.is_empty());
    }
    Ok(())
}
