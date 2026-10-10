//! noevia-core's request authentication in Rust, read-only (full-Rust migration M2,
//! docs/adr-0001-rust-and-repo-split.md "Amendment 2026-10-10" in sbstndalton/noevia).
//!
//! Ports, verdict for verdict, what core server/index.cjs checks before any signed-in route:
//! `requestAuth.authenticate` (device-auth.cjs createRequestAuth over auth.cjs `authenticate`:
//! the `cowork_session` cookie, the legacy UI_AUTH_TOKEN bearer, native-client `nva_` tokens),
//! `csrfValid`, auth.cjs `originValid` and device-auth.cjs `browserOnly` ([`identity`]);
//! app-passwords.cjs `verifyDav` ([`app_passwords`]); and secrets.cjs key loading/`open` with
//! `secrets.key.previous` rotation ([`secrets`]).
//!
//! Everything reads `cowork.db` through server-store's read-only [`server_store::Store`]. The one
//! writer is [`upkeep`] (M3): the session and device-grant writes Node's gate makes on every
//! request, for when Rust owns those tables (`NOEVIA_RUST_AUTH=1`, server-store's [`server_store::Writer`]). Token, CSRF and legacy-token comparisons are constant-time
//! ([`js::ct_eq`]), and a missing app password costs one Argon2 verification like a wrong one.
//! `tests/differential.rs` checks every verdict against Node's own code
//! (tools/gen-auth-fixtures.cjs) and that Rust never accepts what Node rejects.

pub mod app_passwords;
pub mod config;
pub mod identity;
pub mod js;
pub mod password;
pub mod request;
pub mod secrets;
pub mod upkeep;

pub use config::AuthConfig;
pub use identity::{Authenticator, Credential, CurrentOrigin, Identity, PublicUser, Refusal, Role};
pub use request::Creds;
