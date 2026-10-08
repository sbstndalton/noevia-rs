//! The decision core of noevia-core's outbound-URL guard, ported from `server/ssrf.cjs`
//! (`isPublicUrl`, `isPrivateIp`) and `server/public-fetch.cjs` (the URL check before a request
//! and the connect-time "is every answer public" check, noevia#795). The DNS lookup and the
//! socket stay in JS: this crate only answers two questions.
//!
//! 1. [`check_url`]: is this URL acceptable, and if so, which host must the caller resolve (a
//!    name) or has already been judged (an IP literal)? [`Mode::Check`] is `isPublicUrl` before
//!    its DNS step; [`Mode::Fetch`] is `publicFetch` before it opens the socket.
//! 2. [`addresses_public`]: is every resolved address public (`isPrivateIp` false for all, and
//!    at least one)? That is `isPublicUrl`'s last step and `createPublicOnlyLookup`'s rule.
//!
//! The URL is parsed with the `url` crate (WHATWG URL, like Node's `new URL`): decimal, octal and
//! hex IPv4 forms (`http://2130706433/`, `http://0177.1/`, `http://0x7f.1/`) become the dotted
//! address they denote, a userinfo part never becomes the host (`http://8.8.8.8@10.0.0.1/` is
//! 10.0.0.1), a backslash ends the authority, and the port is not part of the host. Addresses are
//! classified by `egress::is_private_ip`, the existing string-for-string port of `isPrivateIp`.
//! The host this crate returns is the WHATWG serialization; noevia-core's loader refuses unless
//! it equals `new URL(url).hostname` (brackets stripped), so a parser difference between the
//! `url` crate and Node's ada can only ever refuse, never redirect the request elsewhere.
//!
//! Stricter than the JS (each a refusal where the JS would go on to DNS or connect):
//! - a host whose name ends with a dot (`example.com.`): `reason` `trailing_dot`. The JS resolves
//!   it; refusing it also closes `metadata.google.internal.` past the name blocklist.
//! - an internationalized host (any label `xn--…`, which is also what a Unicode host becomes):
//!   `reason` `idn`. Node and the `url` crate follow different Unicode/IDNA tables.
//! - [`Mode::Fetch`] refuses `metadata.google.internal` by name too (`blocked_name`); the JS
//!   public fetch only refuses it once it resolves to its link-local address.
//! - a URL over [`MAX_URL_BYTES`], or more than [`MAX_ADDRESSES`] resolved addresses, is a request
//!   error (status 1), which the loader treats as a refusal.
//!
//! Errors are fixed strings (`{"error":"input"|"too_large"}`, `reason` codes); no reply to a
//! refusal carries any input byte.
#![forbid(unsafe_code)]

use serde_json::{json, Value};
use url::{Host, Url};

/// Largest URL accepted, in UTF-8 bytes.
pub const MAX_URL_BYTES: usize = 64 * 1024;
/// Most resolved addresses judged in one call.
pub const MAX_ADDRESSES: usize = 512;
/// Largest JSON request [`run_json`] reads.
pub const MAX_INPUT_BYTES: usize = 128 * 1024;
/// An address text longer than this is not an IP literal (the longest is 45 bytes without a
/// zone; a zone makes it private anyway), so it is private without parsing.
const MAX_ADDRESS_BYTES: usize = 64;

/// `ssrf.cjs` `BLOCKED_HOSTNAMES`.
const BLOCKED_HOSTNAMES: [&str; 1] = ["metadata.google.internal"];

/// Which JS check a URL decision stands in for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `isPublicUrl` up to its DNS step: userinfo is ignored, the name blocklist applies.
    Check,
    /// `publicFetch` before the socket: credentials in the URL are refused.
    Fetch,
}

/// What an accepted host is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An IP literal already judged public (or the QA loopback exemption); never looked up.
    Ip,
    /// A name: the caller resolves it and asks [`addresses_public`] about every answer.
    Name,
}

impl Kind {
    pub fn code(self) -> &'static str {
        match self {
            Kind::Ip => "ip",
            Kind::Name => "name",
        }
    }
}

/// An accepted URL: the host to connect to (IPv6 without brackets), in WHATWG serialization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    pub kind: Kind,
    pub host: String,
}

