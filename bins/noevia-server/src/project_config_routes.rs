//! `POST /api/projects/{projectId}/config` and `POST /api/projects/{project}/skills/install`
//! (sbstndalton/noevia#2191 round 9, capability `rust-project-config`): the two project routes that
//! wait for something outside the store with the request still open. The other instruction-skill
//! routes are plain store work and are served by project_routes.rs.
//!
//! Each runs as spawned work the handler awaits (`tokio::spawn`), so a client that goes away
//! mid-request strands nothing: the engine read and the fetch finish or time out, and a save that
//! has started is finished and answered to nobody.
//!
//! - The configuration needs two facts the store does not hold. Whether a model can answer a chat
//!   (it reads the engine's `/models` listing for the one model the patch names, bounded by the
//!   stats engine's listing deadline; an unreadable listing judges by name, as core does with an
//!   unreadable catalogue, model_kind.rs) and whether a provider is registered (the shared rows
//!   and the account's own rows, provider_rows.rs).
//! - The install is checked against the project first (404), then the body, then the fetch (the
//!   fixed raw host, public https, no redirect, an 8 s deadline, 32 KiB; plugin_directory_routes.rs
//!   `fetch_skill`), then one save under the project store's lock.

use crate::plugin_directory_routes::SkillFetch;
use crate::project_routes::{respond, Regate};
use crate::reply;
use crate::App;
use axum::body::Body;
use axum::http::{Response, StatusCode};
use server_projects::lifecycle::Catalogue;
use server_projects::{provider_rows, Ctx, Reply};
use std::path::PathBuf;
use std::sync::Arc;

/// One configuration request.
pub struct Config {
    pub app: Arc<App>,
    pub data_dir: PathBuf,
    pub user: String,
    pub project: String,
    pub body: Option<server_account::Body>,
    pub regate: Regate,
}

/// One install request.
pub struct Install {
    pub app: Arc<App>,
    pub data_dir: PathBuf,
    pub user: String,
    pub project: String,
    pub body: Option<server_account::Body>,
    pub regate: Regate,
}

fn internal() -> Response<Body> {
    reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true)
}

fn ctx<'a>(data_dir: &'a std::path::Path, user: &'a str) -> Ctx<'a> {
    Ctx {
        data_dir,
        user_id: user,
        switch: server_store::RustProjects::owned(),
        now_ms: crate::identity::now_ms(),
    }
}

/// The model the patch names, when it names one (`typeof patch.model === 'string' && patch.model`).
fn model_of(bytes: &[u8]) -> Option<String> {
    let parsed = js_json::wtf16::parse(&String::from_utf8_lossy(bytes)).ok()?;
    parsed
        .get("model")
        .as_str()
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
}

/// Whether a provider row of this id is registered for the account (a shared row, or one of the
/// account's own that is not a shared copy).
fn provider_registered(data_dir: &std::path::Path, user: &str, id: &str) -> bool {
    let named = |row: &js_json::JValue| row.get("id").as_str() == Some(id);
    let shared = provider_rows::read_shared(data_dir)
        .map(|rows| rows.iter().any(named))
        .unwrap_or(false);
    if shared {
        return true;
    }
    let Ok(dir) = server_projects::store::user_dir(data_dir, user) else {
        return false;
    };
    provider_rows::read_user(&dir)
        .map(|rows| {
            rows.iter()
                .any(|row| named(row) && !row.get("shared").truthy())
        })
        .unwrap_or(false)
}

pub async fn config(work: Config) -> Response<Body> {
    match tokio::spawn(run_config(work)).await {
        Ok(response) => response,
        Err(_) => internal(),
    }
}

