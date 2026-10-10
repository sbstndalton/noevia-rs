//! The environment the Node gate reads, with core index.cjs's names and coercions.

/// What core index.cjs passes to createAuth/createRequestAuth, read the same way.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthConfig {
    /// `PUBLIC_ORIGIN || ''`.
    pub public_origin: String,
    /// `ADDITIONAL_TRUSTED_ORIGINS` split on `,`, trimmed, empties dropped.
    pub additional_origins: Vec<String>,
    /// auth-tokens.cjs: `UI_AUTH_TOKEN` trimmed (JS `trim`).
    pub legacy_token: String,
    /// auth-tokens.cjs: `LEGACY_AUTH_COMPAT === 'true'`.
    pub legacy_compat: bool,
    /// `TRUST_PROXY === 'true'` (nativeClientAuth is unavailable without it).
    pub trust_proxy: bool,
    /// features.cjs parseEnv(NOEVIA_FEATURE_NATIVE_CLIENT_AUTH): `None` when unset or blank.
    pub native_client_auth_env: Option<bool>,
}

impl std::fmt::Debug for AuthConfig {
    // Never prints the legacy token.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("public_origin", &self.public_origin)
            .field("additional_origins", &self.additional_origins)
            .field("legacy_token_set", &!self.legacy_token.is_empty())
            .field("legacy_compat", &self.legacy_compat)
            .field("trust_proxy", &self.trust_proxy)
            .field("native_client_auth_env", &self.native_client_auth_env)
            .finish()
    }
}

impl Drop for AuthConfig {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.legacy_token);
    }
}

/// features.cjs `parseEnv`: blank/unset is `None`; 1/true/on/yes and 0/false/off/no (trimmed, any
/// case); anything else is an error (Node refuses to start).
pub fn parse_feature_env(raw: Option<&str>) -> Result<Option<bool>, String> {
    let Some(raw) = raw else { return Ok(None) };
    let units: Vec<u16> = raw.encode_utf16().collect();
    let trimmed = String::from_utf16_lossy(policy_leaves::js_trim(&units));
    if trimmed.is_empty() {
        return Ok(None);
    }
    match trimmed.to_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Ok(Some(true)),
        "0" | "false" | "off" | "no" => Ok(Some(false)),
        _ => Err("Invalid boolean in feature env var: expected true/false".into()),
    }
}

impl AuthConfig {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let units = |k: &str| {
            get(k)
                .unwrap_or_default()
                .encode_utf16()
                .collect::<Vec<u16>>()
        };
        let compat = get("LEGACY_AUTH_COMPAT").map(|v| v.encode_utf16().collect::<Vec<u16>>());
        // The diary token is not this crate's: an empty one is passed and its warning ignored.
        let tokens = policy_leaves::auth_tokens(&[], &units("UI_AUTH_TOKEN"), compat.as_deref());
        Ok(AuthConfig {
            public_origin: get("PUBLIC_ORIGIN").unwrap_or_default(),
            additional_origins: get("ADDITIONAL_TRUSTED_ORIGINS")
                .unwrap_or_default()
                .split(',')
                .map(|s| {
                    let u: Vec<u16> = s.encode_utf16().collect();
                    String::from_utf16_lossy(policy_leaves::js_trim(&u))
                })
                .filter(|s| !s.is_empty())
                .collect(),
            legacy_token: String::from_utf16_lossy(&tokens.ui_auth_token),
            legacy_compat: tokens.legacy_compat,
            trust_proxy: get("TRUST_PROXY").as_deref() == Some("true"),
            native_client_auth_env: parse_feature_env(
                get("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH").as_deref(),
            )?,
        })
    }

    /// features.cjs `enabled('nativeClientAuth')`: unavailable without TRUST_PROXY; else the env
    /// value, else the stored admin choice (`settings['feature:nativeClientAuth']`, exactly
    /// `'true'`/`'false'`), else off.
    pub fn native_client_auth(&self, stored: Option<&str>) -> bool {
        if !self.trust_proxy {
            return false;
        }
        match self.native_client_auth_env {
            Some(v) => v,
            None => stored == Some("true"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<AuthConfig, String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        AuthConfig::from_lookup(|k| m.get(k).cloned())
    }

    #[test]
    fn reads_node_names() {
        let c = cfg(&[
            ("PUBLIC_ORIGIN", "https://a.test"),
            (
                "ADDITIONAL_TRUSTED_ORIGINS",
                " http://10.0.0.2:8021 ,, http://lan ",
            ),
            ("UI_AUTH_TOKEN", "\u{a0} tok \n"),
            ("LEGACY_AUTH_COMPAT", "true"),
            ("TRUST_PROXY", "true"),
        ])
        .unwrap();
        assert_eq!(
            c.additional_origins,
            vec!["http://10.0.0.2:8021", "http://lan"]
        );
        assert_eq!(c.legacy_token, "tok");
        assert!(c.legacy_compat && c.trust_proxy);
        assert!(!format!("{c:?}").contains("tok\""));
        let c = cfg(&[("LEGACY_AUTH_COMPAT", "TRUE"), ("TRUST_PROXY", "1")]).unwrap();
        assert!(!c.legacy_compat && !c.trust_proxy);
        assert!(cfg(&[("NOEVIA_FEATURE_NATIVE_CLIENT_AUTH", "maybe")]).is_err());
    }
}
