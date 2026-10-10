//! What the gate reads from a request, as Node's `req.headers` presents it, and core auth.cjs
//! `parseCookies` / device-auth.cjs `bearerToken` / `hasSessionCookie`.

use crate::js;
use std::collections::BTreeMap;

/// The credential-bearing parts of one request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Creds {
    /// `req.headers.cookie`: every Cookie header, joined with `"; "`.
    pub cookie: Option<String>,
    /// `req.headers.authorization`: the first Authorization header (Node drops duplicates).
    pub authorization: Option<String>,
    /// `req.headers.origin`: every Origin header, joined with `", "`.
    pub origin: Option<String>,
    /// `req.headers['x-csrf-token']`: joined with `", "`.
    pub csrf: Option<String>,
    /// The method, upper case as it arrived (Node compares it case-sensitively).
    pub method: String,
}

/// Header bytes as Node decodes them: latin1, one character per byte.
pub fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|b| char::from(*b)).collect()
}

impl Creds {
    /// From the raw header list (`name` compared ASCII case-insensitively), with Node's
    /// `IncomingMessage` duplicate rules.
    pub fn from_headers<'a>(
        method: &str,
        headers: impl IntoIterator<Item = (&'a str, &'a [u8])>,
    ) -> Self {
        let mut c = Creds {
            method: method.to_string(),
            ..Creds::default()
        };
        let join = |slot: &mut Option<String>, value: String, sep: &str| match slot {
            Some(prev) => {
                prev.push_str(sep);
                prev.push_str(&value);
            }
            None => *slot = Some(value),
        };
        for (name, value) in headers {
            let value = latin1(value);
            if name.eq_ignore_ascii_case("cookie") {
                join(&mut c.cookie, value, "; ");
            } else if name.eq_ignore_ascii_case("authorization") {
                if c.authorization.is_none() {
                    c.authorization = Some(value);
                }
            } else if name.eq_ignore_ascii_case("origin") {
                join(&mut c.origin, value, ", ");
            } else if name.eq_ignore_ascii_case("x-csrf-token") {
                join(&mut c.csrf, value, ", ");
            }
        }
        c
    }

    /// `parseCookies(req)[name]`.
    pub fn cookie(&self, name: &str) -> Option<String> {
        cookie_value(self.cookie.as_deref().unwrap_or(""), name)
    }
}

/// core auth.cjs `parseCookies`: split on `;`, the first `=` separates; the name is trimmed, the
/// value is not; `decodeURIComponent`, keeping the raw value where it throws; later names win.
/// `__proto__` is never an own key of the JS object (assigning a string to it is a no-op).
pub fn parse_cookies(header: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for part in header.split(';') {
        let Some(i) = part.find('=') else { continue };
        if i == 0 {
            continue;
        }
        let (Some(name), Some(raw)) = (part.get(..i), part.get(i + 1..)) else {
            continue;
        };
        let name = name.trim_matches(js::is_space);
        if name == "__proto__" {
            continue;
        }
        let value = js::decode_uri_component(raw).unwrap_or_else(|| raw.to_string());
        out.insert(name.to_string(), value);
    }
    out
}

/// One cookie of [`parse_cookies`] without building the whole map.
pub fn cookie_value(header: &str, wanted: &str) -> Option<String> {
    let mut found = None;
    for part in header.split(';') {
        let Some(i) = part.find('=') else { continue };
        if i == 0 {
            continue;
        }
        let (Some(name), Some(raw)) = (part.get(..i), part.get(i + 1..)) else {
            continue;
        };
        if name.trim_matches(js::is_space) == wanted {
            found = Some(raw);
        }
    }
    found.map(|raw| js::decode_uri_component(raw).unwrap_or_else(|| raw.to_string()))
}

