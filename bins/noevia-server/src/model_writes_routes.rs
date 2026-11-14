//! Track R11: `PUT /api/models/inference-budget`, `PUT /api/models/idle-unload` and `PUT
//! /api/models/preset` answered by the front (crates/model-writes and model_maint preset.rs,
//! recorded from core), behind `NOEVIA_RUST_MODEL_WRITES`, which is off by default: unset, the PUTs
//! go to core exactly as before (their route rows list GET only). With `rust`, lib.rs hands them to
//! the models routes and they are answered here; core, told the same switch, refuses its own PUTs
//! (a direct request on its port cannot write beside the front).
//!
//! - The budget: the body read as core's `readJson`, the administrator check, the validation
//!   against this host's `/proc/meminfo` (the front and core run in one container), then the
//!   `inference:memory-budget` row and the `inference.budget` audit record in one transaction.
//!   Core reads the row on every use (noevia-core `inference-budget.cjs`, track R11), so the figure
//!   is enforced at once; pair this switch only with such a core.
//! - The idle unload time: the model manager's `GET`/`PUT /api/v1/idle-unload` (its token header,
//!   30 s, no redirects, 8 MiB answers), the write and the router's reload under one maintenance
//!   hold (model_maint `preset_hold`/`reload_held`, the hold `POST /api/models/presets/reload` takes),
//!   the router's presets read through the front's own reload baseline, and the
//!   `models.idle_unload` audit record.
//!
//! Core's write prelude drops its shared model reads (`invalidateModelReads`, which also republishes
//! the auto-roles snapshot) before any models write; the front asks core to do the same over the
//! auto-roles bridge (`/internal/auto-roles/invalidate`) right after the session check, as its own
//! auto-roles write does.
//!
//! - The preset save: the administrator check, then (core `routes/models.cjs`) 404 without a
//!   llama.cpp manager, the body read as core's `readJson` uncaught there (413 `Request exceeds size
//!   limit` over 1 MiB, 400 `invalid JSON`, as index.cjs answers a thrown error), then the save
//!   itself on its own task (model_maint preset.rs, whose module documentation names its
//!   differences), so a client that leaves strands neither the work nor its hold.
//!
//! Named difference: a request body core's `JSON.parse` reads but the port cannot hold is passed to
//! core unchanged (and so refused by it under the switch: 503).
use crate::{identity, reply, App};
use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use js_json::{wtf16, JValue};
use model_writes::idle::Host as _;
use model_writes::{budget, idle};
use std::sync::Arc;
use std::time::Duration;

/// The switch.
pub const SWITCH: &str = "NOEVIA_RUST_MODEL_WRITES";
pub const BUDGET: &str = "/api/models/inference-budget";
pub const IDLE: &str = crate::idle_unload_read::PATH;
pub const PRESET: &str = crate::preset_read_routes::PATH;
/// fetchJson's answer cap (http.cjs `DEFAULT_MAX_RESPONSE_BYTES`).
const MANAGER_ANSWER_CAP: usize = 8 * 1024 * 1024;

/// The switch and what the answers read of the environment.
#[derive(Clone, Default)]
pub struct Settings {
    pub on: bool,
    /// `INFERENCE_MEMORY_BUDGET_GIB`.
    pub budget_gib: Option<String>,
    /// `MODEL_LOADER_URL` as set (core creates the idle setting only when it is not empty).
    pub loader_url: Option<String>,
    pub loader_token: Option<String>,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("on", &self.on)
            .field("budget_gib", &self.budget_gib)
            .field("loader_url", &self.loader_url.is_some())
            .field("loader_token", &self.loader_token.is_some())
            .finish()
    }
}

impl Settings {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Settings {
        Settings {
            on: get(SWITCH).as_deref() == Some("rust"),
            budget_gib: get("INFERENCE_MEMORY_BUDGET_GIB"),
            loader_url: get("MODEL_LOADER_URL"),
            loader_token: get("MODEL_LOADER_TOKEN").filter(|t| !t.is_empty()),
        }
    }

    /// Whether the front answers this request (lib.rs's dispatch and the models routes ask).
    pub fn answers(&self, method: &Method, path: &str) -> bool {
        self.on && method == Method::PUT && (path == BUDGET || path == IDLE || path == PRESET)
    }
}

fn answer(status: u16, body: &JValue) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        wtf16::stringify(body).unwrap_or_default(),
        true,
    )
}

fn log(entry: &JValue) {
    println!("[models] {}", wtf16::stringify(entry).unwrap_or_default());
}

