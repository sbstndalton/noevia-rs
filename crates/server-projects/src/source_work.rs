//! The front's own source jobs (sbstndalton/noevia#2191 round 6, `rust-source-jobs`): this process
//! is the one owner of the source-job store ([`crate::source_jobs`]). It starts a job for
//! `POST /api/projects/{id}/upload?background=1` and `.../sources/sync?background=1`, records the
//! progress and the result of the operation it runs, and answers
//! `GET /api/projects/{id}/source-jobs/{jobId}`. Node no longer reads or writes a source journal.
//!
//! The answers are core routes/projects.cjs's and source-jobs.cjs's, word for word: the project
//! 404, the store's refusals as project-store-client.cjs `sourceCall` worded them (then
//! http.cjs `errorResponse`: a 5xx is `Internal error`), the 202 `{ poll }`, and each poll's
//! `{ done, status, body }` or `{ done: false, stage }`.
//!
//! The operation itself is the bin's (noevia-server source_job_routes.rs runs it); this module is
//! the store side, with no network code.
use crate::source_jobs::{self as jobs, Error};
use crate::{js, originals, Body, Ctx, Reply};
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::Mutex;

/// This process as the source-job owner: the container start, its own birth tick and a random id.
#[derive(Debug, Clone)]
pub struct Owner {
    pub start: String,
    pub born: String,
    pub owner: String,
}

impl Owner {
    /// `start`: NOEVIA_START_ID. The birth tick is Linux `/proc/self/stat` field 22 (what Node
    /// read); elsewhere 0, which only a test reaches.
    pub fn this_process(start: &str) -> Self {
        Owner {
            start: start.to_owned(),
            born: born_tick().unwrap_or_else(|| "0".into()),
            owner: jobs::new_id(),
        }
    }
}

fn born_tick() -> Option<String> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = stat.get(stat.rfind(") ")? + 2..)?;
    let field = rest.split_whitespace().nth(19)?;
    field
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| field.to_owned())
}

/// The users (per data dir and owner) whose earlier owners' jobs this process has interrupted.
static RECOVERED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

fn jctx<'a>(ctx: Ctx<'a>, owner: &'a Owner) -> jobs::Context<'a> {
    jobs::Context {
        data_dir: ctx.data_dir,
        start_id: &owner.start,
    }
}

/// source-jobs.cjs `storeFor`: the first use of a user's store in this process registers this
/// owner and interrupts the jobs of any earlier one (a restart's leftovers).
fn ensure_recovered(ctx: Ctx<'_>, owner: &Owner) -> Result<(), Error> {
    let key = format!(
        "{}\0{}\0{}",
        ctx.data_dir.display(),
        ctx.user_id,
        owner.owner
    );
    if RECOVERED.lock().map(|s| s.contains(&key)).unwrap_or(false) {
        return Ok(());
    }
    jobs::recover(
        jctx(ctx, owner),
        ctx.user_id,
        &owner.start,
        &owner.born,
        &owner.owner,
    )?;
    if let Ok(mut s) = RECOVERED.lock() {
        s.insert(key);
    }
    Ok(())
}

/// A store failure as core answered it (project-store-client.cjs `sourceCall`, then `errorResponse`).
pub fn store_reply(e: &Error) -> Reply {
    match e {
        Error::TooMany => Reply::error(
            429,
            "Source processing is busy; wait for the current operation and retry.",
        ),
        Error::Gone => Reply::error(410, "account no longer exists"),
        Error::Stale => Reply::error(
            409,
            "Source processing was interrupted by a newer server start.",
        ),
        Error::NotFound => Reply::error(404, "Source processing project no longer exists."),
        Error::BadRequest | Error::Busy | Error::Corrupt | Error::Io => {
            Reply::error(503, "Internal error")
        }
    }
}

/// The decoded project id, or the answer: a bad escape is the generic 500
/// (`decodeURIComponent` throws), an unknown project `{ error: 'project not found' }`.
fn project_of(ctx: Ctx<'_>, raw: &str) -> Result<String, Reply> {
    let Some(id) = js::decode_uri_component(raw) else {
        return Err(Reply::internal());
    };
    match originals::project(ctx, &id)? {
        (_, Some(_)) => Ok(id),
        (_, None) => Err(Reply::error(404, "project not found")),
    }
}

