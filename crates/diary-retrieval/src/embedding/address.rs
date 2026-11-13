//! Where an embeddings server may be (mirrors noevia-core's `embedding-address.cjs`).
//!
//! A re-index sends the Diary's text to that address, so it must be on this network: loopback,
//! a private range, or a compose service name that resolves to one. The decision is positive:
//! the URL must parse, the host must resolve (a literal is its own answer) and EVERY answer must
//! be a local address. Public, unspecified, link-local, multicast and reserved addresses, and the
//! IPv6 forms that carry another address, are refused. An unresolvable name is "transient" (it
//! may resolve later); a public or malformed answer is permanent.
//!
//! The address classification reproduces CPython 3.12.15's `ipaddress` (`is_global`,
//! `is_unspecified`, `is_link_local`, `is_multicast` and their range tables, the strict parsers,
//! zone ids) and the part of `urllib.parse.urlsplit` a URL check uses (scheme, netloc, hostname,
//! port, username, password and the bracket and NFKC netloc checks). The fixtures record Python's
//! answer for several hundred addresses and URLs, including every range boundary.

/// An address: IPv4 as its 32-bit value, IPv6 as its 128-bit value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ip {
    V4(u32),
    V6(u128),
}

// ---- parsing: `ipaddress.ip_address(str)` ----

fn parse_octet(octet: &str) -> Option<u8> {
    if octet.is_empty() || !octet.bytes().all(|b| b.is_ascii_digit()) || octet.len() > 3 {
        return None;
    }
    if octet != "0" && octet.starts_with('0') {
        return None; // leading zeros are not permitted
    }
    octet.parse::<u8>().ok()
}

/// `IPv4Address(text)`: exactly four decimal octets, no slash.
pub fn parse_ipv4(text: &str) -> Option<u32> {
    if text.contains('/') || text.is_empty() {
        return None;
    }
    let mut octets = text.split('.');
    let mut value = 0u32;
    for _ in 0..4 {
        value = (value << 8) | u32::from(parse_octet(octets.next()?)?);
    }
    octets.next().is_none().then_some(value)
}

fn parse_hextet(hextet: &str) -> Option<u128> {
    if hextet.is_empty() || hextet.len() > 4 || !hextet.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u128::from_str_radix(hextet, 16).ok()
}

/// `IPv6Address(text)`: the strict parser of `_ip_int_from_string`, with an optional `%scope`
/// (non-empty, no second `%`).
pub fn parse_ipv6(text: &str) -> Option<u128> {
    if text.contains('/') {
        return None;
    }
    let addr = match text.split_once('%') {
        None => text,
        Some((addr, scope)) => {
            if scope.is_empty() || scope.contains('%') {
                return None;
            }
            addr
        }
    };
    if addr.is_empty() || addr.chars().count() > 45 {
        return None;
    }
    // maxsplit=9: at most ten parts, the last keeping any further colons.
    let mut parts: Vec<String> = addr.splitn(10, ':').map(str::to_owned).collect();
    if parts.len() < 3 {
        return None;
    }
    if parts.last().is_some_and(|p| p.contains('.')) {
        let v4 = parse_ipv4(&parts.pop()?)?;
        parts.push(format!("{:x}", (v4 >> 16) & 0xffff));
        parts.push(format!("{:x}", v4 & 0xffff));
    }
    if parts.len() > 9 {
        return None;
    }
    let len = parts.len();
    let is_empty = |i: usize| parts.get(i).is_some_and(String::is_empty);
    let mut skip_index = None;
    for i in 1..len.saturating_sub(1) {
        if is_empty(i) {
            if skip_index.is_some() {
                return None; // at most one '::'
            }
            skip_index = Some(i);
        }
    }
    let (parts_hi, parts_lo, skipped);
    if let Some(skip) = skip_index {
        let mut hi = skip;
        let mut lo = len - skip - 1;
        if is_empty(0) {
            hi -= 1;
            if hi != 0 {
                return None;
            }
        }
        if is_empty(len - 1) {
            lo -= 1;
            if lo != 0 {
                return None;
            }
        }
        let used = hi + lo;
        if used > 7 {
            return None;
        }
        (parts_hi, parts_lo, skipped) = (hi, lo, 8 - used);
    } else {
        if len != 8 || is_empty(0) || is_empty(len - 1) {
            return None;
        }
        (parts_hi, parts_lo, skipped) = (len, 0, 0);
    }
    let mut value = 0u128;
    for part in parts.iter().take(parts_hi) {
        value = (value << 16) | parse_hextet(part)?;
    }
    value = if skipped >= 8 {
        0
    } else {
        value << (16 * skipped)
    };
    for part in parts.iter().skip(len - parts_lo) {
        value = (value << 16) | parse_hextet(part)?;
    }
    Some(value)
}

