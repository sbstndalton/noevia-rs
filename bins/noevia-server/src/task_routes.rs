//! The Code and Browser task reads (sbstndalton/noevia#2257, capability `rust-task-reads`):
//! `GET /api/code/active` (other methods answer core's 405) and the GETs of
//! `/api/projects/{id}/code[/{task}]` and `/api/projects/{id}/browser[/{task}]`, answer for answer with core
//! routes/code.cjs and routes/browser.cjs. Starting, approving, cancelling and running a task stay Node's.
//!
//! The journals are read here (server_projects::task_journals). What is not on disk stays where it is, asked
//! for on the loopback bridge before the journals are read (core rust-task-facts.cjs): the flags as Node's
//! `features.enabled` answers them (Browser mode also needs Playwright in Node's process), the waiting approval
//! cards, which live in Node's memory on purpose so a restart re-asks, whether the egress proxy is answering,
//! and the deployment's Code configuration. That call also runs Node's per-account journal recovery, first:
//! it is what marks a task from a process that no longer exists as interrupted, so a read here never shows
//! one as running. No answer from Node is a 503, never a task list without its approvals.
//!
//! The order is core's: the flag (404), the administrator (403), then the project (404).
use crate::{identity::Rejection, reply, App};
use axum::{
    body::Body,
    http::{Method, Response, StatusCode},
    response::IntoResponse,
};
use js_json::{wtf16, JValue};
use serde_json::json;
use server_projects::task_journals::{self as journals, Found, Surface};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const FACTS_PATH: &str = "/internal/tasks/facts";
const FACTS_DEADLINE: Duration = Duration::from_secs(5);
const ACTIVE_LIMIT: usize = 100;
const NODE_UNREACHABLE: &str = "Task status is unavailable right now. Try again shortly.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Active,
    State(Surface, String),
    Task(Surface, String, String),
}

/// core's `PATTERN`s, the reads only: `None` leaves the request with Node.
pub fn parse(method: &str, path: &str) -> Option<Target> {
    if path == "/api/code/active" {
        return Some(Target::Active);
    }
    if method != "GET" {
        return None;
    }
    let rest = path.strip_prefix("/api/projects/")?;
    let mut parts = rest.split('/');
    let project = parts.next().filter(|p| !p.is_empty())?;
    let surface = match parts.next()? {
        "code" => Surface::Code,
        "browser" => Surface::Browser,
        _ => return None,
    };
    match (parts.next(), parts.next()) {
        (None, _) => Some(Target::State(surface, project.to_owned())),
        (Some(id), None) if journals::valid_id(id) => {
            Some(Target::Task(surface, project.to_owned(), id.to_owned()))
        }
        _ => None,
    }
}

fn answer(status: StatusCode, body: &JValue) -> Response<Body> {
    reply::json_text(
        status,
        wtf16::stringify(body).unwrap_or_else(|| "null".into()),
        true,
    )
}

enum Facts {
    Off,
    Member,
    Admin(JValue),
}

async fn facts(app: &App, user: &str, surface: Surface, admin: bool) -> Option<Facts> {
    let body = json!({ "user": user, "surface": surface.kind(), "admin": admin });
    let (status, bytes) = app
        .config
        .research
        .node_call(FACTS_PATH, &body, FACTS_DEADLINE)
        .await?;
    if status != 200 {
        return None;
    }
    let parsed = wtf16::parse(&String::from_utf8_lossy(&bytes)).ok()?;
    match parsed.get("enabled") {
        JValue::Bool(false) => Some(Facts::Off),
        JValue::Bool(true) if admin => parsed
            .get("facts")
            .is_object()
            .then(|| Facts::Admin(parsed.get("facts").clone())),
        JValue::Bool(true) => Some(Facts::Member),
        _ => None,
    }
}