/// A started job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub project: String,
    pub id: String,
    pub run: String,
    pub request: String,
    /// `/api/projects/<encodeURIComponent(id)>/source-jobs/<job>`.
    pub poll: String,
}

/// The background start up to the job (routes/projects.cjs): the project (404), then `check` (core's
/// worker checked, before it accepted, the storage hold of an automatic sync and the body: 413, 400
/// `invalid JSON`), then the job, persisted created+started, within the store's two-active cap.
pub fn begin_with(
    ctx: Ctx<'_>,
    owner: &Owner,
    raw: &str,
    check: impl FnOnce() -> Result<(), Reply>,
) -> Result<Started, Reply> {
    let project = project_of(ctx, raw)?;
    check()?;
    ensure_recovered(ctx, owner).map_err(|e| store_reply(&e))?;
    let request = jobs::new_id();
    let (id, run) = jobs::start(
        jctx(ctx, owner),
        ctx.user_id,
        &owner.start,
        &owner.born,
        &owner.owner,
        &project,
        &request,
    )
    .map_err(|e| store_reply(&e))?;
    let started = |poll: String| Started {
        poll,
        project: project.clone(),
        id: id.clone(),
        run: run.clone(),
        request: request.clone(),
    };
    let Some(encoded) = originals::encode_uri_component(&project) else {
        // This process wrote the job a moment ago and knows it never began.
        let _ = interrupt(ctx, owner, &started(String::new()));
        return Err(Reply::internal());
    };
    Ok(started(format!("/api/projects/{encoded}/source-jobs/{id}")))
}

/// [`begin_with`] with only the body cap checked (`over`: http.cjs `readBody`'s 413).
pub fn begin(ctx: Ctx<'_>, owner: &Owner, raw: &str, over: bool) -> Result<Started, Reply> {
    begin_with(ctx, owner, raw, || {
        if over {
            Err(Reply::error(413, "Request exceeds size limit"))
        } else {
            Ok(())
        }
    })
}

/// The 202 answer.
pub fn accepted(started: &Started) -> Reply {
    let mut text = String::from("{\"poll\":");
    js_json::quote(&started.poll, &mut text);
    text.push('}');
    Reply::json(202, text)
}

/// A job whose operation never began, because this process saw the worker refuse it (or the client
/// leave before the worker answered): the job is interrupted, so it frees its slot and never runs.
/// No request tombstone is kept, so refusals can never fill the store's bounded tombstone list.
pub fn interrupt(ctx: Ctx<'_>, owner: &Owner, job: &Started) -> Result<(), Error> {
    jobs::interrupt(
        jctx(ctx, owner),
        ctx.user_id,
        &owner.start,
        &owner.born,
        &owner.owner,
        (&job.id, &job.run),
    )
}

/// A start whose reply was lost, so the caller cannot tell whether it began (it holds no job): its
/// request is tombstoned (the oldest tombstone goes past the store's bound) and any job of it
/// interrupted, so it frees its slot and never runs. A refusal this process saw is [`interrupt`].
pub fn abandon(ctx: Ctx<'_>, owner: &Owner, request: &str) -> bool {
    jobs::abandon(
        jctx(ctx, owner),
        ctx.user_id,
        &owner.start,
        &owner.born,
        &owner.owner,
        request,
    )
    .is_ok()
}

/// One progress step (`n` from 1).
pub fn progress(
    ctx: Ctx<'_>,
    owner: &Owner,
    job: &Started,
    stage: &str,
    n: u64,
) -> Result<(), Error> {
    jobs::progress(
        jctx(ctx, owner),
        ctx.user_id,
        &owner.start,
        &owner.born,
        &owner.owner,
        jobs::ProgressUpdate {
            id: &job.id,
            run: &job.run,
            stage,
            n,
        },
    )
}

