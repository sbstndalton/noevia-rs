//! Deep research (sbstndalton/noevia#2218, capability `rust-research`): `GET` and `POST
//! /api/projects/{projectId}/research`, `POST .../research/plan`, `GET .../research/{job}`, `POST
//! .../research/{job}/cancel` and `POST .../research/{job}/save`, answer for answer with core
//! routes/research.cjs and research-service.cjs. Behind `features.deepResearch`, the owner's flag: off (the
//! default) every one is the 404 core gave. Administrators only; a project of the signed-in account only.
//!
//! The rules are crates/research-core, the journal is `server_projects::research_jobs` (unchanged rows and
//! files, so rollback reads what this writes). What is not this process's stays where it is, reached over
//! the loopback bridge with the per-start secret (core rust-research-bridge.cjs): the operator's MCP search
//! server, the project's RAG index, and admission to the shared engine (crates/engine-admission).
//!
//! **Tasks.** A job is a task of its own: the request that started it can go at any moment. A plan is
//! a spawned task the handler awaits and a drop guard cancels, so a client that leaves stops the engine's
//! work and still gives its lease back. A cancel records its intent in the journal first, then stops the
//! job's model and tool calls; the job writes its own terminal row, with the sections it finished.
//!
//! **Restart.** As in Node, a job does not survive the process that runs it: the next request for that
//! account finds its row and ends it as interrupted (`research_jobs::recover`).
use crate::identity::Rejection;
use crate::project_routes::Regate;
use crate::reply;
use crate::App;
use axum::body::Body;
use axum::http::{Method, Response, StatusCode};
use engine_admission::machines::{self, Target};
use engine_admission::{Admission, Ask};
use js_json::{wtf16, JValue};
use model_stream::{self as stream, Cancel};
use research_core::plan as planning;
use research_core::report;
use research_core::runner::{self, Failure, Found, Hit, Options, Search};
use serde_json::{json, Value};
use server_projects::research_jobs as jobs;
use server_projects::research_report as files;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const BODY_LIMIT: usize = 1024 * 1024;
/// core research-model.cjs: `AbortSignal.timeout(120000)` per model step.
const STEP_DEADLINE: Duration = Duration::from_secs(120);
const TOOL_DEADLINE: Duration = Duration::from_secs(120);
const RETRIEVE_DEADLINE: Duration = Duration::from_secs(60);
const AVAILABILITY_DEADLINE: Duration = Duration::from_secs(5);
const MAX_BRIDGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_STAGE_BYTES: usize = 4096;
const FEATURE_SETTING: &str = "feature:deepResearch";
const MACHINES_KEY: &str = "inference:machines";
const PICK_MODEL: &str =
    "Pick a Smart model for Auto routing, or load a model, before starting research.";
const NO_SEARCH: &str = "Web search is not configured on this server.";
const STREAM_FAILED: &str = "The model stream failed. Check the backend before retrying.";
const TOKEN_HEADER: &str = "x-noevia-store-token";

/// What the process's environment says, as core index.cjs reads it.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub engine_base: String,
    pub engine_key: Option<String>,
    pub upstream: String,
    pub token: Option<String>,
    pub flag_env: Option<bool>,
}

impl Settings {
    pub fn from_lookup(
        get: &impl Fn(&str) -> Option<String>,
        upstream: String,
    ) -> Result<Settings, String> {
        let first = |keys: &[&str]| keys.iter().find_map(|k| get(k).filter(|v| !v.is_empty()));
        let flag = server_features::spec(server_features::Feature::DeepResearch);
        Ok(Settings {
            engine_base: first(&["INFERENCE_BASE_URL", "LEMONADE_BASE_URL"])
                .unwrap_or_else(|| "http://host.docker.internal:11434".to_owned()),
            engine_key: first(&["INFERENCE_API_KEY", "LEMONADE_API_KEY"]),
            upstream,
            token: first(&["NOEVIA_PROJECT_STORE_TOKEN"]),
            flag_env: server_features::parse_env(get(flag.env).as_deref())
                .map_err(str::to_owned)?,
        })
    }
}

/// The parts of a path this module answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    List,
    Plan,
    Job(String),
    Cancel(String),
    Save(String),
    /// `.../plan/cancel`, core's 405.
    PlanAction,
    /// A shape core's matcher did not take: core's plain not-found, past its session gate.
    Other,
}

fn job_shape(s: &str) -> bool {
    s.len() == 36
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
}

/// `/api/projects/{id}/research[/...]`: the raw project id and what it asks. `None`: not research.
pub fn parse(path: &str) -> Option<(String, Shape)> {
    let rest = path.strip_prefix("/api/projects/")?;
    let mut parts = rest.split('/');
    let project = parts.next().filter(|p| !p.is_empty())?;
    if parts.next()? != "research" {
        return None;
    }
    let segments: Vec<&str> = parts.collect();
    let target = match segments.as_slice() {
        [] => Shape::List,
        ["plan"] => Shape::Plan,
        ["plan", "cancel" | "save"] => Shape::PlanAction,
        [id] if job_shape(id) => Shape::Job((*id).to_owned()),
        [id, "cancel"] if job_shape(id) => Shape::Cancel((*id).to_owned()),
        [id, "save"] if job_shape(id) => Shape::Save((*id).to_owned()),
        // core's optional groups let `/research/cancel` through as the list and the start: a POST there
        // started a job. Not a route of the web client's; it is a not-found now.
        _ => Shape::Other,
    };
    Some((project.to_owned(), target))
}

#[derive(Clone)]
struct Abandon {
    user: String,
    request: String,
    report: Option<(String, String, Value)>,
}

struct Live {
    cancel: Cancel,
    run: String,
    cancel_attempted: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Default)]
struct State {
    owner: Option<(String, String, String)>,
    recovered: HashSet<String>,
    live: HashMap<String, Live>,
    saving: HashSet<String>,
    abandons: Vec<Abandon>,
    draining: bool,
}

/// The module's process-wide state.
pub struct Research {
    settings: Settings,
    admission: Arc<Admission>,
    local: reqwest::Client,
    machine: reqwest::Client,
    bridge: reqwest::Client,
    state: Mutex<State>,
}

impl std::fmt::Debug for Research {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Research").finish_non_exhaustive()
    }
}

