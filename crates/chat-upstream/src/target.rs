//! Where a turn's model call goes and with what: chat.cjs from the settled provider to the first
//! request (the ChatGPT connection checks at ~1041, the Inference machine swap at ~1243, the
//! address and headers at ~1404, the member-origin check at ~1439, the transport error at ~2151).
//!
//! The provider key is never part of a [`Target`]: the target says whether a bearer is sent
//! ([`KeyUse`]), and [`authorization`] builds the header value from the key the host unseals at
//! call time (so a rotated key is picked up, as Node rebuilds `providerHeaders(provider)`).

use js_json::JValue;

/// The answer chat.cjs gives for a member's own provider whose origin is not approved (400).
pub const ORIGIN_NOT_APPROVED: &str =
    "Provider origin is not approved for member connections; contact an administrator.";
/// The sign-in provider while the sign-in is off on this server (409).
pub const SIGN_IN_OFF: &str = "Sign in with ChatGPT is turned off on this server. Choose another provider in the model popup.";
/// The sign-in provider used outside its owner's own workspace (403).
pub const SIGN_IN_FOREIGN: &str =
    "A ChatGPT connection belongs to one account and cannot be used here.";
/// The sign-in connection expired or was revoked (409).
pub const SIGN_IN_RECONNECT: &str = "Reconnect needed: your ChatGPT sign-in expired or was revoked. Sign in with ChatGPT again in Settings → AI providers.";
/// The sign-in connection is not there (409).
pub const SIGN_IN_MISSING: &str =
    "ChatGPT is not connected. Sign in with ChatGPT in Settings → AI providers.";

/// The address a provider's chat completions are posted to: `baseUrl` without trailing slashes
/// and without one trailing `/v1`, then `/v1/chat/completions` (bare-host and `/v1` bases both
/// work). `None` where Node throws (a `baseUrl` that is not text).
pub fn upstream_url(base_url: &JValue) -> Option<String> {
    let base = base_url.as_str()?;
    // JS `/\/+$/` then `/\/v1$/` (no `m` flag: `$` is the end of the text only).
    let trimmed = base.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    Some(format!("{trimmed}/v1/chat/completions"))
}

/// `providerHeaders`' bearer rule: a key that is not empty and not `local` is sent as
/// `Bearer <key>`; anything else sends no `Authorization` header.
pub fn authorization(api_key: Option<&str>) -> Option<String> {
    match api_key {
        Some(k) if !k.is_empty() && k != "local" => Some(format!("Bearer {k}")),
        _ => None,
    }
}

/// `raw` for Debug output and logs: without user, password, query and fragment (a base address may
/// carry a key in any of them). Anything but a parsed `http:`/`https:` address prints as a
/// placeholder: without a scheme, `user:pass@host` parses as an opaque path the setters cannot clear.
pub fn printable_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut u) if matches!(u.scheme(), "http" | "https") && !u.cannot_be_a_base() => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        _ => "<address not printed>".to_string(),
    }
}

/// The row's `id` for Debug output (a row also holds its key).
pub(crate) fn row_id(row: &JValue) -> Option<&str> {
    row.get("id").as_str()
}

/// The signed-in user as the call path reads it (`authn.user`), or no user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: Option<String>,
    pub admin: bool,
}

/// chat.cjs `memberOwnProvider`: a signed-in non-admin's turn on a provider row that is not shared
/// and is not the default one. Only those are held to the member origin list.
pub fn member_own_provider(user: Option<&User>, row: &JValue, default_id: &str) -> bool {
    let Some(u) = user else { return false };
    !u.admin && !row.get("shared").truthy() && row.get("id").as_str() != Some(default_id)
}

