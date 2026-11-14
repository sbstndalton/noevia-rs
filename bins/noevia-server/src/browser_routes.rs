//! Browser mode's task actions in the front (Rust port track R5, shift 2), behind
//! `NOEVIA_RUST_BROWSER=1`, which is off by default: unset, these requests go to Node exactly as
//! before.
//!
//! `POST /api/projects/{id}/browser` (start, 202) and `POST .../browser/{task}/act|approve|finish|cancel`,
//! answer for answer with core routes/browser.cjs, over crates/browser-task (browser-service.cjs,
//! browser-executor.cjs, the policy host of browser-policy.cjs), server-projects' task store and the
//! front's own egress issuer (code_egress.rs, the proxy Code and Browser tasks share). The browser
//! engine is `NOEVIA_BROWSER_ENGINE`'s; without one Browser mode is unavailable, as it is in Node
//! without Playwright. Core, told the same switch (rust-browser-executor.cjs), refuses these
//! requests, recovers no browser journal, answers the task facts bridge with the flag only and says
//! Browser mode is available exactly when `NOEVIA_BROWSER_ENGINE` is set.
//!
//! The order is core's: the flag (404), the administrator (403), the project (404), the method (405),
//! the body (400/413), then the service's answers. A core that still runs Browser tasks itself (the
//! facts bridge does not say `owner: 'front'`) gets 503, never a second writer of the journals.
//! The reads (`GET .../browser[/{task}]`) stay task_routes.rs's.
use crate::code_routes::FrontEgress;
use crate::task_routes::{self, Facts};
use crate::{identity::Rejection, reply, App};
use axum::{
    body::Body,
    http::{Response, StatusCode},
    response::IntoResponse,
};
use browser_task::routes::{self as browser, Body as ReadBody, Caller};
use browser_task::service::{Egress, Options, Service};
use browser_task::Fail;
use js_json::{wtf16, JValue};
use std::path::PathBuf;
use std::sync::Arc;

/// The switch: `1` hands Browser tasks to the front.
pub const SWITCH: &str = "NOEVIA_RUST_BROWSER";
/// The browser engine the front drives; unset, Browser mode is unavailable.
pub const ENGINE: &str = "NOEVIA_BROWSER_ENGINE";
const UNAVAILABLE: &str = "Browser tasks are unavailable right now. Try again shortly.";
/// core `readJson`'s limit.
const BODY_LIMIT: usize = 1024 * 1024;

/// The deployment's Browser configuration.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub executor: bool,
    pub engine: Option<String>,
}

impl Settings {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Settings {
        Settings {
            executor: get(SWITCH).as_deref() == Some("1"),
            engine: get(ENGINE).map(|e| e.trim().to_string()).filter(|e| !e.is_empty()),
        }
    }
}

/// Whether this request is a Browser task action the front answers when it owns them: everything
/// core's pattern matches, less the reads (task_routes.rs).
pub fn owns(method: &str, path: &str) -> bool {
    browser::parse(path).is_some_and(|t| !(method == "GET" && t.action.is_none()))
}

/// The egress controller as browser-service.cjs uses it: the same client Code tasks have.
impl Egress for FrontEgress {
    fn endpoint(&self) -> Option<String> {
        code_harness::executor::Egress::endpoint(self)
    }
    fn available(&self) -> bool {
        code_harness::executor::Egress::available(self)
    }
    fn grant(&self, task_id: &str, domains: &[String]) -> Result<JValue, Fail> {
        code_harness::executor::Egress::grant(self, task_id, domains).map_err(|e| Fail {
            status: e.status,
            message: e.message,
            public: e.public_message,
            result: None,
        })
    }
    fn revoke(&self, grant: &JValue) -> Result<JValue, Fail> {
        code_harness::executor::Egress::revoke(self, grant).map_err(|e| Fail::plain(e.status, &e.message))
    }
    fn forget(&self, grant: &JValue, snapshot_id: &str) -> Option<Result<JValue, Fail>> {
        Some(code_harness::executor::Egress::forget(self, grant, snapshot_id).map_err(|e| Fail::plain(e.status, &e.message)))
    }
}

/// No engine yet: every launch is refused (Browser mode is unavailable without one anyway).
struct NoEngine;

impl browser_task::engine::Launch for NoEngine {
    fn launch(&self) -> Result<Arc<dyn browser_task::engine::Browser>, Fail> {
        Err(Fail::public(503, "Browser mode needs a browser engine on this server."))
    }
}

fn log_browser(entry: &JValue) {
    println!("[browser] {}", wtf16::stringify(entry).unwrap_or_default());
}

pub fn service(app: &Arc<App>) -> Arc<Service> {
    app.browser_service
        .get_or_init(|| {
            let egress = crate::code_routes::front_egress(app).map(|e| e as Arc<dyn Egress>);
            Service::new(Options::new(Arc::new(NoEngine), egress, Arc::new(|e| log_browser(&e))))
        })
        .clone()
}

