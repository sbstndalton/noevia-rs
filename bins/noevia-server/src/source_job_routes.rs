//! The front's source jobs (sbstndalton/noevia#2191 round 6, `rust-source-jobs`): the background
//! upload and sync starts and their poll, with this process as the one owner of the source-job
//! store (crates/server-projects `source_work`).
//!
//! A start: the project (404) and the body cap (413) are checked, the job is persisted
//! created+started within the two-active cap, then the operation is sent to Node as the synchronous
//! route with the client's own headers and body plus `X-Noevia-Source-Work: 1`. Node answers NDJSON
//! (`{"accepted":true}`, `{"stage":...}` lines, then `{"done":{status,body}}` or
//! `{"failed":{status}}`); a refusal before `accepted` is a plain answer (its 404, the storage
//! hold, `invalid JSON`), which is passed on as it is while the job is abandoned, so it never runs
//! and frees its slot. Once accepted, the client gets core's 202 `{ poll }` and a task of this
//! process writes each progress step and the terminal row, however long the operation takes and
//! whether or not the client is still there. A worker stream that ends before its result (Node
//! restarted) fails the job with core's restart wording.
use crate::reply;
use crate::App;
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
use server_projects::source_work::{self as work, Fold, Line, Outcome, Owner, Started, Step};
use server_projects::Ctx;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The header Node answers the worker call on (core routes/projects.cjs). A client's own copy is
/// dropped before any synchronous upload or sync reaches Node.
pub const WORK_HEADER: &str = "x-noevia-source-work";

/// The longest worker line: a result (the store keeps at most 32 MiB) and its envelope.
const LINE_CAP: usize = 33 * 1024 * 1024;

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

fn respond(r: server_projects::Reply) -> Response<Body> {
    let status = StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    match r.body {
        server_projects::Body::Json(text) => reply::json_text(status, text, true),
        server_projects::Body::Bytes { .. } => {
            reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true)
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

/// Reads NDJSON lines from a worker answer.
struct Lines {
    body: Body,
    buf: Vec<u8>,
    done: bool,
}

impl Lines {
    fn new(body: Body) -> Self {
        Lines {
            body,
            buf: Vec::new(),
            done: false,
        }
    }
    /// The next line; `None` at the end of the stream (a partial last line is [`Line::Invalid`]).
    async fn next(&mut self) -> Option<Line> {
        loop {
            if let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
                let rest = self.buf.split_off(i + 1);
                let mut line = std::mem::replace(&mut self.buf, rest);
                line.pop();
                return Some(work::parse_line(&line));
            }
            if self.done {
                return (!self.buf.is_empty()).then(|| {
                    self.buf.clear();
                    Line::Invalid
                });
            }
            if self.buf.len() > LINE_CAP {
                self.done = true;
                self.buf.clear();
                return Some(Line::Invalid);
            }
            match self.body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        self.buf.extend_from_slice(&data);
                    }
                }
                Some(Err(_)) | None => self.done = true,
            }
        }
    }
}

fn is_stream(res: &Response<Body>) -> bool {
    res.status() == StatusCode::OK
        && res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/x-ndjson"))
}

/// The request Node runs the operation for: the synchronous route (the path, no query), the
/// client's headers (its session and CSRF pass Node's gate as the client's own request did), the
/// body as received, and the worker header.
fn worker_request(path: &str, headers: &HeaderMap, body: Vec<u8>) -> Option<Request<Body>> {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(path)
        .body(Body::from(body.clone()))
        .ok()?;
    let h = req.headers_mut();
    for (k, v) in headers {
        if k == header::CONTENT_LENGTH
            || k == header::TRANSFER_ENCODING
            || k.as_str() == WORK_HEADER
        {
            continue;
        }
        h.append(k.clone(), v.clone());
    }
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
    h.insert(WORK_HEADER, HeaderValue::from_static("1"));
    Some(req)
}

