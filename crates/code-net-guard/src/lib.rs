//! noevia-core's `server/code-net-guard.cjs` decisions in Rust, exported from `dav-parse.wasm`
//! (CODE_NET_GUARD_IMPL). The guard refuses (bare 403) a request that arrived on web's own
//! address on the internal code network, so a sandboxed Code task cannot reach the sign-in or
//! file-sharing listeners directly (noevia#853). DNS lookups, retries, timers, logging and the
//! 403 itself stay in the JS; this crate answers:
//!
//! - [`parse_spec`][]: `parseCodeNetSpec(COWORK_CODE_NET_ADDR)`: entries split on runs of `\s` and
//!   `,`; an entry `net.isIP` accepts is a literal (normalised, deduplicated in order), one that
//!   matches `/^[a-z0-9](?:[a-z0-9.-]{0,251}[a-z0-9])?$/i` is a host name (lower-cased, kept in
//!   order with duplicates), anything else is malformed (the JS throws, naming the entry).
//! - [`resolved_addresses`][]: what `resolveOnce` keeps from a lookup's answers: each normalised,
//!   those `net.isIP` accepts, in order.
//! - [`refuses`][]: `addresses.has(normalizeAddress(localAddress))`.
//!
//! `net.isIP` is Node's regex (`lib/internal/net.js`): dotted-quad IPv4 with no leading zeros, and
//! RFC 4291 IPv6 text with an optional embedded IPv4 tail and an optional `%zone`
//! (`[0-9a-zA-Z-.:]+`). `normalizeAddress` is ASCII `trim`, lower case, and `::ffff:a.b.c.d` (each
//! part 1-3 digits) folded to `a.b.c.d`.
//!
//! # Stricter than the JS
//!
//! Where the JS answer could depend on the Node/ICU version or on the resolver, the port refuses
//! ([`Refusal::Ambiguous`]) and the host fails closed (a spec it refuses stops startup; an answer
//! it refuses marks the port broken, after which every request is refused; a request it refuses
//! is refused):
//!
//! - any non-ASCII code unit (JS `trim`, `\s` and `toLowerCase` are Unicode-table dependent);
//! - a spec literal with a `%zone` (interface names, `%`-encoding);
//! - a spec host name with an `xn--` label (IDNA), whose labels are all digits or `0x` hex
//!   (`getaddrinfo`/`inet_aton` read `010.0.0.1`, `2130706433` or `0x7f.1` as an IPv4 address),
//!   or whose last label is all digits.
//!
//! And [`refuses`] also refuses when the local address and a guarded address are the same IP in a
//! different spelling (`0:0::1` and `::1`, `::ffff:7f00:1` and `127.0.0.1`), where the JS's string
//! comparison would let the request through. It never answers "do not refuse" where the JS
//! refuses: the exact string match is always checked first.
//!
//! No public/private classification happens here, so the IPv6 non-global gap of the SSRF
//! classifiers (noevia#1099) does not apply to this guard.
//!
//! Linear time (hashed sets), bounded input ([`MAX_INPUT_BYTES`], [`MAX_ENTRIES`]), no panics.

#![forbid(unsafe_code)]

use prompt_framing::json::{self, Value};
use std::collections::HashSet;

/// The largest request [`call`] accepts (the op byte and the JSON).
pub const MAX_INPUT_BYTES: usize = 64 * 1024 + 1;
/// The most lookup answers or guarded addresses one call takes.
pub const MAX_ENTRIES: usize = 1024;
/// JSON nesting of a request.
const MAX_DEPTH: usize = 3;

/// Why the port will not answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Input,
    TooLarge,
    /// The JS answer could depend on the runtime or the resolver (see the crate docs).
    Ambiguous,
}

impl Refusal {
    pub fn json(self) -> &'static str {
        match self {
            Refusal::Input => r#"{"error":"input"}"#,
            Refusal::TooLarge => r#"{"error":"too_large"}"#,
            Refusal::Ambiguous => r#"{"error":"ambiguous"}"#,
        }
    }
}

/// What [`parse_spec`] returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Spec {
    /// The literals (normalised, distinct, in order) and host names (lower case, in order).
    Ok {
        literals: Vec<String>,
        hosts: Vec<String>,
    },
    /// The first entry that is neither; the JS throws naming it.
    Malformed(String),
}

