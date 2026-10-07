//! Policy core of noevia's code-task egress proxy, ported from `apps/web/server/code-egress.cjs`
//! and `ssrf.cjs` (`isPrivateIp`). Deny by default: a request needs a live task token, a
//! target on the web ports, a host on the task's list, and only public addresses behind it;
//! the connection then goes to the one address that was checked.
#![forbid(unsafe_code)]

mod decision;
mod grant;
mod ip;
mod policy;
mod tokens;

pub use decision::{check_addresses, check_target, status_text, Allowed, Pending, Refusal};
pub use grant::{
    b64url_decode, b64url_encode, canonical_payload, looks_signed, mint, verify, Act, Admission,
    GrantError, GrantKey, Ledger, SignedGrant, GRANT_KEY_LABEL, GRANT_PREFIX, MAX_CLOCK_SKEW_MS,
    MAX_LIFETIME_MS,
};
pub use ip::{is_ip, is_ipv4, is_ipv6, is_private_ip};
pub use policy::{host_allowed, parse_target, Target, ALLOWED_PORTS};
pub use tokens::{
    basic_token, Authorized, Expired, Grant, GrantId, TokenStore, DEFAULT_TOKEN_TTL_MS,
};