/// How an operation ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The route answered `{ status, body }` (any status): `job.completed`.
    Done(Value),
    /// The operation failed (it threw, a progress write failed, or the worker went away):
    /// `job.failed` with `{ status }` and the message (`None`: core's default).
    Failed(u16, Option<&'static str>),
}

/// How long one operation may run before its job is ended as failed (the front's own bound, in place of
/// the removed worker's 5 minute accept bound, so that a hung operation cannot hold one of the two slots
/// for the store's 6 hours): 2 hours for a folder sync, which reads and extracts every file of a project, 30
/// minutes for an upload.
pub const SYNC_BOUND: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);
pub const UPLOAD_BOUND: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// The bound of an upload (`true`) or a sync.
pub fn bound(upload: bool) -> std::time::Duration {
    if upload {
        UPLOAD_BOUND
    } else {
        SYNC_BOUND
    }
}

/// What a job that passed its bound says.
pub const OVER_BOUND: &str =
    "Processing took too long and was stopped; refresh or re-upload to retry.";

/// The message of a job whose worker went away before its result (core's interrupted wording).
pub const RESTARTED: &str =
    "The server restarted before processing finished; refresh or re-upload to retry.";

/// The terminal row.
pub fn finish(ctx: Ctx<'_>, owner: &Owner, job: &Started, outcome: &Outcome) -> Result<(), Error> {
    let (ok, result, error) = match outcome {
        Outcome::Done(v) => (true, v.clone(), None),
        Outcome::Failed(status, message) => {
            (false, serde_json::json!({ "status": status }), *message)
        }
    };
    jobs::finish(
        jctx(ctx, owner),
        ctx.user_id,
        &owner.start,
        &owner.born,
        &owner.owner,
        jobs::CompletionUpdate {
            id: &job.id,
            run: &job.run,
            ok,
            result: &result,
            error,
        },
    )
}

fn push(out: &mut String, key: &str, v: &Value) -> Result<(), Error> {
    if out.len() > 1 {
        out.push(',');
    }
    js_json::quote(key, out);
    out.push(':');
    out.push_str(&jobs::stringify_js(v)?);
    Ok(())
}

/// JavaScript truthiness of a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// source-jobs.cjs `read`'s answer for a stored job.
fn poll_text(job: &Value) -> Result<String, Error> {
    let null = Value::Null;
    let at = |p: &str| job.pointer(p).unwrap_or(&null);
    let mut out = String::from("{");
    match at("/status").as_str() {
        Some("completed") => {
            push(&mut out, "done", &Value::Bool(true))?;
            // `job.result?.status` / `?.body`: an absent key is left out, as JSON.stringify does.
            if let Some(s) = job.pointer("/result/status") {
                push(&mut out, "status", s)?;
            }
            if let Some(b) = job.pointer("/result/body") {
                push(&mut out, "body", b)?;
            }
        }
        Some("failed") => {
            // `job.result?.status || 500`, `{ error: job.error }`.
            out.push_str("\"done\":true,\"status\":");
            let status = at("/result/status");
            if truthy(status) {
                out.push_str(&jobs::stringify_js(status)?);
            } else {
                out.push_str("500");
            }
            out.push_str(",\"body\":{\"error\":");
            out.push_str(&jobs::stringify_js(at("/error"))?);
            out.push('}');
        }
        Some("interrupted" | "cancelled") => {
            out.push_str("\"done\":true,\"status\":503,\"body\":{\"error\":");
            js_json::quote(RESTARTED, &mut out);
            out.push('}');
        }
        _ => {
            out.push_str("\"done\":false");
            if truthy(at("/stage")) {
                push(&mut out, "stage", at("/stage"))?;
            }
        }
    }
    out.push('}');
    Ok(out)
}

/// GET /api/projects/{id}/source-jobs/{jobId} (`job` as the path carries it, undecoded).
pub fn poll(ctx: Ctx<'_>, owner: &Owner, raw: &str, job: &str) -> Reply {
    let project = match project_of(ctx, raw) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if let Err(e) = ensure_recovered(ctx, owner) {
        return store_reply(&e);
    }
    match jobs::read(jctx(ctx, owner), ctx.user_id, &project, job) {
        Err(e) => store_reply(&e),
        Ok(None) => Reply::error(
            404,
            "Processing status expired or the server restarted; refresh or re-upload to retry.",
        ),
        Ok(Some(v)) => match poll_text(&v) {
            Ok(text) => Reply::json(200, text),
            Err(_) => Reply::internal(),
        },
    }
}

