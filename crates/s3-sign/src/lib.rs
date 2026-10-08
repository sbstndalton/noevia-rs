//! Minimal AWS Signature Version 4 signer for S3-compatible object storage (path-style), and the
//! region rule, ported from noevia-core's `server/s3-sign.cjs` (`signS3Parts`) and
//! `server/s3-region.cjs` (`normalizeS3Region`). The output (canonical request, string to sign,
//! signature and headers) is byte-identical to the JS for every input the JS can express as
//! UTF-8; `tests/fixtures/s3-sign.v1.json` is generated from the JS and shared with noevia-core.
//!
//! JS semantics this keeps on purpose:
//! - header values are trimmed with ECMAScript `String.prototype.trim` (which strips U+FEFF but
//!   not U+0085, unlike `char::is_whitespace`);
//! - query pairs sort by key, then value, comparing UTF-16 code units (JS `<`), before encoding;
//! - each path segment is decoded like `decodeURIComponent` (a malformed escape or invalid UTF-8
//!   leaves the segment literal) and re-encoded with the RFC 3986 unreserved set;
//! - the date stamp is the first 8 UTF-16 code units of `x-amz-date`;
//! - an empty region is `us-east-1`; an empty session token means none.
//!
//! The secret key only ever feeds HMAC: it is copied into a zeroizing buffer, every derived key is
//! zeroized after use, and no error, reply or `Debug` output carries it. Errors are two fixed
//! codes and never echo input. (The `hmac` crate's internal pad state is not zeroized on drop;
//! the WebAssembly host wipes the module's whole linear memory after each call.)

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fmt;
use zeroize::Zeroizing;

/// The JS default (`opts.region || 'us-east-1'`, and `DEFAULT_S3_REGION`).
pub const DEFAULT_REGION: &str = "us-east-1";
/// The SigV4 algorithm name.
pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";
/// Cap on each text field (method, host, path, keys, token, date, each query key and value).
/// The JS has none; core's real values are far below it (an STS token is ~2 KiB).
pub const MAX_FIELD_BYTES: usize = 64 * 1024;
/// Cap on the number of query pairs (core sends at most five).
pub const MAX_QUERY_PAIRS: usize = 256;
/// Cap on the payload (core's largest body is one 4 MiB backup chunk).
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
/// Cap on a region input to [`region_call`] (the module-wide input cap bounds it anyway).
pub const MAX_REGION_BYTES: usize = 1024 * 1024;

/// A refusal. Fixed codes only: no input byte ever reaches an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The input does not have the expected shape, or the signed headers could not be expressed
    /// as UTF-8 (a date stamp cut inside a surrogate pair).
    Input,
    /// A field, the query or the payload is over its cap.
    TooLarge,
}

impl Error {
    /// The public code.
    pub fn code(self) -> &'static str {
        match self {
            Error::Input => "input",
            Error::TooLarge => "too_large",
        }
    }

    /// `{"error":"<code>"}`.
    pub fn json(self) -> Vec<u8> {
        format!("{{\"error\":\"{}\"}}", self.code()).into_bytes()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for Error {}

/// One request to sign: the pieces `signS3Parts` reads from its arguments and `url`.
pub struct Request<'a> {
    /// The HTTP method, verbatim.
    pub method: &'a str,
    /// `url.host` (host, and port unless it is the scheme's default).
    pub host: &'a str,
    /// `url.pathname`, still percent-encoded.
    pub pathname: &'a str,
    /// `url.searchParams` entries, in URL order (decoded).
    pub query: &'a [(&'a str, &'a str)],
    /// The request body bytes.
    pub payload: &'a [u8],
    /// The access key id (it appears in the Authorization header).
    pub access_key: &'a str,
    /// The secret key's bytes (`Buffer.from(secretKey)`). Never returned or printed.
    pub secret_key: &'a [u8],
    /// The region; empty means [`DEFAULT_REGION`].
    pub region: &'a str,
    /// The session token; empty means none.
    pub session_token: &'a str,
    /// `x-amz-date` (`YYYYMMDDTHHMMSSZ`; any non-empty text is signed as the JS would).
    pub amz_date: &'a str,
}