/// Why a URL is refused. Every variant is a refusal under both modes unless noted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Not a URL (`new URL` throws).
    Unparseable,
    /// Not `http:` or `https:`.
    Scheme,
    /// A username or password in the URL ([`Mode::Fetch`] only).
    Credentials,
    /// An IP literal that is not public.
    PrivateAddress,
    /// `metadata.google.internal`.
    BlockedName,
    /// Stricter than the JS: a name ending with a dot.
    TrailingDot,
    /// Stricter than the JS: an internationalized (punycode) label.
    Idn,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Refusal::Unparseable => "unparseable",
            Refusal::Scheme => "scheme",
            Refusal::Credentials => "credentials",
            Refusal::PrivateAddress => "private_address",
            Refusal::BlockedName => "blocked_name",
            Refusal::TrailingDot => "trailing_dot",
            Refusal::Idn => "idn",
        }
    }

    /// True for the refusals the JS does not make (see the crate docs).
    pub fn is_extra_strict(self) -> bool {
        matches!(self, Refusal::TrailingDot | Refusal::Idn)
    }
}

/// `isPrivateIp`: true unless `ip` is an IP literal in public address space.
pub fn is_private_ip(ip: &str) -> bool {
    ip.len() > MAX_ADDRESS_BYTES || egress::is_private_ip(ip)
}

/// True when `addresses` is non-empty and every entry is a public IP literal: `isPublicUrl`'s
/// rule over its DNS answers and `createPublicOnlyLookup`'s over the socket's.
pub fn addresses_public<S: AsRef<str>>(addresses: &[S]) -> bool {
    !addresses.is_empty() && addresses.iter().all(|a| !is_private_ip(a.as_ref()))
}

/// Decide `raw` as `isPublicUrl` ([`Mode::Check`]) or `publicFetch` ([`Mode::Fetch`]) would
/// before any DNS. `loopback` is public-fetch's `allowLoopbackLiteral` (QA only): the literal
/// host `127.0.0.1` is then accepted in [`Mode::Fetch`].
pub fn check_url(raw: &str, mode: Mode, loopback: bool) -> Result<Accepted, Refusal> {
    let url = Url::parse(raw).map_err(|_| Refusal::Unparseable)?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(Refusal::Scheme);
    }
    if mode == Mode::Fetch && (!url.username().is_empty() || url.password().is_some()) {
        return Err(Refusal::Credentials);
    }
    let literal = |host: String| {
        let exempt = mode == Mode::Fetch && loopback && host == "127.0.0.1";
        if exempt || !is_private_ip(&host) {
            Ok(Accepted {
                kind: Kind::Ip,
                host,
            })
        } else {
            Err(Refusal::PrivateAddress)
        }
    };
    match url.host() {
        Some(Host::Ipv4(a)) => literal(a.to_string()),
        Some(Host::Ipv6(_)) => {
            // WHATWG serialization (`[…]`), the same text `new URL().hostname` gives.
            let text = url.host_str().ok_or(Refusal::Unparseable)?;
            let inner = text
                .strip_prefix('[')
                .and_then(|t| t.strip_suffix(']'))
                .ok_or(Refusal::Unparseable)?;
            literal(inner.to_owned())
        }
        Some(Host::Domain(d)) => {
            if d.is_empty() {
                return Err(Refusal::Unparseable);
            }
            // The JS judges any hostname net.isIP accepts as an address; a WHATWG domain never
            // is one, but if it were, judge it the same way.
            if egress::is_ip(d) != 0 {
                return literal(d.to_owned());
            }
            if BLOCKED_HOSTNAMES.contains(&d) {
                return Err(Refusal::BlockedName);
            }
            if d.ends_with('.') {
                return Err(Refusal::TrailingDot);
            }
            if d.split('.')
                .any(|l| l.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("xn--")))
                || !d.is_ascii()
            {
                return Err(Refusal::Idn);
            }
            Ok(Accepted {
                kind: Kind::Name,
                host: d.to_owned(),
            })
        }
        None => Err(Refusal::Unparseable),
    }
}

/// A request error: the input was not a well-formed request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    Input,
    TooLarge,
}

impl RequestError {
    pub fn code(self) -> &'static str {
        match self {
            RequestError::Input => "input",
            RequestError::TooLarge => "too_large",
        }
    }
}

