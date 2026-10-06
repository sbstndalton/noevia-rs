//! Policy core of noevia's code-task egress proxy, ported from `apps/web/server/code-egress.cjs`
//! and `ssrf.cjs` (`isPrivateIp`). Deny by default: a request needs a live task token, a
//! target on the web ports, a host on the task's list, and only public addresses behind it;
//! the connection then goes to the one address that was checked.
#![forbid(unsafe_code)]

mod decision;
mod ip;
mod policy;
mod tokens;

pub use decision::{check_addresses, check_target, status_text, Allowed, Pending, Refusal};
pub use ip::{is_ip, is_ipv4, is_ipv6, is_private_ip};
pub use policy::{host_allowed, parse_target, Target, ALLOWED_PORTS};
pub use tokens::{Authorized, Expired, Grant, GrantId, TokenStore, DEFAULT_TOKEN_TTL_MS};
