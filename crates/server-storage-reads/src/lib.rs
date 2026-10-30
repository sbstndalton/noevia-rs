//! Private, unintegrated read engine for noevia#1368.
//!
//! Private owned child plain HTTP is synthetic-qualified and unintegrated.
//! Private storage row projection binds an already supplied Identity and loaded
//! SecretKeys. Identity fields are public and require a real gate at a future route.
//! No route is integrated; the snapshot is read only and does not upgrade envelopes.
//! A future transport must enforce deadlines. Writes (PUT/MKCOL/DELETE, noevia#2191 round 3) are
//! only in [`writes`], each a port of one core storage-client.cjs write.
#![forbid(unsafe_code)]

pub mod admission;
pub mod child_http;
pub mod child_request;
pub mod connection_row;
pub mod owner_secret;

pub mod isolated;
pub mod json;
pub mod protocol;
pub mod request_ipc;
pub mod response_ipc;
pub mod stream;
pub mod transport;
pub mod writes;

pub const LIST_BYTES: usize = 4 * 1024 * 1024;
pub const TEXT_BYTES: usize = 800_000;
pub const PREVIEW_UNITS: usize = 200_000;
pub const MAX_PAGES: usize = 20;
pub const MAX_ENTRIES: usize = 10_000;
pub const BODY_BYTES: usize = 1024 * 1024;
pub const JSON_DEPTH: usize = 128;
pub const LIST_ENTRY_BYTES: usize = 16 * 1024 * 1024;
pub const LIST_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
/// The largest body one storage write carries: core's DOCUMENT_UPLOAD_CAP (25 MiB, also
/// upload-sniff's CAP). A larger original (a PDF up to pdf-reduce's 60 MiB) is not written through
/// this client yet (noevia#2191 round 3 gap: it needs a streamed child body).
pub const WRITE_BYTES: usize = 25 * 1024 * 1024;
/// One ranged chunk of a streamed binary read (noevia#2191 round 8, [`stream`]): the most one child
/// reply carries. The child's 256 MiB address-space limit, the 64 MiB wire cap and `WRITE_BYTES`
/// are unchanged; a file larger than this is read as several `Range` requests, each in its own child.
pub const CHUNK_BYTES: usize = 8 * 1024 * 1024;
/// The reply payload cap on the wire of a chunk request: [`CHUNK_PREFIX`] bytes the child derives
/// from the response headers, then at most [`CHUNK_BYTES`] of body.
pub const CHUNK_FRAME: usize = CHUNK_BYTES + CHUNK_PREFIX;
/// A chunk reply's prefix: three big-endian u64, `u64::MAX` for what the response does not say: the
/// declared total (the `Content-Range` total of a 206, the `Content-Length` of a 200), the first and the
/// last byte of the range the response claims to carry (a 200 claims the whole file: 0 and unknown).
pub const CHUNK_PREFIX: usize = 24;
/// The largest reply payload cap any request may carry.
pub const MAX_FRAME: usize = CHUNK_FRAME;
/// The whole-file cap of a streamed binary read: noevia's upload limit (core `PROJECT_DOCUMENT_READ_BYTES`
/// default), checked on the declared size before the body and on the bytes actually received.
pub const FILE_CAP: u64 = 96 * 1024 * 1024;
/// A PROPFIND's request body: the fixed `<d:propfind>` query, far below this.
pub const PROPFIND_BODY_BYTES: usize = 4096;
/// The longest ETag a write reply carries; a longer one is treated as none (no conditional cleanup).
pub const ETAG_BYTES: usize = 1024;

/// Public failure text is fixed or derived only from the requested filename/status.
/// Do not insert transport diagnostics, endpoints, credentials or response text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub status: u16,
    pub message: String,
    message_units: Option<Vec<u16>>,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Failure {}

impl Failure {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            message_units: None,
        }
    }
    pub fn units(status: u16, units: Vec<u16>) -> Self {
        Self {
            status,
            message: String::from_utf16_lossy(&units),
            message_units: Some(units),
        }
    }
    pub fn json(&self) -> String {
        let mut out = String::from("{\"error\":");
        let fallback = self.message.encode_utf16().collect::<Vec<_>>();
        json::quote_units(&mut out, self.message_units.as_deref().unwrap_or(&fallback));
        out.push('}');
        out
    }
}

/// Canonical decoded-stream cap. Unlike Node's chunk-dependent legacy helper,
/// any byte beyond the cap marks overflow, including overflow in the last chunk.
/// Caller supplies decoded bytes and must drop/cancel at `done()`; no drain.
pub struct Prefix {
    bytes: Vec<u8>,
    cap: usize,
    overflow: bool,
}

impl Prefix {
    pub fn new(cap: usize) -> Result<Self, Failure> {
        if cap > LIST_BYTES {
            return Err(Failure::new(502, "fetch failed"));
        }
        Ok(Self {
            bytes: Vec::new(),
            cap,
            overflow: false,
        })
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        let remaining = self.cap.saturating_sub(self.bytes.len());
        let take = remaining.min(bytes.len());
        if let Some(prefix) = bytes.get(..take) {
            self.bytes.extend_from_slice(prefix);
        }
        self.overflow |= bytes.len() > remaining;
    }
    pub fn done(&self) -> bool {
        self.overflow
    }
    pub fn finish(self) -> Decoded {
        let mut text = String::from_utf8_lossy(&self.bytes).into_owned();
        // Node removes a terminal replacement at an overflow boundary even if
        // the original stream genuinely ended this prefix with U+FFFD.
        if self.overflow && text.ends_with('\u{fffd}') {
            text.pop();
        }
        Decoded {
            text,
            capped: self.overflow,
        }
    }
}

pub struct Decoded {
    pub text: String,
    pub capped: bool,
}

/// JSON.stringify-compatible UTF-16 preview, including a split surrogate pair.
pub fn preview(name: &str, body: Decoded) -> String {
    let units: Vec<_> = body.text.encode_utf16().take(PREVIEW_UNITS + 1).collect();
    let truncated = body.capped || units.len() > PREVIEW_UNITS;
    let mut out = String::from("{\"name\":");
    json::quote_units(&mut out, &name.encode_utf16().collect::<Vec<_>>());
    out.push_str(",\"content\":");
    json::quote_units(
        &mut out,
        units
            .get(..units.len().min(PREVIEW_UNITS))
            .unwrap_or_default(),
    );
    out.push_str(if truncated {
        ",\"truncated\":true}"
    } else {
        ",\"truncated\":false}"
    });
    out
}