fn card_of(facts: &JValue, job: &journals::Job) -> JValue {
    let id = job.id.as_str().unwrap_or("");
    match facts.get("approvals").get(id) {
        JValue::Undefined => JValue::Null,
        card => card.clone(),
    }
}

fn view_of(surface: Surface, job: &journals::Job, card: &JValue) -> JValue {
    match surface {
        Surface::Code => journals::code_view(job, card),
        Surface::Browser => journals::browser_view(job, card),
    }
}

fn failed(surface: Surface) -> Response<Body> {
    let message = match surface {
        Surface::Code => "Code request failed",
        Surface::Browser => "Browser request failed",
    };
    reply::error(StatusCode::INTERNAL_SERVER_ERROR, message, true)
}

/// One read, after the session gate.
pub async fn serve(
    app: &Arc<App>,
    method: Method,
    authn: server_auth::Identity,
    data_dir: PathBuf,
    target: Target,
) -> Response<Body> {
    let surface = match &target {
        Target::Active => Surface::Code,
        Target::State(s, _) | Target::Task(s, _, _) => *s,
    };
    let admin = authn.user.role == server_auth::Role::Admin;
    let user = authn.user.id.as_str().to_owned();
    let Some(known) = facts(app, &user, surface, admin).await else {
        return reply::error(StatusCode::SERVICE_UNAVAILABLE, NODE_UNREACHABLE, true);
    };
    let facts = match known {
        Facts::Off => {
            let message = match surface {
                Surface::Code => "Code mode is not enabled on this server.",
                Surface::Browser => "Browser mode is not enabled on this server.",
            };
            return reply::error(StatusCode::NOT_FOUND, message, true);
        }
        Facts::Member => {
            return reply::error(StatusCode::FORBIDDEN, "Administrator required", true)
        }
        Facts::Admin(facts) => facts,
    };
    if method != Method::GET {
        return reply::error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", true);
    }
    let switch = app.config.rust_projects;
    let work = tokio::task::spawn_blocking(move || match target {
        Target::Active => active(&data_dir, &user, &facts),
        Target::State(surface, raw) => {
            let Some(id) = server_projects::js::decode_uri_component(&raw) else {
                return failed(surface);
            };
            match project_exists(&data_dir, &user, switch, &id) {
                Ok(true) => state(&data_dir, &user, surface, &id, &facts),
                Ok(false) => reply::error(StatusCode::NOT_FOUND, "project not found", true),
                Err(()) => failed(surface),
            }
        }
        Target::Task(surface, raw, task) => {
            let Some(id) = server_projects::js::decode_uri_component(&raw) else {
                return failed(surface);
            };
            match project_exists(&data_dir, &user, switch, &id) {
                Ok(true) => task_view(&data_dir, &user, surface, &id, &task, &facts),
                Ok(false) => reply::error(StatusCode::NOT_FOUND, "project not found", true),
                Err(()) => failed(surface),
            }
        }
    })
    .await;
    work.unwrap_or_else(|_| Rejection::Unavailable.into_response())
}

fn project_exists(
    data_dir: &std::path::Path,
    user: &str,
    switch: server_store::RustProjects,
    id: &str,
) -> Result<bool, ()> {
    let ctx = server_projects::Ctx {
        data_dir,
        user_id: user,
        switch,
        now_ms: crate::identity::now_ms(),
    };
    match server_projects::originals::project(ctx, id) {
        Ok((_, project)) => Ok(project.is_some()),
        Err(_) => Err(()),
    }
}