/// One JSON request:
/// - `{"op":"url","url":"…","mode":"check"|"fetch","loopback":bool?}` replies
///   `{"ok":true,"kind":"ip"|"name","host":"…"}` or `{"ok":false,"reason":"…"}`.
/// - `{"op":"addresses","addresses":["…",…]}` replies `{"public":bool}`.
pub fn run(input: &str) -> Result<Value, RequestError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(RequestError::TooLarge);
    }
    let req: Value = serde_json::from_str(input).map_err(|_| RequestError::Input)?;
    let obj = req.as_object().ok_or(RequestError::Input)?;
    match obj.get("op").and_then(Value::as_str) {
        Some("url") => {
            let raw = obj
                .get("url")
                .and_then(Value::as_str)
                .ok_or(RequestError::Input)?;
            if raw.len() > MAX_URL_BYTES {
                return Err(RequestError::TooLarge);
            }
            let mode = match obj.get("mode").and_then(Value::as_str) {
                Some("check") => Mode::Check,
                Some("fetch") => Mode::Fetch,
                _ => return Err(RequestError::Input),
            };
            let loopback = match obj.get("loopback") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(RequestError::Input),
            };
            Ok(match check_url(raw, mode, loopback) {
                Ok(a) => json!({"ok": true, "kind": a.kind.code(), "host": a.host}),
                Err(r) => json!({"ok": false, "reason": r.code()}),
            })
        }
        Some("addresses") => {
            let list = obj
                .get("addresses")
                .and_then(Value::as_array)
                .ok_or(RequestError::Input)?;
            if list.len() > MAX_ADDRESSES {
                return Err(RequestError::TooLarge);
            }
            let texts = list
                .iter()
                .map(|v| v.as_str().ok_or(RequestError::Input))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({"public": addresses_public(&texts)}))
        }
        _ => Err(RequestError::Input),
    }
}

