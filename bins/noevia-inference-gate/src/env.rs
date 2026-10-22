//! The gate's configuration from `GATE_*` environment variables. Every value is bounded; an
//! invalid one is an error naming the variable (never its value, which may be a secret).

use crate::GateConfig;
use inference_admission::Config;
use std::net::SocketAddr;
use std::time::Duration;

const MIB: u64 = 1024 * 1024;

/// Reads the configuration through `get` (the process environment in production).
pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<GateConfig, String> {
    let read = |key: &str| {
        get(key)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    let listen: SocketAddr = read("GATE_LISTEN")
        .unwrap_or_else(|| "0.0.0.0:8080".into())
        .parse()
        .map_err(|_| "GATE_LISTEN must be an address:port".to_owned())?;
    let upstream =
        read("GATE_UPSTREAM").ok_or("GATE_UPSTREAM is required (the router's base URL)")?;
    let upstream = crate::upstream_base(&upstream)
        .ok_or("GATE_UPSTREAM must be http://host:port with no path, query or credentials")?;
    let admin_token = match read("GATE_ADMIN_TOKEN") {
        None => None,
        Some(token) => {
            if token.len() < 16
                || token.len() > 256
                || !token.bytes().all(|b| (0x21..=0x7e).contains(&b))
            {
                return Err("GATE_ADMIN_TOKEN must be 16 to 256 printable characters".into());
            }
            Some(token)
        }
    };
    if let Some(slots) = read("GATE_CHAT_SLOTS") {
        if slots != "1" {
            return Err("GATE_CHAT_SLOTS: only one chat slot is supported".into());
        }
    }
    let seconds = |key: &str, default: u64, max: u64| -> Result<u64, String> {
        match read(key) {
            None => Ok(default),
            Some(v) => match v.parse::<u64>() {
                Ok(n) if n >= 1 && n <= max => Ok(n),
                _ => Err(format!("{key} must be a whole number from 1 to {max}")),
            },
        }
    };
    let count = |key: &str, default: u64, max: u64| seconds(key, default, max);
    let exempt = read("GATE_EXEMPT_MODELS")
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let grace = match read("GATE_RESIDENCY_GRACE_SECONDS") {
        None => 20,
        Some(v) => v
            .parse::<u64>()
            .ok()
            .filter(|n| *n <= 3600)
            .ok_or("GATE_RESIDENCY_GRACE_SECONDS must be a whole number from 0 to 3600")?,
    };
    let admission = Config {
        exempt,
        max_waiters: usize::try_from(count("GATE_MAX_WAITERS", 64, 10_000)?).unwrap_or(64),
        max_leases: usize::try_from(count("GATE_MAX_LEASES", 64, 10_000)?).unwrap_or(64),
        max_wait_ms: seconds("GATE_MAX_WAIT_SECONDS", 600, 3600)? * 1000,
        fair_hold_ms: seconds("GATE_FAIR_HOLD_SECONDS", 60, 3600)? * 1000,
        residency_grace_ms: grace * 1000,
        lease_idle_ms: seconds("GATE_LEASE_IDLE_SECONDS", 30, 3600)? * 1000,
        lease_hard_ms: seconds("GATE_LEASE_HARD_SECONDS", 1800, 86_400)? * 1000,
        unload_timeout_ms: seconds("GATE_UNLOAD_TIMEOUT_SECONDS", 45, 3600)? * 1000,
        router_down_grace_ms: seconds("GATE_ROUTER_DOWN_GRACE_SECONDS", 30, 3600)? * 1000,
        session_hold_ms: seconds("GATE_SESSION_HOLD_SECONDS", 120, 3600)? * 1000,
    };
    admission
        .validate()
        .map_err(|e| format!("gate limits: {e}"))?;
    let max_body_bytes = count("GATE_MAX_BODY_BYTES", 64 * MIB, 1024 * MIB)?;
    let max_buffered_bytes = count("GATE_MAX_BUFFERED_BYTES", 256 * MIB, 8 * 1024 * MIB)?;
    if max_buffered_bytes < max_body_bytes {
        return Err("GATE_MAX_BUFFERED_BYTES must be at least GATE_MAX_BODY_BYTES".into());
    }
    Ok(GateConfig {
        listen,
        upstream,
        admin_token,
        admission,
        max_body_bytes: usize::try_from(max_body_bytes)
            .map_err(|_| "GATE_MAX_BODY_BYTES is too large")?,
        max_buffered_bytes: usize::try_from(max_buffered_bytes)
            .map_err(|_| "GATE_MAX_BUFFERED_BYTES is too large")?,
        body_timeout: Duration::from_secs(seconds("GATE_BODY_TIMEOUT_SECONDS", 60, 3600)?),
        header_timeout: Duration::from_secs(seconds(
            "GATE_UPSTREAM_HEADER_TIMEOUT_SECONDS",
            900,
            3600,
        )?),
        header_read_timeout: Duration::from_secs(seconds(
            "GATE_HEADER_READ_TIMEOUT_SECONDS",
            30,
            3600,
        )?),
        max_connections: usize::try_from(count("GATE_MAX_CONNECTIONS", 512, 100_000)?)
            .unwrap_or(512),
        ..GateConfig::defaults(listen, String::new())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn defaults_and_required_upstream() {
        assert!(from_env(env(&[])).is_err());
        let c = from_env(env(&[("GATE_UPSTREAM", "http://router:8080")])).unwrap();
        assert_eq!(c.upstream, "http://router:8080");
        assert_eq!(c.admission.max_wait_ms, 600_000);
        assert_eq!(c.admission.fair_hold_ms, 60_000);
        assert_eq!(c.max_body_bytes, 64 * 1024 * 1024);
        assert!(c.admin_token.is_none());
        assert_eq!(c.admission.session_hold_ms, 120_000);
        assert_eq!(c.max_connections, 512);
        assert_eq!(c.header_read_timeout, Duration::from_secs(30));
    }

    #[test]
    fn refuses_bad_values_without_echoing_them() {
        for pairs in [
            vec![("GATE_UPSTREAM", "https://router:8080")],
            vec![("GATE_UPSTREAM", "http://user:pw@router:8080")],
            vec![("GATE_UPSTREAM", "http://router:8080/v1")],
            vec![
                ("GATE_UPSTREAM", "http://router:8080"),
                ("GATE_CHAT_SLOTS", "2"),
            ],
            vec![
                ("GATE_UPSTREAM", "http://router:8080"),
                ("GATE_MAX_WAIT_SECONDS", "0"),
            ],
            vec![
                ("GATE_UPSTREAM", "http://router:8080"),
                ("GATE_MAX_WAIT_SECONDS", "-5"),
            ],
            vec![
                ("GATE_UPSTREAM", "http://router:8080"),
                ("GATE_MAX_WAITERS", "999999"),
            ],
            vec![
                ("GATE_UPSTREAM", "http://router:8080"),
                ("GATE_ADMIN_TOKEN", "short"),
            ],
            vec![
                ("GATE_UPSTREAM", "http://router:8080"),
                ("GATE_MAX_BODY_BYTES", "1000"),
                ("GATE_MAX_BUFFERED_BYTES", "10"),
            ],
            vec![
                ("GATE_UPSTREAM", "http://router:8080"),
                ("GATE_LEASE_IDLE_SECONDS", "100"),
                ("GATE_LEASE_HARD_SECONDS", "10"),
            ],
            vec![
                ("GATE_UPSTREAM", "http://router:8080"),
                ("GATE_MAX_CONNECTIONS", "0"),
            ],
            vec![
                ("GATE_UPSTREAM", "http://router:8080"),
                ("GATE_SESSION_HOLD_SECONDS", "0"),
            ],
        ] {
            let err = from_env(env(&pairs)).unwrap_err();
            assert!(!err.contains("pw"), "{err}");
        }
        let secret = "s3cret-token-with-length";
        let ok = from_env(env(&[
            ("GATE_UPSTREAM", "http://router:8080"),
            ("GATE_ADMIN_TOKEN", secret),
            ("GATE_EXEMPT_MODELS", " embed, rerank ,"),
        ]))
        .unwrap();
        assert_eq!(ok.admission.exempt.len(), 2);
        assert!(!format!("{:?}", ok.admin_token.as_ref().map(|_| "set")).contains(secret));
    }
}