/// `GET /api/code/active`: one scan for the account, its own visible projects only.
fn active(data_dir: &std::path::Path, user: &str, facts: &JValue) -> Response<Body> {
    const FAILED: &str = "Could not load active tasks";
    let Ok(dir) = server_projects::store::user_dir(data_dir, user) else {
        return reply::error(StatusCode::INTERNAL_SERVER_ERROR, FAILED, true);
    };
    let Ok(projects) = server_projects::store::read(&dir) else {
        return reply::error(StatusCode::INTERNAL_SERVER_ERROR, FAILED, true);
    };
    let names: Vec<(&str, &JValue)> = projects
        .iter()
        .filter_map(|p| Some((p.get("id").as_str()?, p.get("name"))))
        .filter(|(id, _)| !journals::internal_project(id))
        .collect();
    let Ok(jobs) = journals::list(data_dir, user, Surface::Code, None, true) else {
        return reply::error(StatusCode::INTERNAL_SERVER_ERROR, FAILED, true);
    };
    let matching: Vec<_> = jobs
        .iter()
        .filter_map(|job| {
            let own = job.project_id.as_str()?;
            let (_, name) = names.iter().find(|(id, _)| *id == own)?;
            Some((job, *name))
        })
        .collect();
    let tasks = matching
        .iter()
        .take(ACTIVE_LIMIT)
        .map(|(job, name)| journals::active_row(job, name, &card_of(facts, job)))
        .collect();
    answer(
        StatusCode::OK,
        &JValue::Obj(vec![
            ("tasks".to_owned(), JValue::Arr(tasks)),
            ("total".to_owned(), JValue::Num(matching.len() as f64)),
        ]),
    )
}

/// `GET /api/projects/{id}/code|browser`: Node's facts with this project's tasks in place of its approvals.
fn state(
    data_dir: &std::path::Path,
    user: &str,
    surface: Surface,
    project: &str,
    facts: &JValue,
) -> Response<Body> {
    let Ok(jobs) = journals::list(data_dir, user, surface, Some(project), false) else {
        return failed(surface);
    };
    let tasks = jobs
        .iter()
        .map(|job| view_of(surface, job, &card_of(facts, job)))
        .collect();
    let JValue::Obj(items) = facts else {
        return failed(surface);
    };
    let mut body: Vec<(String, JValue)> = items
        .iter()
        .filter(|(k, _)| k != "approvals")
        .cloned()
        .collect();
    body.push(("tasks".to_owned(), JValue::Arr(tasks)));
    answer(StatusCode::OK, &JValue::Obj(body))
}

/// `GET /api/projects/{id}/code|browser/{task}`.
fn task_view(
    data_dir: &std::path::Path,
    user: &str,
    surface: Surface,
    project: &str,
    task: &str,
    facts: &JValue,
) -> Response<Body> {
    match journals::get(data_dir, user, surface, project, task) {
        Ok(Found::Task(job)) => answer(
            StatusCode::OK,
            &view_of(surface, &job, &card_of(facts, &job)),
        ),
        Ok(Found::Missing) => reply::error(StatusCode::NOT_FOUND, "Task not found", true),
        Ok(Found::Unreadable) => reply::error(
            StatusCode::CONFLICT,
            "Unreadable job journal; review required",
            true,
        ),
        Err(_) => failed(surface),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn only_the_reads_are_the_fronts() {
        let id = "01234567-89ab-cdef-0123-456789abcdef";
        assert_eq!(parse("POST", "/api/code/active"), Some(Target::Active));
        assert_eq!(
            parse("GET", "/api/projects/p%201/code"),
            Some(Target::State(Surface::Code, "p%201".into()))
        );
        assert_eq!(
            parse("GET", &format!("/api/projects/p/browser/{id}")),
            Some(Target::Task(Surface::Browser, "p".into(), id.into()))
        );
        for (m, p) in [
            ("POST", "/api/projects/p/code".to_owned()),
            ("HEAD", "/api/projects/p/browser".to_owned()),
            ("GET", format!("/api/projects/p/code/{id}/approve")),
            ("POST", format!("/api/projects/p/code/{id}/approve")),
            ("GET", "/api/projects/p/code/not-a-task".to_owned()),
            ("GET", "/api/projects//code".to_owned()),
            ("GET", "/api/code/active/x".to_owned()),
        ] {
            assert_eq!(parse(m, &p), None, "{m} {p}");
        }
    }
}
