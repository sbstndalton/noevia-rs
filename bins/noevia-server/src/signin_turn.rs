//! The sign-in row's engine call as the native turn reads one (p5-1 CONTINUE 1, the transport half):
//! [`crate::signin_wire`]'s fetch, answered as chat-turn's [`TurnAnswer`] with a body the turn
//! reads through [`read`] and [`text`] instead of the HTTP reply [`crate::signin_wire::into_response`]
//! builds.
//!
//! Inert: nothing calls it yet. Its caller is the native turn's host for the sign-in row (a
//! wrapper like chat-upstream's provider host, whose `request`/`read`/`text` hand over to
//! [`SigninEngine::request`], [`read`] and [`text`]); that host is built only when core offered
//! [`crate::signin_wire::CAPS_TOKEN`], and no core sends it yet.
//!
//! What the turn reads, as core's chat loop reads the answer of the sign-in provider's fetch:
//! - core's own error answer: its status, `application/json`, the provider-message label and
//!   [`signin_transport::reply::error_body`];
//! - a completion: 200 `application/json` and the completion text;
//! - a stream: 200 `text/event-stream` and the chat-completions frames as they are converted;
//! - a labelled backend answer: its status, the allow-listed headers
//!   ([`crate::signin_wire::PASSED_ON`]) and its body as it arrives;
//! - a rejected fetch: the request's error (the turn's failed fetch).
//!
//! Only the request's address and body are used: the sign-in fetch adds its own headers (core's
//! `fetchFor(url, { body })` takes no others), so a lease or a row key never reaches the backend.
//!
//! Named differences: the turn's request deadline (`timeout_ms`, the summary's and the title's)
//! covers the fetch and every later read of the body, and both it and the client leaving end the
//! call with [`ABORTED`]; core's abort gives the same text, its timeout text is not compared.

use crate::signin_wire::{fetch_for, passed_on, Accounts, SigninWire, Transport, SESSIONS};
use crate::App;
use chat_turn::host::{Answer as TurnAnswer, EngineRequest, Read};
use signin_transport::fetch::{Answer, Body, Streaming};
use signin_transport::session::SessionCache;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::time::Instant;

/// The error of a call the client's leaving or the request's deadline ended (the front's own
/// abort text, chat_native.rs).
pub const ABORTED: &str = "This operation was aborted";

/// The answer's body as the turn reads it.
pub struct TurnBody<B> {
    inner: Inner<B>,
    /// The request's deadline, if it had one: a read after it fails with [`ABORTED`].
    deadline: Option<Instant>,
}

enum Inner<B> {
    /// Bytes the front made itself (an error answer, a completion); `None` once read.
    Bytes(Option<Vec<u8>>),
    /// The backend's own body (a labelled pass-through); `None` once it ended or failed.
    Raw(Option<B>),
    /// The converted stream.
    Stream(Box<Streaming<B>>),
}

impl<B> std::fmt::Debug for TurnBody<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.inner {
            Inner::Bytes(b) => match b {
                Some(_) => "bytes",
                None => "bytes (read)",
            },
            Inner::Raw(r) => match r {
                Some(_) => "raw",
                None => "raw (ended)",
            },
            Inner::Stream(_) => "stream",
        };
        f.debug_struct("TurnBody")
            .field("kind", &kind)
            .field("deadline", &self.deadline.is_some())
            .finish()
    }
}

fn header(name: &str, value: &str) -> (String, String) {
    (name.to_owned(), value.to_owned())
}

/// The fetch's answer as the turn's engine answer. `Err(message)`: core's fetch rejected.
pub fn turn_answer<B>(
    answer: Answer<B>,
    deadline: Option<Instant>,
) -> Result<TurnAnswer<TurnBody<B>>, String> {
    let body = |inner| TurnBody { inner, deadline };
    match answer {
        Answer::Thrown(m) => Err(m),
        Answer::Error(r) => Ok(TurnAnswer {
            status: r.status,
            headers: vec![
                header("content-type", "application/json"),
                header("x-noevia-provider-message", "1"),
            ],
            body: body(Inner::Bytes(Some(
                signin_transport::reply::error_body(&r).into_bytes(),
            ))),
        }),
        Answer::Completion(text) => Ok(TurnAnswer {
            status: 200,
            headers: vec![header("content-type", "application/json")],
            body: body(Inner::Bytes(Some(text.into_bytes()))),
        }),
        Answer::Passthrough(u) => Ok(TurnAnswer {
            status: u.status,
            headers: u
                .headers
                .into_iter()
                .filter(|(k, _)| passed_on(k))
                .map(|(k, v)| (k.to_ascii_lowercase(), v))
                .collect(),
            body: body(Inner::Raw(u.body)),
        }),
        Answer::Stream(st) => Ok(TurnAnswer {
            status: 200,
            headers: vec![header("content-type", "text/event-stream")],
            body: body(Inner::Stream(st)),
        }),
    }
}

async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

