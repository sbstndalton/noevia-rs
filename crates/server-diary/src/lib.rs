//! noevia-core's Diary sidecar client in Rust (sbstndalton/noevia#2185): what Node adds before it
//! proxies a request to the Python Diary sidecar, and the Diary routes that are nothing more than
//! that proxy.
//!
//! * [`config`]: `DIARY_BASE_URL`, `DIARY_AUTH_TOKEN`, `DIARY_TENANT_KEY`, `DIARY_SOURCE` as
//!   core `index.cjs` and `auth-tokens.cjs` read them.
//! * [`tenant`]: core `diary.cjs` `diaryHeaders`: the bearer token, `X-Cowork-User-ID`, the
//!   `X-Cowork-Legacy-Owner` marker, the base64url storage descriptor (a blocked one when the
//!   member-origin policy refuses the endpoint, a `secretRef` instead of the secret when the tenant key is
//!   set) and the v2 HMAC assertion (signed by `crates/tenant-assertion`, the one signer).
//! * [`client`]: core `fetchJson` (redirects are an error, 8 MiB reply cap, per-call deadline) and
//!   `withStorageCredential`: with the tenant key set a call goes out without the storage secret
//!   and is repeated once, re-signed with a fresh nonce, with it after a 428.
//! * [`routes`]: `GET|any /api/diary/source`, `/api/diary/today`, `POST /api/diary/storage-import`,
//!   `GET /api/diary/workspace-export` (300 s, 272 MiB) and `POST /api/diary/workspace-import`
//!   (32 MiB body, 120 s, 24 MiB reply), answer for answer with core `routes/diary.cjs`,
//!   `workspace-export.cjs` and `workspace-import.cjs` (deleted there once the front answers).
//!
//! The sidecar stays the single writer of the Diary: nothing here writes it except by passing on
//! the request the signed-in person made. What stays Node's, and why, is in
//! `contracts/http/README.md` ("sbstndalton/noevia#2185").
//!
//! Secrets: the storage secret and the keys are never formatted into an error, a log line or a
//! `Debug` output, and every failure is one of a few fixed answers.

#![forbid(unsafe_code)]

pub mod body;
pub mod client;
pub mod config;
pub mod routes;
pub mod tenant;

pub use body::{ChannelBody, StreamError};
pub use client::{CallError, JsonReply, Sidecar};
pub use config::DiaryConfig;
pub use routes::{Ctx, Diary, Limits, Reply, ReplyBody, Request, Route};
pub use tenant::{Tenant, TenantError};
