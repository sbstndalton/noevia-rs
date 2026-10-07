//! Upstream model-provider error classification (sbstndalton/noevia#1002).
//!
//! noevia-core's `server/chat-context.cjs` `providerError` used to replace every upstream failure
//! with one fixed sentence ("The model stream failed…"), so a llama.cpp 400 such as "Unable to
//! generate parser for this template… Conversation roles must alternate…" never reached the
//! user. [`classify`] sorts an error (HTTP status and body text) into a [`Kind`] and returns a
//! short reason taken from the upstream message, sanitised by [`sanitize`]: control characters
//! flattened, URLs, filesystem paths, host addresses, bearer/basic credentials, `key=value`
//! secrets and token-shaped words replaced, and the result capped at [`MAX_REASON_CHARS`].
//!
//! The context-full test is the JS one, kept exactly (`/context.*(exceed|full|length)|too many
//! tokens|maximum context/i` on the whole text, `.` stopping at JS line terminators, ASCII-only
//! case folding as JS's non-unicode `/i` does); the shared fixture table checks that.
//!
//! The input is a third party's output: untrusted. Nothing here panics on any input.

use serde_json::Value;

/// Bodies longer than this are classified on their first `MAX_BODY_BYTES` bytes (cut at a char
/// boundary). noevia-core reads at most 64 KiB of an error body; decoded with replacement
/// characters that is at most 192 KiB of UTF-8.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// The longest reason returned, in Unicode scalar values, including the trailing `…`.
pub const MAX_REASON_CHARS: usize = 200;

/// The longest template `raise_exception` message quoted in a template reason.
pub const MAX_RAISE_CHARS: usize = 120;

/// What kind of failure an upstream error is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The prompt does not fit the model's context window.
    ContextFull,
    /// The model's chat template cannot render this request: llama.cpp could not build a tool
    /// call parser for it, or the template raised (role order, system role, tools).
    TemplateOrToolsUnsupported,
    /// Any other 4xx: the request was refused.
    BadRequest,
    /// No answer, a gateway/unavailable status, or a backend that is still loading.
    BackendDown,
    /// Everything else.
    Other,
}

impl Kind {
    /// The wire name used by the WebAssembly reply and noevia-core.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::ContextFull => "context_full",
            Kind::TemplateOrToolsUnsupported => "template_or_tools_unsupported",
            Kind::BadRequest => "bad_request",
            Kind::BackendDown => "backend_down",
            Kind::Other => "other",
        }
    }
}

/// A classified error: its kind and a sanitised, capped reason (possibly empty).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Classified {
    pub kind: Kind,
    pub reason: String,
}

/// The longest prefix of `text` of at most `max` bytes that ends on a char boundary.
pub fn cut_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or("")
}

/// JS's `.` does not match these (ECMA-262 LineTerminator).
fn is_js_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// `/context.*(exceed|full|length)|too many tokens|maximum context/i`, as JS evaluates it.
pub fn is_context_full(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("too many tokens") || lower.contains("maximum context") {
        return true;
    }
    lower.split(is_js_line_terminator).any(|line| {
        line.find("context").is_some_and(|at| {
            let after = line.get(at + "context".len()..).unwrap_or("");
            after.contains("exceed") || after.contains("full") || after.contains("length")
        })
    })
}

const TEMPLATE_MARKERS: &[&str] = &[
    "unable to generate parser for this template",
    "automatic parser generation failed",
    "raise_exception",
    "conversation roles must alternate",
    "roles must alternate",
    "tools param requires --jinja",
    "does not support tools",
    "does not support tool",
    "tools are not supported",
    "tool calls are not supported",
    "tool calling is not supported",
    "system role not supported",
];

fn is_template_failure(lower: &str) -> bool {
    TEMPLATE_MARKERS.iter().any(|m| lower.contains(m))
        || (lower.contains("chat template") && lower.contains("tool"))
}

const DOWN_MARKERS: &[&str] = &[
    "connection refused",
    "econnrefused",
    "econnreset",
    "fetch failed",
    "loading model",
    "service unavailable",
    "bad gateway",
    "gateway timeout",
    "no model loaded",
    "model is not loaded",
];

/// The upstream's own message: `error.message`, `error` (a string), `message` or `detail` of a
/// JSON object body, or a JSON string body. Anything else (HTML, plain text, a runtime error)
/// gives no message: noevia#918 keeps raw bodies and runtime errors out of the chat.
pub fn upstream_message(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return String::new();
    };
    let pick = |v: &Value| v.as_str().map(str::to_owned);
    if let Some(obj) = value.as_object() {
        if let Some(err) = obj.get("error") {
            if let Some(s) = pick(err) {
                return s;
            }
            if let Some(s) = err.get("message").and_then(pick) {
                return s;
            }
        }
        for key in ["message", "detail"] {
            if let Some(s) = obj.get(key).and_then(pick) {
                return s;
            }
        }
        return String::new();
    }
    match value {
        Value::String(s) => s,
        _ => String::new(),
    }
}