async fn run_config(work: Config) -> Response<Body> {
    let Config {
        app,
        data_dir,
        user,
        project,
        body,
        regate,
    } = work;
    let (bytes, over) = match body {
        Some(b) => (b.bytes, b.over),
        None => (Vec::new(), false),
    };
    // The engine's word on the model the patch names (a model of another provider is never judged,
    // but that is known only with the stored project, so the listing is read for any named model).
    let named = if over { None } else { model_of(&bytes) };
    let labels = match (&named, app.config.rust_stats.as_ref()) {
        (Some(model), Some(engine)) => match engine.listing().await {
            server_stats::compose::Read::Done { ok: true, body } => {
                crate::model_kind::labels_of(&body, model)
            }
            _ => None,
        },
        _ => None,
    };
    let roles = crate::model_kind::roles_beside(app.config.sizing.presets_path());
    let done = tokio::task::spawn_blocking(move || {
        if let Err(response) = regate.again(&data_dir, &user) {
            return *response;
        }
        let catalogue = Catalogue::read(&data_dir);
        // core index.cjs `DEFAULT_PROVIDER_ID` (provider_test_routes.rs needs a feature this route does not).
        let default_provider = std::env::var("DEFAULT_PROVIDER_ID")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "default".into());
        let known = |id: &str| provider_registered(&data_dir, &user, id);
        let chat = |name: &str| {
            crate::model_kind::is_chat_generation_model(&roles, name, labels.as_deref())
        };
        let env = server_projects::config::Env {
            catalogue: catalogue.as_ref(),
            default_provider: &default_provider,
            provider_known: &known,
            chat_model: &chat,
        };
        respond(server_projects::config::post(
            ctx(&data_dir, &user),
            &project,
            &bytes,
            over,
            &env,
        ))
    })
    .await;
    done.unwrap_or_else(|_| internal())
}

pub async fn install(work: Install) -> Response<Body> {
    match tokio::spawn(run_install(work)).await {
        Ok(response) => response,
        Err(_) => internal(),
    }
}

/// `new Date(ms).toISOString()`.
pub fn iso(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rest = ms.rem_euclid(86_400_000);
    let (y, m, d) = offsite_backup_format::civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

async fn run_install(work: Install) -> Response<Body> {
    let Install {
        app,
        data_dir,
        user,
        project,
        body,
        regate,
    } = work;
    let regate = Arc::new(regate);
    // The project first (core looked it up before it read the body). `Some` is the answer to send.
    let first = {
        let (data_dir, user, project, regate) = (
            data_dir.clone(),
            user.clone(),
            project.clone(),
            Arc::clone(&regate),
        );
        tokio::task::spawn_blocking(move || {
            if let Err(response) = regate.again(&data_dir, &user) {
                return Some(*response);
            }
            server_projects::skill_routes::install_check(ctx(&data_dir, &user), &project)
                .err()
                .map(respond)
        })
        .await
    };
    match first {
        Ok(None) => {}
        Ok(Some(response)) => return response,
        Err(_) => return internal(),
    }
    let (bytes, over) = match body {
        Some(b) => (b.bytes, b.over),
        None => (Vec::new(), false),
    };
    let text = String::from_utf8_lossy(&bytes);
    let parsed = if over {
        None
    } else if text.is_empty() {
        Some(js_json::JValue::Obj(Vec::new()))
    } else {
        js_json::wtf16::parse(&text).ok()
    };
    let Some(parsed) = parsed else {
        return respond(Reply::error(400, "invalid JSON"));
    };
    let Some(name) = server_projects::skill_routes::skill_name(&parsed) else {
        return respond(Reply::error(
            400,
            server_projects::skill_routes::UNKNOWN_SKILL,
        ));
    };
    let content = match app.plugin_directory.fetch_skill(&name).await {
        Ok(content) => content,
        Err(SkillFetch::Missing) => {
            return respond(Reply::error(404, "That skill has no SKILL.md"))
        }
        Err(SkillFetch::TooLarge) => {
            return respond(Reply::error(
                422,
                "That skill is larger than the 32 KiB limit for project skills.",
            ))
        }
        Err(SkillFetch::Unreachable) => {
            return respond(Reply::error(
                502,
                "The skills repository could not be reached right now.",
            ))
        }
    };
    let saved = tokio::task::spawn_blocking(move || {
        if let Err(response) = regate.again(&data_dir, &user) {
            return *response;
        }
        let now = crate::identity::now_ms();
        respond(server_projects::skill_routes::install(
            ctx(&data_dir, &user),
            &project,
            &name,
            &content,
            &iso(now),
        ))
    })
    .await;
    saved.unwrap_or_else(|_| internal())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn iso_is_to_iso_string() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(iso(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn the_model_is_found_only_when_it_is_a_nonempty_string() {
        assert_eq!(model_of(br#"{"model":"m"}"#).as_deref(), Some("m"));
        for none in [
            &br#"{"model":""}"#[..],
            br#"{"model":5}"#,
            b"{}",
            b"nope",
            b"null",
        ] {
            assert_eq!(model_of(none), None);
        }
    }
}
