//! The small pieces of core auth.cjs and device-auth.cjs the routes share, with Node's exact
//! semantics: random tokens, the username rule, the setup address rule, the RP ID rule, client
//! addresses, JS string coercions and the cookies a session sets.

use js_json::JValue;
use rand_core::{OsRng, RngCore};
use server_auth::js;

/// `crypto.randomBytes(n)`.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    OsRng.fill_bytes(&mut b);
    b
}

/// auth.cjs `randomToken(bytes)`: base64url without padding.
pub fn random_token(bytes: usize) -> String {
    passkey::b64::from_buffer(&random_bytes(bytes))
}

/// `crypto.randomBytes(n).toString('hex')`.
pub fn random_hex(bytes: usize) -> String {
    random_bytes(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `crypto.randomUUID()`.
pub fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// auth.cjs `digest(value)`: SHA-256 hex of `String(value)`.
pub fn digest(v: &str) -> String {
    js::digest(v)
}

/// `String(value)`; an object whose `toString` is not callable throws in JS (a 500 in the routes).
pub fn js_string(v: &JValue) -> Result<String, crate::Fault> {
    js_json::to_js_string(v).map_err(|_| crate::Fault::Internal)
}

/// `String(value || fallback)`.
pub fn js_string_or(v: &JValue, fallback: &str) -> Result<String, crate::Fault> {
    if v.truthy() {
        js_string(v)
    } else {
        Ok(fallback.to_string())
    }
}

/// `s.trim()`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(js::is_space)
}

/// auth.cjs `USERNAME_RE`: `/^[A-Za-z0-9._-]{3,32}$/`.
pub fn username_ok(s: &str) -> bool {
    (3..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// `new URL(s).hostname`, or `None` where the constructor throws.
pub fn url_hostname(s: &str) -> Option<String> {
    let u = url::Url::parse(s).ok()?;
    Some(u.host_str().unwrap_or("").to_string())
}

/// auth.cjs `isAcceptablePublicOrigin`.
pub fn acceptable_public_origin(s: &str) -> bool {
    let Ok(u) = url::Url::parse(s) else {
        return false;
    };
    match u.scheme() {
        "https" => return true,
        "http" => {}
        _ => return false,
    }
    let host = u.host_str().unwrap_or("");
    if host == "localhost" || host.ends_with(".localhost") || host == "127.0.0.1" || host == "::1" {
        return true;
    }
    if host.starts_with("10.") || host.starts_with("192.168.") {
        return true;
    }
    if let Some(rest) = host.strip_prefix("172.") {
        // /^172\.(1[6-9]|2\d|3[01])\./
        let b = rest.as_bytes();
        let ok = (b.first() == Some(&b'1') && b.get(1).is_some_and(|x| (b'6'..=b'9').contains(x)))
            || (b.first() == Some(&b'2') && b.get(1).is_some_and(u8::is_ascii_digit))
            || (b.first() == Some(&b'3') && b.get(1).is_some_and(|x| *x == b'0' || *x == b'1'));
        if ok && b.get(2) == Some(&b'.') {
            return true;
        }
    }
    !host.is_empty() && !host.contains('.') && !host.contains(':')
}

/// auth.cjs `rpFor(origin)`: WEBAUTHN_RP_ID while it fits the address's host, else the host
/// (`localhost` without an address).
pub fn rp_for(origin: &str, rp_env: &str) -> Result<String, crate::Fault> {
    let host = if origin.is_empty() {
        "localhost".to_string()
    } else {
        url_hostname(origin).ok_or(crate::Fault::Internal)?
    };
    let fits = !rp_env.is_empty() && (host == rp_env || host.ends_with(&format!(".{rp_env}")));
    Ok(if fits { rp_env.to_string() } else { host })
}

/// auth.cjs `clientAddress(req, trustProxy)` for the front: the socket's address, or with
/// TRUST_PROXY the rightmost X-Forwarded-For entry that is an IP address.
pub fn client_address(peer: std::net::IpAddr, forwarded_for: Option<&str>, trust: bool) -> String {
    let remote = peer.to_string();
    if !trust {
        return remote;
    }
    let joined = forwarded_for.unwrap_or("");
    let last = joined
        .split(',')
        .map(js_trim)
        .rfind(|s| !s.is_empty())
        .unwrap_or("");
    if !last.is_empty() && last.parse::<std::net::IpAddr>().is_ok() {
        return last.to_string();
    }
    remote
}

/// `Buffer.from(s, 'base64url')`: characters outside both base64 alphabets are skipped, `=` ends
/// the data, a lone trailing character is dropped.
pub fn node_base64url_decode(s: &str) -> Vec<u8> {
    let mut vals = Vec::new();
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        vals.push(u32::from(v));
    }
    let mut out = Vec::new();
    for chunk in vals.chunks(4) {
        let get = |i: usize| chunk.get(i).copied().unwrap_or(0);
        let n = (get(0) << 18) | (get(1) << 12) | (get(2) << 6) | get(3);
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        let keep = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => 0,
        };
        out.extend_from_slice(bytes.get(..keep).unwrap_or(&[]));
    }
    out
}

/// `Buffer.from(s, 'hex')`: pairs of hex digits up to the first one that is not.
pub fn node_hex_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let hex = |c: u8| (c as char).to_digit(16);
    let mut i = 0;
    while let (Some(&a), Some(&c)) = (b.get(i), b.get(i + 1)) {
        match (hex(a), hex(c)) {
            (Some(x), Some(y)) => out.push((x * 16 + y) as u8),
            _ => break,
        }
        i += 2;
    }
    out
}

