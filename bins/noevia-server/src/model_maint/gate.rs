//! The inference gate's maintenance hold, from the Rust front's side (core
//! `inference-gate-client.cjs` `holdMaintenance` / `renewMaintenance` / `releaseMaintenance` and
//! `inference-maintenance.cjs`, sbstndalton/noevia#2187). The gate (`bins/noevia-inference-gate`)
//! is the one maintenance authority: a hold taken here is seen by every client of the router, Node
//! included, and Node's chat, which goes through the gate, waits for its end.
//!
//! A [`Hold`] always has an absolute deadline. While it lasts a task renews the gate's hold every
//! third of its idle limit (the gate ends a hold nobody renews), takes it again if the gate lost
//! it, and gives it back at the deadline. A hold dropped without [`Hold::release`] (a panic, a
//! cancelled task) gives its token back from `Drop`; if even that cannot run, the gate ends the
//! hold by itself after its idle limit. A dropped HTTP client never reaches this: the work runs on
//! its own spawned task, which owns the hold.
//!
//! Without an admin token, or when nothing at the engine address knows the hold (404), a hold is
//! this process's alone ([`Maint::acquire`](super::Maint::acquire) says so); callers that cannot
//! run without the gate's exclusion ask for it to be required.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ADMIN_HEADER: &str = "x-noevia-gate-admin";
const CLIENT_HEADER: &str = "x-noevia-client";
/// `RELEASE_TIMEOUT_MS`.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// A gate found without the hold API is asked again after this.
const REPROBE: Duration = Duration::from_secs(5 * 60);
/// How long a renewal that finds the hold gone waits for the engine when it takes the hold again.
const RETAKE_WAIT: Duration = Duration::from_secs(10);
const MAX_WAIT: Duration = Duration::from_secs(600);

/// A hold the gate granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub token: String,
    pub idle_ttl: Duration,
}

/// Why the gate gave no hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateFault {
    /// The gate answered with a refusal of its own (`gate_error`): HTTP status, code, text.
    Refused {
        status: u16,
        code: String,
        message: String,
    },
    /// No (usable) answer: `engine_unavailable`, 502.
    Unavailable,
}

impl GateFault {
    /// 409 `engine_busy` and 503 `maintenance_held`: work is running or another client holds it.
    pub fn busy(&self) -> bool {
        matches!(self, GateFault::Refused { status, code, .. }
            if *status == 409 || code == "engine_busy" || code == "maintenance_held")
    }
    pub fn status(&self) -> u16 {
        match self {
            GateFault::Refused { status, .. } => *status,
            GateFault::Unavailable => 502,
        }
    }
    pub fn message(&self) -> String {
        match self {
            GateFault::Refused { message, .. } => message.clone(),
            GateFault::Unavailable => {
                "The local model engine is not answering. Try again shortly.".into()
            }
        }
    }
}