/// core auth.cjs `audit(action, actor, actor, detail)`.
fn audit(
    tx: &server_store::WriteTx<'_>,
    actor: &str,
    action: &str,
    detail: &JValue,
) -> Result<(), server_store::StoreError> {
    tx.execute(
        "INSERT INTO audit_events(actor_user_id,target_user_id,action,detail,created_at) VALUES(?1,?1,?2,?3,?4)",
        server_store::rusqlite::params![
            actor,
            action,
            js_json::stringify(detail).unwrap_or_default(),
            identity::now_ms()
        ],
    )?;
    Ok(())
}

pub async fn serve(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    req: Request<Body>,
    path: String,
) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let creds = identity::creds_of(&parts);
    let layer = Arc::clone(&app.identity);
    let now = identity::now_ms();
    let gate_path = path.clone();
    let id = match tokio::task::spawn_blocking(move || layer.gate_blocking(&creds, &gate_path, now))
        .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(rejection)) => return rejection.into_response(),
        Err(_) => return identity::Rejection::Unavailable.into_response(),
    };
    let admin = id.user.role == server_auth::Role::Admin;
    // routes/models.cjs's prelude, before the administrator check and whatever the outcome.
    let _ = app
        .config
        .research
        .node_call(
            "/internal/auto-roles/invalidate",
            &serde_json::json!({}),
            Duration::from_secs(2),
        )
        .await;
    if path == PRESET {
        if !admin {
            return answer(403, &model_writes::body::administrator_required());
        }
        return preset(app, conn, parts, body).await;
    }
    // readBody's limit, one byte over to tell "too large" apart.
    let bytes = axum::body::to_bytes(body, model_writes::body::LIMIT + 1)
        .await
        .ok();
    let within = bytes
        .as_deref()
        .filter(|b| b.len() <= model_writes::body::LIMIT);
    let settings = &app.config.model_writes;
    let forward = |bytes: Option<axum::body::Bytes>| {
        let req = Request::from_parts(parts.clone(), Body::from(bytes.unwrap_or_default()));
        app.proxy.forward(conn.peer, req)
    };
    if path == BUDGET {
        let default = budget::default_gib(settings.budget_gib.as_deref());
        let host = budget::host_ram_gib(
            std::fs::read_to_string("/proc/meminfo")
                .as_deref()
                .map_err(|_| ()),
        );
        let Some(put) = budget::put(admin, within, default, host) else {
            return forward(bytes).await;
        };
        if let Some(n) = put.save {
            let (store, actor) = (Arc::clone(app), id.user.id.clone());
            let saved = tokio::task::spawn_blocking(move || {
                store.writes.writer()?.write(|tx| {
                    tx.set_inference_budget(
                        server_store::RustInferenceBudget::owned(),
                        &budget::row(n),
                    )?;
                    audit(tx, &actor, budget::AUDIT, &budget::audit_detail(n))
                })
            })
            .await;
            if !matches!(saved, Ok(Ok(()))) {
                log(&JValue::obj([(
                    "event",
                    JValue::from("budget.save_failed"),
                )]));
                return answer(
                    500,
                    &JValue::obj([("error", JValue::from("Could not save the budget"))]),
                );
            }
            // Only logs: the answer does not wait for it.
            let app = Arc::clone(app);
            tokio::spawn(async move { core_sees(&app, n).await });
        }
        return answer(put.status, &put.body);
    }
    // The idle unload time: core has no such setting without a model manager.
    let Some(base) = settings.loader_url.clone().filter(|u| !u.is_empty()) else {
        if !admin {
            return answer(403, &model_writes::body::administrator_required());
        }
        return answer(404, &idle::unavailable());
    };
    let (app2, actor) = (Arc::clone(app), id.user.id.clone());
    let rt = tokio::runtime::Handle::current();
    let body = within.map(<[u8]>::to_vec);
    let work = tokio::task::spawn_blocking(move || {
        let mut host = FrontIdle {
            app: app2,
            rt,
            base: base.trim_end_matches('/').to_string(),
            actor,
            hold: None,
        };
        let put = idle::put(admin, body.as_deref(), &mut host);
        // Every path of the flow releases the hold; this is the backstop.
        host.release();
        put
    })
    .await;
    match work {
        Ok(Some(put)) => answer(put.status, &put.body),
        Ok(None) => forward(bytes).await,
        // A panic in the flow (its hold released as the host drops): core's answer to a throw.
        Err(_) => answer(
            500,
            &JValue::obj([(
                "error",
                JValue::from("Could not change the idle unload time"),
            )]),
        ),
    }
}

