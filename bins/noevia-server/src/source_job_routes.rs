//! The front's source jobs (sbstndalton/noevia#2191 round 6, `rust-source-jobs`): the background
//! upload and sync starts and their poll, with this process as the one owner of the source-job
//! store (crates/server-projects `source_work`).
//!
//! A start: the project (404) and the body cap (413) are checked, the job is persisted
//! created+started within the two-active cap, then the operation is sent to Node as the synchronous
//! route with the client's own headers and body plus `X-Noevia-Source-Work: 1`. Node answers NDJSON
//! (`{"accepted":true}`, `{"stage":...}` lines, then `{"done":{status,body}}` or
//! `{"failed":{status}}`); a refusal before `accepted` is a plain answer (its 404, the storage
//! hold, `invalid JSON`), which is passed on as it is while the job is interrupted (this process
//! saw the refusal, so no request tombstone is written), so it never runs and frees its slot. From
//! the job's first write on, everything runs in a task of this process, not in the request's own
//! future: a client that leaves at any point cannot strand the job. Once accepted, the client gets
//! core's 202 `{ poll }` and that task writes each progress step and the terminal row, however
//! long the operation takes and whether or not the client is still there. A worker stream that
//! ends before its result (Node restarted) fails the job with core's restart wording.
use crate::reply;
use crate::App;
use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
use server_projects::source_work::{self as work, Fold, Line, Outcome, Owner, Started, Step};
use server_projects::Ctx;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    /// Where the next line starts in `buf`.
    pos: usize,
    /// Where the search for the next newline resumes (never before `pos`): a long line that
    /// arrives in many frames is scanned once, not once per frame.
    scan: usize,
    done: bool,
    /// Bytes looked at for a newline, over the stream (a test pins it linear).
    #[cfg(test)]
    inspected: usize,
}

