//! The front's source jobs (sbstndalton/noevia#2191 round 6, `rust-source-jobs`; the operation itself the
//! front's since round 8, `rust-project-source-work`): the background upload and sync starts and their
//! poll, with this process as the one owner of the source-job store (crates/server-projects
//! `source_work`).
//!
//! A start: the project (404), for a sync the storage hold of an automatic refresh (#1429), and the body
//! (413, 400 `invalid JSON`) are checked, as core's worker checked them before it accepted; then the job is
//! persisted created+started within the two-active cap (429), the client gets core's 202 `{ poll }`, and the
//! operation runs in this process (crates/server-projects `upload`, `sync`): every progress step and the
//! terminal row are written by a task of this process, however long it takes and whether or not the client is
//! still there. From the job's first write on, everything runs in a task of its own, not in the request's
//! future: a client that leaves at any point cannot strand the job.
//!
//! A refusal this process makes before the job exists is a plain answer and starts nothing. A job whose
//! operation cannot be started (the extraction client is missing) is interrupted, not tombstoned, so any
//! number of refusals leaves the store as it was.
use crate::reply;
use crate::App;
use axum::body::Body;
use axum::http::{Response, StatusCode};
use server_projects::source_work::{self as work, Outcome, Owner, Started};
use server_projects::Ctx;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// This process's owner, per container start.
static OWNER: Mutex<Option<Owner>> = Mutex::new(None);

pub fn owner(start: &str) -> Owner {
    let mut slot = match OWNER.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    match slot.as_ref() {
        Some(o) if o.start == start => o.clone(),
        _ => {
            let o = Owner::this_process(start);
            *slot = Some(o.clone());
            o
        }
    }
}

pub fn respond(r: server_projects::Reply) -> Response<Body> {
    let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    match r.body {
        server_projects::Body::Json(text) => reply::json_text(status, text, true),
        server_projects::Body::Bytes { headers, bytes } => {
            let mut res = Response::new(Body::from(bytes));
            *res.status_mut() = status;
            let h = res.headers_mut();
            reply::security_headers(h, true);
            for (k, v) in headers {
                if let (Ok(k), Ok(v)) = (
                    axum::http::HeaderName::from_bytes(k.as_bytes()),
                    axum::http::HeaderValue::from_str(&v),
                ) {
                    h.insert(k, v);
                }
            }
            res
        }
    }
}

/// Who a request acts for.
#[derive(Clone)]
pub struct Account {
    pub data_dir: PathBuf,
    pub user: String,
    pub start: String,
    pub now_ms: i64,
}

impl Account {
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            data_dir: &self.data_dir,
            user_id: &self.user,
            switch: server_store::RustProjects::owned(),
            now_ms: self.now_ms,
        }
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

/// GET /api/projects/{id}/source-jobs/{jobId}.
pub async fn poll(account: Account, project: String, job: String) -> Response<Body> {
    let owner = owner(&account.start);
    match blocking(move || work::poll(account.ctx(), &owner, &project, &job)).await {
        Some(r) => respond(r),
        None => reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
    }
}

/// Which source operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Upload,
    Sync,
}

/// One source operation's inputs.
#[derive(Clone)]
pub struct Work {
    pub kind: Kind,
    /// The project id as the path carries it (undecoded).
    pub project: String,
    pub body: Vec<u8>,
    pub over: bool,
    /// `X-Cowork-Storage-Retry: 1`: an explicit press always runs.
    pub retry: bool,
}