fn preset_answer(a: &crate::model_maint::preset::Answer) -> Response<Body> {
    reply::json_text(
        StatusCode::from_u16(a.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        a.body.clone(),
        true,
    )
}

/// `PUT /api/models/preset` for an administrator (routes/models.cjs, after the write prelude).
async fn preset(
    app: &Arc<App>,
    conn: crate::serve::Conn,
    parts: axum::http::request::Parts,
    body: Body,
) -> Response<Body> {
    use crate::model_maint::preset::{thrown, Answer};
    if app.config.maint.kind != server_stats::Kind::LlamaCpp {
        return answer(
            404,
            &JValue::obj([("error", JValue::from("Native presets are unavailable"))]),
        );
    }
    // readJson, uncaught here: http.cjs readBody's 413 and readJson's 400 as index.cjs answers them.
    let bytes = match axum::body::to_bytes(body, model_writes::body::LIMIT + 1).await {
        Ok(b) if b.len() <= model_writes::body::LIMIT => b,
        _ => return preset_answer(&thrown(Some(413), "Request exceeds size limit")),
    };
    let request = match model_writes::body::read_json(Some(&bytes)) {
        model_writes::body::Read::Value(v) => v,
        model_writes::body::Read::Invalid => {
            return preset_answer(&thrown(Some(400), "invalid JSON"))
        }
        model_writes::body::Read::Unsupported => {
            let req = Request::from_parts(parts, Body::from(bytes));
            return app.proxy.forward(conn.peer, req).await;
        }
    };
    let app = Arc::clone(app);
    let work = tokio::spawn(async move {
        let maint = Arc::clone(&app.config.maint);
        maint
            .apply_preset(&request, || async {
                crate::model_state_routes::sizing_budget(&app).await
            })
            .await
    });
    match work.await {
        Ok(a) => preset_answer(&a),
        // A panic in the work (its hold given back as it drops): core's answer to a throw.
        Err(_) => preset_answer(&Answer {
            status: 500,
            body: r#"{"error":"Internal error"}"#.into(),
        }),
    }
}

/// After a save, asks core which figure it enforces now (the facts bridge the budget GET reads)
/// and logs when it is not the saved one: a core that reads the row only at its start (older than
/// track R11) keeps the old figure until it restarts, which the switch must not be paired with.
async fn core_sees(app: &Arc<App>, n: f64) {
    let asked = app
        .config
        .research
        .node_call(
            "/internal/models/facts",
            &serde_json::json!({"want": "budget"}),
            Duration::from_secs(5),
        )
        .await;
    let enforced = asked
        .filter(|(status, _)| *status == 200)
        .and_then(|(_, bytes)| wtf16::parse(&String::from_utf8_lossy(&bytes)).ok())
        .and_then(|facts| facts.get("budget").get("budgetGib").as_num());
    if enforced != Some(n) {
        log(&JValue::obj([
            ("event", JValue::from("budget.core_differs")),
            ("saved", JValue::Num(n)),
            ("enforced", enforced.map_or(JValue::Null, JValue::Num)),
        ]));
    }
}

/// The idle unload save's host: the model manager, the maintenance hold, the router's presets and
/// the audit, each one async step run to its end on the runtime.
struct FrontIdle {
    app: Arc<App>,
    rt: tokio::runtime::Handle,
    base: String,
    actor: String,
    hold: Option<crate::model_maint::Hold>,
}

fn reply_of(outcome: &crate::model_maint::reload::Outcome) -> idle::Reply {
    idle::Reply {
        ok: (200..300).contains(&outcome.status),
        status: outcome.status,
        body: wtf16::parse(&outcome.body).unwrap_or(JValue::Undefined),
    }
}

fn refusal_of(outcome: &crate::model_maint::reload::Outcome) -> idle::HoldRefusal {
    if outcome.status == 409 {
        return idle::HoldRefusal::Busy;
    }
    let body = wtf16::parse(&outcome.body).unwrap_or(JValue::Undefined);
    idle::HoldRefusal::Failed {
        status: outcome.status,
        message: body.get("error").as_str().unwrap_or("").to_string(),
    }
}

impl FrontIdle {
    async fn call(&self, method: &str, body: Option<String>) -> Option<idle::Reply> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_millis(idle::MANAGER_TIMEOUT_MS))
            .build()
            .ok()?;
        let url = format!("{}{}", self.base, idle::MANAGER_PATH);
        let mut req = match method {
            "PUT" => client.put(&url),
            _ => client.get(&url),
        }
        .header("Content-Type", "application/json");
        if let Some(token) = &self.app.config.model_writes.loader_token {
            req = req.header("X-Model-Loader-Token", token);
        }
        if let Some(b) = body {
            req = req.body(b);
        }
        let res = req.send().await.ok()?;
        // fetch with `redirect: 'error'` throws on a redirect.
        if res.status().is_redirection() {
            return None;
        }
        let status = res.status().as_u16();
        let ok = res.status().is_success();
        let mut res = res;
        let mut buf = Vec::new();
        while let Some(chunk) = res.chunk().await.ok()? {
            buf.extend_from_slice(&chunk);
            if buf.len() > MANAGER_ANSWER_CAP {
                // http.cjs: `{ ok: false, status, error }`, no body.
                return Some(idle::Reply {
                    ok: false,
                    status,
                    body: JValue::Undefined,
                });
            }
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        let body = wtf16::parse(&text).unwrap_or(JValue::Str(text));
        Some(idle::Reply { ok, status, body })
    }
}

