//! The front's source jobs (sbstndalton/noevia#2191 round 6, `rust-source-jobs`; the operation itself the
//! front's since round 8, `rust-project-source-work`): the background upload and sync starts and their
//! poll, with this process as the one owner of the source-job store (crates/server-projects
//! `source_work`).
//!
//! A start: the project (404), for a sync the storage hold of an automatic refresh (#1429), and the body
//! (413, 400 `invalid JSON`) are checked, as core's worker checked them before it accepted; then the job is
//! persisted created+started within the two-active cap (429) and the operation is listed for core
//! ([`server_projects::source_ops`], #1441); only then does the client get core's 202 `{ poll }`, so a chat
//! turn sent the moment it has the answer already finds the file in the list. The operation runs in this
//! process (crates/server-projects `upload`, `sync`): every progress step and the terminal row are written by
//! a task of this process, however long it takes and whether or not the client is still there. From the
//! job's first write on, everything runs in a task of its own, not in the request's future: a client that
//! leaves at any point cannot strand the job.
//!
//! **The operation bound.** The operation is a thread that cannot be stopped from outside, so it is bounded
//! from outside instead: past [`server_projects::source_work::bound`] (2 hours for a sync, 30 minutes for an
//! upload; the removed worker's 5 minute accept bound and the store's 6 hour stale bound were the old
//! guards) the job ends as failed 504, its listing as failed and its slot is free, whether or not the thread
//! has returned. The thread, if it is hung, stays hung (and keeps the project's source lock, which later
//! operations on the project wait for, bounded: 429); it can no longer hold a slot or make a chat wait.
//!
//! A refusal this process makes before the job exists is a plain answer and starts nothing. A job whose
//! operation cannot be started (the extraction client is missing) is interrupted, not tombstoned, so any
//! number of refusals leaves the store as it was.
use crate::reply;
use crate::App;
use axum::body::Body;
use axum::http::{Response, StatusCode};
use server_projects::source_ops::{self, Op};
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
}

impl Account {
    /// The context of one use, stamped with the wall clock of that use: a job may run hours after its
    /// request arrived, and what it writes (holds, listings, records) is dated by when it happens.
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            data_dir: &self.data_dir,
            user_id: &self.user,
            switch: server_store::RustProjects::owned(),
            now_ms: server_projects::wall_ms(),
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
    /// Shared: the body can be 80 MiB and is looked at by more than one step.
    pub body: Arc<Vec<u8>>,
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
    use server_projects::{lifecycle, sync, upload};
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
    // A request waits briefly for the project's source lock and then says 429; a job, already accepted,
    // waits its turn up to its own bound.
    let lock_wait = if check_hold {
        server_projects::project_documents::FOREGROUND_LOCK_WAIT
    } else {
        work::bound(work.kind == Kind::Upload)
    };
    with_storage(app, authn, |storage| match work.kind {
        Kind::Upload => upload::post(
            &upload::Env {
                ctx,
                storage,
                engine: engine.as_ref(),
                limits: &limits,
                catalogue: catalogue.as_ref(),
                project_root: &root,
                progress,
                lock_wait,
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
                lock_wait,
            })
        }
    })
}

/// How the operation shows in the list core reads: (`kind`, `name`), from the body parsed once (`None`: it
/// is empty or not JSON, as a sync's is).
fn listing_of(kind: Kind, parsed: Option<&js_json::JValue>) -> (&'static str, String) {
    match kind {
        Kind::Sync => ("sync", String::new()),
        Kind::Upload => {
            let name = parsed
                .map(|b| b.get("name"))
                .filter(|n| n.truthy())
                .map(|n| js_json::to_js_string(n).unwrap_or_default())
                .and_then(|n| n.rsplit('/').next().map(str::to_owned))
                .unwrap_or_default();
            let import = parsed
                .is_some_and(|b| !matches!(b.get("relativePath"), js_json::JValue::Undefined));
            (if import { "import" } else { "upload" }, name)
        }
    }
}

fn parse_body(body: &[u8]) -> Option<js_json::JValue> {
    js_json::wtf16::parse(&String::from_utf8_lossy(body)).ok()
}

/// The operation is listed for core while it runs (chat waits for a file just sent, #1441; the source-text
/// collector leaves a sync's files alone, #1815).
fn list_operation(account: &Account, work: &Work, parsed: Option<&js_json::JValue>) -> Option<Op> {
    let id = server_projects::js::decode_uri_component(&work.project).unwrap_or_default();
    let (kind, name) = listing_of(work.kind, parsed);
    server_projects::store::user_dir(&account.data_dir, &account.user)
        .ok()
        .map(|d| source_ops::begin(&d, &id, kind, &name))
}