fn failed() -> Response<Body> {
    reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Browser request failed", true)
}

/// The body as core's `readJson` reads it, parsed only when the route reads it.
fn parsed(read: &Result<server_account::Body, ()>) -> ReadBody {
    match read {
        Err(()) => ReadBody::Invalid,
        Ok(r) if r.over => ReadBody::Failed(Fail::public(413, "Request exceeds size limit")),
        Ok(r) if r.bytes.is_empty() => ReadBody::Json(JValue::Obj(Vec::new())),
        Ok(r) => match wtf16::parse(&String::from_utf8_lossy(&r.bytes)) {
            Ok(v) => ReadBody::Json(v),
            Err(_) => ReadBody::Invalid,
        },
    }
}

/// One Browser action, after the session gate.
pub async fn serve(
    app: &Arc<App>,
    authn: server_auth::Identity,
    data_dir: PathBuf,
    method: String,
    path: String,
    body: Body,
) -> Response<Body> {
    let admin = authn.user.role == server_auth::Role::Admin;
    let user = authn.user.id.as_str().to_owned();
    let enabled = match task_routes::browser_flag(app, &user, admin).await {
        None => return task_routes::unreachable(),
        Some(Facts::Off) => false,
        Some(Facts::Member | Facts::Front) => true,
        // Browser mode is on but core still runs Browser tasks itself: the front must not open the
        // tenant's journals beside it.
        Some(Facts::Admin(_)) => {
            return reply::error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE, true)
        }
    };
    let Some(target) = browser::parse(&path) else {
        return failed();
    };
    // The project, looked up before anything blocks (core's `getProject` after the flag and the role).
    let project = if enabled && admin {
        let Some(id) = server_projects::js::decode_uri_component(&target.project) else {
            return failed();
        };
        let (data_dir, user, switch) = (data_dir.clone(), user.clone(), app.config.rust_projects);
        let found = tokio::task::spawn_blocking(move || {
            task_routes::project_record(&data_dir, &user, switch, &id).map(|r| r.map(|_| id))
        })
        .await;
        match found {
            Ok(Ok(found)) => found,
            Ok(Err(())) | Err(_) => return failed(),
        }
    } else {
        None
    };
    let read = crate::account_routes::read_body(body, BODY_LIMIT).await;
    let Ok(dir) = server_projects::store::user_dir(&data_dir, &user) else {
        return failed();
    };
    let service = service(app);
    let work = tokio::task::spawn_blocking(move || {
        let caller = Caller { admin };
        let lookup = |_: &str| project.clone();
        let mut body = || parsed(&read);
        let req = browser::Request { method: &method, path: &path, caller: Some(&caller), enabled };
        browser::handle(&service, &req, &lookup, &dir, &mut body)
    })
    .await;
    match work {
        Ok(Some((status, body))) => reply::json_text(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            wtf16::stringify(&body).unwrap_or_else(|| "null".into()),
            true,
        ),
        Ok(None) => failed(),
        Err(_) => Rejection::Unavailable.into_response(),
    }
}

/// The task reads' facts when the front owns Browser tasks (task_routes.rs).
pub async fn facts(app: &Arc<App>, data_dir: &std::path::Path, user: &str) -> Option<JValue> {
    let dir = server_projects::store::user_dir(data_dir, user).ok()?;
    let service = service(app);
    tokio::task::spawn_blocking(move || service.facts(&dir).ok())
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "0123abcd-0123-4abc-8def-0123456789ab";

    #[test]
    fn the_actions_are_cores_pattern_less_the_reads() {
        assert!(owns("POST", "/api/projects/qa/browser"));
        assert!(owns("POST", &format!("/api/projects/qa/browser/{T}/act")));
        assert!(owns("POST", &format!("/api/projects/qa/browser/{T}/cancel")));
        assert!(owns("PUT", "/api/projects/qa/browser"));
        assert!(owns("GET", &format!("/api/projects/qa/browser/{T}/act")), "core answers it 405");
        assert!(!owns("GET", "/api/projects/qa/browser"));
        assert!(!owns("GET", &format!("/api/projects/qa/browser/{T}")));
        assert!(!owns("POST", "/api/projects/qa/browserx"));
        assert!(!owns("POST", "/api/projects/qa/code"));
    }

    #[test]
    fn the_switch_is_exactly_one() {
        let s = |v: Option<&str>, e: Option<&str>| {
            Settings::from_lookup(&|k: &str| match k {
                SWITCH => v.map(str::to_string),
                ENGINE => e.map(str::to_string),
                _ => None,
            })
        };
        assert!(!s(None, None).executor);
        assert!(!s(Some("rust"), None).executor);
        assert!(!s(Some(" 1"), None).executor);
        assert!(s(Some("1"), None).executor);
        assert_eq!(s(Some("1"), Some("  ")).engine, None);
        assert_eq!(s(Some("1"), Some(" ws://qa-chromium.invalid:9222 ")).engine.as_deref(), Some("ws://qa-chromium.invalid:9222"));
    }
}