/// `ipaddress.ip_address(text)`: IPv4 first, then IPv6.
pub fn ip_address(text: &str) -> Option<Ip> {
    parse_ipv4(text)
        .map(Ip::V4)
        .or_else(|| parse_ipv6(text).map(Ip::V6))
}

// ---- classification ----

fn in4(ip: u32, net: u32, prefix: u32) -> bool {
    match 32u32.checked_sub(prefix) {
        Some(shift) if shift < 32 => ip >> shift == net >> shift,
        _ => true,
    }
}

fn in6(ip: u128, net: u128, prefix: u32) -> bool {
    match 128u32.checked_sub(prefix) {
        Some(shift) if shift < 128 => ip >> shift == net >> shift,
        _ => true,
    }
}

const fn v4(a: u8, b: u8, c: u8, d: u8) -> u32 {
    u32::from_be_bytes([a, b, c, d])
}

const fn v6(h: [u16; 8]) -> u128 {
    let [a, b, c, d, e, f, g, i] = h;
    (a as u128) << 112
        | (b as u128) << 96
        | (c as u128) << 80
        | (d as u128) << 64
        | (e as u128) << 48
        | (f as u128) << 32
        | (g as u128) << 16
        | i as u128
}

/// `_IPv4Constants._private_networks` (not globally reachable, IANA special-purpose registry).
const V4_PRIVATE: [(u32, u32); 14] = [
    (v4(0, 0, 0, 0), 8),
    (v4(10, 0, 0, 0), 8),
    (v4(127, 0, 0, 0), 8),
    (v4(169, 254, 0, 0), 16),
    (v4(172, 16, 0, 0), 12),
    (v4(192, 0, 0, 0), 24),
    (v4(192, 0, 0, 170), 31),
    (v4(192, 0, 2, 0), 24),
    (v4(192, 168, 0, 0), 16),
    (v4(198, 18, 0, 0), 15),
    (v4(198, 51, 100, 0), 24),
    (v4(203, 0, 113, 0), 24),
    (v4(240, 0, 0, 0), 4),
    (v4(255, 255, 255, 255), 32),
];
const V4_PRIVATE_EXCEPTIONS: [(u32, u32); 2] = [(v4(192, 0, 0, 9), 32), (v4(192, 0, 0, 10), 32)];
/// `_IPv4Constants._public_network` (shared address space: neither private nor global).
const V4_SHARED: (u32, u32) = (v4(100, 64, 0, 0), 10);
const V4_LINK_LOCAL: (u32, u32) = (v4(169, 254, 0, 0), 16);
const V4_MULTICAST: (u32, u32) = (v4(224, 0, 0, 0), 4);
/// `_V4_RESERVED` of the module.
const V4_RESERVED: [(u32, u32); 5] = [
    (v4(0, 0, 0, 0), 8),
    (v4(192, 0, 0, 0), 24),
    (v4(192, 0, 2, 0), 24),
    (v4(198, 51, 100, 0), 24),
    (v4(203, 0, 113, 0), 24),
];