fn is_token(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

pub struct Gate {
    client: reqwest::Client,
    base: String,
    token: Option<String>,
    absent_at: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate")
            .field("base", &self.base)
            .field("admin", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

impl Gate {
    pub fn new(base: &str, admin_token: Option<&str>) -> Result<Gate, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| format!("inference gate client: {e}"))?;
        Ok(Gate {
            client,
            base: base.to_owned(),
            token: admin_token
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned),
            absent_at: Mutex::new(None),
        })
    }

    /// Whether this process can ask for holds at all (an admin token is set).
    pub fn has_admin(&self) -> bool {
        self.token.is_some()
    }

    fn absent(&self) -> bool {
        self.absent_at
            .lock()
            .map(|a| a.is_some_and(|t| t.elapsed() < REPROBE))
            .unwrap_or(false)
    }

    fn mark_absent(&self, absent: bool) {
        if let Ok(mut a) = self.absent_at.lock() {
            *a = absent.then(Instant::now);
        }
    }

    /// A JSON answer of the gate's own endpoints: (status, body when JSON).
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<(u16, Option<Value>), GateFault> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            HeaderName::from_static(CLIENT_HEADER),
            HeaderValue::from_static("front"),
        );
        if let Some(t) = self
            .token
            .as_deref()
            .and_then(|t| HeaderValue::from_str(t).ok())
        {
            let mut t = t;
            t.set_sensitive(true);
            headers.insert(HeaderName::from_static(ADMIN_HEADER), t);
        }
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.base))
            .headers(headers)
            .timeout(timeout);
        if let Some(b) = body {
            req = req.body(b.to_string());
        }
        let res = req.send().await.map_err(|_| GateFault::Unavailable)?;
        let status = res.status().as_u16();
        // The gate's answers are small; a larger one is not the gate's.
        let bytes = res.bytes().await.map_err(|_| GateFault::Unavailable)?;
        if bytes.len() > 16 * 1024 {
            return Err(GateFault::Unavailable);
        }
        Ok((status, serde_json::from_slice(&bytes).ok()))
    }

    /// `refusalOf(status, body)`: the gate's own refusal, when the body is one.
    fn refusal(status: u16, body: &Option<Value>) -> Option<GateFault> {
        let e = body.as_ref()?.get("error")?;
        if e.get("type")?.as_str()? != "gate_error" {
            return None;
        }
        let code = e.get("code")?.as_str()?.to_owned();
        let message = e
            .get("message")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .map(|m| clip(m, 300))
            .unwrap_or_else(|| "The inference gate refused the request.".into());
        Some(GateFault::Refused {
            status,
            code,
            message,
        })
    }

    /// Takes the gate's maintenance hold, waiting up to `wait` for the engine to be idle.
    /// `Ok(None)`: no admin token, or nothing at the engine address knows the hold.
    pub async fn hold(&self, reason: &str, wait: Duration) -> Result<Option<Grant>, GateFault> {
        if self.token.is_none() || self.absent() {
            return Ok(None);
        }
        let wait = wait.min(MAX_WAIT);
        let wait_ms = u64::try_from(wait.as_millis()).unwrap_or(0);
        let (status, body) = self
            .call(
                reqwest::Method::POST,
                "/gate/v1/maintenance",
                Some(serde_json::json!({ "reason": clip(reason, 300), "wait_ms": wait_ms })),
                wait + Duration::from_secs(10),
            )
            .await?;
        if status != 404 {
            self.mark_absent(false);
        }
        if status == 201 {
            if let Some(token) = body
                .as_ref()
                .and_then(|b| b.get("hold"))
                .and_then(Value::as_str)
                .filter(|t| is_token(t))
            {
                let ttl = body
                    .as_ref()
                    .and_then(|b| b.get("idle_ttl_ms"))
                    .and_then(Value::as_u64)
                    .filter(|v| *v > 0)
                    .unwrap_or(60_000);
                return Ok(Some(Grant {
                    token: token.to_owned(),
                    idle_ttl: Duration::from_millis(ttl),
                }));
            }
        }
        let refusal = Self::refusal(status, &body);
        if status == 404
            && refusal
                .as_ref()
                .is_none_or(|r| matches!(r, GateFault::Refused { code, .. } if code == "not_found"))
        {
            self.mark_absent(true);
            return Ok(None);
        }
        Err(refusal.unwrap_or(GateFault::Unavailable))
    }

    /// Keeps the hold from expiring. `Some(true)`: renewed; `Some(false)`: it has ended; `None`: no answer.
    pub async fn renew(&self, token: &str, reason: Option<&str>) -> Option<bool> {
        if !is_token(token) {
            return Some(false);
        }
        let body = reason
            .map(|r| serde_json::json!({ "reason": clip(r, 300) }))
            .unwrap_or_else(|| serde_json::json!({}));
        match self
            .call(
                reqwest::Method::POST,
                &format!("/gate/v1/maintenance/{token}/renew"),
                Some(body),
                CALL_TIMEOUT,
            )
            .await
        {
            Ok((200, _)) => Some(true),
            Ok((404, _)) => Some(false),
            _ => None,
        }
    }

    /// Gives the hold back; best effort (the gate ends an unrenewed hold by itself).
    pub async fn release(&self, token: &str) {
        if is_token(token) {
            let _ = self
                .call(
                    reqwest::Method::DELETE,
                    &format!("/gate/v1/maintenance/{token}"),
                    None,
                    CALL_TIMEOUT,
                )
                .await;
        }
    }

    /// Whether any client holds the gate's maintenance hold now (`GET /gate/v1/status`).
    pub async fn held(&self) -> Option<bool> {
        match self
            .call(reqwest::Method::GET, "/gate/v1/status", None, CALL_TIMEOUT)
            .await
        {
            Ok((200, Some(b))) => b
                .get("maintenance")
                .and_then(|m| m.get("held"))
                .and_then(Value::as_bool),
            _ => None,
        }
    }
}

struct Shared {
    token: Mutex<Option<String>>,
    released: AtomicBool,
}

/// A maintenance hold. Always bounded by a deadline; `release().await` ends it, `Drop` ends it too.
pub struct Hold {
    shared: Arc<Shared>,
    gate: Option<Arc<Gate>>,
    renewer: Option<tokio::task::JoinHandle<()>>,
    local: Arc<AtomicBool>,
    reason: String,
}

impl std::fmt::Debug for Hold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hold")
            .field("gate", &self.token().is_some())
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

impl Hold {
    /// A hold that is this process's alone: `local` is the flag it clears when it ends.
    pub(super) fn local_only(local: Arc<AtomicBool>, reason: &str) -> Hold {
        Hold {
            shared: Arc::new(Shared {
                token: Mutex::new(None),
                released: AtomicBool::new(false),
            }),
            gate: None,
            renewer: None,
            local,
            reason: reason.to_owned(),
        }
    }