/// `Number(s)` for an environment string (decimal, `0x`/`0o`/`0b`, `Infinity`; blank is 0).
pub fn js_number(s: &str) -> f64 {
    let t = js_trim(s);
    if t.is_empty() {
        return 0.0;
    }
    let radix = |p: &str, r: u32| -> Option<f64> {
        let d = t
            .strip_prefix(p)
            .or_else(|| t.strip_prefix(&p.to_uppercase()))?;
        if d.is_empty() {
            return Some(f64::NAN);
        }
        u128::from_str_radix(d, r)
            .ok()
            .map(|v| v as f64)
            .or(Some(f64::NAN))
    };
    if let Some(v) = radix("0x", 16)
        .or_else(|| radix("0o", 8))
        .or_else(|| radix("0b", 2))
    {
        return v;
    }
    let (sign, body) = match t.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, t.strip_prefix('+').unwrap_or(t)),
    };
    if body == "Infinity" {
        return sign * f64::INFINITY;
    }
    let ok = !body.is_empty()
        && body.bytes().all(|c| {
            c.is_ascii_digit() || c == b'.' || c == b'e' || c == b'E' || c == b'+' || c == b'-'
        });
    if !ok {
        return f64::NAN;
    }
    body.parse::<f64>().map_or(f64::NAN, |v| sign * v)
}

/// auth.cjs ABSOLUTE_MS / 1000, the cookies' Max-Age.
pub const ABSOLUTE_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// The two Set-Cookie values of auth.cjs `issueSession`.
pub fn session_cookies(raw: &str, csrf: &str, secure: bool) -> Vec<String> {
    let s = if secure { "; Secure" } else { "" };
    let age = ABSOLUTE_MS / 1000;
    vec![
        format!("cowork_session={raw}; Path=/; HttpOnly; SameSite=Lax; Max-Age={age}{s}"),
        format!("cowork_csrf={csrf}; Path=/; SameSite=Lax; Max-Age={age}{s}"),
    ]
}

/// auth.cjs `logout`'s clearing cookies.
pub fn cleared_cookies() -> Vec<String> {
    vec![
        "cowork_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0".into(),
        "cowork_csrf=; Path=/; SameSite=Lax; Max-Age=0".into(),
    ]
}

/// `String(s).slice(0, n)` by UTF-16 units.
pub fn slice16(s: &str, n: usize) -> String {
    js_json::js_slice(s, n)
}