impl Research {
    pub fn new(settings: Settings) -> Result<Research, String> {
        let admission = Arc::new(Admission::new(
            &settings.upstream,
            settings.token.as_deref(),
        )?);
        Ok(Research {
            admission,
            local: stream::client()?,
            machine: stream::net::this_network_client()?,
            bridge: stream::client()?,
            settings,
            state: Mutex::default(),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    fn owner(&self, start: &str) -> (String, String, String) {
        let mut state = self.state();
        match &state.owner {
            Some(o) if o.0 == start => o.clone(),
            _ => {
                let o = crate::source_job_routes::owner(start);
                // A uuid of this module's own, under the process's start and birth: the source jobs keep theirs.
                let mine = (o.start, o.born, server_projects::source_jobs::new_id());
                state.owner = Some(mine.clone());
                mine
            }
        }
    }

    /// Jobs this process is running.
    pub fn running(&self) -> usize {
        self.state().live.len()
    }

    fn bridge_post(
        &self,
        path: &str,
        body: &Value,
        timeout: Duration,
    ) -> Option<reqwest::RequestBuilder> {
        let token = self.settings.token.as_deref()?;
        let mut value = reqwest::header::HeaderValue::from_str(token).ok()?;
        value.set_sensitive(true);
        Some(
            self.bridge
                .post(format!(
                    "{}{path}",
                    self.settings.upstream.trim_end_matches('/')
                ))
                .header(TOKEN_HEADER, value)
                .header("content-type", "application/json")
                .timeout(timeout)
                .body(body.to_string()),
        )
    }

    /// One bridge call: (status, JSON). `None`: no answer, a cancel, or a reply over the cap.
    async fn bridge_call(
        &self,
        path: &str,
        body: &Value,
        timeout: Duration,
        cancel: &Cancel,
    ) -> Option<(u16, Value)> {
        let req = self.bridge_post(path, body, timeout)?;
        let work = async {
            let mut res = req.send().await.ok()?;
            let status = res.status().as_u16();
            let mut bytes = Vec::new();
            while let Some(chunk) = res.chunk().await.ok()? {
                bytes.extend_from_slice(&chunk);
                if bytes.len() > MAX_BRIDGE_BYTES {
                    return None;
                }
            }
            Some((
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            ))
        };
        tokio::select! {
            () = cancel.cancelled() => None,
            r = work => r,
        }
    }
}

/// An answer with core's words.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fail {
    status: u16,
    message: String,
}

fn fail(status: u16, message: impl Into<String>) -> Fail {
    Fail {
        status,
        message: message.into(),
    }
}

fn store_fail(e: &jobs::Error) -> Fail {
    use jobs::Error as E;
    match e {
        E::BadRequest => fail(400, "invalid research job request"),
        E::Gone => fail(410, "account no longer exists"),
        E::NotFound => fail(404, "No such research job."),
        E::Busy => fail(503, "The research job store is busy."),
        E::Stale => fail(409, "stale research job owner, run or operation"),
        E::TooMany => fail(
            409,
            "A research job is already running in this project. Wait for it or cancel it.",
        ),
        E::Corrupt => fail(503, "Research job state unknown; review required"),
        E::Io => fail(503, "research job store failed"),
    }
}

fn answer(status: u16, body: &JValue) -> Response<Body> {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    reply::json_text(
        code,
        wtf16::stringify(body).unwrap_or_else(|| "null".into()),
        true,
    )
}

fn error_answer(f: &Fail) -> Response<Body> {
    let code = StatusCode::from_u16(f.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    reply::error(code, &f.message, true)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Fail> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| fail(500, "Research request failed"))
}

/// One account's view of the module.
#[derive(Clone)]
struct Account {
    app: Arc<App>,
    authn: server_auth::Identity,
    data_dir: PathBuf,
    start: String,
    user: String,
}

impl Account {
    fn research(&self) -> &Arc<Research> {
        &self.app.config.research
    }

    /// Runs `f` with the store's identity, on the blocking pool.
    async fn store<T: Send + 'static>(
        &self,
        f: impl FnOnce(jobs::Context<'_>, jobs::Owner<'_>) -> Result<T, jobs::Error> + Send + 'static,
    ) -> Result<T, Fail> {
        let (start, born, owner) = self.research().owner(&self.start);
        let (data_dir, user) = (self.data_dir.clone(), self.user.clone());
        let store_start = self.start.clone();
        blocking(move || {
            let ctx = jobs::Context {
                data_dir: &data_dir,
                start_id: &store_start,
            };
            f(
                ctx,
                jobs::Owner {
                    user: &user,
                    start: &start,
                    born: &born,
                    owner: &owner,
                },
            )
        })
        .await?
        .map_err(|e| store_fail(&e))
    }

    /// core `storeFor`: the account's old jobs are recovered before anything reads them, once.
    async fn recovered(&self) -> Result<(), Fail> {
        if self.research().state().recovered.contains(&self.user) {
            return Ok(());
        }
        self.store(jobs::recover).await?;
        self.research().state().recovered.insert(self.user.clone());
        Ok(())
    }

    /// The project as it is now.
    async fn project(&self, id: &str) -> Result<Option<JValue>, Fail> {
        let (data_dir, user, id) = (self.data_dir.clone(), self.user.clone(), id.to_owned());
        let switch = self.app.config.rust_projects;
        blocking(move || {
            let ctx = server_projects::Ctx {
                data_dir: &data_dir,
                user_id: &user,
                switch,
                now_ms: crate::identity::now_ms(),
            };
            match server_projects::originals::project(ctx, &id) {
                Ok((_, p)) => Ok(p),
                Err(_) => Err(fail(500, "Research request failed")),
            }
        })
        .await?
    }

    async fn read_job(&self, project: &str, id: &str) -> Result<Option<Value>, Fail> {
        let (project, id) = (project.to_owned(), id.to_owned());
        self.store(move |ctx, o| jobs::read(ctx, o.user, &project, &id))
            .await
    }

    /// core `publicView`.
    async fn public_view(&self, job: &Value, project: &str) -> Result<JValue, Fail> {
        let current = self.project(project).await?;
        let files: Option<Vec<JValue>> = current.map(|p| match p.get("files") {
            JValue::Arr(a) => a.clone(),
            _ => Vec::new(),
        });
        Ok(report::view(job, files.as_deref()))
    }

    /// core `owned`: the account's job of this project, or the 404.
    async fn owned(&self, project: &str, id: &str) -> Result<Value, Fail> {
        match self.read_job(project, id).await? {
            Some(job)
                if job.get("kind").and_then(Value::as_str) == Some("deep_research")
                    && job.get("projectId").and_then(Value::as_str) == Some(project) =>
            {
                Ok(job)
            }
            _ => Err(fail(404, "No such research job.")),
        }
    }

    /// The Smart role's model, `project_model` and Node's last loaded one, in core's order.
    async fn local_model(
        &self,
        project_model: Option<&str>,
        last_loaded: Option<&str>,
    ) -> Option<String> {
        let dir = server_projects::store::user_dir(&self.data_dir, &self.user).ok()?;
        let roles = blocking(move || crate::auto_roles_routes::read_roles(&dir))
            .await
            .ok()?;
        let smart = roles
            .get("smart")
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        smart
            .or_else(|| project_model.map(str::to_owned))
            .or_else(|| last_loaded.map(str::to_owned))
    }

    /// core inference-machines.cjs `chatTarget('smart')`, from the row Node owns. Unreadable: this machine.
    async fn smart_machine(&self) -> Option<Target> {
        let app = Arc::clone(&self.app);
        let row = blocking(move || {
            let writer = app.writes.writer().ok()?;
            writer
                .read(|r| {
                    r.row(
                        "SELECT CASE WHEN typeof(value)='text' THEN value ELSE NULL END FROM settings WHERE key=?1",
                        [MACHINES_KEY],
                        |row| row.get::<_, Option<String>>(0),
                    )
                })
                .ok()?
                .flatten()
        })
        .await
        .ok()?;
        machines::chat_target(row.as_deref(), "smart")
    }

    /// What only Node knows: whether its search tools are offered, and the model it last loaded.
    async fn node_facts(&self) -> (Option<String>, Option<String>) {
        let body = json!({ "user": self.user });
        let r = self
            .research()
            .bridge_call(
                "/internal/research/availability",
                &body,
                AVAILABILITY_DEADLINE,
                &Cancel::new(),
            )
            .await;
        match r {
            Some((200, v)) => (
                v.get("reason").and_then(Value::as_str).map(str::to_owned),
                v.get("lastLoaded")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            ),
            _ => (Some(NO_SEARCH.to_owned()), None),
        }
    }

    /// core's `available()`: the reason research cannot run, or `None`.
    async fn unavailable(&self) -> Option<String> {
        let (tools, last_loaded) = self.node_facts().await;
        if tools.is_some() {
            return tools;
        }
        if self.smart_machine().await.is_some() {
            return None;
        }
        if self
            .local_model(None, last_loaded.as_deref())
            .await
            .is_none()
        {
            return Some(PICK_MODEL.to_owned());
        }
        None
    }

    /// core research-model.cjs `complete(project, messages, { signal, maxTokens })`.
    async fn complete(
        &self,
        project_model: Option<&str>,
        messages: &JValue,
        max_tokens: u64,
        cancel: &Cancel,
    ) -> Result<String, Failure> {
        let research = Arc::clone(self.research());
        if let Some(target) = self.smart_machine().await {
            if !stream::net::url_allowed(&target.base_url) {
                return Err(Failure(machines::connect_error(&target, true, false)));
            }
            let lease = research
                .admission
                .enter(
                    Ask::Machine {
                        role: "smart",
                        machine: &target.machine_id,
                        model: &target.model,
                    },
                    cancel,
                )
                .await
                .map_err(|r| Failure(r.message))?;
            let url = format!("{}/v1/chat/completions", target.base_url);
            let got = model_step(
                &research.machine,
                &url,
                Vec::new(),
                &target.model,
                messages,
                max_tokens,
                cancel,
                lease.lost(),
            )
            .await;
            lease.release().await;
            return got.map_err(|e| match e {
                Stop::Connect { refused } => {
                    Failure(machines::connect_error(&target, refused, false))
                }
                Stop::Redirect => Failure(machines::connect_error(&target, false, true)),
                Stop::Words(w) => Failure(w),
            });
        }
        let (_, last_loaded) = self.node_facts().await;
        let Some(model) = self
            .local_model(project_model, last_loaded.as_deref())
            .await
        else {
            return Err(Failure(PICK_MODEL.to_owned()));
        };
        let lease = research
            .admission
            .enter(Ask::Engine { model: &model }, cancel)
            .await
            .map_err(|r| Failure(r.message))?;
        let base = research.settings.engine_base.trim_end_matches('/');
        let base = base.strip_suffix("/v1").unwrap_or(base);
        let url = format!("{base}/v1/chat/completions");
        let mut headers: Vec<(String, String)> = lease.headers().to_vec();
        if let Some(key) = research
            .settings
            .engine_key
            .as_deref()
            .filter(|k| *k != "local")
        {
            headers.push(("authorization".to_owned(), format!("Bearer {key}")));
        }
        let got = model_step(
            &research.local,
            &url,
            headers,
            &model,
            messages,
            max_tokens,
            cancel,
            lease.lost(),
        )
        .await;
        lease.release().await;
        got.map_err(|e| match e {
            Stop::Connect { .. } | Stop::Redirect => {
                Failure("The model engine could not be reached.".to_owned())
            }
            Stop::Words(w) => Failure(w),
        })
    }
}

enum Stop {
    Connect { refused: bool },
    Redirect,
    Words(String),
}

/// One non-tool completion over the streaming client: core `answerOf`'s rules on its answer. Stops on the
/// caller's cancel and on the lease's end alike.
#[allow(clippy::too_many_arguments)]
async fn model_step(
    client: &reqwest::Client,
    url: &str,
    headers: Vec<(String, String)>,
    model: &str,
    messages: &JValue,
    max_tokens: u64,
    cancel: &Cancel,
    lost: &Cancel,
) -> Result<String, Stop> {
    let stop = Cancel::new();
    let follow_cancel = stop.follow(cancel);
    let follow_lost = stop.follow(lost);
    let body = stream::chat_body(model, max_tokens, messages, &[]);
    let got = stream::run(
        client,
        stream::Request {
            url,
            headers: &headers,
            body: &body,
            limits: stream::Limits {
                deadline: STEP_DEADLINE,
                idle: None,
            },
        },
        &stop,
        &mut |_| {},
    )
    .await;
    follow_cancel.abort();
    follow_lost.abort();
    match got {
        Ok(done) if done.finish_reason.as_deref() == Some("length") => Err(Stop::Words(
            "A research step was cut off by the output limit.".to_owned(),
        )),
        Ok(done) => Ok(done.content),
        Err(e) if e.is_redirect() => Err(Stop::Redirect),
        Err(stream::Error::Status { status, .. }) => {
            Err(Stop::Words(format!("The model provider answered {status}.")))
        }
        Err(stream::Error::Connect { refused_address, .. }) => Err(Stop::Connect {
            refused: refused_address,
        }),
        Err(stream::Error::Cancelled) if lost.is_cancelled() && !cancel.is_cancelled() => Err(Stop::Words(
            "The model engine was taken for maintenance during this step; try again when it is done.".to_owned(),
        )),
        Err(stream::Error::Cancelled) => Err(Stop::Words("Research was cancelled.".to_owned())),
        Err(stream::Error::Deadline | stream::Error::Idle) => {
            Err(Stop::Words("A research step did not finish in time.".to_owned()))
        }
        Err(stream::Error::TooLarge) => Err(Stop::Words("The model's answer was too large.".to_owned())),
        Err(e @ stream::Error::Provider { .. }) => Err(Stop::Words(
            e.classified()
                .map(|c| c.reason)
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| STREAM_FAILED.to_owned()),
        )),
        Err(_) => Err(Stop::Words(STREAM_FAILED.to_owned())),
    }
}