/// POST /api/projects/{id}/upload|sources/sync with `background=1`. `body` is what the client sent
/// (at most the cap + 1 bytes; `over` past it). `regate` re-checks the session right before the
/// job is written (the body was read for as long as the client took).
#[allow(clippy::too_many_arguments)]
pub async fn start(
    app: &Arc<App>,
    peer: std::net::SocketAddr,
    account: Account,
    path: String,
    project: String,
    headers: HeaderMap,
    body: Vec<u8>,
    over: bool,
    regate: impl FnOnce() -> Result<(), Box<Response<Body>>> + Send + 'static,
) -> Response<Body> {
    let owner = owner(&account.start);
    let begun = {
        let (account, owner) = (account.clone(), owner.clone());
        blocking(move || {
            regate()?;
            work::begin(account.ctx(), &owner, &project, over).map_err(|r| Box::new(respond(r)))
        })
        .await
    };
    let job = match begun {
        Some(Ok(job)) => job,
        Some(Err(r)) => return *r,
        None => return reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
    };
    let abandon = |account: Account, owner: Owner, request: String| {
        blocking(move || work::abandon(account.ctx(), &owner, &request))
    };
    let Some(req) = worker_request(&path, &headers, body) else {
        abandon(account, owner, job.request.clone()).await;
        return reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true);
    };
    let res = app.proxy.forward(peer, req).await;
    if !is_stream(&res) {
        // Node refused before taking the work (or is not there): its answer, and no job.
        abandon(account, owner, job.request.clone()).await;
        return res;
    }
    let mut lines = Lines::new(res.into_body());
    if lines.next().await != Some(Line::Accepted) {
        abandon(account, owner, job.request.clone()).await;
        return reply::error(
            StatusCode::BAD_GATEWAY,
            "The server is not answering. Please retry.",
            true,
        );
    }
    let answer = respond(work::accepted(&job));
    tokio::spawn(run(account, owner, job, lines));
    answer
}

/// The job after `accepted`: every progress step, then the terminal row (retried while the store
/// cannot be written; abandoned at last, so the slot is never held for good).
async fn run(account: Account, owner: Owner, job: Started, mut lines: Lines) {
    let mut fold = Fold::default();
    let outcome = loop {
        match fold.line(lines.next().await) {
            Step::Continue => {}
            Step::Progress(stage, n) => {
                let (a, o, j) = (account.clone(), owner.clone(), job.clone());
                let ok = blocking(move || work::progress(a.ctx(), &o, &j, &stage, n).is_ok())
                    .await
                    .unwrap_or(false);
                fold.progress_result(ok);
            }
            Step::Finish(outcome) => break outcome,
        }
    };
    drop(lines);
    finish(account, owner, job, outcome).await;
}

async fn finish(account: Account, owner: Owner, job: Started, outcome: Outcome) {
    use server_projects::source_jobs::Error;
    for attempt in 0..12 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
        let (a, o, j, out) = (account.clone(), owner.clone(), job.clone(), outcome.clone());
        // Written; or a newer owner interrupted it; or the account is gone.
        if let Some(
            Ok(()) | Err(Error::Stale | Error::Gone | Error::NotFound | Error::BadRequest),
        ) = blocking(move || work::finish(a.ctx(), &o, &j, &out)).await
        {
            return;
        }
    }
    eprintln!("noevia-server: a source job's result could not be written; it is abandoned");
    let request = job.request.clone();
    blocking(move || work::abandon(account.ctx(), &owner, &request)).await;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_worker_request_is_the_synchronous_route_with_the_clients_headers() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("session=synthetic"),
        );
        h.insert("x-csrf-token", HeaderValue::from_static("synthetic"));
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_static("999"));
        h.insert(WORK_HEADER, HeaderValue::from_static("0"));
        let req = worker_request("/api/projects/p%201/upload", &h, b"{}".to_vec()).unwrap();
        assert_eq!(req.uri().path(), "/api/projects/p%201/upload");
        assert_eq!(req.uri().query(), None);
        assert_eq!(req.method(), Method::POST);
        assert_eq!(req.headers()[header::COOKIE], "session=synthetic");
        assert_eq!(req.headers()["x-csrf-token"], "synthetic");
        assert_eq!(req.headers()[header::CONTENT_LENGTH], "2");
        assert_eq!(req.headers().get_all(WORK_HEADER).iter().count(), 1);
        assert_eq!(req.headers()[WORK_HEADER], "1");
    }

    #[tokio::test]
    async fn lines_split_a_stream_and_mark_a_torn_end() {
        let mut l = Lines::new(Body::from(
            "{\"accepted\":true}\n{\"stage\":\"a\"}\n{\"done\":{\"status\":200,\"body\":{}}}\n{\"st",
        ));
        assert_eq!(l.next().await, Some(Line::Accepted));
        assert_eq!(l.next().await, Some(Line::Stage("a".into())));
        assert!(matches!(l.next().await, Some(Line::Done(_))));
        assert_eq!(l.next().await, Some(Line::Invalid));
        assert_eq!(l.next().await, None);
    }

    #[test]
    fn one_owner_per_start() {
        let a = owner("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(a.owner, owner("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").owner);
        assert_ne!(a.owner, owner("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").owner);
    }
}