impl fmt::Debug for Request<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("host", &self.host)
            .field("pathname", &self.pathname)
            .field("query", &self.query)
            .field("payload_len", &self.payload.len())
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .field("region", &self.region)
            .field("session_token", &"<redacted>")
            .field("amz_date", &self.amz_date)
            .finish()
    }
}

/// Everything `signS3Parts` computes. Holds no secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    pub canonical_request: String,
    pub string_to_sign: String,
    /// Lowercase hex HMAC-SHA256.
    pub signature: String,
    /// The headers to send, in the JS object's order: host, x-amz-content-sha256, x-amz-date,
    /// x-amz-security-token (when there is a token), Authorization.
    pub headers: Vec<(&'static str, String)>,
}

/// ECMAScript WhiteSpace and LineTerminator (what `String.prototype.trim` strips).
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `String.prototype.trim`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

fn nibble(n: u8) -> char {
    char::from_digit(u32::from(n & 15), 16).unwrap_or('0')
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(nibble(b >> 4));
        out.push(nibble(b));
    }
    out
}

/// `sha256Hex`.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac(key: &[u8], data: &[u8]) -> Result<Zeroizing<[u8; 32]>, Error> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| Error::Input)?;
    mac.update(data);
    let mut out = Zeroizing::new([0u8; 32]);
    out.copy_from_slice(&mac.finalize().into_bytes());
    Ok(out)
}

