//! Address classification, ported string-for-string from noevia's `ssrf.cjs` (`isPrivateIp`)
//! and Node's `net.isIP`.
//!
//! The JS classifier works on the address *text*, not on a parsed address, so this port does
//! too: the differential corpus (`tests/fixtures/egress-diff.json`) holds the JS verdict for
//! every boundary and a seeded fuzz set, and the Rust answer must agree on all of them.
//!
//! The rule, in short: IPv4 is private in 0/8, 10/8, 127/8, 100.64/10, 169.254/16, 172.16/12,
//! 192.168/16, 192.0.0.0/24, 192.0.2.0/24, 192.88.99.0/24, 198.18/15, 198.51.100/24,
//! 203.0.113/24 and 224/3. IPv6 is parsed to 16 bytes and matched by prefix: `::/96`
//! (including IPv4-compatible) and any `%zone` are private; `::ffff:0:0/96` is judged by its
//! embedded IPv4; `fe80::/10`, `fc00::/7`, everything outside `2000::/3`, Teredo `2001::/32`,
//! `2001:db8::/32` and 6to4 `2002::/16` are private. Anything that is not an IP literal at
//! all is private (fail closed).

/// Node's `net.isIP`: 4 for a dotted-quad IPv4 literal, 6 for an IPv6 literal (optionally with a
/// `%zone`), 0 otherwise.
pub fn is_ip(s: &str) -> u8 {
    if is_ipv4(s) {
        4
    } else if is_ipv6(s) {
        6
    } else {
        0
    }
}

/// Node's IPv4 grammar: exactly four decimal octets 0-255, no leading zeros, nothing else.
pub fn is_ipv4(s: &str) -> bool {
    let mut count = 0usize;
    for part in s.split('.') {
        count += 1;
        if count > 4 || !is_v4_octet(part) {
            return false;
        }
    }
    count == 4
}

fn is_v4_octet(part: &str) -> bool {
    let b = part.as_bytes();
    if b.is_empty() || b.len() > 3 || !b.iter().all(u8::is_ascii_digit) {
        return false;
    }
    if b.len() > 1 && b.first() == Some(&b'0') {
        return false;
    }
    part.parse::<u16>().is_ok_and(|n| n <= 255)
}

/// Node's IPv6 grammar (`IPv6Reg` in lib/internal/net.js): eight 1-4 digit hex groups, or
/// fewer with exactly one `::` standing for at least one group; a dotted IPv4 tail counts as
/// two groups and may only end the address; an optional `%zone` of `[0-9A-Za-z.:-]+`.
pub fn is_ipv6(s: &str) -> bool {
    let (addr, zone) = match s.split_once('%') {
        Some((a, z)) => (a, Some(z)),
        None => (s, None),
    };
    if let Some(z) = zone {
        if z.is_empty()
            || !z
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.' || c == b':')
        {
            return false;
        }
    }
    match addr.split_once("::") {
        None => groups(addr, true).is_some_and(|n| n == 8),
        Some((head, tail)) => {
            if tail.contains("::") {
                return false;
            }
            let Some(h) = (if head.is_empty() {
                Some(0)
            } else {
                groups(head, false)
            }) else {
                return false;
            };
            let Some(t) = (if tail.is_empty() {
                Some(0)
            } else {
                groups(tail, true)
            }) else {
                return false;
            };
            h + t <= 7
        }
    }
}

/// Counts the groups of a non-empty `a:b:c` run; a dotted IPv4 tail (two groups) is only
/// accepted as the last element when `v4_tail_ok`.
fn groups(run: &str, v4_tail_ok: bool) -> Option<usize> {
    let parts: Vec<&str> = run.split(':').collect();
    let last = parts.len().checked_sub(1)?;
    let mut n = 0usize;
    for (i, p) in parts.iter().enumerate() {
        if i == last && v4_tail_ok && p.contains('.') {
            if !is_ipv4(p) {
                return None;
            }
            n += 2;
        } else if !p.is_empty() && p.len() <= 4 && p.bytes().all(|c| c.is_ascii_hexdigit()) {
            n += 1;
        } else {
            return None;
        }
    }
    Some(n)
}

