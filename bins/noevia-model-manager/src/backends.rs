//! noevia#1318: native `POST /api/v1/backends/{name}/restart` and `POST /api/v1/backends/{name}/test`.
//!
//! Python (`api.restart_backend` / `api.backend_test`, `services.restart_llama_backend`,
//! `services.test_prompt`, `_resolve_internal_port`, `_probe_loaded_model`):
//!
//! - restart: an effective backend (LLAMA_CONTAINERS plus discovered llama.cpp containers) is
//!   looked up, its last restart error cleared, and `restart(timeout=15)` queued in the background;
//!   a failure is kept as that backend's `last_restart_error`, which `GET /api/v1/backends` shows.
//!   The native sampler's restart-error store holds it. The child's copy (its private
//!   `PUT /api/v1/internal/backend-restart-error`) is set and cleared only in test builds, so
//!   production restarts leave the child's backend snapshot unchanged.
//! - test: the backend's llama-server port is resolved from `docker inspect` (published port, else
//!   `--port`/`LLAMA_ARG_PORT`, else LLAMA_DEFAULT_PORT), its `/v1/models` probed for a loaded
//!   model, and one non-streaming chat completion sent.
use crate::{BackendSpec, DockerRead};
use std::time::Duration;

/// `settings.llama_default_port`.
pub const DEFAULT_PORT: u16 = 8080;
/// Largest llama-server answer read.
pub const ANSWER_CAP: usize = 8 * 1024 * 1024;
/// The child's private restart-error call.
pub const RESTART_ERROR_PATH: &str = "/api/v1/internal/backend-restart-error";

pub type Clock = std::sync::Arc<dyn Fn() -> f64 + Send + Sync>;

pub struct Backends {
    pub default_port: Option<u16>,
    clock: Clock,
    probe: reqwest::Client,
    chat: reqwest::Client,
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// `int(cont_port.split("/")[0])`: Python's int() of an ASCII-digit string.
fn port_number(text: &str) -> Option<u32> {
    let t = model_autoconfig::pyval::py_strip(text);
    let digits = t.strip_prefix('+').unwrap_or(t);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `_resolve_internal_port(attrs)[1]`. Docker writes the ports map with sorted keys and Python
/// keeps the last key that parses; `ports` is that sorted list.
pub fn internal_port(spec: &BackendSpec, default: Option<u16>) -> Option<u32> {
    let mut internal = None;
    for (key, _) in &spec.ports {
        if let Some(n) = key.split('/').next().and_then(port_number) {
            internal = Some(n);
        }
    }
    if internal.is_some() {
        return internal;
    }
    // `--port[=\s]+(\d+)` over " ".join(Cmd + Entrypoint).
    let joined = spec
        .cmd
        .iter()
        .chain(&spec.entrypoint)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let mut rest = joined.as_str();
    while let Some(at) = rest.find("--port") {
        let after = &rest[at + "--port".len()..];
        let sep = after
            .char_indices()
            .take_while(|(_, c)| *c == '=' || c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}'))
            .count();
        let tail: String = after.chars().skip(sep).collect();
        let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
        if sep > 0 && !digits.is_empty() {
            if let Ok(n) = digits.parse() {
                return Some(n);
            }
        }
        rest = &rest[at + 1..];
    }
    for entry in &spec.env {
        if let Some((name, value)) = entry.split_once('=') {
            let v = model_autoconfig::pyval::py_strip(value);
            if name == "LLAMA_ARG_PORT" && !v.is_empty() {
                if let Some(n) = port_number(v) {
                    return Some(n);
                }
            }
        }
    }
    default.filter(|p| *p != 0).map(u32::from)
}

/// Why a probe found no model: its note, or `Raised` where Python's probe raises (500).
#[derive(Debug, PartialEq, Eq)]
pub enum NoModel {
    Note(String),
    Raised,
}

impl From<String> for NoModel {
    fn from(note: String) -> Self {
        NoModel::Note(note)
    }
}
impl From<&str> for NoModel {
    fn from(note: &str) -> Self {
        NoModel::Note(note.to_owned())
    }
}

/// `_probe_loaded_model`: the loaded model ids (`", "`-joined) or why there is none.
pub async fn probe(client: &reqwest::Client, host: &str, port: u32) -> Result<String, NoModel> {
    let url = format!("http://{host}:{port}/v1/models");
    let response = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => return Err("timeout".into()),
        Err(e) if e.is_connect() => return Err("connection refused".into()),
        Err(_) => return Err("request failed".into()),
    };
    let status = response.status().as_u16();
    if status != 200 {
        return Err(format!("HTTP {status}").into());
    }
    let bytes = read_capped(response).await.map_err(NoModel::from)?;
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Err("invalid response".into());
    };
    let Ok(data) = model_files::json::parse(text) else {
        return Err("invalid response".into());
    };
    use model_files::json::Value as J;
    // `data.get(...)` on a non-dict raises AttributeError, which the probe does not catch.
    if !matches!(data, J::Obj(_)) {
        return Err(NoModel::Raised);
    }
    let items = match data.get("data") {
        Some(J::Arr(a)) => a.clone(),
        Some(v) if !v.truthy() => Vec::new(),
        None => Vec::new(),
        // Iterating a non-list truthy value yields non-dicts whose `.get` raises.
        Some(_) => return Err(NoModel::Raised),
    };
    if items.is_empty() {
        return Err("no models configured".into());
    }
    let mut loaded = Vec::new();
    for it in &items {
        let J::Obj(_) = it else {
            return Err(NoModel::Raised);
        };
        let status_loaded = match it.get("status") {
            Some(J::Obj(_)) => {
                matches!(it.get("status").and_then(|s| s.get("value")), Some(J::Str(v)) if v == "loaded")
            }
            _ => true,
        };
        if status_loaded {
            let id = match it.get("id") {
                Some(J::Str(s)) => s.clone(),
                Some(v) if !v.truthy() => String::new(),
                Some(v) => crate::prompts::py_str(Some(v)).unwrap_or_default(),
                None => String::new(),
            };
            loaded.push(id);
        }
    }
    if !loaded.is_empty() {
        return Ok(loaded
            .into_iter()
            .filter(|i| !i.is_empty())
            .collect::<Vec<_>>()
            .join(", "));
    }
    Err(format!("{} configured, none loaded", items.len()).into())
}