/// The storage the operation uses, opened on the blocking pool.
fn with_storage<T>(
    app: &Arc<App>,
    authn: &server_auth::Identity,
    f: impl FnOnce(&server_projects::source_storage::Storage<'_>) -> T,
) -> T {
    #[cfg(feature = "rust-storage-browse")]
    {
        let opened = crate::source_storage_io::open(app, authn);
        f(&opened.storage())
    }
    #[cfg(not(feature = "rust-storage-browse"))]
    {
        let _ = (app, authn);
        f(&server_projects::source_storage::Storage::None)
    }
}

/// The hold of an automatic sync (before a job exists), or none.
pub fn hold(
    app: &Arc<App>,
    authn: &server_auth::Identity,
    account: &Account,
    work: &Work,
) -> Option<server_projects::Reply> {
    if work.kind != Kind::Sync {
        return None;
    }
    with_storage(app, authn, |storage| {
        server_projects::sync::automatic_hold(account.ctx(), storage, work.retry)
    })
}

/// Runs one operation to its answer (blocking): the synchronous routes and every job. `progress` is told
/// each stage; `check_hold` is false for a job, whose start already took the hold's probe.
pub fn execute(
    app: &Arc<App>,
    authn: &server_auth::Identity,
    account: &Account,
    work: &Work,
    check_hold: bool,
    progress: &dyn Fn(&str),
) -> server_projects::Reply {
    use server_projects::{lifecycle, source_ops, sync, upload};
    let Some(engine) = app.extraction.clone() else {
        return server_projects::Reply::error(
            503,
            "Document extraction is unavailable; retry shortly.",
        );
    };
    let ctx = account.ctx();
    let limits = server_projects::extract::Limits::from_env();
    let catalogue = lifecycle::Catalogue::read(&account.data_dir);
    let root = lifecycle::project_root_folder();
    let user_dir = server_projects::store::user_dir(&account.data_dir, &account.user).ok();
    // The operation is listed for core while it runs (chat waits for a file just sent, #1441; the
    // source-text collector leaves a sync's files alone, #1815).
    let id = server_projects::js::decode_uri_component(&work.project).unwrap_or_default();
    let (kind, name) = match work.kind {
        Kind::Sync => ("sync", String::new()),
        Kind::Upload => {
            let parsed = js_json::wtf16::parse(&String::from_utf8_lossy(&work.body)).ok();
            let name = parsed
                .as_ref()
                .map(|b| b.get("name"))
                .filter(|n| n.truthy())
                .map(|n| js_json::to_js_string(n).unwrap_or_default())
                .and_then(|n| n.rsplit('/').next().map(str::to_owned))
                .unwrap_or_default();
            let import = parsed
                .as_ref()
                .is_some_and(|b| !matches!(b.get("relativePath"), js_json::JValue::Undefined));
            (if import { "import" } else { "upload" }, name)
        }
    };
    let op = user_dir
        .as_deref()
        .map(|d| source_ops::begin(d, &id, kind, &name, account.now_ms));
    let reply = with_storage(app, authn, |storage| match work.kind {
        Kind::Upload => upload::post(
            &upload::Env {
                ctx,
                storage,
                engine: engine.as_ref(),
                limits: &limits,
                catalogue: catalogue.as_ref(),
                project_root: &root,
                progress,
            },
            &work.project,
            &work.body,
            work.over,
        ),
        Kind::Sync => {
            let Some(id) = server_projects::js::decode_uri_component(&work.project) else {
                return server_projects::Reply::internal();
            };
            match server_projects::originals::project(ctx, &id) {
                Err(r) => return r,
                Ok((_, None)) => return server_projects::Reply::error(404, "no such project"),
                Ok(_) => {}
            }
            if check_hold {
                if let Some(held) = sync::automatic_hold(ctx, storage, work.retry) {
                    return held;
                }
            }
            sync::run(&sync::SyncEnv {
                ctx,
                project: &id,
                storage,
                engine: engine.as_ref(),
                limits: &limits,
                catalogue: catalogue.as_ref(),
                project_root: &root,
                progress,
            })
        }
    });
    if let Some(op) = op {
        op.end(reply.status < 400, account.now_ms);
    }
    reply
}

/// The synchronous form: the operation, run on the blocking pool, answered as it ends.
pub async fn run_sync(
    app: &Arc<App>,
    authn: server_auth::Identity,
    account: Account,
    work: Work,
) -> Response<Body> {
    let app = Arc::clone(app);
    match blocking(move || execute(&app, &authn, &account, &work, true, &|_| {})).await {
        Some(r) => respond(r),
        None => reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
    }
}

/// POST /api/projects/{id}/upload|sources/sync with `background=1`. `regate` re-checks the session right
/// before the job is written (the body was read for as long as the client took).
///
/// The request's own future only waits for the answer: the work, from the job's first write on, runs in a
/// task of its own, so a client that disconnects (dropping this future) at any point leaves the job to be
/// run or ended there.
pub async fn start(
    app: &Arc<App>,
    authn: server_auth::Identity,
    account: Account,
    work: Work,
    regate: impl FnOnce() -> Result<(), Box<Response<Body>>> + Send + 'static,
) -> Response<Body> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    let app = Arc::clone(app);
    tokio::spawn(async move {
        operate(app, authn, account, work, regate, reply).await;
    });
    answer
        .await
        .unwrap_or_else(|_| reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true))
}