/// `ssrf.cjs` `isPrivateIp`: true unless `ip` is an IP literal in public address space.
pub fn is_private_ip(ip: &str) -> bool {
    match is_ip(ip) {
        4 => parse_ipv4(ip).is_none_or(|o| is_private_ipv4(&o)),
        6 => parse_ipv6(ip).is_none_or(|b| is_private_ipv6(&b)),
        _ => true,
    }
}

/// A strict dotted quad as four octets (the only shape `is_ip` calls version 4).
fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    if !is_ipv4(s) {
        return None;
    }
    let mut out = [0u8; 4];
    let mut parts = s.split('.');
    for slot in &mut out {
        *slot = parts.next()?.parse().ok()?;
    }
    Some(out)
}

fn is_private_ipv4(o: &[u8; 4]) -> bool {
    let [a, b, c, _] = *o;
    a == 0 // "this network"
        || a == 10 // RFC1918
        || a == 127 // loopback
        || (a == 100 && (64..=127).contains(&b)) // CGNAT
        || (a == 169 && b == 254) // link-local
        || (a == 172 && (16..=31).contains(&b)) // RFC1918
        || (a == 192 && b == 168) // RFC1918
        || (a == 192 && b == 0 && (c == 0 || c == 2)) // 192.0.0.0/24 + 192.0.2.0/24 only
        || (a == 192 && b == 88 && c == 99) // 6to4 relay anycast
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || (a == 198 && b == 51 && c == 100) // TEST-NET-2
        || (a == 203 && b == 0 && c == 113) // TEST-NET-3
        || a >= 224 // multicast + reserved
}

/// An IPv6 literal as 16 bytes, or `None` when it is not one plain address. A zone
/// (`fe80::1%eth0`) is `None` too: a scoped address names an interface on this host, which is
/// never a public destination (`parseIPv6` in ssrf.cjs).
fn parse_ipv6(ip: &str) -> Option<[u8; 16]> {
    if ip.contains('%') || !is_ipv6(ip) {
        return None;
    }
    let (head, tail) = match ip.split_once("::") {
        Some((h, t)) => (h, Some(t)),
        None => (ip, None),
    };
    let head = hextets(head)?;
    let rest = match tail {
        Some(t) => hextets(t)?,
        None => Vec::new(),
    };
    let groups: Vec<u16> = match tail {
        Some(_) => {
            let fixed = head.len() + rest.len();
            if fixed > 7 {
                return None;
            }
            head.into_iter()
                .chain(std::iter::repeat_n(0, 8 - fixed))
                .chain(rest)
                .collect()
        }
        None => head,
    };
    if groups.len() != 8 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, g) in groups.iter().enumerate() {
        let [hi, lo] = g.to_be_bytes();
        *bytes.get_mut(2 * i)? = hi;
        *bytes.get_mut(2 * i + 1)? = lo;
    }
    Some(bytes)
}

/// `a:b:c` (possibly empty) as 16-bit groups; a dotted IPv4 tail is two groups.
fn hextets(run: &str) -> Option<Vec<u16>> {
    if run.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    let parts: Vec<&str> = run.split(':').collect();
    let last = parts.len() - 1;
    for (i, p) in parts.iter().enumerate() {
        if i == last && p.contains('.') {
            let [a, b, c, d] = parse_ipv4(p)?;
            out.push(u16::from_be_bytes([a, b]));
            out.push(u16::from_be_bytes([c, d]));
        } else {
            out.push(u16::from_str_radix(p, 16).ok().filter(|_| p.len() <= 4)?);
        }
    }
    Some(out)
}

/// True when the first `bits` bits of `bytes` equal those of `prefix` (zero-extended).
fn in_prefix(bytes: &[u8; 16], prefix: &[u8], bits: usize) -> bool {
    (0..bits).all(|i| {
        let mask = 0x80u8 >> (i & 7);
        let a = bytes.get(i >> 3).copied().unwrap_or(0);
        let b = prefix.get(i >> 3).copied().unwrap_or(0);
        a & mask == b & mask
    })
}