const V6_PRIVATE: [(u128, u32); 11] = [
    (1, 128),
    (0, 128),
    (v6([0, 0, 0, 0, 0, 0xffff, 0, 0]), 96),
    (v6([0x64, 0xff9b, 1, 0, 0, 0, 0, 0]), 48),
    (v6([0x100, 0, 0, 0, 0, 0, 0, 0]), 64),
    (v6([0x2001, 0, 0, 0, 0, 0, 0, 0]), 23),
    (v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0]), 32),
    (v6([0x2002, 0, 0, 0, 0, 0, 0, 0]), 16),
    (v6([0x3fff, 0, 0, 0, 0, 0, 0, 0]), 20),
    (v6([0xfc00, 0, 0, 0, 0, 0, 0, 0]), 7),
    (v6([0xfe80, 0, 0, 0, 0, 0, 0, 0]), 10),
];
const V6_PRIVATE_EXCEPTIONS: [(u128, u32); 6] = [
    (v6([0x2001, 1, 0, 0, 0, 0, 0, 1]), 128),
    (v6([0x2001, 1, 0, 0, 0, 0, 0, 2]), 128),
    (v6([0x2001, 3, 0, 0, 0, 0, 0, 0]), 32),
    (v6([0x2001, 4, 0x112, 0, 0, 0, 0, 0]), 48),
    (v6([0x2001, 0x20, 0, 0, 0, 0, 0, 0]), 28),
    (v6([0x2001, 0x30, 0, 0, 0, 0, 0, 0]), 28),
];
const V6_LINK_LOCAL: (u128, u32) = (v6([0xfe80, 0, 0, 0, 0, 0, 0, 0]), 10);
const V6_MULTICAST: (u128, u32) = (v6([0xff00, 0, 0, 0, 0, 0, 0, 0]), 8);
/// `_V6_RESERVED` of the module.
///
/// Beyond Python's list, `2002::/16` (6to4) and `64:ff9b:1::/48` (local-use NAT64) are refused:
/// both embed an IPv4 address that may be public (known difference 10).
const V6_RESERVED: [(u128, u32); 6] = [
    (v6([0x2002, 0, 0, 0, 0, 0, 0, 0]), 16),
    (v6([0x64, 0xff9b, 1, 0, 0, 0, 0, 0]), 48),
    (v6([0x64, 0xff9b, 0, 0, 0, 0, 0, 0]), 96),
    (v6([0x2001, 0, 0, 0, 0, 0, 0, 0]), 32),
    (v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0]), 32),
    (v6([0x100, 0, 0, 0, 0, 0, 0, 0]), 64),
];

fn v4_private(ip: u32) -> bool {
    V4_PRIVATE.iter().any(|&(n, p)| in4(ip, n, p))
        && !V4_PRIVATE_EXCEPTIONS.iter().any(|&(n, p)| in4(ip, n, p))
}

fn v4_is_global(ip: u32) -> bool {
    !in4(ip, V4_SHARED.0, V4_SHARED.1) && !v4_private(ip)
}

fn v6_private(ip: u128) -> bool {
    V6_PRIVATE.iter().any(|&(n, p)| in6(ip, n, p))
        && !V6_PRIVATE_EXCEPTIONS.iter().any(|&(n, p)| in6(ip, n, p))
}

/// `non_local_reason(address)`: `None` when `address` is on this network (loopback, private,
/// Tailscale), else why not. A `%zone` suffix is ignored; an IPv4-mapped IPv6 address is judged
/// as the IPv4 address it carries.
pub fn non_local_reason(address: &str) -> Option<&'static str> {
    let bare = address.split('%').next().unwrap_or("");
    let Some(mut ip) = ip_address(bare) else {
        return Some("not an address");
    };
    if let Ip::V6(v) = ip {
        if v >> 32 == 0xffff {
            ip = Ip::V4((v & 0xffff_ffff) as u32);
        }
    }
    match ip {
        Ip::V4(a) => {
            if v4_is_global(a) {
                return Some("a public address");
            }
            if a == 0 {
                return Some("the unspecified address");
            }
            if in4(a, V4_LINK_LOCAL.0, V4_LINK_LOCAL.1) {
                return Some("a link-local address");
            }
            if in4(a, V4_MULTICAST.0, V4_MULTICAST.1) {
                return Some("a multicast address");
            }
            if a >> 24 >= 240 || V4_RESERVED.iter().any(|&(n, p)| in4(a, n, p)) {
                return Some("a reserved address");
            }
        }
        Ip::V6(a) => {
            if !v6_private(a) {
                return Some("a public address");
            }
            if a == 0 {
                return Some("the unspecified address");
            }
            if in6(a, V6_LINK_LOCAL.0, V6_LINK_LOCAL.1) {
                return Some("a link-local address");
            }
            if in6(a, V6_MULTICAST.0, V6_MULTICAST.1) {
                return Some("a multicast address");
            }
            // ::/96 minus ::1: the IPv4-compatible forms.
            if (a >> 32 == 0 && a != 1) || V6_RESERVED.iter().any(|&(n, p)| in6(a, n, p)) {
                return Some("a reserved address");
            }
        }
    }
    None
}

