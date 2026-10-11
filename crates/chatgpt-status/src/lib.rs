//! Private native ChatGPT status closure. HTTP ownership/integration is not yet wired.
#![forbid(unsafe_code)]
mod json;
mod keys;
mod sqlite;
pub use keys::{Root, StatusKeys};
pub use sqlite::{SqliteService, StatusCancellation, StatusJob};
use zeroize::Zeroizing;

pub const ROW_BYTES: usize = 256 * 1024;
pub const PLAIN_BYTES: usize = 192 * 1024;
pub const USER_BYTES: usize = 4096;
pub const OUTPUT_BYTES: usize = 256 * 1024;

/// Fixed refusals never contain an input or secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Unavailable,
    Limit,
    Cancelled,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatGPT sign-in failed.")
    }
}
impl std::error::Error for Error {}

/// Request CPU cancellation. No task is spawned here: the caller retains its lease until return.
#[derive(Clone)]
pub struct Control {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    deadline: std::time::Instant,
}
impl Default for Control {
    fn default() -> Self {
        Self {
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(2),
        }
    }
}
impl Control {
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release)
    }
    pub fn with_deadline(deadline: std::time::Instant) -> Self {
        Self {
            deadline,
            ..Self::default()
        }
    }
    pub(crate) fn check(&self) -> Result<(), Error> {
        if self.cancelled.load(std::sync::atomic::Ordering::Acquire)
            || std::time::Instant::now() >= self.deadline
        {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}
/// Already-authorized identity is supplied by the owning HTTP adapter in the future.
/// No request can select another user's OAuth row.
pub fn status(keys: &StatusKeys, user: &str, row: Option<(&str, &str)>) -> Result<String, Error> {
    status_with(&Control::default(), keys, user, row)
}
pub fn status_with(
    control: &Control,
    keys: &StatusKeys,
    user: &str,
    row: Option<(&str, &str)>,
) -> Result<String, Error> {
    control.check()?;
    if user.is_empty() || user.len() > USER_BYTES {
        return Err(Error::Limit);
    }
    let Some((state, ciphertext)) = row else {
        return Ok(simple("disconnected"));
    };
    if state.len() > 64 || ciphertext.len() > ROW_BYTES {
        return Err(Error::Limit);
    }
    // Node decrypts before testing state. Preserve malformed-data behavior, but enforce caps.
    let opened = keys.open(ciphertext, user);
    control.check()?;
    let plain = match opened {
        Ok(p) => p,
        Err(Error::Limit) => return Err(Error::Limit),
        Err(_) => return Ok(simple("reconnect")),
    };
    let data = match json::parse(&plain, control) {
        Ok(v) => v,
        Err(json::Failure::Limit) => return Err(Error::Limit),
        Err(json::Failure::Cancelled) => return Err(Error::Cancelled),
        Err(json::Failure::Syntax) => return Ok(simple("reconnect")),
    };
    let json::Value::Object(items) = &data else {
        return Ok(simple("reconnect"));
    };
    if state != "connected" {
        return Ok(simple("reconnect"));
    }
    let lookup = |name: &str| {
        items
            .iter()
            .find(|(k, _)| k.iter().copied().eq(name.encode_utf16()))
            .map(|(_, v)| v)
    };
    let email = match lookup("email") {
        Some(json::Value::String(s)) => mask_email(s),
        _ => None,
    };
    let null = json::Value::Null;
    let plan = lookup("plan").filter(|v| v.truthy()).unwrap_or(&null);
    let mut out = String::from("{\"state\":\"connected\",\"account\":{\"email\":");
    match email {
        Some(s) => json::quote(&s, &mut out, control)?,
        None => out.push_str("null"),
    };
    out.push_str(",\"plan\":");
    json::emit(plan, &mut out, control)?;
    out.push_str("},\"providerId\":\"chatgpt-oauth\",\"external\":true}");
    control.check()?;
    if out.len() > OUTPUT_BYTES {
        return Err(Error::Limit);
    }
    Ok(out)
}
fn simple(state: &str) -> String {
    format!("{{\"state\":\"{state}\",\"providerId\":\"chatgpt-oauth\",\"external\":true}}")
}
fn mask_email(s: &[u16]) -> Option<Vec<u16>> {
    let at = s.iter().rposition(|u| *u == 64)?;
    let mut out = Vec::with_capacity(120);
    if at > 0 {
        if let Some(first) = s.first() {
            out.push(*first)
        }
    }
    out.extend([0x2026, 64]);
    out.extend(s.get(at + 1..)?.iter().copied().take(120 - out.len()));
    Some(out)
}

/// Feature parsing follows actual features.cjs, including blank as unset.
pub fn feature_env(raw: Option<&str>) -> Result<Option<bool>, Error> {
    let Some(raw) = raw else { return Ok(None) };
    if raw.len() > 4096 {
        return Err(Error::Limit);
    }
    let units: Vec<u16> = raw.encode_utf16().collect();
    let trimmed = String::from_utf16_lossy(policy_leaves::js_trim(&units)).to_ascii_lowercase();
    match trimmed.as_str() {
        "" => Ok(None),
        "1" | "true" | "on" | "yes" => Ok(Some(true)),
        "0" | "false" | "off" | "no" => Ok(Some(false)),
        _ => Err(Error::Unavailable),
    }
}
/// Existing keys are a bounded startup snapshot, matching createSecretStore.
/// This service never mutates/reloads keys; coordinated restart is required after replacement.
pub struct Service {
    keys: StatusKeys,
}
impl Service {
    pub fn new(root: &Root, previous_env: Option<&str>) -> Result<Self, Error> {
        Ok(Self {
            keys: StatusKeys::load(root, previous_env)?,
        })
    }
    pub fn read(
        &self,
        product: bool,
        user: &str,
        read: impl FnOnce() -> Result<Option<(Zeroizing<String>, String)>, Error>,
    ) -> Result<(u16, String), Error> {
        self.read_with(&Control::default(), product, user, read)
    }
    pub fn read_with(
        &self,
        control: &Control,
        product: bool,
        user: &str,
        read: impl FnOnce() -> Result<Option<(Zeroizing<String>, String)>, Error>,
    ) -> Result<(u16, String), Error> {
        control.check()?;
        if !product {
            return Ok((
                404,
                "{\"error\":\"Sign in with ChatGPT is turned off on this server.\"}".into(),
            ));
        }
        let result = read();
        control.check()?;
        let row = result?;
        Ok((
            200,
            status_with(
                control,
                &self.keys,
                user,
                row.as_ref()
                    .map(|(enc, state)| (state.as_str(), enc.as_str())),
            )?,
        ))
    }
}