fn is_private_ipv6(b: &[u8; 16]) -> bool {
    // ::/96: unspecified, loopback and the deprecated IPv4-compatible ::a.b.c.d.
    if in_prefix(b, &[], 96) {
        return true;
    }
    // ::ffff:0:0/96 IPv4-mapped: the connection goes to the embedded IPv4, so judge that.
    if in_prefix(b, &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff], 96) {
        return b
            .get(12..16)
            .and_then(|s| <[u8; 4]>::try_from(s).ok())
            .is_none_or(|o| is_private_ipv4(&o));
    }
    in_prefix(b, &[0xfe, 0x80], 10) // link-local fe80::/10 (fe80-febf)
        || in_prefix(b, &[0xfc], 7) // ULA fc00::/7
        || !in_prefix(b, &[0x20], 3) // outside global unicast 2000::/3
        || in_prefix(b, &[0x20, 0x01, 0x00, 0x00], 32) // Teredo 2001::/32
        || in_prefix(b, &[0x20, 0x01, 0x0d, 0xb8], 32) // documentation 2001:db8::/32
        || in_prefix(b, &[0x20, 0x02], 16) // 6to4 2002::/16, refused outright
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_checks() {
        assert!(is_private_ip("127.0.0.1"));
        assert!(!is_private_ip("8.8.8.8"));
        assert!(is_private_ip("::ffff:10.0.0.1"));
        assert!(!is_private_ip("::ffff:8.8.8.8"));
        assert!(is_private_ip("64:ff9b::808:808"));
        assert!(!is_private_ip("2606:4700::1111"));
        assert!(is_private_ip("example.com"));
        assert_eq!(is_ip("fe80::1%eth0"), 6);
        assert_eq!(is_ip("1:2:3:4:5:6::1.2.3.4"), 0);
        assert_eq!(is_ip("::01.2.3.4"), 0);
    }

    #[test]
    fn special_purpose_ranges_are_private() {
        // 6to4 is refused outright, whatever IPv4 it embeds; Teredo and documentation too.
        for ip in [
            "2002::1",
            "2002:c0a8:101::1",
            "2002:808:808::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "2001:db8::1",
            "2001:DB8:ffff::1",
            "198.51.100.7",
            "203.0.113.7",
            "192.88.99.1",
            "192.0.0.9",
            "192.0.2.9",
        ] {
            assert!(is_private_ip(ip), "{ip}");
        }
        // Just outside each range stays public.
        for ip in [
            "2001:1::1",
            "2001:db7::1",
            "2001:db9::1",
            "2003::1",
            "198.51.99.1",
            "198.51.101.1",
            "203.0.112.1",
            "203.0.114.1",
            "192.88.98.1",
            "192.88.100.1",
            "192.0.1.1",
            "192.0.78.9",
        ] {
            assert!(!is_private_ip(ip), "{ip}");
        }
    }

    #[test]
    fn ipv6_prefixes_zones_and_mapped_addresses() {
        // fe80::/10 in full (fe80-febf), then fec0 is merely outside 2000::/3: private as well.
        for ip in ["fe80::1", "fe9f::1", "febf::1", "fec0::1"] {
            assert!(is_private_ip(ip), "{ip}");
        }
        // Any zone makes a scoped literal private, even a public-looking one.
        assert!(is_private_ip("2606:4700::1111%eth0"));
        assert!(is_private_ip("2606:4700::1111%1"));
        // All of ::/96, including IPv4-compatible ::a.b.c.d with a public tail.
        for ip in ["::", "::1", "::8.8.8.8", "::808:808", "::1:0:0"] {
            assert!(is_private_ip(ip), "{ip}");
        }
        // IPv4-mapped is judged by the embedded IPv4, in dotted or hex spelling.
        assert!(is_private_ip("::ffff:10.0.0.1"));
        assert!(is_private_ip("::ffff:0a00:0001"));
        assert!(is_private_ip("::ffff:c633:6401"));
        assert!(!is_private_ip("::ffff:8.8.8.8"));
        assert!(!is_private_ip("::ffff:808:808"));
        assert!(!is_private_ip("::FFFF:0808:0808"));
    }
}