/// What a running job asks of the world.
struct JobTools {
    account: Account,
    project: String,
    project_model: Option<String>,
    job: String,
    run: String,
    seq: AtomicU64,
    stop: Cancel,
    failed: Mutex<Option<String>>,
}

impl JobTools {
    async fn event(&self, kind: &'static str, data: Value) {
        let n = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let (id, run) = (self.job.clone(), self.run.clone());
        let done = self
            .account
            .store(move |ctx, o| jobs::event(ctx, o, &id, &run, n, kind, &data))
            .await;
        // core `enqueue`: a journal write that fails stops the job.
        if let Err(f) = done {
            let mut failed = match self.failed.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            failed.get_or_insert(f.message);
            self.stop.cancel();
        }
    }

    fn failure(&self) -> Option<String> {
        match self.failed.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }

    async fn tool(&self, name: &str, args: Value, cancel: &Cancel) -> Result<String, Failure> {
        let body = json!({ "user": self.account.user, "name": name, "args": args });
        let research = self.account.research();
        match research
            .bridge_call("/internal/research/tool", &body, TOOL_DEADLINE, cancel)
            .await
        {
            Some((200, v)) => Ok(v
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()),
            Some((_, v)) => Err(Failure(
                v.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or(NO_SEARCH)
                    .to_owned(),
            )),
            None if cancel.is_cancelled() => Err(Failure("Research was cancelled.".to_owned())),
            None => Err(Failure(NO_SEARCH.to_owned())),
        }
    }
}

