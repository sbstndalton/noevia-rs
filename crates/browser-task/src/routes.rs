//! core routes/browser.cjs (`createBrowserRoutes`): which request is a browser action, and the
//! answer, in core's order: the flag (404), an administrator (403), the project (404), the method
//! (405), the body (400), then the service's answers with core's error mapping.
//!
//! ```text
//! POST /api/projects/:id/browser                 { domains }              -> 202 { taskId, domains }
//! POST /api/projects/:id/browser/:task/act       { type, ... }            -> action result
//! POST /api/projects/:id/browser/:task/approve   { decision, approvalId } -> { ok }
//! POST /api/projects/:id/browser/:task/finish    { result }               -> { ok }
//! POST /api/projects/:id/browser/:task/cancel                             -> task
//! ```

use crate::service::Service;
use crate::Fail;
use js_json::JValue;
use std::path::Path;
use std::sync::Arc;

pub const NOT_ENABLED: &str = "Browser mode is not enabled on this server.";

/// A request `PATTERN` matches: the project id (still percent-encoded), the task and the action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub project: String,
    pub task: Option<String>,
    pub action: Option<String>,
}

/// `PATTERN` = `^/api/projects/([^/]+)/browser(?:/([0-9a-f-]{36}))?(?:/(act|approve|finish|cancel))?$`.
pub fn parse(path: &str) -> Option<Target> {
    let rest = path.strip_prefix("/api/projects/")?;
    let (project, rest) = rest.split_once('/')?;
    if project.is_empty() {
        return None;
    }
    let mut rest = rest.strip_prefix("browser")?;
    let mut task = None;
    if let Some(after) = rest.strip_prefix('/') {
        let candidate = after.get(..36).filter(|c| c.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-'));
        if let Some(c) = candidate {
            let tail = after.get(36..).unwrap_or("");
            if tail.is_empty() || tail.starts_with('/') {
                task = Some(c.to_string());
                rest = tail;
            }
        }
    }
    let action = match rest {
        "" => None,
        "/act" => Some("act"),
        "/approve" => Some("approve"),
        "/finish" => Some("finish"),
        "/cancel" => Some("cancel"),
        _ => return None,
    };
    Some(Target { project: project.to_string(), task, action: action.map(str::to_string) })
}

/// What reading the body gave (core `readJson`).
pub enum Body {
    Json(JValue),
    /// `JSON.parse` threw (a SyntaxError): 400 `invalid JSON`.
    Invalid,
    /// Reading failed otherwise (too large, cut off): core's error mapping.
    Failed(Fail),
}

/// The caller as the route sees it: `None` signed out.
pub struct Caller {
    pub admin: bool,
}

fn error(status: u16, text: &str) -> (u16, JValue) {
    (status, JValue::obj([("error", JValue::Str(text.to_string()))]))
}

fn mapped(f: &Fail) -> (u16, JValue) {
    let (status, text) = f.answer();
    error(status, &text)
}

/// One request. `None`: not a browser route (core's handler answers `false`). `enabled` is
/// `features.enabled('browserExecutor')`; `project` resolves the decoded id to the project's id;
/// `body` is read only when core reads it.
pub fn handle(
    service: &Arc<Service>,
    method: &str,
    path: &str,
    caller: Option<&Caller>,
    enabled: bool,
    project: &dyn Fn(&str) -> Option<String>,
    dir: &Path,
    body: &mut dyn FnMut() -> Body,
) -> Option<(u16, JValue)> {
    let target = parse(path)?;
    if !enabled {
        return Some(error(404, NOT_ENABLED));
    }
    if !caller.is_some_and(|c| c.admin) {
        return Some(error(403, "Administrator required"));
    }
    // A project id core's decodeURIComponent would throw on: the front's task reads answer it so.
    let Some(decoded) = server_projects::js::decode_uri_component(&target.project) else {
        return Some(error(500, "Browser request failed"));
    };
    let Some(project_id) = project(&decoded) else {
        return Some(error(404, "project not found"));
    };
    let read = |body: &mut dyn FnMut() -> Body| -> Result<JValue, (u16, JValue)> {
        match body() {
            Body::Json(v) => Ok(v),
            Body::Invalid => Err(error(400, "invalid JSON")),
            Body::Failed(f) => Err(mapped(&f)),
        }
    };
    let answer = (|| -> Result<(u16, JValue), (u16, JValue)> {
        let Some(task) = target.task.as_deref() else {
            if method != "POST" {
                return Ok(error(405, "method not allowed"));
            }
            let b = read(body)?;
            return service.start(dir, &project_id, &b).map(|v| (202, v)).map_err(|f| mapped(&f));
        };
        let Some(action) = target.action.as_deref().filter(|_| method == "POST") else {
            return Ok(error(405, "method not allowed"));
        };
        if action == "cancel" {
            return service.cancel(dir, &project_id, task).map(|v| (200, v)).map_err(|f| mapped(&f));
        }
        let b = read(body)?;
        let outcome = match action {
            "act" => service.act(dir, &project_id, task, &b),
            "finish" => service.finish(dir, &project_id, task, b.get("result")),
            _ => {
                let text = |v: &JValue| if v.truthy() { crate::executor::js_string(v) } else { String::new() };
                service.decide(dir, &project_id, task, &text(b.get("decision")), &text(b.get("approvalId")))
            }
        };
        outcome.map(|v| (200, v)).map_err(|f| mapped(&f))
    })();
    Some(answer.unwrap_or_else(|e| e))
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "00000000-0000-4000-8000-000000000001";

    #[test]
    fn pattern() {
        let t = |p: &str| parse(p).map(|t| (t.project, t.task, t.action));
        assert_eq!(t("/api/projects/p/browser"), Some(("p".into(), None, None)));
        assert_eq!(t(&format!("/api/projects/p/browser/{T}/act")), Some(("p".into(), Some(T.into()), Some("act".into()))));
        assert_eq!(t(&format!("/api/projects/p/browser/{T}")), Some(("p".into(), Some(T.into()), None)));
        // The task is optional in the pattern: an action straight after `browser` is a start.
        assert_eq!(t("/api/projects/p/browser/act"), Some(("p".into(), None, Some("act".into()))));
        assert_eq!(t("/api/projects/p/browserx"), None);
        assert_eq!(t("/api/projects/p/browser/not-a-uuid/act"), None);
        assert_eq!(t(&format!("/api/projects/p/browser/{T}/run")), None);
        assert_eq!(t("/api/projects//browser"), None);
        assert_eq!(t("/api/projects/a/b/browser"), None);
    }
}