/// device-auth.cjs `cleanClientName`: printable, single-line, at most 60 units, trimmed.
pub fn clean_client_name(v: &JValue) -> String {
    let Some(raw) = v.as_str() else {
        return String::new();
    };
    // [\p{C}\p{Zl}\p{Zp}] -> ' ' (u flag: code points).
    let replaced: String = raw
        .chars()
        .map(|c| {
            if crate::unicode::is_other_or_separator(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    // /\s+/g -> ' '
    let mut collapsed = String::with_capacity(replaced.len());
    let mut in_space = false;
    for c in replaced.chars() {
        if js::is_space(c) {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(c);
            in_space = false;
        }
    }
    let t = js_trim(&collapsed);
    js_trim(&slice16(t, 60)).to_string()
}

/// device-auth.cjs USER_CODE_ALPHABET.
pub const USER_CODE_ALPHABET: &str = "BCDFGHJKLMNPQRSTVWXZ";

/// device-auth.cjs `randomUserCode` (rejection sampling over random bytes).
pub fn random_user_code() -> String {
    let alpha = USER_CODE_ALPHABET.as_bytes();
    let mut out = String::new();
    while out.len() < 8 {
        for b in random_bytes(16) {
            if b >= 240 {
                continue;
            }
            if let Some(c) = alpha.get(usize::from(b) % alpha.len()) {
                out.push(char::from(*c));
            }
            if out.len() == 8 {
                break;
            }
        }
    }
    out
}

/// device-auth.cjs `normalizeUserCode`.
pub fn normalize_user_code(v: &JValue) -> Result<String, crate::Fault> {
    let raw = js_string_or(v, "")?;
    let value: String = raw
        .to_uppercase()
        .chars()
        .filter(|c| !js::is_space(*c) && *c != '-')
        .collect();
    if js_json::js_len(&value) != 8 || !value.chars().all(|c| USER_CODE_ALPHABET.contains(c)) {
        return Ok(String::new());
    }
    Ok(value)
}

/// device-auth.cjs `formatUserCode`.
pub fn format_user_code(code: &str) -> String {
    let (a, b) = code.split_at(code.len().min(4));
    format!("{a}-{b}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn origin_rules_like_auth_cjs() {
        for ok in [
            "https://x.example",
            "http://localhost:8021",
            "http://a.localhost",
            "http://127.0.0.1",
            "http://10.1.2.3:8021",
            "http://192.168.0.5",
            "http://172.16.0.1",
            "http://172.31.9.9",
            "http://nas",
            "http://NAS:8021",
        ] {
            assert!(acceptable_public_origin(ok), "{ok}");
        }
        for bad in [
            "http://example.com",
            "http://172.32.0.1",
            "http://172.15.0.1",
            "http://[::1]:8021",
            "ftp://x",
            "not a url",
            "",
        ] {
            assert!(!acceptable_public_origin(bad), "{bad}");
        }
        assert_eq!(
            rp_for("https://app.example.com", "example.com").unwrap(),
            "example.com"
        );
        assert_eq!(
            rp_for("https://other.test", "example.com").unwrap(),
            "other.test"
        );
        assert_eq!(rp_for("", "").unwrap(), "localhost");
        assert_eq!(rp_for("http://127.0.0.1:18021", "").unwrap(), "127.0.0.1");
        assert!(username_ok("a.b_c-9") && !username_ok("ab") && !username_ok("é_é"));
    }

    #[test]
    fn decoders_like_node() {
        assert_eq!(node_base64url_decode("YWI"), b"ab");
        assert_eq!(node_base64url_decode("Y W\nI="), b"ab");
        assert_eq!(node_base64url_decode("-_8"), vec![0xfb, 0xff]);
        assert_eq!(node_base64url_decode("+/8"), vec![0xfb, 0xff]);
        assert_eq!(node_hex_decode("0aFFzz12"), vec![0x0a, 0xff]);
        assert_eq!(js_number(" 8040 "), 8040.0);
        assert_eq!(js_number(""), 0.0);
        assert!(js_number("abc").is_nan());
        assert_eq!(js_number("0x1f"), 31.0);
        assert!(js_number("inf").is_nan());
    }

    #[test]
    fn client_addresses_like_node() {
        let peer: std::net::IpAddr = "10.0.0.2".parse().unwrap();
        assert_eq!(client_address(peer, Some("1.2.3.4"), false), "10.0.0.2");
        assert_eq!(
            client_address(peer, Some("6.6.6.6, 1.2.3.4 "), true),
            "1.2.3.4"
        );
        assert_eq!(
            client_address(peer, Some("1.2.3.4, junk"), true),
            "10.0.0.2"
        );
        assert_eq!(client_address(peer, None, true), "10.0.0.2");
    }

    #[test]
    fn device_codes() {
        let c = random_user_code();
        assert_eq!(c.len(), 8);
        assert_eq!(
            normalize_user_code(&JValue::from(format_user_code(&c).to_lowercase())).unwrap(),
            c
        );
        assert_eq!(normalize_user_code(&JValue::from("ABCD-EFGH")).unwrap(), "");
        assert_eq!(
            clean_client_name(&JValue::from("  My\u{0}\tMac \u{2028} Pro  ")),
            "My Mac Pro"
        );
        assert_eq!(clean_client_name(&JValue::from(5.0)), "");
    }
}
