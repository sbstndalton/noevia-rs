//! `executeToolCall` as a whole: [`route`](crate::route) the call, run the executor it names through the
//! host's [`Executors`], and end it as core does ([`Outcome`]). The host implements one method per
//! executor; nothing here knows how a tool runs.

use crate::{fence_refused, finish, refused, route, Offer, Outcome, Route};
use js_json::JValue;
use std::future::Future;

/// What one executor answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ran {
    /// The tool's text; `flagged` when the executor marked the call failed itself (core's `fail(...)`,
    /// used by the built-ins). Delegated failures (`ERROR: ` / `ERROR from tool: `) are found in the text.
    Text { text: String, flagged: bool },
    /// The approval fence refused the write at its outbound boundary (`tool-write-fence.cjs` `send`).
    FenceRefused,
}

impl Ran {
    /// A tool's text, not flagged.
    pub fn text(text: impl Into<String>) -> Self {
        Ran::Text {
            text: text.into(),
            flagged: false,
        }
    }

    /// A built-in's own failure text.
    pub fn failed(text: impl Into<String>) -> Self {
        Ran::Text {
            text: text.into(),
            flagged: true,
        }
    }
}

/// The executors one call can go to; each gets the parsed arguments (a plain object).
pub trait Executors {
    /// The coworker page tool.
    fn specialist(&self, args: &JValue) -> impl Future<Output = Ran> + Send;
    /// An MCP tool: `server` the routed one, `None` = the executor picks among the discovered servers (and
    /// refuses a name more than one offers).
    fn mcp(&self, name: &str, server: Option<&str>, args: &JValue)
        -> impl Future<Output = Ran> + Send;
    /// An offline-encyclopedia tool.
    fn kiwix(&self, name: &str, args: &JValue) -> impl Future<Output = Ran> + Send;
    /// A Drive tool.
    fn drive(&self, name: &str, args: &JValue) -> impl Future<Output = Ran> + Send;
    /// The clock; an unknown zone answers `Ran::failed(unknown_zone(zone))`.
    fn current_time(&self, timezone: Option<&str>) -> impl Future<Output = Ran> + Send;
    /// The project file read.
    fn read_project_file(&self, args: &JValue) -> impl Future<Output = Ran> + Send;
    /// A call whose arguments this port does not read as `JSON.parse` does ([`Route::NotPorted`]): the
    /// host hands it, unchanged, to core's own executor and returns core's answer.
    fn not_ported(&self, name: &str, raw_args: &str) -> impl Future<Output = Outcome> + Send;
}

/// `executeToolCall(project, name, rawArgs, allowed, signal, outcome, options)` over `executors`.
pub async fn execute<E: Executors>(
    executors: &E,
    name: &str,
    raw_args: &str,
    offer: &Offer<'_>,
) -> Outcome {
    let ran = match route(name, raw_args, offer) {
        Route::Refused(text) => return refused(text),
        Route::NotPorted => return executors.not_ported(name, raw_args).await,
        Route::Specialist(args) => executors.specialist(&args).await,
        Route::Mcp { server, args } => executors.mcp(name, server.as_deref(), &args).await,
        Route::Kiwix(args) => executors.kiwix(name, &args).await,
        Route::Drive(args) => executors.drive(name, &args).await,
        Route::CurrentTime { timezone } => executors.current_time(timezone.as_deref()).await,
        Route::ReadProjectFile(args) => executors.read_project_file(&args).await,
    };
    match ran {
        Ran::Text { text, flagged } => finish(text, flagged),
        Ran::FenceRefused => fence_refused(),
    }
}

/// Whether core counts the call in the account's tool use (`recordToolUse`): every call that was not
/// refused by the fence, refusals of [`route`] included (core counts a name it was never offered, or one
/// that does not exist; noevia#2668). The host decides whether to keep that.
pub fn counted_by_core(outcome: &Outcome) -> bool {
    !outcome.not_run
}
