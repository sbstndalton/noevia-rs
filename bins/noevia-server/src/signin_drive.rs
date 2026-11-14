//! The sign-in row's turn from the plan to the turn loop (council p9-3, p7-3 CONTINUE 1: the part
//! that needs no other branch). The provider-row plan (chat_provider.rs, council p6-1/p8-3) hands
//! a cut whose fetch is the sign-in broker (chat-upstream's `Fetch::Broker { owner, conversation }`)
//! to [`plan`]; chat_native.rs then builds the engine with [`front_engine`], the account re-check
//! with [`account_check`] over [`front_live`], and runs the turn with [`run`]. So the plug-in on
//! that branch is a few lines.
//!
//! Inert: nothing calls it. [`plan`] refuses unless core offered
//! [`crate::signin_wire::CAPS_TOKEN`], and no core sends it yet.
//!
//! What [`plan`] holds a sign-in turn to, each failing closed before anything is sent:
//! - core offered the sign-in transport for this turn ([`NOT_OFFERED`]);
//! - the broker's owner is the turn's account (core answers a foreign use 403 before the cut;
//!   here a cut that names another owner is refused again, [`FOREIGN`]);
//! - the frame asks for no local lease, model manager or title (none is the sign-in row's,
//!   [`LOCAL_ENGINE`]);
//! - the frame's `upstreamUrl` is the cut's address ([`ADDRESS`]), the frame names a model
//!   ([`NO_MODEL`]) and the binding's expiry is a finite time ([`BAD_EXPIRY`]).
//!
//! [`run`] refuses an engine built for another account or conversation than the plan's
//! ([`FOREIGN`]); the turn's own requests are then checked by [`SigninHost::admit`] (expiry, the
//! account re-check, the address, the model).

use crate::signin_host::{SigninBinding, SigninHost};
use crate::signin_turn::SigninEngine;
use crate::signin_wire::{self, Accounts, FrontAccounts};
use crate::App;
use chat_turn::host::Host;
use chat_turn::native::{self, Ended, Helpers, Process};
use chat_turn::prepared::Prepared;
use std::sync::Arc;

/// Why a sign-in turn is not run here (logged by the caller; the client sees the native failure
/// text, as for the provider plan's refusals).
pub const NOT_OFFERED: &str = "core did not offer the sign-in transport for this turn";
pub const FOREIGN: &str = "the sign-in broker's owner is not the turn's account";
pub const LOCAL_ENGINE: &str =
    "the frame asks for the local engine's lease, model manager or title on the sign-in row";
pub const ADDRESS: &str = "the frame's upstreamUrl is not the sign-in cut's address";
pub const NO_MODEL: &str = "the frame names no model";
pub const BAD_EXPIRY: &str = "the sign-in binding's expiry is not a time";
pub const NO_BROKER: &str = "the front has no sign-in broker or backend client";
/// The account re-check's error before a request (the turn's user removed or disabled, or the
/// account store unreadable: fail closed).
pub const NOT_ACTIVE: &str = "the turn's account is no longer an active account";

/// The sign-in row's part of a provider cut: the broker's owner and conversation (the chat id,
/// else the space id) and the cut's address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SigninCut<'a> {
    pub owner: &'a str,
    pub conversation: Option<&'a str>,
    pub url: &'a str,
}

/// A sign-in turn the front may run: whose broker, which conversation, and the binding every
/// request is checked against.
#[derive(Debug, Clone, PartialEq)]
pub struct SigninPlan {
    pub owner: String,
    pub conversation: Option<String>,
    pub binding: SigninBinding,
}

/// The plan for a sign-in cut of frame `p` for the turn's account `user`. `caps` is core's
/// capability list for the turn; `expires_ms` the binding's end (the cut's time plus the provider
/// plan's binding life).
pub fn plan(
    caps: &str,
    user: &str,
    cut: &SigninCut<'_>,
    p: &Prepared,
    expires_ms: f64,
) -> Result<SigninPlan, &'static str> {
    if !signin_wire::offered(caps) {
        return Err(NOT_OFFERED);
    }
    if user.is_empty() || cut.owner != user {
        return Err(FOREIGN);
    }
    if p.manager_enabled || p.model_lease || p.title.enabled {
        return Err(LOCAL_ENGINE);
    }
    if cut.url.is_empty() || cut.url != p.upstream_url {
        return Err(ADDRESS);
    }
    if p.model.is_empty() {
        return Err(NO_MODEL);
    }
    if !expires_ms.is_finite() {
        return Err(BAD_EXPIRY);
    }
    Ok(SigninPlan {
        owner: cut.owner.to_owned(),
        conversation: cut.conversation.map(str::to_owned),
        binding: SigninBinding {
            url: cut.url.to_owned(),
            model: p.model.clone(),
            expires_ms,
        },
    })
}

/// The re-check [`SigninHost`] runs before each request: `live(user)` must say the account is
/// active; `false` or an unreadable store ends the request with [`NOT_ACTIVE`].
pub fn account_check<F>(live: F, user: String) -> impl Fn() -> Result<(), &'static str> + Sync
where
    F: Fn(&str) -> Result<bool, ()> + Sync,
{
    move || match live(&user) {
        Ok(true) => Ok(()),
        Ok(false) | Err(()) => Err(NOT_ACTIVE),
    }
}

/// core `asUser` over the front's account store: a `users` row that is not disabled; `Err` when
/// it cannot be read (the same query as edit_target_native.rs).
pub fn front_live(app: &Arc<App>) -> impl Fn(&str) -> Result<bool, ()> + Send + Sync {
    let app = Arc::clone(app);
    move |user: &str| {
        let writer = app.writes.writer().map_err(|_| ())?;
        writer
            .read(|r| {
                r.row(
                    "SELECT 1 FROM users WHERE id=?1 AND (disabled_at IS NULL OR disabled_at=0)",
                    [user],
                    |row| row.get::<_, i64>(0),
                )
            })
            .map(|row| row.is_some())
            .map_err(|_| ())
    }
}

/// The plan's engine over the front's own broker ([`NO_BROKER`] while the front has none).
pub fn front_engine(
    app: &Arc<App>,
    plan: &SigninPlan,
) -> Result<SigninEngine<FrontAccounts>, &'static str> {
    SigninEngine::front(app, &plan.owner, plan.conversation.as_deref()).ok_or(NO_BROKER)
}

/// Runs the rest of the turn on the sign-in row: the native turn loop over [`SigninHost`] around
/// the front's host `inner`. `Err` (nothing sent, the inner host untouched) when the engine is not
/// the plan's account and conversation.
pub async fn run<H, A, C>(
    inner: &H,
    engine: SigninEngine<A>,
    plan: SigninPlan,
    check: C,
    process: &Process,
    helpers: &Helpers<'_>,
    p: &Prepared,
) -> Result<Ended, &'static str>
where
    H: Host,
    A: Accounts + Clone,
    C: Fn() -> Result<(), &'static str> + Sync,
{
    if engine.user != plan.owner || engine.conversation != plan.conversation {
        return Err(FOREIGN);
    }
    let host = SigninHost::new(inner, engine, plan.binding, check);
    Ok(native::run(&host, process, helpers, p).await)
}

#[cfg(test)]
#[path = "signin_drive_tests.rs"]
mod tests;