// ---- urlsplit ----

/// Characters (beyond ASCII) whose NFKC form contains one of `/ ? # @ :`, the ones
/// `urllib.parse._checknetloc` refuses in a netloc. Unicode 15.0, recorded from CPython 3.12.
const NFKC_NETLOC_RANGES: [(u32, u32); 13] = [
    (0x2047, 0x2049),
    (0x2100, 0x2101),
    (0x2105, 0x2106),
    (0x2A74, 0x2A74),
    (0xFE13, 0xFE13),
    (0xFE16, 0xFE16),
    (0xFE55, 0xFE56),
    (0xFE5F, 0xFE5F),
    (0xFE6B, 0xFE6B),
    (0xFF03, 0xFF03),
    (0xFF0F, 0xFF0F),
    (0xFF1A, 0xFF1A),
    (0xFF1F, 0xFF20),
];

/// The non-ASCII characters of the NFKC table, for the differential test.
pub fn nfkc_netloc_chars() -> Vec<u32> {
    NFKC_NETLOC_RANGES
        .iter()
        .flat_map(|&(a, b)| a..=b)
        .collect()
}

/// `urlsplit(url)` reduced to what the check reads: `(scheme, netloc)`, or `Err` for the
/// `ValueError`s Python raises.
fn urlsplit(url: &str) -> Result<(String, String), ()> {
    let url = url.trim_start_matches(|c: char| u32::from(c) <= 0x20);
    let cleaned: String = url
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let mut rest = cleaned.as_str();
    let mut scheme = String::new();
    if let Some(i) = rest.find(':') {
        let head = rest.get(..i).unwrap_or("");
        let first_ok = head.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
        if i > 0
            && first_ok
            && head
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            scheme = head.to_ascii_lowercase();
            rest = rest.get(i + 1..).unwrap_or("");
        }
    }
    let mut netloc = String::new();
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = after.get(..end).unwrap_or("").to_owned();
        let has_open = netloc.contains('[');
        let has_close = netloc.contains(']');
        if has_open != has_close {
            return Err(());
        }
        if has_open && has_close {
            check_bracketed_netloc(&netloc)?;
        }
    }
    check_netloc(&netloc)?;
    Ok((scheme, netloc))
}

fn check_bracketed_netloc(netloc: &str) -> Result<(), ()> {
    let hostinfo = netloc.rsplit('@').next().unwrap_or("");
    let hostname = match hostinfo.split_once('[') {
        Some((before, bracketed)) => {
            if !before.is_empty() {
                return Err(());
            }
            let (hostname, after) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            if !after.is_empty() && !after.starts_with(':') {
                return Err(());
            }
            hostname
        }
        None => hostinfo.split_once(':').map_or(hostinfo, |(h, _)| h),
    };
    check_bracketed_host(hostname)
}