impl Lines {
    fn new(body: Body) -> Self {
        Lines {
            body,
            buf: Vec::new(),
            pos: 0,
            scan: 0,
            done: false,
            #[cfg(test)]
            inspected: 0,
        }
    }
    /// The next line; `None` at the end of the stream (a partial last line is [`Line::Invalid`]).
    async fn next(&mut self) -> Option<Line> {
        loop {
            let fresh = self.buf.get(self.scan..).unwrap_or_default();
            #[cfg(test)]
            {
                self.inspected += fresh.len();
            }
            if let Some(i) = fresh.iter().position(|b| *b == b'\n') {
                let end = self.scan + i;
                let line = work::parse_line(self.buf.get(self.pos..end).unwrap_or_default());
                self.pos = end + 1;
                self.scan = self.pos;
                return Some(line);
            }
            self.scan = self.buf.len();
            let pending = self.buf.len() - self.pos;
            if self.done {
                self.buf.clear();
                (self.pos, self.scan) = (0, 0);
                return (pending > 0).then_some(Line::Invalid);
            }
            if pending > LINE_CAP {
                self.done = true;
                self.buf.clear();
                (self.pos, self.scan) = (0, 0);
                return Some(Line::Invalid);
            }
            // Drop the lines already given out before reading on.
            if self.pos > 0 {
                self.buf.drain(..self.pos);
                self.scan -= self.pos;
                self.pos = 0;
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

/// How long the worker has to answer `accepted`. It answers before it does any work, so this is
/// only a bound on a worker that hangs: the request is detached from the client, so nothing else
/// would end the wait.
const ACCEPT_WAIT: Duration = Duration::from_secs(5 * 60);

/// POST /api/projects/{id}/upload|sources/sync with `background=1`. `body` is what the client sent
/// (at most the cap + 1 bytes; `over` past it). `regate` re-checks the session right before the
/// job is written (the body was read for as long as the client took).
///
/// The request's own future only waits for the answer: the work, from the job's first write on,
/// runs in a task of its own, so a client that disconnects (dropping this future) at any point
/// leaves the job to be run or ended there.
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
    let (reply, answer) = tokio::sync::oneshot::channel();
    let app = Arc::clone(app);
    tokio::spawn(async move {
        let job = Job {
            app,
            peer,
            account,
            path,
            project,
            headers,
            body,
            over,
        };
        job.operate(regate, reply).await;
    });
    answer
        .await
        .unwrap_or_else(|_| reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true))
}

/// A start's inputs.
struct Job {
    app: Arc<App>,
    peer: std::net::SocketAddr,
    account: Account,
    path: String,
    project: String,
    headers: HeaderMap,
    body: Vec<u8>,
    over: bool,
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

impl Job {
    async fn operate(
        self,
        regate: impl FnOnce() -> Result<(), Box<Response<Body>>> + Send + 'static,
        reply: tokio::sync::oneshot::Sender<Response<Body>>,
    ) {
        let Job {
            app,
            peer,
            account,
            path,
            project,
            headers,
            body,
            over,
        } = self;
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
        let ending = Ending {
            account: account.clone(),
            owner: owner.clone(),
            job: job.clone(),
        };
        let Some(req) = worker_request(&path, &headers, body) else {
            ending
                .refuse(
                    reply,
                    reply::error(StatusCode::INTERNAL_SERVER_ERROR, "Internal error", true),
                )
                .await;
            return;
        };
        let answered = tokio::time::timeout(ACCEPT_WAIT, async {
            let res = app.proxy.forward(peer, req).await;
            if !is_stream(&res) {
                // Node refused before taking the work (or is not there): its answer, and no job.
                return Err(res);
            }
            let mut lines = Lines::new(res.into_body());
            if lines.next().await == Some(Line::Accepted) {
                Ok(lines)
            } else {
                Err(not_answering())
            }
        })
        .await;
        match answered {
            Ok(Ok(lines)) => {
                let _ = reply.send(respond(work::accepted(&job)));
                run(account, owner, job, lines).await;
            }
            Ok(Err(res)) => ending.refuse(reply, res).await,
            Err(_) => ending.refuse(reply, not_answering()).await,
        }
    }
}

fn not_answering() -> Response<Body> {
    reply::error(
        StatusCode::BAD_GATEWAY,
        "The server is not answering. Please retry.",
        true,
    )
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
    /// The worker did not take the work and this process saw it: the job is interrupted (no
    /// tombstone, so any number of refusals leaves the store as it was), then the client gets
    /// `answer`. A store that cannot be written is retried after the answer.
    async fn refuse(
        self,
        reply: tokio::sync::oneshot::Sender<Response<Body>>,
        answer: Response<Body>,
    ) {
        let ended = self.once().await;
        let _ = reply.send(answer);
        if ended {
            return;
        }
        for _ in 1..12 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if self.once().await {
                return;
            }
        }
        eprintln!("noevia-server: a refused source job could not be ended; the next start does");
    }
}

/// The job after `accepted`: every progress step, then the terminal row (retried while the store
/// cannot be written; interrupted at last, so the slot is never held for good).
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
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

    /// Review #2191 r6+7: the buffer was searched from its start for every frame, so a long line
    /// arriving in small frames cost time quadratic in its length.
    #[tokio::test]
    async fn a_long_line_in_many_frames_is_scanned_once() {
        let (mut tx, rx) = http_body_util::channel::Channel::<bytes::Bytes>::new(8);
        let sender = tokio::spawn(async move {
            tx.send_data(bytes::Bytes::from_static(b"{\"stage\":\""))
                .await
                .unwrap();
            for _ in 0..3000 {
                tx.send_data(bytes::Bytes::from(vec![b'x'; 1024]))
                    .await
                    .unwrap();
            }
            tx.send_data(bytes::Bytes::from_static(b"\"}\n{\"accepted\":true}\n"))
                .await
                .unwrap();
        });
        let mut l = Lines::new(Body::new(rx));
        assert!(matches!(l.next().await, Some(Line::Stage(_))));
        assert_eq!(l.next().await, Some(Line::Accepted));
        assert_eq!(l.next().await, None);
        sender.await.unwrap();
        // Each byte is looked at about once (the stream is 3000 KiB); a rescan per frame is ~1.5e9.
        let twice = 2 * 3000 * 1024;
        assert!(
            l.inspected <= twice,
            "{} bytes searched in a 3000 KiB stream",
            l.inspected
        );
    }

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

    /// Review #2191 r6+7: the store refuses a result over 32 MiB with a bad request, which was taken
    /// for "written": the job stayed active, holding one of the two slots, for good.
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