/// `ssrf.cjs` `endpointApproved(authn, rawUrl)`: an `http:`/`https:` address without user or
/// password, and either an admin or an origin listed in `MEMBER_OUTBOUND_ORIGINS` (comma list,
/// each entry trimmed, compared exactly with the URL's origin). Anything unparseable is refused.
pub fn endpoint_approved(admin: bool, member_origins: &str, raw_url: &str) -> bool {
    let Ok(u) = url::Url::parse(raw_url) else {
        return false;
    };
    if !matches!(u.scheme(), "http" | "https") || !u.username().is_empty() || u.password().is_some()
    {
        return false;
    }
    if admin {
        return true;
    }
    let origin = u.origin().ascii_serialization();
    member_origins.split(',').any(|x| x.trim() == origin)
}

/// Which transport carries the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch {
    /// The ordinary outbound fetch (the default engine, a provider row, a route target's row).
    Direct,
    /// One of the owner's Inference machines (LAN/Tailscale only, no key, redirects refused).
    Machine { id: String },
    /// The sign-in broker, for the owner, keyed by the conversation (chat id, else space id).
    Broker {
        owner: String,
        conversation: Option<String>,
    },
}

/// Whether the call sends the row's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyUse {
    /// The host unseals the row's key and sends [`authorization`] of it (none for empty/`local`).
    RowKey,
    /// No key: a machine (chat-machines.cjs sends `apiKey: ''`) or the broker (it adds its own).
    None,
}

/// The sign-in connection's state, as `chatgptOAuth.status(owner).state` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    Connected,
    Reconnect,
    Missing,
}

/// What [`resolve`] reads: the provider row as `getProvider` returned it (after any route or
/// machine swap), and the facts chat.cjs consults on the way.
#[derive(Clone)]
pub struct Turn<'a> {
    pub row: &'a JValue,
    pub default_id: &'a str,
    pub user: Option<&'a User>,
    /// `MEMBER_OUTBOUND_ORIGINS` as read at call time ("" when unset).
    pub member_origins: &'a str,
    /// The Inference machine the turn moved to (`machineTarget.machine.id`), if any.
    pub machine: Option<&'a str>,
    /// `isChatGptProvider(provider)` (after any reroute back to the local default, false).
    pub sign_in: bool,
    /// `chatgptOAuth && chatgptEnabled()`.
    pub sign_in_enabled: bool,
    /// `chatgptOAuth.status(owner).state`, asked only for the sign-in provider.
    pub connection: Connection,
    /// The workspace's owner (`chatWorkspace.userId`), when the workspace has one.
    pub workspace_user: Option<&'a str>,
    pub chat_id: Option<&'a str>,
    pub space_id: Option<&'a str>,
}

impl std::fmt::Debug for Turn<'_> {
    /// The row prints as its id only (it holds the key); the origin list is not printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Turn")
            .field("row", &row_id(self.row))
            .field("default_id", &self.default_id)
            .field("user", &self.user)
            .field("machine", &self.machine)
            .field("sign_in", &self.sign_in)
            .field("sign_in_enabled", &self.sign_in_enabled)
            .field("connection", &self.connection)
            .field("workspace_user", &self.workspace_user)
            .field("chat_id", &self.chat_id)
            .field("space_id", &self.space_id)
            .finish_non_exhaustive()
    }
}

/// A settled call target.
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    pub provider_id: String,
    /// `provider.label` (the transport error names it).
    pub label: String,
    pub url: String,
    pub fetch: Fetch,
    pub key: KeyUse,
    /// The member-own-provider rule applied (and passed).
    pub member_own: bool,
}

impl std::fmt::Debug for Target {
    /// The address prints without userinfo, query or fragment ([`printable_url`]).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("provider_id", &self.provider_id)
            .field("label", &self.label)
            .field("url", &printable_url(&self.url))
            .field("fetch", &self.fetch)
            .field("key", &self.key)
            .field("member_own", &self.member_own)
            .finish()
    }
}

/// Why the call is not sent: Node's status and its `{ error }` text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub status: u16,
    pub error: &'static str,
}

