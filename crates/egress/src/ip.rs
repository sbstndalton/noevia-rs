//! Address classification, ported string-for-string from noevia's `ssrf.cjs` (`isPrivateIp`)
//! and Node's `net.isIP`.
//!
//! The JS classifier works on the address *text*, not on a parsed address, so this port does
//! too: the differential corpus (`tests/fixtures/egress-diff.json`) holds the JS verdict for
//! every boundary and a seeded fuzz set, and the Rust answer must agree on all of them.
//!
//! The rule, in short: IPv4 is private when it is in 0/8, 10/8, 127/8, 100.64/10, 169.254/16,
//! 172.16/12, 192.168/16, 192.0/16, 198.18/15 or 224/3 (multicast + reserved + broadcast).
//! IPv6 is public only inside 2000::/3, after `::`, `::1`, `fe80*`, `fc*`, `fd*`, IPv4-mapped
//! (`::ffff:a.b.c.d`, judged as its IPv4 self) and IPv4-compatible (`::a.b.c.d`, likewise) are
//! taken out. Anything that is not an IP literal at all is private (fail closed).

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
        4 => is_private_ipv4(ip),
        6 => is_private_ipv6(ip),
        _ => true,
    }
}

/// JS `Number(part)` for the strings `isPrivateIPv4` can actually receive. Only a strict
/// dotted quad ever reaches it with four parts (a `%zone` always lands a `%` in some part,
/// and a hex tail has no dots), so plain decimal and the empty string (`Number('') === 0`)
/// are the only numeric shapes; everything else is NaN, which the JS treats as private.
fn js_number(part: &str) -> Option<u64> {
    if part.is_empty() {
        return Some(0);
    }
    if !part.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // Saturate: any value past 255 is out of range anyway.
    Some(part.parse::<u64>().unwrap_or(u64::MAX))
}

fn is_private_ipv4(ip: &str) -> bool {
    let parts: Vec<Option<u64>> = ip.split('.').map(js_number).collect();
    let [Some(a), Some(b), Some(c), Some(d)] = parts.as_slice() else {
        return true; // unparseable → treat as private
    };
    if [*a, *b, *c, *d].iter().any(|&n| n > 255) {
        return true;
    }
    let (a, b) = (*a, *b);
    a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0)
        || (a == 198 && (b == 18 || b == 19))
        || a >= 224
}

/// `^::([0-9]{1,3}\.){3}[0-9]{1,3}$`
fn is_v4_compat_text(lower: &str) -> bool {
    let Some(rest) = lower.strip_prefix("::") else {
        return false;
    };
    let mut count = 0usize;
    for p in rest.split('.') {
        count += 1;
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    count == 4
}

fn is_private_ipv6(ip: &str) -> bool {
    // Valid IPv6 text (zone included) is ASCII, so ASCII lowering is JS `toLowerCase` here.
    let lower = ip.to_ascii_lowercase();
    if lower == "::" || lower == "::1" {
        return true;
    }
    if lower.starts_with("fe80") || lower.starts_with("fc") || lower.starts_with("fd") {
        return true; // link-local + ULA
    }
    if let Some(v4) = lower.strip_prefix("::ffff:") {
        return is_private_ipv4(v4); // IPv4-mapped
    }
    if is_v4_compat_text(&lower) {
        return lower.get(2..).is_none_or(is_private_ipv4); // IPv4-compatible
    }
    // parseInt(lower.split(':')[0] || '0', 16): leading hex digits of the first group.
    let first_group = lower.split(':').next().unwrap_or("");
    let first_group = if first_group.is_empty() {
        "0"
    } else {
        first_group
    };
    let hex: String = first_group
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect();
    if let Ok(first) = u64::from_str_radix(&hex, 16) {
        if first & 0xe000 == 0x2000 {
            return false; // global unicast 2000::/3
        }
    }
    true // everything else is reserved/special-purpose
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
}