/// `s` as ASCII bytes, or `None` when it holds any other code unit.
pub fn ascii(s: &[u16]) -> Option<Vec<u8>> {
    s.iter()
        .map(|&c| u8::try_from(c).ok().filter(u8::is_ascii))
        .collect()
}

/// ECMAScript white space within ASCII (`\s` and `trim`): tab, LF, VT, FF, CR, space.
fn is_space(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ')
}

fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&b| !is_space(b)).unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|&b| !is_space(b))
        .map_or(start, |i| i + 1);
    s.get(start..end).unwrap_or(&[])
}

/// One IPv4 part as Node's `v4Seg`: `25[0-5]|2[0-4]\d|1\d\d|[1-9]\d|\d`.
fn v4_part(p: &[u8]) -> Option<u8> {
    if p.is_empty() || p.len() > 3 || !p.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if p.len() > 1 && p.first() == Some(&b'0') {
        return None;
    }
    let n = p.iter().fold(0u32, |n, &d| n * 10 + u32::from(d - b'0'));
    u8::try_from(n).ok()
}

/// A dotted-quad IPv4 address as `net.isIPv4` accepts it.
pub fn parse_v4(s: &[u8]) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut n = 0;
    for p in s.split(|&b| b == b'.') {
        let slot = out.get_mut(n)?;
        *slot = v4_part(p)?;
        n += 1;
    }
    (n == 4).then_some(out)
}

/// `:`-separated groups; the last may be an IPv4 tail when `v4_last`. Empty text is no group.
fn groups(part: &[u8], v4_last: bool, out: &mut Vec<u16>) -> Option<()> {
    if part.is_empty() {
        return Some(());
    }
    let pieces: Vec<&[u8]> = part.split(|&b| b == b':').collect();
    let last = pieces.len().saturating_sub(1);
    for (i, p) in pieces.into_iter().enumerate() {
        if out.len() > 8 {
            return None;
        }
        if i == last && v4_last && p.contains(&b'.') {
            let [a, b, c, d] = parse_v4(p)?;
            out.push(u16::from_be_bytes([a, b]));
            out.push(u16::from_be_bytes([c, d]));
        } else {
            if p.is_empty() || p.len() > 4 || !p.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            let v = p.iter().fold(0u32, |n, &h| {
                n * 16 + char::from(h).to_digit(16).unwrap_or(0)
            });
            out.push(u16::try_from(v).ok()?);
        }
    }
    Some(())
}

/// An IPv6 address without a zone, as Node's `IPv6Reg` accepts it, as its eight groups.
pub fn parse_v6(s: &[u8]) -> Option<[u16; 8]> {
    let mut g = Vec::with_capacity(9);
    let at = s.windows(2).position(|w| w == b"::");
    let mut out = [0u16; 8];
    match at {
        Some(p) => {
            groups(s.get(..p)?, false, &mut g)?;
            let left = g.len();
            groups(s.get(p + 2..)?, true, &mut g)?;
            if g.len() > 7 {
                return None;
            }
            let right = g.len() - left;
            for (i, v) in g.iter().enumerate() {
                let to = if i < left { i } else { 8 - right + (i - left) };
                *out.get_mut(to)? = *v;
            }
        }
        None => {
            groups(s, true, &mut g)?;
            if g.len() != 8 {
                return None;
            }
            for (slot, v) in out.iter_mut().zip(g) {
                *slot = v;
            }
        }
    }
    Some(out)
}

/// `s` split at its first `%` into the address and the zone, if any.
fn split_zone(s: &[u8]) -> (&[u8], Option<&[u8]>) {
    match s.iter().position(|&b| b == b'%') {
        Some(p) => (s.get(..p).unwrap_or(&[]), s.get(p + 1..)),
        None => (s, None),
    }
}

/// `net.isIPv6`.
pub fn is_ipv6(s: &[u8]) -> bool {
    let (addr, zone) = split_zone(s);
    let zone_ok = zone.is_none_or(|z| {
        !z.is_empty()
            && z.iter()
                .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b':'))
    });
    zone_ok && parse_v6(addr).is_some()
}

/// `net.isIP(s) !== 0`.
pub fn is_ip(s: &[u8]) -> bool {
    parse_v4(s).is_some() || is_ipv6(s)
}