/// The call target of a settled turn, or the answer Node gives instead. `None` where Node throws
/// (a row without a text `baseUrl`). Checks in Node's order: the sign-in provider's (on, owner,
/// connected) before the address; the member origin last, never for the sign-in provider (its
/// destination is a server constant).
pub fn resolve(t: &Turn<'_>) -> Option<Result<Target, Refused>> {
    let row = t.row;
    let mut fetch = Fetch::Direct;
    let mut key = KeyUse::RowKey;
    if let Some(id) = t.machine {
        fetch = Fetch::Machine { id: id.to_string() };
        key = KeyUse::None;
    } else if t.sign_in {
        if !t.sign_in_enabled {
            return Some(Err(Refused {
                status: 409,
                error: SIGN_IN_OFF,
            }));
        }
        let owner = t
            .user
            .and_then(|u| u.id.as_deref())
            .filter(|s| !s.is_empty());
        let foreign =
            matches!((t.workspace_user, owner), (Some(w), Some(o)) if !w.is_empty() && w != o);
        let Some(owner) = owner.filter(|_| !row.get("shared").truthy() && !foreign) else {
            return Some(Err(Refused {
                status: 403,
                error: SIGN_IN_FOREIGN,
            }));
        };
        match t.connection {
            Connection::Connected => {}
            Connection::Reconnect => {
                return Some(Err(Refused {
                    status: 409,
                    error: SIGN_IN_RECONNECT,
                }))
            }
            Connection::Missing => {
                return Some(Err(Refused {
                    status: 409,
                    error: SIGN_IN_MISSING,
                }))
            }
        }
        // `chatId || spaceId || null`: an empty chat id falls through to the space.
        let conversation = [t.chat_id, t.space_id]
            .into_iter()
            .flatten()
            .find(|s| !s.is_empty())
            .map(str::to_string);
        fetch = Fetch::Broker {
            owner: owner.to_string(),
            conversation,
        };
        key = KeyUse::None;
    }
    let url = upstream_url(row.get("baseUrl"))?;
    let member_own = member_own_provider(t.user, row, t.default_id);
    if member_own && !matches!(fetch, Fetch::Broker { .. }) {
        let admin = t.user.is_some_and(|u| u.admin);
        if !endpoint_approved(admin, t.member_origins, &url) {
            return Some(Err(Refused {
                status: 400,
                error: ORIGIN_NOT_APPROVED,
            }));
        }
    }
    Some(Ok(Target {
        provider_id: js_text(row.get("id")),
        label: js_text(row.get("label")),
        url,
        fetch,
        key,
        member_own,
    }))
}

/// The error event text for a call that failed before any answer on a non-machine target
/// (`${provider.label} unreachable: ${err.message}`). A machine's text is its own
/// (`chatMachines.connectError`, code `machineUnreachable`): not this.
pub fn unreachable_text(label: &str, message: &str) -> String {
    format!("{label} unreachable: {message}")
}

/// `String(v)` for the text fields a row carries (undefined → "undefined", as a template does).
fn js_text(v: &JValue) -> String {
    js_json::to_js_string(v).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_shapes() {
        let u = |s: &str| upstream_url(&JValue::Str(s.into()));
        assert_eq!(
            u("http://h:1").as_deref(),
            Some("http://h:1/v1/chat/completions")
        );
        assert_eq!(
            u("http://h/v1/").as_deref(),
            Some("http://h/v1/chat/completions")
        );
        assert_eq!(
            u("https://h/api/v1").as_deref(),
            Some("https://h/api/v1/chat/completions")
        );
        assert_eq!(
            u("http://h/v1/v1//").as_deref(),
            Some("http://h/v1/v1/chat/completions")
        );
        assert_eq!(upstream_url(&JValue::Undefined), None);
    }

    #[test]
    fn bearer_rule() {
        assert_eq!(authorization(Some("k")).as_deref(), Some("Bearer k"));
        assert_eq!(authorization(Some("local")), None);
        assert_eq!(authorization(Some("")), None);
        assert_eq!(authorization(None), None);
    }
}
