//! What crosses a relay between the provider's answer and the executor: the upstream status in its
//! own header (so a relay's refusal, such as an expired binding, is never read as the provider's)
//! and an allowlist of response headers: the content type and the ones the failure text and the
//! retry advice read. Never cookies, never request headers echoed back, never the transfer
//! headers (the relay re-frames the bytes it already decoded).

/// The provider's status, carried on the relay's own 200.
pub const STATUS_HEADER: &str = "x-noevia-upstream-status";

/// Response headers that may cross, lower-case. `x-noevia-gate-refusal` and
/// `x-noevia-provider-message` are the two `providerFailureText` reads; `retry-after` is the
/// rate-limit advice.
pub const ALLOWED: &[&str] = &[
    "content-type",
    "retry-after",
    "x-noevia-gate-refusal",
    "x-noevia-provider-message",
];

/// The headers a relay answers with for a provider answer of `status` with `headers`: the status
/// header first, then each allowed header in the provider's order (names lower-cased; repeated
/// names kept as they came).
pub fn forward(status: u16, headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = vec![(STATUS_HEADER.to_string(), status.to_string())];
    for (k, v) in headers {
        let name = k.to_ascii_lowercase();
        if name != STATUS_HEADER && ALLOWED.contains(&name.as_str()) {
            out.push((name, v.clone()));
        }
    }
    out
}

/// The provider's status from a relay answer's headers: `Some` only for exactly one status header
/// holding a status code (100..=599, digits only). `None` means the answer is the relay's own,
/// whatever its status: the executor must not treat it as the provider's.
pub fn upstream_status(headers: &[(String, String)]) -> Option<u16> {
    let mut found = None;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case(STATUS_HEADER) {
            if found.is_some() {
                return None;
            }
            let v = v.trim();
            if v.len() != 3 || !v.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let n: u16 = v.parse().ok()?;
            if !(100..=599).contains(&n) {
                return None;
            }
            found = Some(n);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn allowlist_only() {
        let got = forward(
            429,
            &h(&[
                ("Content-Type", "text/event-stream"),
                ("Set-Cookie", "s=1"),
                ("Content-Encoding", "gzip"),
                ("Content-Length", "10"),
                ("Retry-After", "3"),
                ("Authorization", "Bearer x"),
                ("x-noevia-upstream-status", "200"),
            ]),
        );
        assert_eq!(
            got,
            h(&[
                ("x-noevia-upstream-status", "429"),
                ("content-type", "text/event-stream"),
                ("retry-after", "3"),
            ])
        );
        assert_eq!(upstream_status(&got), Some(429));
    }

    #[test]
    fn relay_own_answers() {
        assert_eq!(upstream_status(&h(&[("content-type", "x")])), None);
        assert_eq!(upstream_status(&h(&[(STATUS_HEADER, "2000")])), None);
        assert_eq!(upstream_status(&h(&[(STATUS_HEADER, "+20")])), None);
        assert_eq!(upstream_status(&h(&[(STATUS_HEADER, "099")])), None);
        assert_eq!(
            upstream_status(&h(&[(STATUS_HEADER, "200"), (STATUS_HEADER, "200")])),
            None
        );
        assert_eq!(
            upstream_status(&h(&[("X-Noevia-Upstream-Status", " 503 ")])),
            Some(503)
        );
    }
}