async fn read_capped(response: reqwest::Response) -> Result<Vec<u8>, &'static str> {
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "request failed")?;
        if out.len() + chunk.len() > ANSWER_CAP {
            return Err("request failed");
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// `round(x, digits)` of a float, as JSON (`repr`); non-finite is `null`-free: Python would write NaN.
fn py_round(x: f64, digits: usize) -> String {
    let rounded: f64 = format!("{x:.digits$}").parse().unwrap_or(x);
    gguf::py_float_repr(rounded)
}

/// Python raised inside `test_prompt`: FastAPI's 500.
#[derive(Debug, PartialEq, Eq)]
pub struct Raised;

/// What a test answers: `Ok(json)`, or `Err(Raised)`.
pub type TestAnswer = Result<String, Raised>;

fn fail(err: &str, model: Option<&str>) -> TestAnswer {
    Ok(match model {
        None => format!("{{\"ok\":false,\"err\":{}}}", json_string(err)),
        Some(m) => format!(
            "{{\"ok\":false,\"err\":{},\"model\":{}}}",
            json_string(err),
            json_string(m)
        ),
    })
}

/// `int(usage.get(key) or 0)`: `None` where Python's int() raises.
fn usage_int(v: Option<&model_files::json::Value>) -> Option<i64> {
    use model_files::json::Value as J;
    match v {
        None => Some(0),
        Some(v) if !v.truthy() => Some(0),
        Some(J::Int(n)) => n.to_string().parse().ok(),
        Some(J::Float(f)) if f.is_finite() => Some(f.trunc() as i64),
        Some(J::Bool(true)) => Some(1),
        Some(J::Str(s)) => port_number(s).map(i64::from),
        _ => None,
    }
}

impl Backends {
    pub fn new(default_port: Option<u16>) -> Result<Self, reqwest::Error> {
        Self::with_clock(default_port, std::sync::Arc::new(now))
    }
    pub fn with_clock(default_port: Option<u16>, clock: Clock) -> Result<Self, reqwest::Error> {
        let base = || {
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
        };
        Ok(Self {
            default_port,
            clock,
            probe: base()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(5))
                .build()?,
            chat: base()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(190))
                .build()?,
        })
    }

    /// `services.test_prompt(name, prompt, max_tokens)`.
    pub async fn test(
        &self,
        docker: &dyn DockerRead,
        name: &str,
        prompt: &str,
        max_tokens: i64,
    ) -> TestAnswer {
        let port = match docker.backend_spec(name).await {
            Ok(Some(spec)) => internal_port(&spec, self.default_port),
            _ => None,
        };
        let Some(port) = port else {
            return fail("container not reachable", None);
        };
        let loaded = match probe(&self.probe, name, port).await {
            Ok(l) if !l.is_empty() => l,
            Ok(_) => return fail("no model loaded on this backend", None),
            Err(NoModel::Note(e)) => return fail(&e, None),
            Err(NoModel::Raised) => return Err(Raised),
        };
        let model = loaded
            .split(',')
            .next()
            .map(|s| model_autoconfig::pyval::py_strip(s).to_owned())
            .unwrap_or_default();
        let payload = format!(
            "{{\"model\":{},\"messages\":[{{\"role\":\"user\",\"content\":{}}}],\"max_tokens\":{max_tokens},\"stream\":false}}",
            json_string(&model),
            json_string(prompt)
        );
        let url = format!("http://{name}:{port}/v1/chat/completions");
        let t0 = (self.clock)();
        let response = self
            .chat
            .post(&url)
            .header("content-type", "application/json")
            .body(payload)
            .send()
            .await;
        let response = match response {
            Ok(r) => r,
            Err(e) if e.is_timeout() => return fail("ReadTimeout: timeout", Some(&model)),
            Err(e) if e.is_connect() => {
                return fail("ConnectError: connection failed", Some(&model))
            }
            Err(_) => return fail("HTTPError: request failed", Some(&model)),
        };
        let status = response.status();
        if !status.is_success() {
            // httpx's raise_for_status() text, as Python renders `{type(e).__name__}: {e}`.
            let code = status.as_u16();
            let kind = match code {
                100..=199 => "Informational response",
                300..=399 => "Redirect response",
                400..=499 => "Client error",
                _ => "Server error",
            };
            let reason = status.canonical_reason().unwrap_or("");
            return fail(
                &format!(
                    "HTTPStatusError: {kind} '{code} {reason}' for url '{url}'\nFor more information check: https://developer.mozilla.org/en-US/docs/Web/HTTP/Status/{code}"
                ),
                Some(&model),
            );
        }
        let Ok(bytes) = read_capped(response).await else {
            return fail("HTTPError: request failed", Some(&model));
        };
        // `r.json()` raising ValueError and `.get` on a non-dict are not caught in Python: 500.
        let text = std::str::from_utf8(&bytes).map_err(|_| Raised)?;
        let data = model_files::json::parse(text).map_err(|_| Raised)?;
        use model_files::json::Value as J;
        if !matches!(data, J::Obj(_)) {
            return Err(Raised);
        }
        let elapsed = (self.clock)() - t0;
        let reply = match data.get("choices") {
            Some(J::Arr(choices)) if !choices.is_empty() => {
                let Some(first) = choices.first() else {
                    return Err(Raised);
                };
                let message = match first {
                    J::Obj(_) => first.get("message"),
                    v if !v.truthy() => None,
                    _ => return Err(Raised),
                };
                match message {
                    Some(m @ J::Obj(_)) => match m.get("content") {
                        Some(J::Str(s)) => J::Str(s.clone()),
                        Some(v) if v.truthy() => v.clone(),
                        _ => J::Str(String::new()),
                    },
                    Some(v) if v.truthy() => return Err(Raised),
                    _ => J::Str(String::new()),
                }
            }
            Some(v) if v.truthy() && !matches!(v, J::Arr(_)) => return Err(Raised),
            _ => J::Str(String::new()),
        };
        let usage = match data.get("usage") {
            Some(u @ J::Obj(_)) => Some(u.clone()),
            Some(v) if v.truthy() => return Err(Raised),
            _ => None,
        };
        let get = |k: &str| usage.as_ref().and_then(|u| u.get(k));
        let completion = usage_int(get("completion_tokens")).ok_or(Raised)?;
        let prompt_tokens = usage_int(get("prompt_tokens")).ok_or(Raised)?;
        let tps = if elapsed > 0.0 && completion != 0 {
            #[allow(clippy::cast_precision_loss)]
            let v = completion as f64 / elapsed;
            py_round(v, 1)
        } else {
            "null".to_owned()
        };
        let reply_json = match &reply {
            J::Str(s) => json_string(s),
            other => crate::prompts::py_json(other),
        };
        Ok(format!(
            "{{\"ok\":true,\"model\":{},\"reply\":{reply_json},\"completion_tokens\":{completion},\"prompt_tokens\":{prompt_tokens},\"elapsed_s\":{},\"tokens_per_s\":{tps}}}",
            json_string(&model),
            py_round(elapsed, 2),
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn spec(ports: &[&str], cmd: &[&str], env: &[&str]) -> BackendSpec {
        BackendSpec {
            ports: ports.iter().map(|p| ((*p).to_owned(), vec![])).collect(),
            cmd: cmd.iter().map(|s| (*s).to_owned()).collect(),
            entrypoint: vec![],
            env: env.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn port_resolution_follows_python() {
        assert_eq!(
            internal_port(&spec(&["10000/tcp", "8080/tcp"], &[], &[]), Some(1)),
            Some(8080)
        );
        assert_eq!(
            internal_port(&spec(&["x/tcp"], &["--port", "9001"], &[]), Some(1)),
            Some(9001)
        );
        assert_eq!(
            internal_port(&spec(&[], &["--port=9002"], &[]), Some(1)),
            Some(9002)
        );
        assert_eq!(
            internal_port(&spec(&[], &["--port-x", "1", "--port 7"], &[]), Some(1)),
            Some(7)
        );
        assert_eq!(
            internal_port(&spec(&[], &[], &["LLAMA_ARG_PORT= 9003 "]), Some(1)),
            Some(9003)
        );
        assert_eq!(
            internal_port(&spec(&[], &[], &["LLAMA_ARG_PORT=x"]), Some(8080)),
            Some(8080)
        );
        assert_eq!(internal_port(&spec(&[], &[], &[]), None), None);
    }
}