/// Whether a store answer ends the matter: written; or a newer owner took over; or the account or
/// the job is gone; or the store will not take it (retrying cannot change that).
fn settled(r: &Option<Result<(), server_projects::source_jobs::Error>>) -> bool {
    use server_projects::source_jobs::Error;
    matches!(
        r,
        Some(Ok(()) | Err(Error::Stale | Error::Gone | Error::NotFound | Error::BadRequest))
    )
}

async fn operate(
    app: Arc<App>,
    authn: server_auth::Identity,
    account: Account,
    work: Work,
    regate: impl FnOnce() -> Result<(), Box<Response<Body>>> + Send + 'static,
    reply: tokio::sync::oneshot::Sender<Response<Body>>,
) {
    let owner = owner(&account.start);
    let begun = {
        let (account, owner, app, authn, work) = (
            account.clone(),
            owner.clone(),
            Arc::clone(&app),
            authn.clone(),
            work.clone(),
        );
        blocking(move || {
            regate()?;
            // core's worker, before it accepted: the hold of an automatic sync, then the body.
            let check = || -> Result<(), server_projects::Reply> {
                if let Some(held) = hold(&app, &authn, &account, &work) {
                    return Err(held);
                }
                if work.over {
                    return Err(server_projects::Reply::error(
                        413,
                        "Request exceeds size limit",
                    ));
                }
                let text = String::from_utf8_lossy(&work.body);
                if !text.is_empty() && js_json::wtf16::parse(&text).is_err() {
                    return Err(server_projects::Reply::error(400, "invalid JSON"));
                }
                Ok(())
            };
            work::begin_with(account.ctx(), &owner, &work.project, check)
                .map_err(|r| Box::new(respond(r)))
        })
        .await
    };
    let job = match begun {
        Some(Ok(job)) => job,
        Some(Err(r)) => {
            let _ = reply.send(*r);
            return;
        }
        None => {
            let _ = reply.send(reply::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error",
                true,
            ));
            return;
        }
    };
    let _ = reply.send(respond(work::accepted(&job)));
    run(app, authn, account, owner, job, work).await;
}

/// The job after its start: every progress step, then the terminal row (retried while the store cannot be
/// written; interrupted at last, so the slot is never held for good).
async fn run(
    app: Arc<App>,
    authn: server_auth::Identity,
    account: Account,
    owner: Owner,
    job: Started,
    work: Work,
) {
    let tracker = Arc::new(Mutex::new(work::Tracker::default()));
    let answer = {
        let (account, owner, job, tracker) = (
            account.clone(),
            owner.clone(),
            job.clone(),
            Arc::clone(&tracker),
        );
        blocking(move || {
            let progress = |stage: &str| {
                let n = match tracker.lock() {
                    Ok(mut t) => t.advance(),
                    Err(_) => return,
                };
                let ok = work::progress(account.ctx(), &owner, &job, stage, n).is_ok();
                if let Ok(mut t) = tracker.lock() {
                    t.progress_result(ok);
                }
            };
            execute(&app, &authn, &account, &work, false, &progress)
        })
        .await
    };
    let failed = match tracker.lock() {
        Ok(t) => t.failed(),
        Err(_) => true,
    };
    let outcome = match answer {
        Some(r) => work::outcome(&r, failed),
        // The task panicked: the operation threw.
        None => Outcome::Failed(500, None),
    };
    finish(account, owner, job, outcome).await;
}

async fn finish(account: Account, owner: Owner, job: Started, outcome: Outcome) {
    finish_with(account, owner, job, outcome, Duration::from_secs(5)).await;
}