fn check_bracketed_host(hostname: &str) -> Result<(), ()> {
    if let Some(rest) = hostname.strip_prefix('v') {
        // \Av[a-fA-F0-9]+\..+\Z
        let hex = rest.chars().take_while(char::is_ascii_hexdigit).count();
        let tail = rest.get(hex..).unwrap_or("");
        let ok = hex > 0
            && tail
                .strip_prefix('.')
                .is_some_and(|t| !t.is_empty() && !t.contains('\n'));
        return if ok { Ok(()) } else { Err(()) };
    }
    match ip_address(hostname) {
        Some(Ip::V6(_)) => Ok(()),
        _ => Err(()), // not an address, or an IPv4 address in brackets
    }
}

fn check_netloc(netloc: &str) -> Result<(), ()> {
    if netloc.is_empty() || netloc.is_ascii() {
        return Ok(());
    }
    let bad = netloc.chars().any(|c| {
        let n = u32::from(c);
        NFKC_NETLOC_RANGES
            .iter()
            .any(|&(a, b)| (a..=b).contains(&n))
    });
    if bad {
        Err(())
    } else {
        Ok(())
    }
}

/// `(hostname, port)` of `_hostinfo`, the hostname before any lower-casing.
fn host_and_port(netloc: &str) -> (String, Option<String>) {
    let hostinfo = netloc.rsplit('@').next().unwrap_or("");
    let (hostname, port) = match hostinfo.split_once('[') {
        Some((_, bracketed)) => {
            let (hostname, after) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            let port = after.split_once(':').map_or("", |(_, p)| p);
            (hostname, port)
        }
        None => hostinfo.split_once(':').unwrap_or((hostinfo, "")),
    };
    (
        hostname.to_owned(),
        (!port.is_empty()).then(|| port.to_owned()),
    )
}

/// `parts.port`: raises `ValueError` for a port that is not ASCII digits or is above 65535.
fn port_ok(port: Option<&str>) -> bool {
    let Some(port) = port else {
        return true;
    };
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let digits = port.trim_start_matches('0');
    digits.len() <= 5
        && digits
            .parse::<u32>()
            .map_or(digits.is_empty(), |v| v <= 65535)
}

/// `parts.username`, `parts.password` of the netloc (`None` when there is no `@` / `:`).
fn userinfo(netloc: &str) -> (Option<String>, Option<String>) {
    match netloc.rsplit_once('@') {
        None => (None, None),
        Some((user, _)) => match user.split_once(':') {
            Some((name, password)) => (Some(name.to_owned()), Some(password.to_owned())),
            None => (Some(user.to_owned()), None),
        },
    }
}

/// `parts.hostname`: lower-cased except for a `%zone`; `None` when empty.
fn hostname(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    Some(match raw.split_once('%') {
        Some((host, zone)) => format!("{}%{zone}", host.to_lowercase()),
        None => raw.to_lowercase(),
    })
}

// ---- check_embedding_address ----

/// The answer of `check_embedding_address`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddressCheck {
    pub ok: bool,
    pub reason: String,
    pub transient: bool,
}

impl AddressCheck {
    pub fn ok() -> Self {
        Self {
            ok: true,
            ..Self::default()
        }
    }

    pub fn refused(reason: impl Into<String>) -> Self {
        Self {
            ok: false,
            reason: reason.into(),
            transient: false,
        }
    }

    pub fn transient(reason: impl Into<String>) -> Self {
        Self {
            ok: false,
            reason: reason.into(),
            transient: true,
        }
    }
}

/// A resolver that fails: the Python exception's type name (`gaierror`, `TimeoutError`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveError {
    pub type_name: String,
}

