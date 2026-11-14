//! `routes/embedding.cjs`'s `apply()` and `GET` for the non-service actions: the order of checks
//! (request reading, the private-address rule, the served-model probes), the projects half (the
//! migration), the Diary half (the sidecar's own endpoint, service token only), the audit line,
//! the embedding-service sync after a change and the status of the two-half answer.
//!
//! `service-switch` and `service-off` are not here yet: [`apply`] answers [`Outcome::Service`] for
//! them and the caller must leave them to core (CONTINUE in the council report).

use serde_json::{json, Map, Value};

use embedding_admin::{aggregate_status, read_request, served_probes, Action};

use crate::js;
use crate::migration::{refusal_half, Host, Migration};
use crate::served::{check_served, refusal_json};
use crate::target::{Refusal, Target};

/// The projects half: the migration's actions (a trait so the route can be tested on its own).
pub trait Projects {
    fn act(
        &mut self,
        action: Action,
        target: Option<&Value>,
        force: bool,
    ) -> Result<Value, Refusal>;
    /// `rag.servedModels({ baseUrl, model })`.
    fn served_models(&self, base_url: Option<&str>, model: &str) -> Option<Vec<Value>>;
    fn status(&self) -> Value;
}

impl<H: Host> Projects for Migration<H> {
    fn act(
        &mut self,
        action: Action,
        target: Option<&Value>,
        force: bool,
    ) -> Result<Value, Refusal> {
        match action {
            Action::SetNext => self.set_next(target),
            Action::ClearNext => self.clear_next(),
            Action::Switch => self.switch_to_next(force),
            Action::Rollback => self.rollback(),
            Action::Finish => self.finish(),
            Action::ServiceSwitch | Action::ServiceOff => {
                Err(Refusal::plain("service actions are not the migration's"))
            }
        }
    }

    fn served_models(&self, base_url: Option<&str>, model: &str) -> Option<Vec<Value>> {
        let t = Target {
            base_url: base_url.map(str::to_owned),
            model: model.to_owned(),
            profile: String::new(),
        };
        self.host.served_models(&t)
    }

    fn status(&self) -> Value {
        Migration::status(self)
    }
}

/// The rest of the server, as the route sees it.
pub trait RouteHost {
    /// `checkEmbeddingAddress(address)`: Err(reason) unless every address it resolves to is private.
    fn check_address(&self, address: &str) -> Result<(), String>;
    /// `diary(method, body)`: the Diary sidecar's answer body, or `{ error, status?, code? }`
    /// (core's mapping of a failed or unreachable sidecar, `notConfigured` without a base).
    fn diary(&self, method: &str, body: Option<&Value>) -> Value;
    /// `authService.audit(event, user, null, detail)`.
    fn audit(&self, event: &str, user: &str, detail: Value);
    /// `syncService()`: the embedding service serves only what is still configured.
    fn sync_service(&self);
    /// `service.refresh()` then, when the service is managed, `service.status()`.
    fn service_status(&self) -> Option<Value>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Reply(u16, Value),
    /// `service-switch` / `service-off`: still core's.
    Service,
}

fn reply(status: u16, body: Value) -> Outcome {
    Outcome::Reply(status, body)
}

/// What a call to the Diary's `/api/internal/embedding` came to.
pub enum DiaryCall {
    /// No Diary base is configured here.
    NotConfigured,
    /// The sidecar answered: `ok` (2xx), the status and the parsed body (Null when none).
    Answered { ok: bool, status: u16, body: Value },
    /// The request failed (the error's message).
    Failed(String),
}

/// `diary(method, body)`'s mapping of the sidecar's answer to the Diary half.
pub fn diary_half(call: DiaryCall) -> Value {
    match call {
        DiaryCall::NotConfigured => {
            json!({ "error": "The Diary sidecar is not configured.", "code": "notConfigured" })
        }
        DiaryCall::Failed(e) => {
            json!({ "error": format!("The Diary could not be reached ({}).", crate::served::slice_units(&e, 120)) })
        }
        DiaryCall::Answered { ok: true, body, .. } => body,
        DiaryCall::Answered { status, body, .. } => {
            let said = [body.get("error"), body.get("detail")]
                .into_iter()
                .find(|v| js::js_truthy(*v))
                .flatten();
            let text = said.map_or_else(
                || format!("The Diary answered HTTP {status}."),
                |v| js::js_string(Some(v)),
            );
            let mut out = Map::new();
            out.insert(
                "error".into(),
                json!(crate::served::slice_units(&text, 300)),
            );
            out.insert("status".into(), json!(status));
            if let Some(code) = body.get("code").filter(|c| js::js_truthy(Some(*c))) {
                out.insert("code".into(), code.clone());
            }
            Value::Object(out)
        }
    }
}