/// `text` cut to at most `max` bytes on a character boundary.
fn clip_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or("")
}

impl runner::Tools for JobTools {
    async fn project_retrieve(&self, question: &str, cancel: &Cancel) -> Result<Vec<Hit>, Failure> {
        let body =
            json!({ "user": self.account.user, "projectId": self.project, "question": question });
        let research = self.account.research();
        match research
            .bridge_call(
                "/internal/research/retrieve",
                &body,
                RETRIEVE_DEADLINE,
                cancel,
            )
            .await
        {
            Some((200, v)) => Ok(v
                .get("hits")
                .and_then(Value::as_array)
                .map(|hits| {
                    hits.iter()
                        .take(5)
                        .filter_map(|h| {
                            Some(Hit {
                                file: h.get("file")?.as_str()?.to_owned(),
                                text: h.get("text")?.as_str()?.to_owned(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default()),
            Some((_, v)) => Err(Failure(
                v.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("The project's sources could not be searched.")
                    .to_owned(),
            )),
            None if cancel.is_cancelled() => Err(Failure("Research was cancelled.".to_owned())),
            None => Err(Failure(
                "The project's sources could not be searched.".to_owned(),
            )),
        }
    }

    async fn search(&self, question: &str, cancel: &Cancel) -> Result<Search, Failure> {
        let text = self
            .tool(
                "tavily_search",
                json!({ "query": question, "max_results": 5 }),
                cancel,
            )
            .await?;
        if text.starts_with("ERROR") {
            return Ok(Search::Failed);
        }
        let found: Vec<Found> = runner::parse_search_results(&text);
        Ok(Search::Results(found))
    }

    async fn extract(&self, url: &str, cancel: &Cancel) -> Result<String, Failure> {
        let text = self
            .tool("tavily_extract", json!({ "urls": [url] }), cancel)
            .await?;
        if text.starts_with("ERROR") {
            return Err(Failure(text));
        }
        Ok(text)
    }

    async fn complete(
        &self,
        messages: JValue,
        max_tokens: u64,
        cancel: &Cancel,
    ) -> Result<String, Failure> {
        self.account
            .complete(self.project_model.as_deref(), &messages, max_tokens, cancel)
            .await
    }

    fn now(&self) -> i64 {
        crate::identity::now_ms()
    }

    async fn progress(&self, stage: &str) {
        if self.stop.is_cancelled() {
            return;
        }
        // The store refuses a stage over its limit, which would end the job over a progress line.
        let stage = clip_bytes(stage, MAX_STAGE_BYTES).to_owned();
        self.event("progress", json!({ "stage": stage })).await;
    }

    async fn checkpoint(&self, data: Value) {
        self.event("checkpoint.created", data).await;
    }
}

struct SavingGuard {
    research: Arc<Research>,
    id: String,
}

impl Drop for SavingGuard {
    fn drop(&mut self) {
        self.research.state().saving.remove(&self.id);
    }
}

fn file_env_fail(f: &files::Failure) -> Fail {
    fail(f.status, f.message.clone())
}

impl Account {
    /// Runs `f` with the report writer's environment, on the blocking pool.
    async fn with_files<T: Send + 'static>(
        &self,
        f: impl FnOnce(&files::Env<'_>) -> Result<T, files::Failure> + Send + 'static,
    ) -> Result<T, files::Failure> {
        let (app, authn, data_dir, user) = (
            Arc::clone(&self.app),
            self.authn.clone(),
            self.data_dir.clone(),
            self.user.clone(),
        );
        let done = tokio::task::spawn_blocking(move || {
            let Some(engine) = app.extraction.clone() else {
                return Err(files::Failure {
                    status: 503,
                    message: "Document extraction is unavailable; retry shortly.".to_owned(),
                    definitely_not_written: true,
                });
            };
            let ctx = server_projects::Ctx {
                data_dir: &data_dir,
                user_id: &user,
                switch: app.config.rust_projects,
                now_ms: crate::identity::now_ms(),
            };
            let limits = server_projects::extract::Limits::from_env();
            let catalogue = server_projects::lifecycle::Catalogue::read(&data_dir);
            let root = server_projects::lifecycle::project_root_folder();
            crate::source_job_routes::with_storage(&app, &authn, |storage| {
                f(&files::Env {
                    ctx,
                    storage,
                    engine: engine.as_ref(),
                    limits: &limits,
                    catalogue: catalogue.as_ref(),
                    project_root: &root,
                    lock_wait: server_projects::project_documents::FOREGROUND_LOCK_WAIT,
                })
            })
        })
        .await;
        done.unwrap_or_else(|_| {
            Err(files::Failure {
                status: 500,
                message: "The report could not be written.".to_owned(),
                definitely_not_written: false,
            })
        })
    }

    /// core `save(workspace, project, id, result, { retryPending })`: every external write has its intent
    /// durable first; an unknown outcome is never retried as an ordinary write.
    async fn save(
        &self,
        project: &str,
        id: &str,
        run: &str,
        result: &Value,
        retry_pending: bool,
    ) -> Result<(), Fail> {
        {
            let mut state = self.research().state();
            if !state.saving.insert(id.to_owned()) {
                return Err(fail(409, "This report is already being saved."));
            }
        }
        let _guard = SavingGuard {
            research: Arc::clone(self.research()),
            id: id.to_owned(),
        };
        let Some(mut job) = self.read_job(project, id).await? else {
            return Err(fail(410, "The project was deleted while this research was running; the report could not be saved."));
        };
        let status = job
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if !["completed", "failed", "cancelled", "interrupted"].contains(&status.as_str()) {
            let (i, r, res) = (id.to_owned(), run.to_owned(), result.clone());
            self.store(move |ctx, o| jobs::report(ctx, o, &i, &r, &res))
                .await?;
        }
        let Some(current) = self.project(project).await? else {
            return Err(fail(410, "The project was deleted while this research was running; the report could not be saved."));
        };
        let listed: Vec<JValue> = match current.get("files") {
            JValue::Arr(a) => a.clone(),
            _ => Vec::new(),
        };
        if let Some(pending) = job.get("pendingFile").filter(|p| p.is_object()).cloned() {
            let name = pending
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if !report::indexed_pending(&listed, &pending) {
                let file = if retry_pending {
                    report::pending_report_file(result, &pending)
                } else {
                    None
                };
                let Some(file) = file else {
                    return Err(fail(409, format!("The save of \"{name}\" has an unknown outcome. Sync this project's Sources and review that file before trying again.")));
                };
                let (p, n) = (project.to_owned(), name.clone());
                let target = self
                    .with_files(move |env| files::target(env, &p, &n))
                    .await
                    .map_err(|f| file_env_fail(&f))?;
                if Some(target.as_str()) != pending.get("target").and_then(Value::as_str) {
                    return Err(fail(409, format!("The storage target for \"{name}\" changed. Review the original storage location before retrying.")));
                }
                let (p, n, text) = (project.to_owned(), name.clone(), file.text.clone());
                let wrote = self
                    .with_files(move |env| files::write(env, &p, &n, &text, &target))
                    .await;
                if wrote.is_err() {
                    return Err(fail(500, format!("The save of \"{name}\" remains uncertain. Sync this project's Sources and review that file before trying again.")));
                }
            }
            let (p, i, r, n) = (
                project.to_owned(),
                id.to_owned(),
                run.to_owned(),
                name.clone(),
            );
            let hash = pending
                .get("hash")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let bytes = pending.get("bytes").and_then(Value::as_u64).unwrap_or(0);
            self.store(move |ctx, o| {
                jobs::artifact(
                    ctx,
                    o,
                    &p,
                    &i,
                    &r,
                    jobs::Artifact {
                        name: &n,
                        hash: &hash,
                        bytes,
                    },
                )
            })
            .await?;
            job = self
                .read_job(project, id)
                .await?
                .ok_or_else(|| fail(404, "No such research job."))?;
        }
        let names: HashSet<String> = listed
            .iter()
            .filter_map(|f| f.get("name").as_str())
            .map(|n| n.rsplit('/').next().unwrap_or(n).to_owned())
            .collect();
        let existing = job
            .get("artifacts")
            .and_then(Value::as_array)
            .and_then(|a| {
                a.iter()
                    .filter_map(|x| x.get("name").and_then(Value::as_str))
                    .find(|n| n.ends_with(".md"))
                    .map(|n| n.strip_suffix(".md").unwrap_or(n).to_owned())
            });
        let pair = match existing {
            Some(base) => report::report_files_named(result, &base),
            None => report::report_files(result, crate::identity::now_ms(), &|n| names.contains(n)),
        };
        for file in pair {
            job = self
                .read_job(project, id)
                .await?
                .ok_or_else(|| fail(404, "No such research job."))?;
            let saved = job
                .get("artifacts")
                .and_then(Value::as_array)
                .is_some_and(|a| {
                    a.iter()
                        .any(|x| x.get("name").and_then(Value::as_str) == Some(file.name.as_str()))
                });
            if saved {
                continue;
            }
            let (hash, bytes) = (file.hash(), file.bytes());
            let (p, n) = (project.to_owned(), file.name.clone());
            let target = self
                .with_files(move |env| files::target(env, &p, &n))
                .await
                .map_err(|f| file_env_fail(&f))?;
            let (p, i, r, n, h, t) = (
                project.to_owned(),
                id.to_owned(),
                run.to_owned(),
                file.name.clone(),
                hash.clone(),
                target.clone(),
            );
            self.store(move |ctx, o| {
                jobs::prepare_file(
                    ctx,
                    o,
                    &p,
                    &i,
                    &r,
                    jobs::FileIntent {
                        name: &n,
                        hash: &h,
                        bytes,
                        target: &t,
                    },
                )
            })
            .await?;
            let (p, n, text) = (project.to_owned(), file.name.clone(), file.text.clone());
            let wrote = self
                .with_files(move |env| files::write(env, &p, &n, &text, &target))
                .await;
            if let Err(f) = wrote {
                if f.definitely_not_written {
                    let (p, i, r, n, h) = (
                        project.to_owned(),
                        id.to_owned(),
                        run.to_owned(),
                        file.name.clone(),
                        hash.clone(),
                    );
                    self.store(move |ctx, o| jobs::clear_file(ctx, o, &p, &i, &r, &n, &h))
                        .await?;
                    return Err(fail(f.status, f.message));
                }
                return Err(fail(f.status.max(500), format!("The save of \"{}\" has an unknown outcome. Sync this project's Sources and review that file before trying again.", file.name)));
            }
            let (p, i, r, n, h) = (
                project.to_owned(),
                id.to_owned(),
                run.to_owned(),
                file.name.clone(),
                hash.clone(),
            );
            self.store(move |ctx, o| {
                jobs::artifact(
                    ctx,
                    o,
                    &p,
                    &i,
                    &r,
                    jobs::Artifact {
                        name: &n,
                        hash: &h,
                        bytes,
                    },
                )
            })
            .await?;
        }
        Ok(())
    }

    /// The job's whole life, on its own task. core `jobs.run`, with the runner and the save inside it.
    #[allow(clippy::too_many_arguments)]
    async fn run_job(
        self,
        project: String,
        project_model: Option<String>,
        id: String,
        run: String,
        request: String,
        question: String,
        questions: Vec<String>,
        cancel: Cancel,
        cancel_attempted: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let tools = JobTools {
            account: self.clone(),
            project: project.clone(),
            project_model,
            job: id.clone(),
            run: run.clone(),
            seq: AtomicU64::new(0),
            stop: cancel.clone(),
            failed: Mutex::new(None),
        };
        let options = Options::service();
        let mut result: Option<Value> = None;
        let mut error: Option<String> = None;
        match runner::run(&question, &questions, &tools, &cancel, &options).await {
            Ok(done) => {
                if !cancel.is_cancelled() {
                    runner::Tools::progress(&tools, "Saving the report").await;
                    if tools.failure().is_none() {
                        if let Err(e) = self.save(&project, &id, &run, &done, false).await {
                            error = Some(e.message);
                        }
                    }
                }
                result = Some(done);
            }
            Err(stopped) => error = Some(stopped.message),
        }
        if let Some(journal) = tools.failure() {
            error.get_or_insert(journal);
        }
        let attempted = cancel_attempted.load(Ordering::SeqCst);
        if attempted && error.is_none() {
            error = Some(
                "Research cancellation was not confirmed; review the job before retrying."
                    .to_owned(),
            );
        }
        let ok = error.is_none();
        let error: Option<String> = error.map(|e| e.chars().take(500).collect());
        let durable = result.clone().unwrap_or(Value::Null);
        let keep_report = report::has_report(&durable);
        let (i, r, d, e) = (id.clone(), run.clone(), durable.clone(), error.clone());
        let finished = self
            .store(move |ctx, o| {
                if keep_report {
                    jobs::report(ctx, o, &i, &r, &d)?;
                }
                jobs::finish(ctx, o, &i, &r, ok, &d, e.as_deref())
            })
            .await;
        if finished.is_err() {
            // A lost terminal write cannot keep this project's only active slot for good.
            let abandon = Abandon {
                user: self.user.clone(),
                request,
                report: keep_report.then(|| (id.clone(), run.clone(), durable)),
            };
            self.queue_abandon(abandon);
        }
        self.research().state().live.remove(&id);
    }

    fn queue_abandon(&self, abandon: Abandon) {
        let research = Arc::clone(self.research());
        let start = {
            let mut state = research.state();
            state.abandons.push(abandon);
            !std::mem::replace(&mut state.draining, true)
        };
        if start {
            let account = self.clone();
            tokio::spawn(async move {
                loop {
                    account.drain_abandons().await;
                    {
                        let mut state = account.research().state();
                        if state.abandons.is_empty() {
                            state.draining = false;
                            return;
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            });
        }
    }

    /// core `drainAbandons`: an entry goes once the store said it holds, or that it is no longer ours.
    async fn drain_abandons(&self) {
        let entries: Vec<Abandon> = {
            let state = self.research().state();
            state
                .abandons
                .iter()
                .filter(|a| a.user == self.user)
                .cloned()
                .collect()
        };
        for entry in entries {
            let again = entry.clone();
            let done = self
                .store(move |ctx, o| {
                    if let Some((id, run, result)) = &again.report {
                        match jobs::report(ctx, o, id, run, result) {
                            Ok(()) | Err(jobs::Error::Stale) => {}
                            Err(e) => return Err(e),
                        }
                    }
                    match jobs::abandon(ctx, o, &again.request) {
                        Ok(()) | Err(jobs::Error::Stale | jobs::Error::Gone) => Ok(()),
                        Err(e) => Err(e),
                    }
                })
                .await;
            if done.is_ok() {
                self.research()
                    .state()
                    .abandons
                    .retain(|a| a.request != entry.request);
            }
        }
    }

    async fn start(
        &self,
        project: &str,
        project_model: Option<String>,
        body: &JValue,
    ) -> Result<JValue, Fail> {
        let question = js_string(body.get("question"));
        let question = context_fit::js_trim(&question).to_owned();
        if question.is_empty() {
            return Err(fail(400, "Write a research question first."));
        }
        if wtf16::len(&question) > 2000 {
            return Err(fail(
                400,
                "Keep the research question under 2000 characters.",
            ));
        }
        let mode = match body.get("plan").as_str() {
            Some(m @ ("proposed" | "edited")) => m.to_owned(),
            _ => "skipped".to_owned(),
        };
        let planned = if mode == "skipped" {
            None
        } else {
            Some(
                planning::sanitize_plan(body.get("subQuestions"), 1)
                    .map_err(|r| fail(400, r.message))?,
            )
        };
        self.recovered().await?;
        self.drain_abandons().await;
        if self
            .research()
            .state()
            .abandons
            .iter()
            .any(|a| a.user == self.user)
        {
            return Err(fail(
                503,
                "The research job store is reconciling an unconfirmed job; retry shortly.",
            ));
        }
        // The store's `start` is the atomic admission (one active job per project); this is its words.
        let questions = planned
            .filter(|q| !q.is_empty())
            .unwrap_or_else(|| vec![question.clone()]);
        let request = server_projects::source_jobs::new_id();
        let (p, r, q, m, list) = (
            project.to_owned(),
            request.clone(),
            question.clone(),
            mode.clone(),
            json!(questions.clone()),
        );
        let (id, run) = self
            .store(move |ctx, o| jobs::start(ctx, o, &p, &r, &q, &m, &list))
            .await?;
        let cancel = Cancel::new();
        let cancel_attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.research().state().live.insert(
            id.clone(),
            Live {
                cancel: cancel.clone(),
                run: run.clone(),
                cancel_attempted: Arc::clone(&cancel_attempted),
            },
        );
        // Everything from here on is the job's own task: a client that leaves cannot strand it.
        tokio::spawn(self.clone().run_job(
            project.to_owned(),
            project_model,
            id.clone(),
            run,
            request,
            question,
            questions,
            cancel,
            cancel_attempted,
        ));
        let job = self.owned(project, &id).await?;
        self.public_view(&job, project).await
    }

    async fn cancel(&self, project: &str, id: &str) -> Result<JValue, Fail> {
        self.owned(project, id).await?;
        let run = self
            .research()
            .state()
            .live
            .get(id)
            .map_or(String::new(), |l| l.run.clone());
        let (p, i, r) = (project.to_owned(), id.to_owned(), run);
        let committed = self
            .store(move |ctx, o| jobs::cancel(ctx, o, &p, &i, &r))
            .await;
        // A committed intent may have lost its reply: stop the job whatever the store said. The store
        // decides the terminal status.
        if let Some(live) = self.research().state().live.get(id) {
            live.cancel_attempted.store(true, Ordering::SeqCst);
            live.cancel.cancel();
        }
        let job = committed?;
        self.public_view(&job, project).await
    }

    async fn save_partial(&self, project: &str, id: &str) -> Result<JValue, Fail> {
        let job = self.owned(project, id).await?;
        if !report::can_save(&job) {
            let has_artifacts = job
                .get("artifacts")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty());
            return Err(fail(
                409,
                if has_artifacts {
                    "This report is already saved."
                } else {
                    "Only a cancelled job with finished sections, or a report that failed to save, can be saved."
                },
            ));
        }
        let result = job.get("result").cloned().unwrap_or(Value::Null);
        // The run a late artifact is recorded under is the job's own.
        let run = job_run(self, id).await;
        self.save(project, id, &run, &result, true).await?;
        let job = self.owned(project, id).await?;
        self.public_view(&job, project).await
    }
}

/// A finished job's run id. The store checks it against the job's first row for a live job; for a late
/// artifact the job's own row supplies it (see research_jobs `save_allowed`).
async fn job_run(account: &Account, id: &str) -> String {
    account
        .research()
        .state()
        .live
        .get(id)
        .map_or(String::new(), |l| l.run.clone())
}

fn js_string(v: &JValue) -> String {
    if !v.truthy() {
        return String::new();
    }
    js_json::to_js_string(v).unwrap_or_default()
}

fn project_model_of(project: &JValue) -> Option<String> {
    project
        .get("model")
        .as_str()
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
}

fn feature_on(app: &App, env: Option<bool>) -> bool {
    let read = || -> Option<Option<String>> {
        if env.is_some() {
            return Some(None);
        }
        let writer = app.writes.writer().ok()?;
        writer
            .read(|r| {
                r.row(
                    "SELECT CASE WHEN typeof(value)='text' AND value IN ('true','false') THEN value ELSE NULL END FROM settings WHERE key=?1",
                    [FEATURE_SETTING],
                    |row| row.get::<_, Option<String>>(0),
                )
            })
            .ok()
            .map(Option::flatten)
    };
    match read() {
        // Unreadable: off, as an unknown fact never enables a flag.
        None => false,
        Some(stored) => {
            server_features::evaluate(
                server_features::Feature::DeepResearch,
                env,
                stored.as_deref(),
                true,
            )
            .enabled
        }
    }
}

/// One research request, after the session gate. Runs on the request's task.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    app: &Arc<App>,
    method: Method,
    body: Body,
    authn: server_auth::Identity,
    data_dir: PathBuf,
    start: String,
    regate: Regate,
    raw_project: String,
    target: Shape,
) -> Response<Body> {
    // routes/research.cjs: the flag (absent unless on), the administrator, the project.
    let on = {
        let a = Arc::clone(app);
        let env = app.config.research.settings.flag_env;
        tokio::task::spawn_blocking(move || feature_on(&a, env))
            .await
            .unwrap_or(false)
    };
    if !on {
        return reply::error(
            StatusCode::NOT_FOUND,
            "Deep research is not enabled on this server.",
            true,
        );
    }
    // `authn.user.role`: a device token acts as a member.
    if authn.user.role != server_auth::Role::Admin {
        return reply::error(StatusCode::FORBIDDEN, "Administrator required", true);
    }
    let Some(project_id) = server_projects::js::decode_uri_component(&raw_project) else {
        return reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true);
    };
    let account = Account {
        app: Arc::clone(app),
        user: authn.user.id.as_str().to_owned(),
        authn,
        data_dir,
        start,
    };
    if account.start.is_empty() {
        return axum::response::IntoResponse::into_response(Rejection::Unavailable);
    }
    let project = match account.project(&project_id).await {
        Ok(Some(p)) => p,
        Ok(None) => return reply::error(StatusCode::NOT_FOUND, "project not found", true),
        Err(f) => return error_answer(&f),
    };
    let dir = account.data_dir.clone();
    let user = account.user.clone();
    let regated = tokio::task::spawn_blocking(move || regate.again(&dir, &user)).await;
    match regated {
        Ok(Ok(())) => {}
        Ok(Err(refused)) => return *refused,
        Err(_) => return axum::response::IntoResponse::into_response(Rejection::Unavailable),
    }
    let not_allowed = || reply::error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", true);
    let is_post = method == Method::POST;
    let outcome: Result<Response<Body>, Fail> = async {
        match target {
            Shape::Other => Ok(reply::error(StatusCode::NOT_FOUND, "Not found", true)),
            Shape::PlanAction => Ok(not_allowed()),
            Shape::Plan if !is_post => Ok(not_allowed()),
            Shape::Plan => {
                if let Some(reason) = account.unavailable().await {
                    return Err(fail(409, reason));
                }
                let parsed = read_json(body).await?;
                let messages = planning::plan_messages(
                    &js_string(parsed.get("question")),
                    Options::service().window_tokens,
                )
                .map_err(|r| fail(r.status, r.message))?;
                let model = project_model_of(&project);
                // A task of its own, so a dropped request cancels the engine's work and the task still ends
                // its own lease; the guard is what carries the client's departure across.
                let cancel = Cancel::new();
                let _departure = CancelOnDrop(cancel.clone());
                let (acc, c) = (account.clone(), cancel.clone());
                let planned = tokio::spawn(async move {
                    acc.complete(model.as_deref(), &messages, planning::REPLY_TOKENS, &c)
                        .await
                })
                .await
                .map_err(|_| fail(500, "Research request failed"))?;
                let text = planned.map_err(|Failure(m)| fail(500, m))?;
                let questions =
                    planning::parse_plan(&text).map_err(|r| fail(r.status, r.message))?;
                Ok(answer(
                    200,
                    &JValue::obj([(
                        "subQuestions",
                        JValue::Arr(questions.into_iter().map(JValue::Str).collect()),
                    )]),
                ))
            }
            Shape::List if method == Method::GET => {
                account.recovered().await?;
                let reason = account.unavailable().await;
                let (p, u) = (project_id.clone(), account.user.clone());
                let list = account.store(move |ctx, _| jobs::list(ctx, &u, &p)).await?;
                let mut views = Vec::new();
                for job in list.iter().take(20) {
                    views.push(account.public_view(job, &project_id).await?);
                }
                let budget = Options::service();
                Ok(answer(
                    200,
                    &JValue::obj([
                        (
                            "budget",
                            JValue::obj([
                                ("maxWebCalls", JValue::Num(budget.max_web_calls as f64)),
                                ("maxMs", JValue::Num(budget.max_ms as f64)),
                                (
                                    "resultsPerQuery",
                                    JValue::Num(budget.results_per_query as f64),
                                ),
                            ]),
                        ),
                        ("available", JValue::Bool(reason.is_none())),
                        ("reason", reason.map_or(JValue::Null, JValue::Str)),
                        ("jobs", JValue::Arr(views)),
                    ]),
                ))
            }
            Shape::List if !is_post => Ok(not_allowed()),
            Shape::List => {
                if let Some(reason) = account.unavailable().await {
                    return Err(fail(409, reason));
                }
                let parsed = read_json(body).await?;
                let view = account
                    .start(&project_id, project_model_of(&project), &parsed)
                    .await?;
                Ok(answer(202, &view))
            }
            Shape::Job(id) => {
                if method != Method::GET {
                    return Ok(not_allowed());
                }
                account.recovered().await?;
                let job = account.owned(&project_id, &id).await?;
                Ok(answer(200, &account.public_view(&job, &project_id).await?))
            }
            Shape::Cancel(_) | Shape::Save(_) if !is_post => Ok(not_allowed()),
            Shape::Cancel(id) => {
                account.recovered().await?;
                // Spawned: once asked, a cancel completes whether or not the client is still there.
                let (acc, p) = (account.clone(), project_id.clone());
                let view = tokio::spawn(async move { acc.cancel(&p, &id).await })
                    .await
                    .map_err(|_| fail(500, "Research request failed"))??;
                Ok(answer(200, &view))
            }
            Shape::Save(id) => {
                account.recovered().await?;
                let (acc, p) = (account.clone(), project_id.clone());
                let view = tokio::spawn(async move { acc.save_partial(&p, &id).await })
                    .await
                    .map_err(|_| fail(500, "Research request failed"))??;
                Ok(answer(200, &view))
            }
        }
    }
    .await;
    outcome.unwrap_or_else(|f| error_answer(&f))
}

struct CancelOnDrop(Cancel);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// core `readJson`: at most 1 MiB, an empty body is `{}`.
async fn read_json(body: Body) -> Result<JValue, Fail> {
    let read = crate::account_routes::read_body(body, BODY_LIMIT)
        .await
        .map_err(|()| fail(400, "invalid JSON"))?;
    if read.over {
        return Err(fail(413, "Request exceeds size limit"));
    }
    if read.bytes.is_empty() {
        return Ok(JValue::Obj(Vec::new()));
    }
    let text = String::from_utf8_lossy(&read.bytes);
    wtf16::parse(&text).map_err(|_| fail(400, "invalid JSON"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_path_shapes_as_core_matched_them() {
        let id = "01234567-89ab-cdef-0123-456789abcdef";
        let p = |s: &str| parse(&format!("/api/projects/p%201/research{s}")).map(|x| x.1);
        assert_eq!(p(""), Some(Shape::List));
        assert_eq!(p("/plan"), Some(Shape::Plan));
        assert_eq!(p("/plan/save"), Some(Shape::PlanAction));
        assert_eq!(p(&format!("/{id}")), Some(Shape::Job(id.into())));
        assert_eq!(p(&format!("/{id}/cancel")), Some(Shape::Cancel(id.into())));
        assert_eq!(p(&format!("/{id}/save")), Some(Shape::Save(id.into())));
        assert_eq!(p("/x1"), Some(Shape::Other));
        // core took this as the list and the start.
        assert_eq!(p("/cancel"), Some(Shape::Other));
        assert_eq!(p(&format!("/{id}/nope")), Some(Shape::Other));
        assert_eq!(parse("/api/projects/p/researches"), None);
        assert_eq!(parse("/api/projects//research"), None);
        assert_eq!(parse("/api/other/p/research"), None);
    }

    #[test]
    fn clipping_keeps_characters_whole() {
        assert_eq!(clip_bytes("h\u{e9}llo", 2), "h");
        assert_eq!(clip_bytes("abc", 9), "abc");
    }
}