impl idle::Host for FrontIdle {
    fn manager_missing(&self) -> bool {
        self.base.is_empty()
    }
    fn manager(&mut self, method: &str, body: Option<&str>) -> Option<idle::Reply> {
        let body = body.map(str::to_string);
        self.rt.block_on(self.call(method, body))
    }
    fn has_writer(&self) -> bool {
        self.app.config.maint.kind == server_stats::Kind::LlamaCpp
    }
    fn hold(&mut self) -> Result<(), idle::HoldRefusal> {
        let maint = Arc::clone(&self.app.config.maint);
        match self.rt.block_on(maint.preset_hold()) {
            Ok(h) => {
                self.hold = Some(h);
                Ok(())
            }
            Err(outcome) => Err(refusal_of(&outcome)),
        }
    }
    fn reload_held(&mut self) -> Result<idle::Reply, idle::CoverageLost> {
        let Some(hold) = self.hold.as_ref() else {
            // Not reachable (save asks only under a hold): no reload, `{ ok: false }`.
            return Ok(idle::Reply {
                ok: false,
                status: 0,
                body: JValue::Undefined,
            });
        };
        let maint = Arc::clone(&self.app.config.maint);
        let outcome = self.rt.block_on(maint.reload_held(hold));
        if crate::model_maint::reload::lease_lost(&outcome) {
            let body = wtf16::parse(&outcome.body).unwrap_or(JValue::Undefined);
            return Err(idle::CoverageLost {
                status: outcome.status,
                message: body.get("error").as_str().unwrap_or("").to_string(),
            });
        }
        Ok(reply_of(&outcome))
    }
    fn release(&mut self) {
        if let Some(hold) = self.hold.take() {
            self.rt.block_on(hold.release());
        }
    }
    fn reload(&mut self) -> Option<Option<idle::Reply>> {
        // Core's reloadPresets without a held writer belongs to a manager that is not llama.cpp,
        // which has no preset file: `{ ok: false }`.
        Some(Some(idle::Reply {
            ok: false,
            status: 0,
            body: JValue::Undefined,
        }))
    }
    fn router_text(&mut self) -> Option<String> {
        let maint = Arc::clone(&self.app.config.maint);
        self.rt.block_on(async move { maint.router_text().await })
    }
    fn audit(&mut self, action: &str, detail: JValue) -> Result<(), idle::AuditFailed> {
        let (actor, action) = (self.actor.clone(), action.to_string());
        let written = self
            .app
            .writes
            .writer()
            .and_then(|w| w.write(|tx| audit(tx, &actor, &action, &detail)));
        written.map_err(|_| {
            log(&JValue::obj([(
                "event",
                JValue::from("idle_unload.audit_failed"),
            )]));
            idle::AuditFailed
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_off_unless_named_and_covers_three_puts() {
        let settings = |v: Option<&str>| {
            let v = v.map(str::to_string);
            Settings::from_lookup(&move |k: &str| (k == SWITCH).then(|| v.clone()).flatten())
        };
        for off in [None, Some(""), Some("on"), Some("Rust")] {
            assert!(!settings(off).answers(&Method::PUT, BUDGET), "{off:?}");
        }
        let on = settings(Some("rust"));
        assert!(on.answers(&Method::PUT, BUDGET));
        assert!(on.answers(&Method::PUT, IDLE));
        assert!(!on.answers(&Method::GET, BUDGET));
        assert!(!on.answers(&Method::POST, IDLE));
        assert!(on.answers(&Method::PUT, PRESET));
        assert!(!settings(None).answers(&Method::PUT, PRESET));
        assert!(!on.answers(&Method::GET, PRESET));
        assert!(!on.answers(&Method::PUT, "/api/models/preset/"));
        assert!(!on.answers(&Method::PUT, "/api/models/preset/suggest"));
    }

    #[test]
    fn the_loader_token_stays_out_of_debug() {
        let s = Settings::from_lookup(&|k: &str| match k {
            "MODEL_LOADER_TOKEN" => Some("qa-secret-token".into()),
            "MODEL_LOADER_URL" => Some("http://qa-loader.invalid".into()),
            _ => None,
        });
        assert!(!format!("{s:?}").contains("qa-secret"));
    }
}