/// `GET /api/embedding`'s body.
pub fn get(projects: &dyn Projects, host: &dyn RouteHost) -> Value {
    let service = host.service_status();
    let mut out = Map::new();
    out.insert("projects".into(), projects.status());
    out.insert("diary".into(), host.diary("GET", None));
    if let Some(s) = service {
        out.insert("service".into(), s);
    }
    Value::Object(out)
}

/// `POST /api/embedding` for an administrator (`user`), after JSON parsing.
pub fn apply(
    body: &Value,
    user: &str,
    projects: &mut dyn Projects,
    host: &dyn RouteHost,
) -> Outcome {
    let request = match read_request(body) {
        Ok(r) => r,
        Err(r) => return reply(r.status, json!({ "error": r.error })),
    };
    if request.action.is_service() {
        return Outcome::Service;
    }
    let target = body.get("target");
    if request.action == Action::SetNext {
        // Both raw addresses, as sent (an empty one is the inference engine).
        for key in ["baseUrl", "diaryBaseUrl"] {
            let raw = target.and_then(|t| t.get(key));
            if matches!(raw, None | Some(Value::Null)) {
                continue;
            }
            let text = js::js_string(raw);
            let address = js::trim(&text);
            if address.is_empty() {
                continue;
            }
            if let Err(reason) = host.check_address(address) {
                return reply(
                    400,
                    json!({
                        "error": format!("The embeddings server must be on this network: it receives the text of every project and the Diary ({reason})."),
                        "code": "publicEndpoint",
                    }),
                );
            }
        }
        let probes = match served_probes(&request) {
            Ok(p) => p,
            Err(r) => {
                return reply(
                    r.status,
                    json!({ "error": r.error, "code": "diaryEndpointRequired" }),
                )
            }
        };
        let model = request
            .target
            .as_ref()
            .map(|t| t.model.clone())
            .unwrap_or_default();
        for base_url in probes {
            let ids = projects.served_models(base_url.as_deref(), &model);
            if let Err(r) = check_served(ids.as_deref(), &model) {
                return reply(409, refusal_json(&r));
            }
        }
    }
    let mut out = Map::new();
    if request.scope.includes_projects() {
        let half = match projects.act(request.action, target, request.force) {
            Ok(v) => v,
            Err(r) => refusal_half(&r),
        };
        out.insert("projects".into(), half);
    }
    if request.scope.includes_diary() {
        let mut send = Map::new();
        send.insert("action".into(), json!(request.action.as_str()));
        if let Some(Value::Object(t)) = target {
            let mut d = Map::new();
            for key in ["model", "profile"] {
                if let Some(v) = t.get(key) {
                    d.insert(key.into(), v.clone());
                }
            }
            let base = match (t.get("diaryBaseUrl"), t.get("baseUrl")) {
                (Some(v), _) if !v.is_null() => v.clone(),
                (_, Some(v)) if !v.is_null() => v.clone(),
                _ => json!(""),
            };
            d.insert("base_url".into(), base);
            send.insert("target".into(), Value::Object(d));
        } else if let Some(Value::Array(_)) = target {
            // typeof [] === 'object': an array target is sent with only the empty address.
            send.insert("target".into(), json!({ "base_url": "" }));
        }
        if request.force {
            send.insert("force".into(), json!(true));
        }
        out.insert(
            "diary".into(),
            host.diary("POST", Some(&Value::Object(send))),
        );
    }
    let verdict = |half: Option<&Value>| match half {
        None => "skipped",
        Some(h) if js::js_truthy(h.get("error")) => "refused",
        Some(_) => "ok",
    };
    let truthy_or_null = |key: &str| {
        let v = target.and_then(|t| t.get(key));
        if js::js_truthy(v) {
            v.cloned().unwrap_or(Value::Null)
        } else {
            Value::Null
        }
    };
    host.audit(
        &format!("embedding.{}", request.action.as_str()),
        user,
        json!({
            "scope": request.scope.as_str(), "model": truthy_or_null("model"), "profile": truthy_or_null("profile"),
            "force": request.force, "projects": verdict(out.get("projects")), "diary": verdict(out.get("diary")),
        }),
    );
    let changes = matches!(
        request.action,
        Action::Finish | Action::ClearNext | Action::Rollback | Action::Switch
    );
    if changes
        && [out.get("projects"), out.get("diary")]
            .into_iter()
            .flatten()
            .any(|h| !js::js_truthy(h.get("error")))
    {
        host.sync_service();
    }
    let status = aggregate_status(out.get("projects"), out.get("diary"));
    reply(status, Value::Object(out))
}