/// [`run`] as the wasm ABI's `(status, reply)`: 0 and the reply, or 1 and `{"error":"…"}`.
pub fn run_json(input: &str) -> (u32, String) {
    match run(input) {
        Ok(v) => (0, v.to_string()),
        Err(e) => (1, format!("{{\"error\":\"{}\"}}", e.code())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn check(u: &str) -> Result<Accepted, Refusal> {
        check_url(u, Mode::Check, false)
    }

    #[test]
    fn literals_in_every_encoding() {
        for u in [
            "http://127.0.0.1/",
            "http://2130706433/",
            "http://0177.0.0.1/",
            "http://0x7f.1/",
            "http://0x7F000001/",
            "http://127.1/",
            "http://127.0.0.1./",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::ffff:7f00:1]/",
            "http://[::127.0.0.1]/",
            "http://169.254.169.254/latest/meta-data",
            "http://[fe80::1]/",
            "http://[fd00::1]/",
            "http://100.64.0.1/",
            "http://[2002:c0a8:101::1]/",
            "http://8.8.8.8@10.0.0.1/",
            "http://10.0.0.1\\@8.8.8.8/",
            "http://10.0.0.1:443/",
            "HTTP://10.0.0.1/",
            "http://%31%30.0.0.1/",
        ] {
            assert_eq!(check(u), Err(Refusal::PrivateAddress), "{u}");
        }
        assert_eq!(
            check("http://0x08.0x08.0x08.0x08:8080/"),
            Ok(Accepted {
                kind: Kind::Ip,
                host: "8.8.8.8".into()
            })
        );
        assert_eq!(
            check("http://[2606:4700::1111]/").unwrap().host,
            "2606:4700::1111"
        );
    }

    #[test]
    fn names_schemes_and_credentials() {
        assert_eq!(
            check("https://Example.COM:8443/x?y#z"),
            Ok(Accepted {
                kind: Kind::Name,
                host: "example.com".into()
            })
        );
        assert_eq!(
            check("http://metadata.google.internal/"),
            Err(Refusal::BlockedName)
        );
        assert_eq!(
            check("http://METADATA.google.internal./"),
            Err(Refusal::TrailingDot)
        );
        assert_eq!(check("http://example.com./"), Err(Refusal::TrailingDot));
        assert_eq!(check("http://xn--bcher-kva.example/"), Err(Refusal::Idn));
        assert_eq!(check("http://bücher.example/"), Err(Refusal::Idn));
        for u in [
            "ftp://8.8.8.8/",
            "file:///etc/passwd",
            "gopher://x/",
            "javascript:1",
        ] {
            assert_eq!(check(u), Err(Refusal::Scheme), "{u}");
        }
        for u in [
            "",
            "http://",
            "http://[::1/",
            "http://a b/",
            "http://1.2.3.4.5/",
            "http://0x100000000/",
        ] {
            assert_eq!(check(u), Err(Refusal::Unparseable), "{u}");
        }
        assert!(check("http://u:p@8.8.8.8/").is_ok());
        assert_eq!(
            check_url("http://u:p@8.8.8.8/", Mode::Fetch, false),
            Err(Refusal::Credentials)
        );
        assert_eq!(
            check_url("http://:p@8.8.8.8/", Mode::Fetch, false),
            Err(Refusal::Credentials)
        );
        assert!(check_url("http://@8.8.8.8/", Mode::Fetch, false).is_ok());
        assert_eq!(
            check_url("http://metadata.google.internal/", Mode::Fetch, false),
            Err(Refusal::BlockedName)
        );
    }

    #[test]
    fn loopback_exemption_is_exactly_one_literal_in_fetch() {
        assert_eq!(
            check_url("http://127.0.0.1:9/", Mode::Fetch, true)
                .unwrap()
                .kind,
            Kind::Ip
        );
        assert!(check_url("http://127.1/", Mode::Fetch, true).is_ok());
        assert!(check_url("http://127.0.0.2/", Mode::Fetch, true).is_err());
        assert!(check_url("http://[::1]/", Mode::Fetch, true).is_err());
        assert!(
            check_url("http://localhost/", Mode::Fetch, true)
                .unwrap()
                .kind
                == Kind::Name
        );
        assert!(check_url("http://127.0.0.1/", Mode::Check, true).is_err());
    }

    #[test]
    fn addresses_rule() {
        assert!(!addresses_public::<&str>(&[]));
        assert!(addresses_public(&["8.8.8.8", "2606:4700::1111"]));
        assert!(!addresses_public(&["8.8.8.8", "10.0.0.1"]));
        assert!(!addresses_public(&["8.8.8.8", "not-an-ip"]));
        assert!(!addresses_public(&["2606:4700::1111%eth0"]));
        assert!(is_private_ip(&"1".repeat(65)));
    }

    #[test]
    fn json_shapes() {
        assert_eq!(
            run_json(r#"{"op":"url","url":"http://10.0.0.1/","mode":"check"}"#),
            (0, r#"{"ok":false,"reason":"private_address"}"#.into())
        );
        assert_eq!(
            run_json(r#"{"op":"url","url":"http://a.example/","mode":"fetch","loopback":false}"#),
            (0, r#"{"host":"a.example","kind":"name","ok":true}"#.into())
        );
        assert_eq!(
            run_json(r#"{"op":"addresses","addresses":["8.8.8.8"]}"#),
            (0, r#"{"public":true}"#.into())
        );
        for bad in [
            "{",
            "[]",
            r#"{"op":"x"}"#,
            r#"{"op":"url","url":1,"mode":"check"}"#,
            r#"{"op":"url","url":"http://x/","mode":"other"}"#,
            r#"{"op":"url","url":"http://x/","mode":"check","loopback":"yes"}"#,
            r#"{"op":"addresses","addresses":[1]}"#,
            r#"{"op":"addresses"}"#,
        ] {
            assert_eq!(run_json(bad), (1, r#"{"error":"input"}"#.into()), "{bad}");
        }
        let many = format!(
            r#"{{"op":"addresses","addresses":[{}]}}"#,
            vec![r#""8.8.8.8""#; MAX_ADDRESSES + 1].join(",")
        );
        assert_eq!(run_json(&many).1, r#"{"error":"too_large"}"#);
        let long = format!(
            r#"{{"op":"url","url":"http://x/{}","mode":"check"}}"#,
            "a".repeat(MAX_URL_BYTES)
        );
        assert_eq!(run_json(&long).1, r#"{"error":"too_large"}"#);
    }
}
