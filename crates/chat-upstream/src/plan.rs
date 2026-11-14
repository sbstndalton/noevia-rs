//! One request of a turn to its provider, as chat.cjs sends it: always `POST` with redirects
//! refused (`redirect: 'error'`), JSON headers plus the bearer when the target sends the row's key
//! (rebuilt for every request from the key as it is now), and the deadline of its call site:
//! a streamed round has none (its idle limit is the executor's), the non-streamed fallback of a
//! round 300 s (~2277), the context summary 180 s (~1911).

use crate::target::{authorization, printable_url, KeyUse, Target};

/// Which call site a request is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    /// A streamed tool round (and its tool-choice and tools-off retries).
    Round,
    /// The non-streamed fallback of a round.
    Fallback,
    /// The context summary of the compaction step.
    Summary,
}

impl Call {
    /// The call's own deadline in milliseconds (`AbortSignal.timeout`), if it has one.
    pub fn deadline_ms(self) -> Option<u64> {
        match self {
            Call::Round => None,
            Call::Fallback => Some(300_000),
            Call::Summary => Some(180_000),
        }
    }
}

/// What the host sends. `headers` hold the key in clear when one is sent: never log them.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub deadline_ms: Option<u64>,
}

impl std::fmt::Debug for Request {
    /// Header values are not printed (one of them may be the key), nor the address's userinfo,
    /// query or fragment.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("Request")
            .field("url", &printable_url(&self.url))
            .field("headers", &names)
            .field("deadline_ms", &self.deadline_ms)
            .finish()
    }
}

/// The request for `call` on `target`, with `row_key` the row's key as unsealed now (ignored when
/// the target sends no key). Header order is `providerHeaders`': `Content-Type`, then
/// `Authorization`.
pub fn request(target: &Target, call: Call, row_key: Option<&str>) -> Request {
    let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    if target.key == KeyUse::RowKey {
        if let Some(v) = authorization(row_key) {
            headers.push(("Authorization".to_string(), v));
        }
    }
    Request {
        url: target.url.clone(),
        headers,
        deadline_ms: call.deadline_ms(),
    }
}