/// `uriEncode`: every byte outside the RFC 3986 unreserved set as `%XX` (upper case).
pub fn uri_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(nibble(b >> 4).to_ascii_uppercase());
            out.push(nibble(b).to_ascii_uppercase());
        }
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `decodeURIComponent`, or `None` where it throws (a malformed escape, or escapes that do not
/// decode to UTF-8 — overlong forms and surrogates included).
pub fn decode_uri_component(seg: &str) -> Option<String> {
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hi = hex_val(*bytes.get(i + 1)?)?;
            let lo = hex_val(*bytes.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `canonicalUri`: each `/` segment decoded once (left literal where that fails) and encoded.
pub fn canonical_uri(pathname: &str) -> String {
    let pathname = if pathname.is_empty() { "/" } else { pathname };
    let out = pathname
        .split('/')
        .map(|seg| match decode_uri_component(seg) {
            Some(raw) => uri_encode(&raw),
            None => uri_encode(seg),
        })
        .collect::<Vec<_>>()
        .join("/");
    if out.is_empty() {
        "/".to_owned()
    } else {
        out
    }
}

/// JS `<` on strings: UTF-16 code unit order.
pub fn cmp_utf16(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `amzDate.slice(0, 8)`; `None` when the cut falls inside a surrogate pair.
fn date_stamp(amz_date: &str) -> Option<&str> {
    let mut units = 0usize;
    for (i, c) in amz_date.char_indices() {
        if units == 8 {
            return amz_date.get(..i);
        }
        units += c.len_utf16();
        if units > 8 {
            return None;
        }
    }
    Some(amz_date)
}

fn check_field(s: &str) -> Result<(), Error> {
    if s.len() > MAX_FIELD_BYTES {
        Err(Error::TooLarge)
    } else {
        Ok(())
    }
}

/// `signS3Parts`.
pub fn sign(req: &Request<'_>) -> Result<Signed, Error> {
    for f in [
        req.method,
        req.host,
        req.pathname,
        req.access_key,
        req.region,
        req.session_token,
        req.amz_date,
    ] {
        check_field(f)?;
    }
    if req.secret_key.len() > MAX_FIELD_BYTES {
        return Err(Error::TooLarge);
    }
    if req.query.len() > MAX_QUERY_PAIRS {
        return Err(Error::TooLarge);
    }
    for (k, v) in req.query {
        check_field(k)?;
        check_field(v)?;
    }
    if req.payload.len() > MAX_PAYLOAD_BYTES {
        return Err(Error::TooLarge);
    }
    // core always supplies a date (the loader stamps `new Date()` as the JS does).
    if req.amz_date.is_empty() {
        return Err(Error::Input);
    }
    let region = if req.region.is_empty() {
        DEFAULT_REGION
    } else {
        req.region
    };
    let date_stamp = date_stamp(req.amz_date).ok_or(Error::Input)?;
    let payload_hash = sha256_hex(req.payload);

    let mut headers: Vec<(&'static str, String)> = vec![
        ("host", req.host.to_owned()),
        ("x-amz-content-sha256", payload_hash.clone()),
        ("x-amz-date", req.amz_date.to_owned()),
    ];
    if !req.session_token.is_empty() {
        headers.push(("x-amz-security-token", req.session_token.to_owned()));
    }
    // The names above are already in sorted order.
    let mut canonical_headers = String::new();
    for (k, v) in &headers {
        canonical_headers.push_str(k);
        canonical_headers.push(':');
        canonical_headers.push_str(js_trim(v));
        canonical_headers.push('\n');
    }
    let signed_headers = headers
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(";");

    let mut pairs: Vec<(&str, &str)> = req.query.to_vec();
    pairs.sort_by(|a, b| cmp_utf16(a.0, b.0).then_with(|| cmp_utf16(a.1, b.1)));
    let canonical_query = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", uri_encode(k), uri_encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let canonical_request = [
        req.method,
        &canonical_uri(req.pathname),
        &canonical_query,
        &canonical_headers,
        &signed_headers,
        &payload_hash,
    ]
    .join("\n");

    let scope = format!("{date_stamp}/{region}/s3/aws4_request");
    let string_to_sign = [
        ALGORITHM,
        req.amz_date,
        &scope,
        &sha256_hex(canonical_request.as_bytes()),
    ]
    .join("\n");

    let mut k_secret = Zeroizing::new(Vec::with_capacity(4 + req.secret_key.len()));
    k_secret.extend_from_slice(b"AWS4");
    k_secret.extend_from_slice(req.secret_key);
    let k_date = hmac(&k_secret, date_stamp.as_bytes())?;
    drop(k_secret);
    let k_region = hmac(&*k_date, region.as_bytes())?;
    drop(k_date);
    let k_service = hmac(&*k_region, b"s3")?;
    drop(k_region);
    let k_signing = hmac(&*k_service, b"aws4_request")?;
    drop(k_service);
    let signature = hex(&*hmac(&*k_signing, string_to_sign.as_bytes())?);
    drop(k_signing);

    headers.push((
        "Authorization",
        format!(
            "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            req.access_key
        ),
    ));
    Ok(Signed {
        canonical_request,
        string_to_sign,
        signature,
        headers,
    })
}

/// `normalizeS3Region` on the text of `String(value || '')`.
pub fn normalize_region(value: &str) -> String {
    let region = js_trim(value).to_lowercase();
    let ok = (1..=32).contains(&region.len())
        && region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        region
    } else {
        DEFAULT_REGION.to_owned()
    }
}

fn push_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The headers as a UTF-8 JSON object, in order.
pub fn headers_json(signed: &Signed) -> String {
    let mut out = String::from("{");
    for (i, (k, v)) in signed.headers.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_json_str(&mut out, k);
        out.push(':');
        push_json_str(&mut out, v);
    }
    out.push('}');
    out
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn u32(&mut self) -> Result<usize, Error> {
        let (head, rest) = self.0.split_first_chunk::<4>().ok_or(Error::Input)?;
        self.0 = rest;
        usize::try_from(u32::from_le_bytes(*head)).map_err(|_| Error::Input)
    }

    fn bytes(&mut self, cap: usize) -> Result<&'a [u8], Error> {
        let n = self.u32()?;
        if n > cap {
            return Err(Error::TooLarge);
        }
        let (field, rest) = self.0.split_at_checked(n).ok_or(Error::Input)?;
        self.0 = rest;
        Ok(field)
    }

    fn text(&mut self) -> Result<&'a str, Error> {
        std::str::from_utf8(self.bytes(MAX_FIELD_BYTES)?).map_err(|_| Error::Input)
    }
}