async fn next_of<B: Body>(inner: &mut Inner<B>) -> Read {
    match inner {
        Inner::Bytes(b) => match b.take() {
            Some(v) if !v.is_empty() => Read::Chunk(v),
            _ => Read::End,
        },
        Inner::Raw(r) => {
            let Some(raw) = r.as_mut() else {
                return Read::End;
            };
            match raw.chunk().await {
                Some(Ok(c)) => Read::Chunk(c),
                Some(Err(e)) => {
                    *r = None;
                    Read::Failed(e)
                }
                None => {
                    *r = None;
                    Read::End
                }
            }
        }
        Inner::Stream(st) => loop {
            match st.next().await {
                Some(Ok(frames)) if frames.is_empty() => continue,
                Some(Ok(frames)) => return Read::Chunk(frames.concat().into_bytes()),
                Some(Err(e)) => return Read::Failed(e),
                None => return Read::End,
            }
        },
    }
}

enum Ended {
    Read(Read),
    Left,
    Deadline,
}

/// The next read of the body. `left` resolves when the client went away. No idle limit: core
/// applies the local idle limit to the default provider and machines only, not to this row.
///
/// When several are ready at once the order is fixed (council p9-3): the client leaving, then the
/// deadline, then the body's data, so a read past the deadline fails even with data waiting. Once
/// the deadline ended a read the body is let go and the deadline cleared: later reads end.
pub async fn read<B: Body>(body: &mut TurnBody<B>, left: impl Future<Output = ()>) -> Read {
    let deadline = body.deadline;
    let ended = tokio::select! {
        biased;
        () = left => Ended::Left,
        () = until(deadline) => Ended::Deadline,
        r = next_of(&mut body.inner) => Ended::Read(r),
    };
    match ended {
        Ended::Read(r) => r,
        Ended::Left => {
            // The turn's own abort: the backend connection is let go.
            body.inner = Inner::Bytes(None);
            Read::Aborted
        }
        Ended::Deadline => {
            body.inner = Inner::Bytes(None);
            body.deadline = None;
            Read::Failed(ABORTED.to_owned())
        }
    }
}

/// The rest of the body as text, at most `cap` bytes, and whether it was cut (the front's own
/// `text`: a failed or aborted read ends it with what came so far).
pub async fn text<B: Body>(
    mut body: TurnBody<B>,
    cap: usize,
    left: impl Future<Output = ()>,
) -> (String, bool) {
    let mut left = std::pin::pin!(left);
    let mut out: Vec<u8> = Vec::new();
    while out.len() <= cap {
        match read(&mut body, left.as_mut()).await {
            Read::Chunk(c) => out.extend_from_slice(&c),
            _ => break,
        }
    }
    let capped = out.len() > cap;
    out.truncate(cap);
    (String::from_utf8_lossy(&out).into_owned(), capped)
}

/// The process's backend client (no proxy, no redirect followed); `None` if it cannot be built,
/// and then the sign-in row's turn is refused rather than sent with another client.
pub static CLIENT: LazyLock<Option<reqwest::Client>> = LazyLock::new(crate::signin_wire::client);

/// One turn's sign-in engine: the broker, the backend client, core's texts and address, and the
/// turn's user and conversation (the chat id, else the space id).
pub struct SigninEngine<A> {
    pub accounts: A,
    pub client: reqwest::Client,
    pub transport: Transport,
    pub user: String,
    pub conversation: Option<String>,
}

impl SigninEngine<crate::signin_wire::FrontAccounts> {
    /// The engine over the front's own broker; `None` while the front has no sign-in broker or
    /// no client could be built.
    pub fn front(
        app: &Arc<App>,
        user: &str,
        conversation: Option<&str>,
    ) -> Option<SigninEngine<crate::signin_wire::FrontAccounts>> {
        let accounts = crate::signin_wire::FrontAccounts::new(app)?;
        let client = CLIENT.as_ref()?.clone();
        let transport = app.chatgpt_oauth.as_ref()?.transport();
        Some(SigninEngine {
            accounts,
            client,
            transport,
            user: user.to_owned(),
            conversation: conversation.map(str::to_owned),
        })
    }
}

impl<A: Accounts + Clone> SigninEngine<A> {
    /// One engine request of the turn over the process's session cache.
    pub async fn request(
        &self,
        request: &EngineRequest,
        left: impl Future<Output = ()>,
    ) -> Result<TurnAnswer<TurnBody<crate::signin_wire::HttpBody>>, String> {
        self.request_with(&SESSIONS, request, left).await
    }

    /// [`SigninEngine::request`] over a given cache (the tests' own).
    pub async fn request_with(
        &self,
        cache: &Mutex<SessionCache>,
        request: &EngineRequest,
        left: impl Future<Output = ()>,
    ) -> Result<TurnAnswer<TurnBody<crate::signin_wire::HttpBody>>, String> {
        let deadline = request
            .timeout_ms
            .map(|ms| Instant::now() + Duration::from_millis(ms));
        let mut wire = SigninWire {
            accounts: self.accounts.clone(),
            client: self.client.clone(),
        };
        let call = fetch_for(
            &mut wire,
            cache,
            &self.transport,
            &self.user,
            self.conversation.as_deref(),
            &request.url,
            &request.body,
        );
        // The client leaving and the deadline win over an answer that is ready at the same time.
        let answer = tokio::select! {
            biased;
            () = left => return Err(ABORTED.to_owned()),
            () = until(deadline) => return Err(ABORTED.to_owned()),
            a = call => a,
        };
        turn_answer(answer, deadline)
    }
}

#[cfg(test)]
#[path = "signin_turn_tests.rs"]
mod tests;