/// `normalizeAddress` over ASCII text.
pub fn normalize(s: &[u8]) -> Vec<u8> {
    let t = trim(s).to_ascii_lowercase();
    if let Some(rest) = t.strip_prefix(b"::ffff:") {
        let parts: Vec<&[u8]> = rest.split(|&b| b == b'.').collect();
        if parts.len() == 4
            && parts
                .iter()
                .all(|p| (1..=3).contains(&p.len()) && p.iter().all(u8::is_ascii_digit))
        {
            return rest.to_vec();
        }
    }
    t
}

/// The host-name pattern of `parseCodeNetSpec` (case-insensitive).
fn host_ok(s: &[u8]) -> bool {
    let alnum = |b: &u8| b.is_ascii_alphanumeric();
    (1..=253).contains(&s.len())
        && s.first().is_some_and(alnum)
        && s.last().is_some_and(alnum)
        && s.iter().all(|b| alnum(b) || matches!(b, b'.' | b'-'))
}

/// A host name whose resolution the port will not vouch for (see the crate docs).
fn host_ambiguous(lower: &[u8]) -> bool {
    let labels: Vec<&[u8]> = lower.split(|&b| b == b'.').collect();
    let digits = |l: &[u8]| !l.is_empty() && l.iter().all(u8::is_ascii_digit);
    let numberish = |l: &[u8]| {
        digits(l)
            || l.strip_prefix(b"0x")
                .is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit))
    };
    labels.iter().any(|l| l.starts_with(b"xn--"))
        || labels.iter().all(|l| numberish(l))
        || labels.last().is_some_and(|l| digits(l))
}

fn text(b: &[u8]) -> String {
    b.iter().map(|&c| char::from(c)).collect()
}

/// `parseCodeNetSpec(raw)`.
pub fn parse_spec(raw: &[u16]) -> Result<Spec, Refusal> {
    let raw = ascii(raw).ok_or(Refusal::Ambiguous)?;
    let mut literals = Vec::new();
    let mut seen = HashSet::new();
    let mut hosts = Vec::new();
    for entry in raw
        .split(|&b| is_space(b) || b == b',')
        .filter(|e| !e.is_empty())
    {
        if is_ip(entry) {
            if entry.contains(&b'%') {
                return Err(Refusal::Ambiguous);
            }
            let n = text(&normalize(entry));
            if seen.insert(n.clone()) {
                literals.push(n);
            }
        } else if host_ok(entry) {
            let lower = entry.to_ascii_lowercase();
            if host_ambiguous(&lower) {
                return Err(Refusal::Ambiguous);
            }
            hosts.push(text(&lower));
        } else {
            return Ok(Spec::Malformed(text(entry)));
        }
    }
    Ok(Spec::Ok { literals, hosts })
}

/// What `resolveOnce` keeps of one lookup: each answer (`None`: not a string) normalised, those
/// that are IP addresses, in order.
pub fn resolved_addresses(answers: &[Option<&[u16]>]) -> Result<Vec<String>, Refusal> {
    if answers.len() > MAX_ENTRIES {
        return Err(Refusal::TooLarge);
    }
    let mut out = Vec::new();
    for a in answers.iter().flatten() {
        let a = ascii(a).ok_or(Refusal::Ambiguous)?;
        let n = normalize(&a);
        if is_ip(&n) {
            out.push(text(&n));
        }
    }
    Ok(out)
}

/// One address as a comparable value: IPv4 (an IPv4-mapped IPv6 address folded to it) or IPv6.
/// Zoned addresses have none (only the exact text matches them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Key {
    V4([u8; 4]),
    V6([u16; 8]),
}

fn key(n: &[u8]) -> Option<Key> {
    if let Some(v4) = parse_v4(n) {
        return Some(Key::V4(v4));
    }
    if n.contains(&b'%') {
        return None;
    }
    let g = parse_v6(n)?;
    if let [0, 0, 0, 0, 0, 0xffff, hi, lo] = g {
        let [a, b] = hi.to_be_bytes();
        let [c, d] = lo.to_be_bytes();
        return Some(Key::V4([a, b, c, d]));
    }
    Some(Key::V6(g))
}

