//! One relayed request, start to end, as a pure decision: the caller (a relay endpoint, or the
//! front's own call path) reads `{turnToken, body}`, asks [`admit`] with the facts as they are now,
//! and either sends the [`Admitted`] request or answers the typed [`Refusal`]. After the call it
//! answers with [`crate::envelope::forward`] headers, or [`transport_error`] when the provider
//! never answered.

use js_json::JValue;

use crate::binding::{self, Binding, Now, Recheck, TokenError};
use crate::plan::{self, Call, Request};
use crate::target::{KeyUse, Target};

/// A relay request body over this many bytes is refused unread (the chat's own body cap,
/// rust-chat-native.cjs CHAT_BODY_CAP, is 32 MiB; a round's body is below it).
pub const BODY_CAP: usize = 32 * 1024 * 1024;

/// Why a relayed request is not sent: the relay's own status (never the provider's: the answer
/// carries no [`crate::envelope::STATUS_HEADER`]) and a fixed code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// 400: the body is not `{turnToken: text, body: object}` or is over [`BODY_CAP`].
    BadRequest,
    /// 401: the token is malformed or its MAC is wrong.
    BadToken,
    /// 410: the binding expired.
    Expired,
    /// 403: another user, or the user is no longer active.
    UserChanged,
    /// 409: the row is gone or changed (address, sharing).
    RowChanged,
    /// 403: a member's own row whose origin is no longer approved.
    OriginRefused,
}

impl Refusal {
    pub fn status(self) -> u16 {
        match self {
            Refusal::BadRequest => 400,
            Refusal::BadToken => 401,
            Refusal::Expired => 410,
            Refusal::UserChanged | Refusal::OriginRefused => 403,
            Refusal::RowChanged => 409,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Refusal::BadRequest => "bad-request",
            Refusal::BadToken => "bad-token",
            Refusal::Expired => "expired",
            Refusal::UserChanged => "user-changed",
            Refusal::RowChanged => "row-changed",
            Refusal::OriginRefused => "origin-refused",
        }
    }
}

/// A request that may be sent: the binding it was admitted under, the body with the bound model,
/// and the request (headers with the key as unsealed now).
#[derive(Debug, Clone)]
pub struct Admitted {
    pub binding: Binding,
    pub body: JValue,
    pub request: Request,
}

/// The facts of this request, all read now by the caller.
pub struct Facts<'a> {
    pub secret: &'a [u8],
    pub now_ms: f64,
    pub now: Now<'a>,
    /// The bound target as resolved again for this request (its key use decides the bearer).
    pub key: KeyUse,
    /// The row's key, unsealed for this request.
    pub row_key: Option<&'a str>,
    pub call: Call,
}

/// Read a relay body (`{turnToken, body}`) and decide. Checks in order: shape and size, token,
/// expiry, re-check (user, row, origin); then the model is pinned and the request built.
pub fn admit(raw: &[u8], f: &Facts<'_>) -> Result<Admitted, Refusal> {
    if raw.len() > BODY_CAP {
        return Err(Refusal::BadRequest);
    }
    let text = std::str::from_utf8(raw).map_err(|_| Refusal::BadRequest)?;
    let v = js_json::parse(text).map_err(|_| Refusal::BadRequest)?;
    let token = v.get("turnToken").as_str().ok_or(Refusal::BadRequest)?;
    let mut body = v.get("body").clone();
    if !matches!(body, JValue::Obj(_)) {
        return Err(Refusal::BadRequest);
    }
    let b = binding::open(f.secret, token, f.now_ms).map_err(|e| match e {
        TokenError::Expired => Refusal::Expired,
        TokenError::Malformed | TokenError::BadMac => Refusal::BadToken,
    })?;
    match binding::recheck(&b, &f.now) {
        Recheck::Ok => {}
        Recheck::UserChanged => return Err(Refusal::UserChanged),
        Recheck::RowGone | Recheck::RowChanged => return Err(Refusal::RowChanged),
        Recheck::OriginRefused => return Err(Refusal::OriginRefused),
    }
    binding::pin_model(&mut body, &b.model);
    let target = Target {
        provider_id: b.provider_id.clone(),
        label: String::new(),
        url: b.url.clone(),
        fetch: crate::target::Fetch::Direct,
        key: f.key,
        member_own: f.now.member_own,
    };
    let request = plan::request(&target, f.call, f.row_key);
    Ok(Admitted {
        binding: b,
        body,
        request,
    })
}

/// The relay's answer when the provider never answered (a socket error, a refused redirect, the
/// call's deadline): `{transportError: message}`. The executor shows
/// [`crate::target::unreachable_text`] of the provider's label and this message.
pub fn transport_error(message: &str) -> JValue {
    JValue::obj([("transportError", JValue::Str(message.to_string()))])
}

/// The message of a relay answer that is a transport error, if it is one.
pub fn read_transport_error(answer: &JValue) -> Option<&str> {
    answer.get("transportError").as_str()
}