/// The terminal row, `pause` apart between attempts.
async fn finish_with(
    account: Account,
    owner: Owner,
    job: Started,
    mut outcome: Outcome,
    pause: Duration,
) {
    use server_projects::source_jobs::Error;
    let mut attempt = 0;
    while attempt < 12 {
        if attempt > 0 {
            tokio::time::sleep(pause).await;
        }
        attempt += 1;
        let (a, o, j, out) = (account.clone(), owner.clone(), job.clone(), outcome.clone());
        let written = blocking(move || work::finish(a.ctx(), &o, &j, &out)).await;
        // A result the store will not keep (over its 32 MiB) is a failed job, not an unfinished
        // one: the same job ends with a 500 and the default message.
        if matches!(written, Some(Err(Error::BadRequest))) && matches!(outcome, Outcome::Done(_)) {
            outcome = Outcome::Failed(500, None);
            attempt -= 1;
            continue;
        }
        // Written; or a newer owner interrupted it; or the account is gone.
        if settled(&written) {
            return;
        }
    }
    eprintln!("noevia-server: a source job's result could not be written; it is interrupted");
    let ending = Ending {
        account,
        owner,
        job,
    };
    ending.once().await;
}

/// A job to end without a result.
struct Ending {
    account: Account,
    owner: Owner,
    job: Started,
}

impl Ending {
    async fn once(&self) -> bool {
        let (a, o, j) = (self.account.clone(), self.owner.clone(), self.job.clone());
        settled(&blocking(move || work::interrupt(a.ctx(), &o, &j)).await)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn account(dir: &std::path::Path) -> Account {
        let user = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let udir = dir.join("users").join(user);
        std::fs::create_dir_all(&udir).unwrap();
        std::fs::write(udir.join("projects.json"), r#"{"projects":[{"id":"a"}]}"#).unwrap();
        Account {
            data_dir: dir.to_path_buf(),
            user: user.into(),
            start: "0123456789abcdef0123456789abcdef".into(),
            now_ms: 1,
        }
    }

    fn polled(account: &Account, owner: &Owner, job: &Started) -> String {
        match work::poll(account.ctx(), owner, "a", &job.id).body {
            server_projects::Body::Json(text) => text,
            server_projects::Body::Bytes { .. } => panic!("not JSON"),
        }
    }

    #[tokio::test]
    async fn a_result_over_the_stores_cap_fails_the_job_instead_of_leaving_it_active() {
        let dir = tempfile::tempdir().unwrap();
        let account = account(dir.path());
        let owner = owner(&account.start);
        let job = work::begin(account.ctx(), &owner, "a", false).unwrap();
        let huge = Outcome::Done(serde_json::json!({ "body": "x".repeat(33 * 1024 * 1024) }));
        finish_with(
            account.clone(),
            owner.clone(),
            job.clone(),
            huge,
            Duration::ZERO,
        )
        .await;
        assert_eq!(
            polled(&account, &owner, &job),
            r#"{"done":true,"status":500,"body":{"error":"Source processing failed; refresh or re-upload to retry."}}"#
        );
        // A result within the cap is written as it is.
        let job = work::begin(account.ctx(), &owner, "a", false).unwrap();
        let ok = Outcome::Done(serde_json::json!({ "status": 200, "body": {} }));
        finish_with(
            account.clone(),
            owner.clone(),
            job.clone(),
            ok,
            Duration::ZERO,
        )
        .await;
        assert_eq!(
            polled(&account, &owner, &job),
            r#"{"done":true,"status":200,"body":{}}"#
        );
    }

    /// A job this process ends without a result takes no tombstone, whatever the count.
    #[tokio::test]
    async fn ending_a_refused_job_writes_no_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let account = account(dir.path());
        let owner = owner(&account.start);
        for _ in 0..300 {
            let job = work::begin(account.ctx(), &owner, "a", false).unwrap();
            let ending = Ending {
                account: account.clone(),
                owner: owner.clone(),
                job,
            };
            assert!(ending.once().await);
        }
        assert!(work::begin(account.ctx(), &owner, "a", false).is_ok());
        let state = std::fs::read_to_string(
            dir.path()
                .join("users")
                .join(&account.user)
                .join("source-jobs-owner.json"),
        )
        .unwrap();
        assert!(state.contains(r#""abandoned":[]"#), "{state}");
    }

    #[test]
    fn one_owner_per_start() {
        let a = owner("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(a.owner, owner("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").owner);
        assert_ne!(a.owner, owner("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").owner);
    }
}