/// How an operation's bounded run ended.
enum Within<T> {
    Done(T),
    /// The task panicked: the operation threw.
    Panicked,
    /// Past the bound. The thread is left to finish (or hang) on its own.
    TimedOut,
}

async fn within<T: Send + 'static>(
    bound: Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Within<T> {
    match tokio::time::timeout(bound, blocking(f)).await {
        Ok(Some(v)) => Within::Done(v),
        Ok(None) => Within::Panicked,
        Err(_) => Within::TimedOut,
    }
}

/// Ends the operation's listing (on the blocking pool: it writes a file).
async fn end_listing(op: Option<Op>, ok: bool) {
    if let Some(op) = op {
        let _ = blocking(move || op.end(ok)).await;
    }
}

/// The synchronous form: the operation, run on the blocking pool, answered as it ends (or, past the
/// bound, as a 504).
pub async fn run_sync(
    app: &Arc<App>,
    authn: server_auth::Identity,
    account: Account,
    work: Work,
) -> Response<Body> {
    let app = Arc::clone(app);
    let bound = work::bound(work.kind == Kind::Upload);
    let listed = {
        let (account, work) = (account.clone(), work.clone());
        blocking(move || {
            let parsed = (work.kind == Kind::Upload)
                .then(|| parse_body(&work.body))
                .flatten();
            list_operation(&account, &work, parsed.as_ref())
        })
        .await
    };
    let op = listed.flatten();
    let answer = within(bound, move || {
        execute(&app, &authn, &account, &work, true, &|_| {})
    })
    .await;
    let (reply, ok) = match answer {
        Within::Done(r) => {
            let ok = r.status < 400;
            (respond(r), ok)
        }
        Within::Panicked => (
            reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
            false,
        ),
        Within::TimedOut => (
            respond(server_projects::Reply::error(504, work::OVER_BOUND)),
            false,
        ),
    };
    end_listing(op, ok).await;
    reply
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

/// The start up to the client's answer: the job (persisted within the two-active cap) and the operation's
/// listing, both written before anyone is told. `check` is core's worker's, before it accepted: it returns
/// the parsed body (or `None` for an empty one) or the refusal.
fn accept(
    account: &Account,
    owner: &Owner,
    work: &Work,
    check: impl FnOnce() -> Result<Option<js_json::JValue>, server_projects::Reply>,
) -> Result<(Started, Option<Op>), server_projects::Reply> {
    let mut parsed = None;
    let job = work::begin_with(account.ctx(), owner, &work.project, || {
        parsed = check()?;
        Ok(())
    })?;
    // Listed before the 202: the client's next request may be a chat turn that waits for this file.
    let op = list_operation(account, work, parsed.as_ref());
    Ok((job, op))
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
            let check = || -> Result<Option<js_json::JValue>, server_projects::Reply> {
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
                if text.is_empty() {
                    return Ok(None);
                }
                // Parsed once: the listing takes the file's name from it.
                js_json::wtf16::parse(&text)
                    .map(Some)
                    .map_err(|_| server_projects::Reply::error(400, "invalid JSON"))
            };
            accept(&account, &owner, &work, check).map_err(|r| Box::new(respond(r)))
        })
        .await
    };
    let (job, op) = match begun {
        Some(Ok(started)) => started,
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
    let bound = work::bound(work.kind == Kind::Upload);
    run_job(
        account.clone(),
        owner,
        job,
        op,
        bound,
        move |progress: &dyn Fn(&str)| execute(&app, &authn, &account, &work, false, progress),
    )
    .await;
}

