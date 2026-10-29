//! The Diary sidecar's environment, read as core `index.cjs` and `auth-tokens.cjs` read it.

use std::sync::Arc;

/// core `index.cjs`: `process.env.DIARY_BASE_URL || 'http://cowork-diary-companion:8010'`.
pub const DEFAULT_BASE: &str = "http://cowork-diary-companion:8010";

/// What the front needs to reach the sidecar and to speak for a tenant.
#[derive(Clone)]
pub struct DiaryConfig {
    /// `DIARY_BASE_URL`; calls are `<base>/api/...` by string concatenation, as core builds them.
    /// It is parsed per call: a value that is not a URL fails every call the way `fetch` did.
    pub base: String,
    /// `DIARY_AUTH_TOKEN`, trimmed as JS `trim` (auth-tokens.cjs, decided by `policy-leaves`).
    /// Empty: no `Authorization` header.
    token: String,
    /// `DIARY_TENANT_KEY`, trimmed. Empty: no tenant assertion, and a remote storage secret rides
    /// on every call (an older sidecar needs it).
    tenant_key: String,
    /// `SECRETS_KEY_PREVIOUS`, for opening a stored storage credential after a key rotation.
    previous_key_env: Option<String>,
    /// `MEMBER_OUTBOUND_ORIGINS`, read at call time (core `createEndpointApproved`).
    member_origins: Arc<dyn Fn() -> String + Send + Sync>,
}

impl std::fmt::Debug for DiaryConfig {
    // Never prints the token or the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiaryConfig")
            .field("base", &self.base)
            .field("token_set", &!self.token.is_empty())
            .field("tenant_key_set", &!self.tenant_key.is_empty())
            .finish_non_exhaustive()
    }
}

fn trimmed(raw: &str) -> String {
    let units: Vec<u16> = raw.encode_utf16().collect();
    String::from_utf16_lossy(policy_leaves::js_trim(&units))
}

impl DiaryConfig {
    /// Explicit values (tests, and callers that already resolved them). `member_origins` is asked
    /// on every call.
    pub fn new(
        base: impl Into<String>,
        token: &str,
        tenant_key: &str,
        member_origins: impl Fn() -> String + Send + Sync + 'static,
    ) -> Self {
        Self {
            base: base.into(),
            token: trimmed(token),
            tenant_key: trimmed(tenant_key),
            previous_key_env: None,
            member_origins: Arc::new(member_origins),
        }
    }

    /// From the environment. `DIARY_SOURCE` other than `sidecar` is a startup error, as it is in
    /// core `diary.cjs` (any other value would be a source that throws on every read).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let source = get("DIARY_SOURCE")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "sidecar".to_owned());
        if source != "sidecar" {
            return Err(format!(
                "Unsupported DIARY_SOURCE '{source}': only 'sidecar' is supported (unset DIARY_SOURCE to use it)"
            ));
        }
        let diary_units: Vec<u16> = get("DIARY_AUTH_TOKEN")
            .unwrap_or_default()
            .encode_utf16()
            .collect();
        // auth-tokens.cjs: both tokens trimmed, no fallback between them; the warnings are the
        // process's to print (core prints them), so they are not repeated here.
        let tokens = policy_leaves::auth_tokens(&diary_units, &[], None);
        Ok(Self {
            base: get("DIARY_BASE_URL")
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_BASE.to_owned()),
            token: String::from_utf16_lossy(&tokens.diary_token),
            tenant_key: trimmed(&get("DIARY_TENANT_KEY").unwrap_or_default()),
            previous_key_env: get("SECRETS_KEY_PREVIOUS"),
            member_origins: Arc::new(|| {
                std::env::var("MEMBER_OUTBOUND_ORIGINS").unwrap_or_default()
            }),
        })
    }

    /// Replaces the `SECRETS_KEY_PREVIOUS` value (tests).
    #[must_use]
    pub fn with_previous_key_env(mut self, value: Option<String>) -> Self {
        self.previous_key_env = value;
        self
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }
    pub(crate) fn tenant_key(&self) -> &str {
        &self.tenant_key
    }
    pub(crate) fn previous_key_env(&self) -> Option<&str> {
        self.previous_key_env.as_deref()
    }
    pub(crate) fn member_origins(&self) -> String {
        (self.member_origins)()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<DiaryConfig, String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        DiaryConfig::from_lookup(|k| m.get(k).cloned())
    }

    #[test]
    fn defaults_are_cores() {
        let c = cfg(&[]).unwrap();
        assert_eq!(c.base, "http://cowork-diary-companion:8010");
        assert_eq!(c.token(), "");
        assert_eq!(c.tenant_key(), "");
        // An empty value is the default, as `||` makes it.
        let c = cfg(&[("DIARY_BASE_URL", ""), ("DIARY_SOURCE", "")]).unwrap();
        assert_eq!(c.base, DEFAULT_BASE);
    }

    #[test]
    fn token_and_key_are_trimmed_as_js_trim() {
        let c = cfg(&[
            ("DIARY_AUTH_TOKEN", " \u{a0}tok\n"),
            ("DIARY_TENANT_KEY", "\tkey \u{feff}"),
            ("DIARY_BASE_URL", "http://127.0.0.1:1"),
        ])
        .unwrap();
        assert_eq!(c.token(), "tok");
        assert_eq!(c.tenant_key(), "key");
        assert_eq!(c.base, "http://127.0.0.1:1");
    }

    #[test]
    fn an_unsupported_source_stops_startup_and_names_the_value() {
        let e = cfg(&[("DIARY_SOURCE", "local")]).unwrap_err();
        assert_eq!(
            e,
            "Unsupported DIARY_SOURCE 'local': only 'sidecar' is supported (unset DIARY_SOURCE to use it)"
        );
        assert!(cfg(&[("DIARY_SOURCE", "sidecar")]).is_ok());
    }

    #[test]
    fn debug_never_prints_the_token_or_the_key() {
        let c = cfg(&[
            ("DIARY_AUTH_TOKEN", "synthetic-bearer-token"),
            ("DIARY_TENANT_KEY", "synthetic-tenant-key"),
        ])
        .unwrap();
        let shown = format!("{c:?}");
        assert!(!shown.contains("synthetic"));
        assert!(shown.contains("token_set: true") && shown.contains("tenant_key_set: true"));
    }
}