/// The message of the first `raise_exception('…')` / `raise_exception("…")` in `text`.
pub fn raise_message(text: &str) -> Option<String> {
    let at = text.find("raise_exception(")?;
    let rest = text.get(at + "raise_exception(".len()..)?.trim_start();
    let quote = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let body = rest.get(quote.len_utf8()..)?;
    let end = body.find(quote)?;
    let msg = body.get(..end)?;
    let clean = sanitize_capped(msg, MAX_RAISE_CHARS);
    (!clean.is_empty()).then_some(clean)
}

/// Classify an upstream failure. `status` is the HTTP status (0 when there was no response).
pub fn classify(status: u32, body: &str) -> Classified {
    let body = cut_bytes(body, MAX_BODY_BYTES);
    let lower = body.to_ascii_lowercase();
    let message = upstream_message(body);
    let kind = if is_context_full(body) {
        Kind::ContextFull
    } else if is_template_failure(&lower) {
        Kind::TemplateOrToolsUnsupported
    } else if status == 0
        || matches!(status, 502..=504)
        || DOWN_MARKERS.iter().any(|m| lower.contains(m))
    {
        Kind::BackendDown
    } else if (400..500).contains(&status) {
        Kind::BadRequest
    } else {
        Kind::Other
    };
    let reason = if kind == Kind::TemplateOrToolsUnsupported {
        // llama.cpp's message carries the whole template source after its first line; keep the
        // first line and, when there is one, the template's own exception text.
        let first = message.split(is_js_line_terminator).next().unwrap_or("");
        let head = sanitize(first.trim_end_matches([':', ' ']));
        match raise_message(&message).or_else(|| raise_message(body)) {
            Some(raised) if !head.is_empty() => sanitize(&format!("{head} (template: {raised})")),
            Some(raised) => sanitize(&format!("template: {raised}")),
            None => head,
        }
    } else {
        sanitize(&message)
    };
    Classified { kind, reason }
}

/// `{"kind":"…","reason":"…"}`.
pub fn reply_json(c: &Classified) -> String {
    serde_json::json!({ "kind": c.kind.as_str(), "reason": c.reason }).to_string()
}

const SECRET_KEYS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "apikey",
    "api_key",
    "api-key",
    "authorization",
    "auth",
    "key",
    "cookie",
    "session",
    "credential",
];

const TOKEN_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "pk-",
    "rk-",
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "github_pat_",
    "glpat-",
    "xox",
    "hf_",
    "eyj",
];

fn trim_punct(word: &str) -> &str {
    word.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\''
                | '`'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | ','
                | ';'
                | '.'
                | '!'
                | '?'
        )
    })
}

fn is_ipv4(word: &str) -> bool {
    let host = word.split(':').next().unwrap_or("");
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()))
}

/// `name:port` or `[v6]:port`.
fn is_host_port(word: &str) -> bool {
    let Some((host, port)) = word.rsplit_once(':') else {
        return false;
    };
    !host.is_empty()
        && !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|b| b.is_ascii_digit())
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '[' | ']' | ':'))
        && host.chars().any(|c| c.is_ascii_alphanumeric())
}

fn is_token_like(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    if word.len() >= 8 && TOKEN_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    word.len() >= 20
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '/' | '='))
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_alphabetic())
}

fn is_path(word: &str) -> bool {
    let lower = word.to_ascii_lowercase();
    (word.starts_with('/') && word.get(1..).is_some_and(|r| r.contains('/')))
        || word.starts_with("~/")
        || lower.ends_with(".gguf")
        || (word.len() > 3
            && word.as_bytes().get(1) == Some(&b':')
            && word.as_bytes().get(2) == Some(&b'\\'))
}

/// Replace one whitespace-free word if it may carry a secret or an internal location.
fn redact_word(word: &str, after_scheme: bool) -> String {
    if after_scheme {
        return "[redacted]".to_owned();
    }
    let core = trim_punct(word);
    if core.is_empty() {
        return word.to_owned();
    }
    if word.contains("://") {
        return "[url]".to_owned();
    }
    // Markup (`<b>`, `</div>`) never reaches the chat; `<=` and `->` stay.
    if word.char_indices().any(|(i, c)| {
        c == '<'
            && word
                .get(i + 1..)
                .and_then(|r| r.chars().next())
                .is_some_and(|n| n.is_ascii_alphabetic() || n == '/' || n == '!')
    }) {
        return "[markup]".to_owned();
    }
    // key=value / key:value with a secret-looking key.
    for sep in ['=', ':'] {
        if let Some((key, value)) = core.split_once(sep) {
            let k = key.to_ascii_lowercase();
            let k = k.trim_start_matches(['-', '_']);
            if !value.is_empty() && SECRET_KEYS.iter().any(|s| k.ends_with(s)) {
                return format!("{key}{sep}[redacted]");
            }
        }
    }
    if core.contains('@') && core.contains('.') && !core.starts_with('@') {
        return "[email]".to_owned();
    }
    if is_ipv4(core) || is_host_port(core) {
        return "[address]".to_owned();
    }
    if is_path(core) {
        return "[path]".to_owned();
    }
    if is_token_like(core) {
        return "[redacted]".to_owned();
    }
    word.to_owned()
}

