//! Host and target rules from `code-egress.cjs` (`hostAllowed`, `parseTarget`, `ALLOWED_PORTS`).

/// Only the web ports. Anything else (SSH, a database, a mail relay) is refused.
pub const ALLOWED_PORTS: [u16; 2] = [80, 443];

/// A proxy target: lower-cased host (brackets removed for IPv6) and port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

/// Exact host or a subdomain of a granted domain; a lookalike suffix (`notexample.com`) is not.
///
/// As in the JS: the host loses one trailing dot, a domain loses one leading and one trailing
/// dot, both are lower-cased (full Unicode lowering, like `String.prototype.toLowerCase`), and
/// empty domains never match.
pub fn host_allowed<S: AsRef<str>>(host: &str, domains: &[S]) -> bool {
    let lowered = host.to_lowercase();
    let name = lowered.strip_suffix('.').unwrap_or(&lowered);
    domains.iter().any(|d| {
        let lowered = d.as_ref().to_lowercase();
        let domain = lowered.strip_prefix('.').unwrap_or(&lowered);
        let domain = domain.strip_suffix('.').unwrap_or(domain);
        !domain.is_empty()
            && (name == domain
                || name
                    .strip_suffix(domain)
                    .is_some_and(|rest| rest.ends_with('.')))
    })
}

/// `host`, `host:port`, `[v6]` or `[v6]:port`; the port defaults to `default_port` and must be
/// 1-65535. Mirrors the two JS regexes `^\[([^\]]+)\](?::(\d+))?$` and
/// `^([^:/?#]+)(?::(\d+))?$` exactly, including `Number()` of a zero-padded port.
pub fn parse_target(raw: &str, default_port: u16) -> Option<Target> {
    let (host, port_text) = if let Some(rest) = raw.strip_prefix('[') {
        let (inner, after) = rest.split_once(']')?;
        if inner.is_empty() {
            return None;
        }
        (inner, optional_port(after)?)
    } else {
        let (host, after) = match raw.find(':') {
            Some(i) => (raw.get(..i)?, raw.get(i..)?),
            None => (raw, ""),
        };
        if host.is_empty() || host.contains(['/', '?', '#']) {
            return None;
        }
        (host, optional_port(after)?)
    };
    let port = match port_text {
        Some(digits) => port_number(digits)?,
        None => default_port,
    };
    if port == 0 {
        return None;
    }
    Some(Target {
        host: host.to_lowercase(),
        port,
    })
}

/// `(?::(\d+))?$` applied to what follows the host: `Some(None)` for nothing, `Some(Some(d))`
/// for `:digits`, `None` when it does not match.
fn optional_port(after: &str) -> Option<Option<&str>> {
    if after.is_empty() {
        return Some(None);
    }
    let digits = after.strip_prefix(':')?;
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(Some(digits))
}

/// `Number(digits)` restricted to 1..=65535; leading zeros are allowed, as in JS.
fn port_number(digits: &str) -> Option<u16> {
    let trimmed = digits.trim_start_matches('0');
    if trimmed.len() > 5 {
        return None;
    }
    if trimmed.is_empty() {
        return None; // 0
    }
    trimmed
        .parse::<u32>()
        .ok()
        .and_then(|n| u16::try_from(n).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookalikes_do_not_match() {
        assert!(host_allowed("api.example.com", &["example.com"]));
        assert!(host_allowed("EXAMPLE.com.", &[".example.com."]));
        assert!(!host_allowed("notexample.com", &["example.com"]));
        assert!(!host_allowed("example.com.evil", &["example.com"]));
        assert!(!host_allowed("example.com", &[".", ""]));
    }

    #[test]
    fn targets() {
        assert_eq!(
            parse_target("[::1]:443", 80),
            Some(Target {
                host: "::1".into(),
                port: 443
            })
        );
        assert_eq!(parse_target("x:0", 80), None);
        assert_eq!(parse_target("x:65536", 80), None);
        assert_eq!(parse_target("x:0080", 443).map(|t| t.port), Some(80));
        assert_eq!(parse_target("a/b", 80), None);
    }
}