/// The job after its start: every progress step, then the terminal row (retried while the store cannot be
/// written; interrupted at last, so the slot is never held for good). `exec` is the operation; past `bound`
/// the job ends as failed whether or not it has returned.
async fn run_job<F>(
    account: Account,
    owner: Owner,
    job: Started,
    op: Option<Op>,
    bound: Duration,
    exec: F,
) where
    F: FnOnce(&dyn Fn(&str)) -> server_projects::Reply + Send + 'static,
{
    let tracker = Arc::new(Mutex::new(work::Tracker::default()));
    let answer = {
        let (account, owner, job, tracker) = (
            account.clone(),
            owner.clone(),
            job.clone(),
            Arc::clone(&tracker),
        );
        within(bound, move || {
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
            exec(&progress)
        })
        .await
    };
    let failed = match tracker.lock() {
        Ok(t) => t.failed(),
        Err(_) => true,
    };
    let (outcome, ok) = match answer {
        Within::Done(r) => {
            let ok = r.status < 400;
            (work::outcome(&r, failed), ok)
        }
        // The task panicked: the operation threw.
        Within::Panicked => (Outcome::Failed(500, None), false),
        Within::TimedOut => {
            eprintln!(
                "noevia-server: a source operation passed its bound; its job is ended as failed"
            );
            (Outcome::Failed(504, Some(work::OVER_BOUND)), false)
        }
    };
    end_listing(op, ok).await;
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

    fn upload_work(body: &str) -> Work {
        Work {
            kind: Kind::Upload,
            project: "a".into(),
            body: Arc::new(body.as_bytes().to_vec()),
            over: false,
            retry: false,
        }
    }

    /// #2191 round 8 review (#1441): the operation is listed before the client is told, so a chat turn
    /// sent the moment it has the 202 already finds the file.
    #[test]
    fn a_started_job_is_listed_before_the_client_can_be_told() {
        let dir = tempfile::tempdir().unwrap();
        let account = account(dir.path());
        let owner = owner(&account.start);
        let work = upload_work(r#"{"name":"Notes/report.pdf","dataBase64":"AA=="}"#);
        let parse = || Ok(parse_body(&work.body));
        let (job, op) = accept(&account, &owner, &work, parse).unwrap();
        let user = server_projects::store::user_dir(dir.path(), &account.user).unwrap();
        assert_eq!(
            source_ops::listed(&user),
            vec![("upload".into(), "report.pdf".into(), "running".into())],
            "listed by the time `accept` returns, which is before the reply is sent"
        );
        assert!(polled(&account, &owner, &job).contains(r#""done":false"#));
        // A folder import's file lists as one; a refused start lists nothing.
        drop(op);
        let work = upload_work(r#"{"name":"b.md","relativePath":"x/b.md"}"#);
        let parse = || Ok(parse_body(&work.body));
        let (_, op) = accept(&account, &owner, &work, parse).unwrap();
        assert!(source_ops::listed(&user).contains(&(
            "import".into(),
            "b.md".into(),
            "running".into()
        )));
        drop(op);
        let refused = accept(&account, &owner, &work, || {
            Err(server_projects::Reply::error(400, "invalid JSON"))
        });
        assert_eq!(refused.err().map(|r| r.status), Some(400));
        assert_eq!(source_ops::listed(&user).len(), 2, "nothing was added");
    }

    /// The listing and the latch are dated when they happen (#1429, #1441), never by a request time.
    #[test]
    fn a_context_is_stamped_at_its_use_not_at_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let account = account(dir.path());
        let first = account.ctx().now_ms;
        std::thread::sleep(Duration::from_millis(5));
        assert!(account.ctx().now_ms > first);
        assert!(first > 1_700_000_000_000);
    }

    /// A hung operation cannot hold its slot: past the bound the job ends as failed 504 and its listing
    /// as failed, with the thread still running.
    #[tokio::test]
    async fn an_operation_past_its_bound_ends_the_job_and_the_listing_without_waiting_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let account = account(dir.path());
        let owner = owner(&account.start);
        let work = upload_work(r#"{"name":"hung.pdf"}"#);
        let parse = || Ok(parse_body(&work.body));
        let (job, op) = accept(&account, &owner, &work, parse).unwrap();
        let user = server_projects::store::user_dir(dir.path(), &account.user).unwrap();
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let started = std::time::Instant::now();
        run_job(
            account.clone(),
            owner.clone(),
            job.clone(),
            op,
            Duration::from_millis(100),
            move |_progress: &dyn Fn(&str)| {
                let _ = hold.recv_timeout(Duration::from_secs(20));
                server_projects::Reply::error(200, "too late")
            },
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "did not wait for the thread"
        );
        let text = polled(&account, &owner, &job);
        assert!(text.contains(r#""done":true,"status":504"#), "{text}");
        assert!(text.contains(work::OVER_BOUND), "{text}");
        assert_eq!(
            source_ops::listed(&user),
            vec![("upload".into(), "hung.pdf".into(), "failed".into())]
        );
        // The slot is free: two more jobs fit within the two-active cap.
        let a = work::begin(account.ctx(), &owner, "a", false);
        let b = work::begin(account.ctx(), &owner, "a", false);
        assert!(a.is_ok() && b.is_ok());
        drop(release);
    }

    #[test]
    fn the_bounds_are_two_hours_for_a_sync_and_half_an_hour_for_an_upload() {
        assert_eq!(work::bound(false), Duration::from_secs(7200));
        assert_eq!(work::bound(true), Duration::from_secs(1800));
    }

    #[test]
    fn one_owner_per_start() {
        let a = owner("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(a.owner, owner("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").owner);
        assert_ne!(a.owner, owner("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").owner);
    }
}