/// Counts a running operation's progress steps (`n` from 1) and remembers a write that failed: once
/// one has, no clean completion follows it (core's source-jobs.cjs `start`): the job ends failed with 503
/// whatever the operation answered.
#[derive(Debug, Default)]
pub struct Tracker {
    n: u64,
    failed: bool,
}

impl Tracker {
    /// The next step number (a step written after a failed one is skipped by the caller's choice; the
    /// number still advances).
    pub fn advance(&mut self) -> u64 {
        self.n += 1;
        self.n
    }
    pub fn progress_result(&mut self, ok: bool) {
        if !ok {
            self.failed = true;
        }
    }
    pub fn failed(&self) -> bool {
        self.failed
    }
}

/// The terminal row for the answer the operation gave: the route's `{ status, body }` (any status), or
/// a failed job (503, core's default message) when a progress write failed meanwhile.
pub fn outcome(answer: &Reply, progress_failed: bool) -> Outcome {
    if progress_failed {
        return Outcome::Failed(503, None);
    }
    let Body::Json(text) = &answer.body else {
        return Outcome::Failed(500, None);
    };
    match jobs::parse_js(text.as_bytes()) {
        Ok(body) => Outcome::Done(serde_json::json!({ "status": answer.status, "body": body })),
        Err(_) => Outcome::Failed(500, None),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::path::Path;

    const USER: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const START: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("users").join(USER);
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(
            user.join("projects.json"),
            r#"{"projects":[{"id":"p 1","name":"Synthetic","files":[]}]}"#,
        )
        .unwrap();
        dir
    }
    fn ctx(dir: &Path) -> Ctx<'_> {
        Ctx {
            data_dir: dir,
            user_id: USER,
            switch: server_store::RustProjects::owned(),
            now_ms: 1,
        }
    }
    fn owner() -> Owner {
        Owner {
            start: START.into(),
            born: "100".into(),
            owner: jobs::new_id(),
        }
    }
    fn text(r: &Reply) -> String {
        match &r.body {
            crate::Body::Json(t) => t.clone(),
            crate::Body::Bytes { .. } => panic!("bytes"),
        }
    }

    #[test]
    fn a_job_runs_from_start_to_poll_in_core_words() {
        let dir = fixture();
        let (c, o) = (ctx(dir.path()), owner());
        assert_eq!(
            begin(c, &o, "ghost", false).unwrap_err(),
            Reply::error(404, "project not found")
        );
        assert_eq!(begin(c, &o, "%E0", false).unwrap_err(), Reply::internal());
        assert_eq!(
            begin(c, &o, "p%201", true).unwrap_err(),
            Reply::error(413, "Request exceeds size limit")
        );
        let job = begin(c, &o, "p%201", false).unwrap();
        assert_eq!(job.project, "p 1");
        assert_eq!(
            job.poll,
            format!("/api/projects/p%201/source-jobs/{}", job.id)
        );
        assert_eq!(
            text(&accepted(&job)),
            format!("{{\"poll\":\"{}\"}}", job.poll)
        );
        let r = poll(c, &o, "p%201", &job.id);
        assert_eq!((r.status, text(&r)), (200, "{\"done\":false}".into()));
        progress(c, &o, &job, "Reading synthetic.pdf", 1).unwrap();
        let r = poll(c, &o, "p%201", &job.id);
        assert_eq!(
            text(&r),
            "{\"done\":false,\"stage\":\"Reading synthetic.pdf\"}"
        );
        let done = serde_json::json!({"status":200,"body":{"name":"a.txt","bytes":3}});
        finish(c, &o, &job, &Outcome::Done(done)).unwrap();
        let r = poll(c, &o, "p%201", &job.id);
        let body = jobs::stringify_js(&serde_json::json!({"name":"a.txt","bytes":3})).unwrap();
        assert_eq!(
            text(&r),
            format!("{{\"done\":true,\"status\":200,\"body\":{body}}}")
        );
        assert_eq!(
            poll(c, &o, "ghost", &job.id),
            Reply::error(404, "project not found")
        );
        assert_eq!(
            poll(c, &o, "p%201", "11111111-1111-4111-8111-111111111111"),
            Reply::error(
                404,
                "Processing status expired or the server restarted; refresh or re-upload to retry."
            )
        );
    }

    #[test]
    fn failures_the_cap_an_abandon_and_a_restart_read_as_core_read_them() {
        let dir = fixture();
        let (c, o) = (ctx(dir.path()), owner());
        let a = begin(c, &o, "p%201", false).unwrap();
        let b = begin(c, &o, "p%201", false).unwrap();
        assert_eq!(
            begin(c, &o, "p%201", false).unwrap_err(),
            Reply::error(
                429,
                "Source processing is busy; wait for the current operation and retry."
            )
        );
        finish(c, &o, &a, &Outcome::Failed(422, None)).unwrap();
        assert_eq!(
            text(&poll(c, &o, "p%201", &a.id)),
            "{\"done\":true,\"status\":422,\"body\":{\"error\":\"Source processing failed; refresh or re-upload to retry.\"}}"
        );
        assert!(abandon(c, &o, &b.request));
        assert_eq!(
            text(&poll(c, &o, "p%201", &b.id)),
            format!("{{\"done\":true,\"status\":503,\"body\":{{\"error\":\"{RESTARTED}\"}}}}")
        );
        // A worker gone before its result.
        let d = begin(c, &o, "p%201", false).unwrap();
        finish(c, &o, &d, &Outcome::Failed(503, Some(RESTARTED))).unwrap();
        assert_eq!(
            text(&poll(c, &o, "p%201", &d.id)),
            format!("{{\"done\":true,\"status\":503,\"body\":{{\"error\":\"{RESTARTED}\"}}}}")
        );
        // The next process interrupts what this one left running, at its first use.
        let e = begin(c, &o, "p%201", false).unwrap();
        let next = Owner {
            born: "200".into(),
            ..owner()
        };
        assert_eq!(
            text(&poll(c, &next, "p%201", &e.id)),
            format!("{{\"done\":true,\"status\":503,\"body\":{{\"error\":\"{RESTARTED}\"}}}}")
        );
        // ... and the earlier owner can no longer write.
        assert_eq!(
            finish(c, &o, &e, &Outcome::Failed(500, None)).map_err(|e| store_reply(&e).status),
            Err(409)
        );
        // A failed result without a status reads as 500.
        let f = begin(c, &next, "p%201", false).unwrap();
        jobs::finish(
            jctx(c, &next),
            USER,
            START,
            "200",
            &next.owner,
            jobs::CompletionUpdate {
                id: &f.id,
                run: &f.run,
                ok: false,
                result: &Value::Null,
                error: None,
            },
        )
        .unwrap();
        assert!(text(&poll(c, &next, "p%201", &f.id)).starts_with("{\"done\":true,\"status\":500,"));
    }

    #[test]
    fn an_operations_answer_becomes_the_jobs_terminal_row() {
        // The route's answer, any status, is the result; a lone surrogate survives.
        let ok = Reply::json(200, "{\"name\":\"a.txt\",\"note\":\"\\ud800\"}".into());
        match outcome(&ok, false) {
            Outcome::Done(v) => {
                assert_eq!(v["status"], 200);
                assert_eq!(v["body"]["name"], "a.txt");
            }
            other => panic!("{other:?}"),
        }
        let refused = Reply::error(422, "no");
        assert!(matches!(outcome(&refused, false), Outcome::Done(v) if v["status"] == 422));
        // A progress write that failed means no clean completion after it, whatever the answer was.
        assert_eq!(outcome(&ok, true), Outcome::Failed(503, None));
        let mut t = Tracker::default();
        assert_eq!(t.advance(), 1);
        t.progress_result(true);
        assert!(!t.failed());
        assert_eq!(t.advance(), 2);
        t.progress_result(false);
        assert!(t.failed());
    }
}
