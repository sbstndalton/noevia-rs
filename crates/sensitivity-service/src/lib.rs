#![forbid(unsafe_code)]
mod config;
mod http;
mod validation;
mod worker;
pub use http::{serve_with_shutdown, CONNECTION_CAP};
pub use validation::{validate, BODY_CAP};
pub use worker::{Config, Failure, Pool};
#[cfg(test)]
#[path = "../tests/support/oracle.rs"]
mod oracle;