/// Python's default resolver, `getaddrinfo` for a stream socket, sorted and de-duplicated.
/// A scope id is not part of the text here (Python's would read `fe80::1%eth0`); every failure
/// is reported as `gaierror`.
pub fn system_resolver(host: &str) -> Result<Vec<String>, ResolveError> {
    use std::net::ToSocketAddrs;
    let found = (host, 0u16).to_socket_addrs().map_err(|_| ResolveError {
        type_name: "gaierror".to_owned(),
    })?;
    let mut out: Vec<String> = found.map(|a| a.ip().to_string()).collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// `check_embedding_address(url, resolver)`. Never fails; `resolver(host)` returns every
/// address of the name.
pub fn check_embedding_address(
    url: &str,
    resolver: &mut dyn FnMut(&str) -> Result<Vec<String>, ResolveError>,
) -> AddressCheck {
    let not_a_url = || AddressCheck::refused("not a URL");
    let Ok((scheme, netloc)) = urlsplit(url) else {
        return not_a_url();
    };
    let (raw_host, port) = host_and_port(&netloc);
    if !port_ok(port.as_deref()) {
        return not_a_url();
    }
    let host = hostname(&raw_host).unwrap_or_default();
    if scheme != "http" && scheme != "https" {
        return AddressCheck::refused("not an http(s) URL");
    }
    let (user, password) = userinfo(&netloc);
    if user.is_some_and(|u| !u.is_empty()) || password.is_some_and(|p| !p.is_empty()) {
        return AddressCheck::refused("credentials in the URL");
    }
    if host.is_empty() {
        return AddressCheck::refused("no host");
    }
    let addresses = if ip_address(&host).is_some() {
        vec![host.clone()]
    } else {
        match resolver(&host) {
            Ok(list) => list,
            Err(e) => {
                return AddressCheck::transient(format!(
                    "the name does not resolve ({})",
                    e.type_name
                ))
            }
        }
    };
    if addresses.is_empty() {
        return AddressCheck::transient("the name does not resolve");
    }
    for address in &addresses {
        if let Some(why) = non_local_reason(address) {
            return AddressCheck::refused(format!(
                "{host} is not on this network ({address} is {why})"
            ));
        }
    }
    AddressCheck::ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_and_public_addresses() {
        assert_eq!(non_local_reason("10.1.2.3"), None);
        assert_eq!(non_local_reason("127.0.0.1"), None);
        assert_eq!(non_local_reason("::1"), None);
        assert_eq!(
            non_local_reason("100.64.0.1"),
            None,
            "CGNAT is neither global nor private"
        );
        assert_eq!(non_local_reason("8.8.8.8"), Some("a public address"));
        assert_eq!(non_local_reason("::ffff:8.8.8.8"), Some("a public address"));
        assert_eq!(non_local_reason("::ffff:10.0.0.1"), None);
        assert_eq!(
            non_local_reason("fe80::1%eth0"),
            Some("a link-local address")
        );
        assert_eq!(non_local_reason("0.0.0.0"), Some("the unspecified address"));
        // Python's `is_global` is true for multicast, so the multicast reason is never reached.
        assert_eq!(non_local_reason("224.0.0.1"), Some("a public address"));
        assert_eq!(non_local_reason("ff02::1"), Some("a public address"));
        assert_eq!(non_local_reason("250.0.0.1"), Some("a reserved address"));
        // 6to4 and local-use NAT64 carry an IPv4 address: refused here, accepted by Python
        assert_eq!(
            non_local_reason("2002:808:808::1"),
            Some("a reserved address")
        );
        assert_eq!(
            non_local_reason("64:ff9b:1::808:808"),
            Some("a reserved address")
        );
        assert_eq!(non_local_reason("1.2.3"), Some("not an address"));
        assert_eq!(non_local_reason("01.2.3.4"), Some("not an address"));
    }

    #[test]
    fn urls_without_resolution() {
        let mut none = |_: &str| -> Result<Vec<String>, ResolveError> { Ok(vec![]) };
        assert!(check_embedding_address("http://10.0.0.5:8080/v1", &mut none).ok);
        assert!(check_embedding_address("http://[::1]:8080", &mut none).ok);
        assert_eq!(
            check_embedding_address("http://u:p@10.0.0.5", &mut none).reason,
            "credentials in the URL"
        );
        assert_eq!(
            check_embedding_address("http://h:99999", &mut none).reason,
            "not a URL"
        );
        assert_eq!(
            check_embedding_address("ftp://10.0.0.5", &mut none).reason,
            "not an http(s) URL"
        );
        let empty = check_embedding_address("http://qa-embed.invalid", &mut none);
        assert!(empty.transient && empty.reason == "the name does not resolve");
    }
}