/// `addresses.has(normalizeAddress(local))` (`None`: not a string), and also true where the two
/// are the same IP spelled differently. Every guarded address must be an IP address.
pub fn refuses(addresses: &[&[u16]], local: Option<&[u16]>) -> Result<bool, Refusal> {
    if addresses.len() > MAX_ENTRIES {
        return Err(Refusal::TooLarge);
    }
    let mut exact = HashSet::with_capacity(addresses.len());
    let mut keys = HashSet::with_capacity(addresses.len());
    for a in addresses {
        let a = ascii(a).ok_or(Refusal::Ambiguous)?;
        if !is_ip(&a) {
            return Err(Refusal::Input);
        }
        if let Some(k) = key(&a) {
            keys.insert(k);
        }
        exact.insert(a);
    }
    let Some(local) = local else {
        // normalizeAddress(undefined) is '', which no address equals.
        return Ok(false);
    };
    let n = normalize(&ascii(local).ok_or(Refusal::Ambiguous)?);
    if exact.contains(&n) {
        return Ok(true);
    }
    Ok(key(&n).is_some_and(|k| keys.contains(&k)))
}

fn string_list(v: &Value) -> Option<Vec<&[u16]>> {
    let Value::Arr(items) = v else { return None };
    items.iter().map(Value::as_str).collect()
}

fn push_list(out: &mut Vec<u8>, items: &[String]) {
    out.push(b'[');
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        json::push_ascii(out, s);
    }
    out.push(b']');
}

fn run(op: u8, tree: &Value) -> Result<Vec<u8>, Refusal> {
    let mut out = Vec::new();
    match op {
        1 => {
            let raw = tree.as_str().ok_or(Refusal::Input)?;
            match parse_spec(raw)? {
                Spec::Ok { literals, hosts } => {
                    out.extend_from_slice(b"{\"literals\":");
                    push_list(&mut out, &literals);
                    out.extend_from_slice(b",\"hosts\":");
                    push_list(&mut out, &hosts);
                    out.push(b'}');
                }
                Spec::Malformed(entry) => {
                    out.extend_from_slice(b"{\"malformed\":");
                    json::push_ascii(&mut out, &entry);
                    out.push(b'}');
                }
            }
        }
        2 => {
            let Value::Arr(items) = tree else {
                return Err(Refusal::Input);
            };
            let answers = items
                .iter()
                .map(|v| match v {
                    Value::Str(s) => Ok(Some(s.as_slice())),
                    Value::Null => Ok(None),
                    _ => Err(Refusal::Input),
                })
                .collect::<Result<Vec<_>, _>>()?;
            out.extend_from_slice(b"{\"addresses\":");
            push_list(&mut out, &resolved_addresses(&answers)?);
            out.push(b'}');
        }
        3 => {
            let Value::Arr(pair) = tree else {
                return Err(Refusal::Input);
            };
            let [addresses, local] = pair.as_slice() else {
                return Err(Refusal::Input);
            };
            let addresses = string_list(addresses).ok_or(Refusal::Input)?;
            let local = match local {
                Value::Str(s) => Some(s.as_slice()),
                Value::Null => None,
                _ => return Err(Refusal::Input),
            };
            let r = refuses(&addresses, local)?;
            out.extend_from_slice(if r {
                b"{\"refuses\":true}"
            } else {
                b"{\"refuses\":false}"
            });
        }
        _ => return Err(Refusal::Input),
    }
    Ok(out)
}