    /// A hold on the gate's grant, renewed until `deadline`.
    pub(super) fn gated(
        local: Arc<AtomicBool>,
        gate: Arc<Gate>,
        grant: Grant,
        reason: &str,
        deadline: Instant,
    ) -> Hold {
        let shared = Arc::new(Shared {
            token: Mutex::new(Some(grant.token)),
            released: AtomicBool::new(false),
        });
        let every = (grant.idle_ttl / 3).max(Duration::from_secs(1));
        let renewer = tokio::spawn(renew_loop(
            Arc::clone(&shared),
            Arc::clone(&gate),
            every,
            deadline,
            reason.to_owned(),
        ));
        Hold {
            shared,
            gate: Some(gate),
            renewer: Some(renewer),
            local,
            reason: reason.to_owned(),
        }
    }

    /// The gate's hold token, while there is one: the header this process's requests to the
    /// engine carry (`X-Noevia-Maintenance`).
    pub fn token(&self) -> Option<String> {
        self.shared.token.lock().ok().and_then(|t| t.clone())
    }

    /// Whether the gate (not just this process) holds the engine.
    pub fn gated_now(&self) -> bool {
        self.token().is_some()
    }

    /// Updates the reason the gate shows people.
    pub async fn set_reason(&self, reason: &str) {
        if let (Some(gate), Some(token)) = (self.gate.as_ref(), self.token()) {
            let _ = gate.renew(&token, Some(reason)).await;
        }
    }

    /// Ends the hold: the gate's token is given back, the local flag cleared.
    pub async fn release(mut self) {
        self.finish();
        let token = self.shared.token.lock().ok().and_then(|mut t| t.take());
        if let (Some(gate), Some(token)) = (self.gate.as_ref(), token) {
            gate.release(&token).await;
        }
    }

    fn finish(&mut self) {
        self.shared.released.store(true, Ordering::Release);
        if let Some(r) = self.renewer.take() {
            r.abort();
        }
        self.local.store(false, Ordering::Release);
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.finish();
        let token = self.shared.token.lock().ok().and_then(|mut t| t.take());
        if let (Some(gate), Some(token)) = (self.gate.take(), token) {
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move { gate.release(&token).await });
            }
        }
    }
}

async fn renew_loop(
    shared: Arc<Shared>,
    gate: Arc<Gate>,
    every: Duration,
    deadline: Instant,
    reason: String,
) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        tokio::time::sleep(every.min(deadline - now)).await;
        if shared.released.load(Ordering::Acquire) {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        let Some(token) = shared.token.lock().ok().and_then(|t| t.clone()) else {
            return;
        };
        if gate.renew(&token, None).await == Some(false) {
            // The gate has forgotten the hold: take it again, briefly waiting for the engine.
            match gate.hold(&reason, RETAKE_WAIT).await {
                Ok(Some(g)) => {
                    if shared.released.load(Ordering::Acquire) {
                        gate.release(&g.token).await;
                        return;
                    }
                    if let Ok(mut t) = shared.token.lock() {
                        *t = Some(g.token);
                    }
                    eprintln!("[inference-gate] the gate had lost this process's maintenance hold; it was taken again");
                }
                _ => {
                    if let Ok(mut t) = shared.token.lock() {
                        *t = None;
                    }
                    eprintln!("[inference-gate] the gate lost this process's maintenance hold and it could not be taken again; this work goes on without it, so other clients may use the engine until it ends");
                    return;
                }
            }
        }
    }
    // The deadline: the work is overdue. The hold is given back so it cannot outlive its work.
    let token = shared.token.lock().ok().and_then(|mut t| t.take());
    if let Some(token) = token {
        eprintln!("[inference-gate] a maintenance hold passed its deadline and was given back");
        gate.release(&token).await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_32_lowercase_hex() {
        assert!(is_token("0123456789abcdef0123456789abcdef"));
        assert!(!is_token("0123456789ABCDEF0123456789abcdef"));
        assert!(!is_token("short"));
    }

    #[test]
    fn a_gate_refusal_is_read_from_its_error_shape() {
        let body = Some(
            serde_json::json!({"error": {"type": "gate_error", "code": "engine_busy", "message": "x", "retryable": true}}),
        );
        let r = Gate::refusal(409, &body).unwrap();
        assert!(r.busy());
        assert_eq!(r.status(), 409);
        assert!(Gate::refusal(409, &Some(serde_json::json!({"error": "plain"}))).is_none());
        let held = Gate::refusal(
            503,
            &Some(serde_json::json!({"error": {"type": "gate_error", "code": "maintenance_held"}})),
        )
        .unwrap();
        assert!(held.busy());
        assert_eq!(held.message(), "The inference gate refused the request.");
        let admin = Gate::refusal(
            403,
            &Some(serde_json::json!({"error": {"type": "gate_error", "code": "admin_required", "message": "m"}})),
        )
        .unwrap();
        assert!(!admin.busy());
    }
}