/// The `s3_sign` ABI: input `field*8 u32le(n) (key value)*n payload`, each field and each key
/// and value `u32le(len) bytes`, in the order method, host, pathname, access key, secret key,
/// region, session token, amz date; every one but the secret key is UTF-8. Reply: the headers
/// as UTF-8 JSON ([`headers_json`]).
pub fn sign_call(input: &[u8]) -> Result<Vec<u8>, Error> {
    let mut r = Reader(input);
    let method = r.text()?;
    let host = r.text()?;
    let pathname = r.text()?;
    let access_key = r.text()?;
    let secret_key = r.bytes(MAX_FIELD_BYTES)?;
    let region = r.text()?;
    let session_token = r.text()?;
    let amz_date = r.text()?;
    let n = r.u32()?;
    if n > MAX_QUERY_PAIRS {
        return Err(Error::TooLarge);
    }
    let mut query = Vec::with_capacity(n);
    for _ in 0..n {
        let k = r.text()?;
        let v = r.text()?;
        query.push((k, v));
    }
    let signed = sign(&Request {
        method,
        host,
        pathname,
        query: &query,
        payload: r.0,
        access_key,
        secret_key,
        region,
        session_token,
        amz_date,
    })?;
    Ok(headers_json(&signed).into_bytes())
}

/// The `s3_region` ABI: input the UTF-8 text of `String(value || '')`; reply the region (ASCII).
pub fn region_call(input: &[u8]) -> Result<Vec<u8>, Error> {
    if input.len() > MAX_REGION_BYTES {
        return Err(Error::TooLarge);
    }
    let text = std::str::from_utf8(input).map_err(|_| Error::Input)?;
    Ok(normalize_region(text).into_bytes())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn probe() -> Signed {
        sign(&Request {
            method: "GET",
            host: "s3.example.com",
            pathname: "/diary-bucket",
            query: &[("list-type", "2"), ("max-keys", "1")],
            payload: b"",
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret_key: b"wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            region: "",
            session_token: "",
            amz_date: "20130524T000000Z",
        })
        .unwrap()
    }

    #[test]
    fn matches_the_python_signer_on_the_probe() {
        // The value noevia-core's s3-sign.test.cjs pins from the diary sidecar's Python signer.
        assert_eq!(
            probe().signature,
            "4560899e7ffad2d2164e3dbc99454334a44ba5a4a86bf34dadad3be59e0364ad"
        );
    }

    #[test]
    fn trim_is_ecmascript_trim() {
        assert_eq!(js_trim("\u{FEFF} a \u{3000}"), "a");
        assert_eq!(js_trim("\u{0085}a"), "\u{0085}a");
    }

    #[test]
    fn date_stamp_counts_utf16_units() {
        assert_eq!(date_stamp("20130524T"), Some("20130524"));
        assert_eq!(date_stamp("2013"), Some("2013"));
        assert_eq!(date_stamp("1234567\u{1F600}"), None);
        assert_eq!(date_stamp("123456\u{1F600}x"), Some("123456\u{1F600}"));
    }

    #[test]
    fn canonical_uri_cases() {
        assert_eq!(canonical_uri("/b/a%2Fb.md"), "/b/a%2Fb.md");
        assert_eq!(canonical_uri("/b/100%.md"), "/b/100%25.md");
        assert_eq!(canonical_uri(""), "/");
        assert_eq!(canonical_uri("/b/%ED%A0%80"), "/b/%25ED%25A0%2580");
    }

    #[test]
    fn debug_redacts_the_secret() {
        let r = Request {
            method: "GET",
            host: "h",
            pathname: "/",
            query: &[],
            payload: b"",
            access_key: "a",
            secret_key: b"top-secret-value",
            region: "",
            session_token: "tok-value",
            amz_date: "20130524T000000Z",
        };
        let d = format!("{r:?}");
        assert!(!d.contains("top-secret") && !d.contains("tok-value"));
    }
}