/// The wasm call: input `u8(op)` and UTF-8 JSON. Op 1 `COWORK_CODE_NET_ADDR` (a string), reply
/// `{"literals":[…],"hosts":[…]}` or `{"malformed":"entry"}`; op 2 `[answer|null,…]`, reply
/// `{"addresses":[…]}`; op 3 `[[address,…], localAddress|null]`, reply `{"refuses":bool}`.
/// Status 1 refuses with `{"error":"input"|"too_large"|"ambiguous"}`.
pub fn call(input: &[u8]) -> (u32, String) {
    if input.len() > MAX_INPUT_BYTES {
        return (1, Refusal::TooLarge.json().to_owned());
    }
    let Some((&op, body)) = input.split_first() else {
        return (1, Refusal::Input.json().to_owned());
    };
    let Some(tree) = json::parse_utf8(body, MAX_DEPTH) else {
        return (1, Refusal::Input.json().to_owned());
    };
    match run(op, &tree).map(String::from_utf8) {
        Ok(Ok(s)) => (0, s),
        Ok(Err(_)) => (1, Refusal::Input.json().to_owned()),
        Err(r) => (1, r.json().to_owned()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use prompt_framing::js::units;

    fn ip(s: &str) -> bool {
        is_ip(s.as_bytes())
    }

    #[test]
    fn node_is_ip() {
        for s in [
            "0.0.0.0",
            "255.255.255.255",
            "172.30.0.2",
            "::",
            "::1",
            "1::",
            "1:2:3:4:5:6:7:8",
            "1:2:3:4:5:6:7::",
            "::2:3:4:5:6:7:8",
            "1:2:3:4:5::1.2.3.4",
            "1:2:3:4:5:6:1.2.3.4",
            "::ffff:1.2.3.4",
            "::1.2.3.4",
            "FE80::1",
            "fe80::1%eth0",
            "fe80::1%a.b-c:d",
        ] {
            assert!(ip(s), "{s}");
        }
        for s in [
            "",
            "1.2.3",
            "01.2.3.4",
            "1.2.3.256",
            "1.2.3.4.",
            ":::",
            "1:::2",
            "1::2::3",
            ":1::2",
            "1::2:",
            "1:2:3:4:5:6:7:8:9",
            "1:2:3:4:5:6:7:8::",
            "1:2:3:4:5:6::1.2.3.4",
            "1.2.3.4::",
            "12345::",
            "fe80::1%",
            "fe80::1%e/0",
            "g::",
            " ::1",
        ] {
            assert!(!ip(s), "{s}");
        }
    }

    #[test]
    fn normalize_maps() {
        assert_eq!(normalize(b" ::FFFF:172.30.0.2\t"), b"172.30.0.2");
        assert_eq!(normalize(b"::ffff:999.1.1.1"), b"999.1.1.1");
        assert_eq!(normalize(b"::ffff:7f00:1"), b"::ffff:7f00:1");
        assert_eq!(normalize(b"FE80::A"), b"fe80::a");
    }

    #[test]
    fn spec() {
        assert_eq!(
            parse_spec(&units(" 172.30.0.2,Egress  ::FFFF:172.30.0.2,egress ")),
            Ok(Spec::Ok {
                literals: vec!["172.30.0.2".into()],
                hosts: vec!["egress".into(), "egress".into()]
            })
        );
        assert_eq!(
            parse_spec(&units("egress,a_b")),
            Ok(Spec::Malformed("a_b".into()))
        );
        for amb in [
            "fe80::1%eth0",
            "xn--e1a.example",
            "2130706433",
            "010.0.0.1",
            "0x7f.1",
            "a.1",
            "egr\u{e9}ss",
            "egress\u{a0}",
        ] {
            assert_eq!(parse_spec(&units(amb)), Err(Refusal::Ambiguous), "{amb}");
        }
    }

    #[test]
    fn refuses_exact_and_same_ip() {
        let a = [units("172.30.0.2"), units("0:0::1")];
        let a: Vec<&[u16]> = a.iter().map(Vec::as_slice).collect();
        let r = |l: &str| refuses(&a, Some(&units(l))).unwrap();
        assert!(r("172.30.0.2"));
        assert!(r("::ffff:172.30.0.2"));
        assert!(r("::ffff:ac1e:2"), "stricter: same IP, other spelling");
        assert!(r("::1"), "stricter: same IP, other spelling");
        assert!(!r("172.30.0.3"));
        assert!(!refuses(&a, None).unwrap());
        assert_eq!(
            refuses(&[&units("egress")], Some(&units("x"))),
            Err(Refusal::Input)
        );
    }

    #[test]
    fn call_shapes() {
        assert_eq!(
            call(b"\x01\"egress\""),
            (0, r#"{"literals":[],"hosts":["egress"]}"#.into())
        );
        assert_eq!(
            call(b"\x02[\" ::ffff:1.2.3.4 \",null,\"nope\"]"),
            (0, r#"{"addresses":["1.2.3.4"]}"#.into())
        );
        assert_eq!(
            call(b"\x03[[\"1.2.3.4\"],\"1.2.3.4\"]"),
            (0, r#"{"refuses":true}"#.into())
        );
        assert_eq!(call(b""), (1, Refusal::Input.json().into()));
        assert_eq!(call(b"\x09\"x\""), (1, Refusal::Input.json().into()));
        assert_eq!(call(b"\x03[[],1]"), (1, Refusal::Input.json().into()));
    }
}
