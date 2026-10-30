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
use crate::{js, originals, Ctx, Reply};
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

/// The background start up to the job (routes/projects.cjs): the project (404), the body cap
/// (`over`: http.cjs `readBody`'s 413), then the job, persisted created+started, within the
/// store's two-active cap.
pub fn begin(ctx: Ctx<'_>, owner: &Owner, raw: &str, over: bool) -> Result<Started, Reply> {
    let project = project_of(ctx, raw)?;
    if over {
        return Err(Reply::error(413, "Request exceeds size limit"));
    }
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
    let Some(encoded) = originals::encode_uri_component(&project) else {
        abandon(ctx, owner, &request);
        return Err(Reply::internal());
    };
    Ok(Started {
        poll: format!("/api/projects/{encoded}/source-jobs/{id}"),
        project,
        id,
        run,
        request,
    })
}

/// The 202 answer.
pub fn accepted(started: &Started) -> Reply {
    let mut text = String::from("{\"poll\":");
    js_json::quote(&started.poll, &mut text);
    text.push('}');
    Reply::json(202, text)
}

/// A start whose operation never began (the worker refused it first): its request is tombstoned
/// and the job interrupted, so it frees its slot and never runs.
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

/// One NDJSON line of the worker's answer (core routes/projects.cjs, `X-Noevia-Source-Work`).
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    Accepted,
    Stage(String),
    Done(Value),
    Failed(u16),
    /// Anything else: the worker's answer cannot be trusted any further.
    Invalid,
}

/// Parses one line (JSON as JSON.stringify writes it, lone surrogates included).
pub fn parse_line(raw: &[u8]) -> Line {
    let Ok(v) = jobs::parse_js(raw) else {
        return Line::Invalid;
    };
    let Value::Object(map) = &v else {
        return Line::Invalid;
    };
    if map.len() != 1 {
        return Line::Invalid;
    }
    if map.get("accepted") == Some(&Value::Bool(true)) {
        return Line::Accepted;
    }
    if let Some(Value::String(s)) = map.get("stage") {
        return Line::Stage(s.clone());
    }
    if let Some(done @ Value::Object(d)) = map.get("done") {
        if d.get("status").is_some_and(Value::is_number) {
            return Line::Done(done.clone());
        }
    }
    if let Some(Value::Object(f)) = map.get("failed") {
        let status = f
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|s| u16::try_from(s).ok())
            .filter(|s| (100..=599).contains(s))
            .unwrap_or(500);
        return Line::Failed(status);
    }
    Line::Invalid
}

/// Folds the worker's lines after `accepted` into progress writes and the outcome, as core's
/// source-jobs.cjs `start` did: once a progress write fails, no further progress is written and
/// the job ends failed with 503 whatever the operation answered; a stream that ends (or turns
/// unreadable) before its result is the worker gone ([`RESTARTED`]).
#[derive(Debug, Default)]
pub struct Fold {
    n: u64,
    progress_failed: bool,
}

/// What [`Fold::line`] asks the caller to do.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// Write this progress step (`n`), then report back with [`Fold::progress_result`].
    Progress(String, u64),
    Continue,
    Finish(Outcome),
}

impl Fold {
    pub fn line(&mut self, line: Option<Line>) -> Step {
        match line {
            Some(Line::Stage(stage)) if !self.progress_failed => {
                self.n += 1;
                Step::Progress(stage, self.n)
            }
            Some(Line::Stage(_)) => Step::Continue,
            Some(Line::Done(v)) if !self.progress_failed => Step::Finish(Outcome::Done(v)),
            Some(Line::Done(_)) => Step::Finish(Outcome::Failed(503, None)),
            Some(Line::Failed(status)) => Step::Finish(Outcome::Failed(
                if self.progress_failed { 503 } else { status },
                None,
            )),
            Some(Line::Accepted | Line::Invalid) | None => {
                Step::Finish(Outcome::Failed(503, Some(RESTARTED)))
            }
        }
    }
    pub fn progress_result(&mut self, ok: bool) {
        if !ok {
            self.progress_failed = true;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
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
    fn worker_lines_fold_into_progress_and_an_outcome() {
        assert_eq!(parse_line(b"{\"accepted\":true}"), Line::Accepted);
        assert_eq!(parse_line(b"{\"stage\":\"x\"}"), Line::Stage("x".into()));
        assert_eq!(
            parse_line(b"{\"failed\":{\"status\":422}}"),
            Line::Failed(422)
        );
        assert_eq!(parse_line(b"{\"failed\":{}}"), Line::Failed(500));
        assert!(matches!(
            parse_line(b"{\"done\":{\"status\":200,\"body\":{\"a\":\"\\ud800\"}}}"),
            Line::Done(_)
        ));
        for bad in [
            &b"{\"done\":{}}"[..],
            b"[]",
            b"{",
            b"{\"stage\":1}",
            b"{\"accepted\":true,\"x\":1}",
        ] {
            assert_eq!(parse_line(bad), Line::Invalid);
        }
        let mut f = Fold::default();
        assert_eq!(
            f.line(Some(Line::Stage("a".into()))),
            Step::Progress("a".into(), 1)
        );
        f.progress_result(true);
        assert_eq!(
            f.line(Some(Line::Stage("b".into()))),
            Step::Progress("b".into(), 2)
        );
        let done = serde_json::json!({"status":200});
        assert_eq!(
            f.line(Some(Line::Done(done.clone()))),
            Step::Finish(Outcome::Done(done.clone()))
        );
        f.progress_result(false);
        assert_eq!(f.line(Some(Line::Stage("c".into()))), Step::Continue);
        assert_eq!(
            f.line(Some(Line::Done(done))),
            Step::Finish(Outcome::Failed(503, None))
        );
        assert_eq!(
            f.line(Some(Line::Failed(422))),
            Step::Finish(Outcome::Failed(503, None))
        );
        let mut g = Fold::default();
        assert_eq!(
            g.line(Some(Line::Failed(422))),
            Step::Finish(Outcome::Failed(422, None))
        );
        assert_eq!(
            g.line(None),
            Step::Finish(Outcome::Failed(503, Some(RESTARTED)))
        );
        assert_eq!(
            g.line(Some(Line::Invalid)),
            Step::Finish(Outcome::Failed(503, Some(RESTARTED)))
        );
    }
}