/// device-auth.cjs `hasSessionCookie`: any `;` part whose text before the first `=` (or all of
/// it), trimmed, is `cowork_session` or `cowork_csrf`.
pub fn has_session_cookie(creds: &Creds) -> bool {
    creds
        .cookie
        .as_deref()
        .unwrap_or("")
        .split(';')
        .any(|part| {
            let name = part
                .split('=')
                .next()
                .unwrap_or("")
                .trim_matches(js::is_space);
            name == "cowork_session" || name == "cowork_csrf"
        })
}

/// device-auth.cjs `ACCESS_PREFIX`.
pub const ACCESS_PREFIX: &str = "nva_";

/// device-auth.cjs `bearerToken`: `/^Bearer\s+(\S+)\s*$/i`, and only when the token starts with
/// `nva_`; else `""`.
pub fn bearer_token(creds: &Creds) -> &str {
    let header = creds.authorization.as_deref().unwrap_or("");
    let Some(rest) = js::strip_prefix_ci(header, "Bearer") else {
        return "";
    };
    let after = js::trim_start(rest);
    if after.len() == rest.len() {
        return ""; // \s+ needs at least one
    }
    let end = after.find(js::is_space).unwrap_or(after.len());
    let (token, tail) = after.split_at(end);
    if token.is_empty() || !tail.chars().all(js::is_space) || !token.starts_with(ACCESS_PREFIX) {
        return "";
    }
    token
}

/// auth.cjs's legacy bearer: `String(req.headers.authorization || '').replace(/^Bearer\s+/i, '')`.
pub fn legacy_supplied(creds: &Creds) -> &str {
    let header = creds.authorization.as_deref().unwrap_or("");
    match js::strip_prefix_ci(header, "Bearer") {
        Some(rest) => {
            let after = js::trim_start(rest);
            if after.len() == rest.len() {
                header
            } else {
                after
            }
        }
        None => header,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn auth(v: &str) -> Creds {
        Creds {
            authorization: Some(v.to_string()),
            ..Creds::default()
        }
    }

    #[test]
    fn bearer_shapes() {
        assert_eq!(bearer_token(&auth("Bearer nva_x")), "nva_x");
        assert_eq!(bearer_token(&auth("bearer \t nva_x \u{a0}")), "nva_x");
        assert_eq!(bearer_token(&auth("Bearer nva_x y")), "");
        assert_eq!(bearer_token(&auth("Bearernva_x")), "");
        assert_eq!(bearer_token(&auth("Bearer nvr_x")), "");
        assert_eq!(bearer_token(&auth(" Bearer nva_x")), "");
        assert_eq!(legacy_supplied(&auth("Bearer  tok")), "tok");
        assert_eq!(legacy_supplied(&auth("Bearertok")), "Bearertok");
        assert_eq!(legacy_supplied(&auth("tok")), "tok");
        assert_eq!(legacy_supplied(&auth("Bearer Bearer tok")), "Bearer tok");
    }

    #[test]
    fn node_header_joining() {
        let h: Vec<(&str, &[u8])> = vec![
            ("Cookie", b"a=1"),
            ("authorization", b"Bearer first"),
            ("cookie", b"b=2"),
            ("Authorization", b"Bearer second"),
            ("origin", b"https://a"),
            ("Origin", b"https://b"),
            ("x-csrf-token", b"t\xe9"),
        ];
        let c = Creds::from_headers("POST", h);
        assert_eq!(c.cookie.as_deref(), Some("a=1; b=2"));
        assert_eq!(c.authorization.as_deref(), Some("Bearer first"));
        assert_eq!(c.origin.as_deref(), Some("https://a, https://b"));
        assert_eq!(c.csrf.as_deref(), Some("t\u{e9}"));
    }

    #[test]
    fn cookie_value_agrees_with_parse() {
        for h in [
            "a=1; a=2",
            " a =%41",
            "a",
            "=a; a=b=c",
            "__proto__=1; a=%",
            "a=%C3%A9;a",
        ] {
            let all = parse_cookies(h);
            assert_eq!(cookie_value(h, "a"), all.get("a").cloned(), "{h}");
        }
    }
}
