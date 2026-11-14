//! The two calls a front makes for a provider-row turn: [`cut`] once, where the turn is handed off (resolve the
//! target in Node's order, then bind it), and [`rows_now`] before each request (the facts the re-check reads,
//! computed by the same rules [`crate::target::resolve`] applied at the cut). With these the front's part is
//! its provider store lookups and the key's unsealing.

use js_json::JValue;

use crate::binding::{fingerprint, Binding};
use crate::host::{RowsNow, Unsupported};
use crate::target::{endpoint_approved, member_own_provider, resolve, Fetch, KeyUse, Refused, Target, Turn, User};

/// A settled, bound provider-row target.
#[derive(Debug, Clone, PartialEq)]
pub struct Cut {
    pub target: Target,
    pub binding: Binding,
}

/// Why a turn is not cut for the provider host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotCut {
    /// Node answers the turn itself with this status and text (the sign-in checks, an unapproved origin).
    Node(Refused),
    /// Node throws (a row without a text `baseUrl`): the turn stays in Node, which fails it as it does now.
    Throws,
    /// Not a row this host takes (the default provider, a machine, the broker).
    Unsupported(Unsupported),
    /// No signed-in user to bind the turn to: it stays in Node (fail closed).
    NoUser,
}

/// The target and binding of `turn` for `model`, bound until `expires_ms`.
pub fn cut(
    turn: &Turn<'_>,
    model: &str,
    chat_key: Option<&str>,
    expires_ms: f64,
) -> Result<Cut, NotCut> {
    let target = match resolve(turn) {
        None => return Err(NotCut::Throws),
        Some(Err(r)) => return Err(NotCut::Node(r)),
        Some(Ok(t)) => t,
    };
    if target.provider_id == turn.default_id {
        return Err(NotCut::Unsupported(Unsupported::DefaultProvider));
    }
    if target.fetch != Fetch::Direct || target.key != KeyUse::RowKey {
        return Err(NotCut::Unsupported(Unsupported::Transport));
    }
    let user = turn
        .user
        .and_then(|u| u.id.as_deref())
        .filter(|id| !id.is_empty())
        .ok_or(NotCut::NoUser)?;
    let binding = Binding {
        user: user.to_string(),
        provider_id: target.provider_id.clone(),
        url: target.url.clone(),
        fingerprint: fingerprint(turn.row),
        model: model.to_string(),
        chat_key: chat_key.map(str::to_string),
        expires_ms,
    };
    Ok(Cut { target, binding })
}

/// The re-check's facts as of one request: `user` and `active` as the account store says now, `row` the row with
/// the bound id looked up as this user now (exactly: no default fallback), `member_origins` the
/// `MEMBER_OUTBOUND_ORIGINS` read now. The member-own rule and the origin check are [`resolve`]'s.
pub fn rows_now(
    user: Option<&User>,
    active: bool,
    row: Option<&JValue>,
    default_id: &str,
    member_origins: &str,
    binding: &Binding,
) -> RowsNow {
    RowsNow {
        user: user
            .and_then(|u| u.id.clone())
            .unwrap_or_default(),
        user_active: active,
        row: row.cloned(),
        member_own: row.is_some_and(|r| member_own_provider(user, r, default_id)),
        origin_approved: endpoint_approved(
            user.is_some_and(|u| u.admin),
            member_origins,
            &binding.url,
        ),
    }
}