/// [`sanitize`] with an explicit cap.
pub fn sanitize_capped(text: &str, max_chars: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| {
            if c.is_control() || is_js_line_terminator(c) || c == '\u{FEFF}' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let mut out: Vec<String> = Vec::new();
    let mut scheme = false;
    for word in flat.split_whitespace() {
        out.push(redact_word(word, scheme));
        let lower = trim_punct(word).to_ascii_lowercase();
        scheme = matches!(lower.as_str(), "bearer" | "basic" | "token");
    }
    let joined = out.join(" ");
    if joined.chars().count() <= max_chars {
        return joined;
    }
    let keep: String = joined.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", keep.trim_end())
}

/// Flatten, redact and cap a reason for display (see the crate docs).
pub fn sanitize(text: &str) -> String {
    sanitize_capped(text, MAX_REASON_CHARS)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const GEMMA_400: &str = r#"{"error":{"code":400,"message":"Unable to generate parser for this template. Automatic parser generation failed: \n------------\nWhile executing CallExpression at line 19, column 12 in source:\n{{ raise_exception(\"Conversation roles must alternate user/assistant/user/assistant/...\") }}","type":"invalid_request_error"}}"#;

    #[test]
    fn gemma_template_failure_is_classified_with_its_reason() {
        let c = classify(400, GEMMA_400);
        assert_eq!(c.kind, Kind::TemplateOrToolsUnsupported);
        assert_eq!(
            c.reason,
            "Unable to generate parser for this template. Automatic parser generation failed (template: Conversation roles must alternate user/assistant/user/assistant/...)"
        );
    }

    #[test]
    fn context_full_wins_and_matches_js() {
        assert_eq!(
            classify(400, r#"{"error":{"message":"the request exceeds the available context size, try increasing it","type":"exceed_context_size_error"}}"#).kind,
            Kind::ContextFull
        );
        assert!(is_context_full("Context window length"));
        assert!(!is_context_full("context\nexceeded"));
        assert!(is_context_full("TOO MANY TOKENS"));
        // ASCII-only folding: a dotless i is not an i to JS's non-unicode /i.
        assert!(!is_context_full("maxımum context"));
        assert!(is_context_full("maximum context"));
    }

    #[test]
    fn statuses_without_markers() {
        assert_eq!(classify(0, "").kind, Kind::BackendDown);
        assert_eq!(classify(503, "Loading model").kind, Kind::BackendDown);
        assert_eq!(classify(500, "Loading model").kind, Kind::BackendDown);
        assert_eq!(classify(422, r#"{"error":"bad"}"#).kind, Kind::BadRequest);
        assert_eq!(classify(500, "boom").kind, Kind::Other);
        assert_eq!(classify(500, "boom").reason, "");
        assert_eq!(classify(500, r#"{"error":"boom"}"#).reason, "boom");
    }

    #[test]
    fn reasons_are_sanitised() {
        let r = sanitize("failed at http://10.0.0.5:8080/v1 with Bearer abc.def and api_key=sk-12345678 from /models/gemma/x.gguf host llama:8080 192.168.1.2 user a@b.co");
        assert_eq!(r, "failed at [url] with Bearer [redacted] and api_key=[redacted] from [path] host [address] [address] user [email]");
        assert_eq!(sanitize("tok ghp_aaaaaaaaaaaaaaaa"), "tok [redacted]");
        assert_eq!(sanitize("x a1b2c3d4e5f6a7b8c9d0e1f2"), "x [redacted]");
        assert_eq!(sanitize("line\none\ttab\u{0}"), "line one tab");
        assert_eq!(sanitize("a <b>x</b> <= 2"), "a [markup] <= 2");
        let long = "word ".repeat(100);
        let capped = sanitize(&long);
        assert!(capped.chars().count() <= MAX_REASON_CHARS);
        assert!(capped.ends_with('…'));
    }

    #[test]
    fn upstream_message_shapes() {
        assert_eq!(upstream_message(r#"{"error":"x"}"#), "x");
        assert_eq!(upstream_message(r#"{"error":{"message":"y"}}"#), "y");
        assert_eq!(upstream_message(r#"{"detail":"z"}"#), "z");
        assert_eq!(upstream_message(r#"{"other":1}"#), "");
        assert_eq!(upstream_message(r#""s""#), "s");
        assert_eq!(upstream_message("plain"), "");
    }

    #[test]
    fn cut_bytes_respects_char_boundaries() {
        assert_eq!(cut_bytes("aé", 2), "a");
        assert_eq!(cut_bytes("abc", 10), "abc");
    }
}
