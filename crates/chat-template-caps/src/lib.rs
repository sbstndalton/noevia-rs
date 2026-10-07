//! Static capability analysis of a model's Jinja chat template (sbstndalton/noevia#1002) and the
//! serving check autotune signs a profile off with (noevia#1003).
//!
//! llama.cpp (router mode) builds a tool-call parser from the template whenever a request carries
//! `tools`. For a template with no tool support that raises on unexpected input (Gemma 2/3:
//! `raise_exception('Conversation roles must alternate…')`), that generation renders synthetic
//! conversations, the template raises, and every request with `tools` fails with HTTP 400.
//! [`analyze`] reads the template text (from llama.cpp's `/props` `chat_template`) without
//! running it and reports what it references; [`Caps::send_tools`] is the decision noevia-core
//! makes from it: send `tools` when the template handles them natively, or when it never raises
//! (llama.cpp's generic tool handling then works); otherwise do not.
//!
//! The scan is lexical: only code inside `{{ … }}` and `{% … %}` counts (text and `{# … #}`
//! comments do not), string literals are read as literals (so `'tools'` is not the `tools`
//! variable, and a `}}` inside a string does not end the tag), and an attribute (`x.tools`) is
//! not the variable. The input is untrusted; nothing here panics or recurses.

use provider_error::{classify, Kind};
use serde_json::Value;

/// Templates longer than this are not analysed ([`CapsError::TooLarge`]); real ones are < 20 KiB.
pub const MAX_TEMPLATE_BYTES: usize = 256 * 1024;

/// What a template references.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    /// A non-blank template was analysed. When false every other field is false and
    /// `send_tools` is true (nothing is known, so behaviour is unchanged).
    pub known: bool,
    /// The template reads the `tools` variable: native tool support.
    pub tools: bool,
    /// The template renders assistant `tool_calls`.
    pub tool_calls: bool,
    /// The template handles `tool` (or `ipython`) role messages.
    pub tool_role: bool,
    /// The template accepts a system message (no exception says the system role is unsupported).
    pub system_role: bool,
    /// The template raises when user/assistant turns do not alternate.
    pub strict_alternation: bool,
    /// The template calls `raise_exception` anywhere.
    pub raises: bool,
    /// The template reads `enable_thinking` or `reasoning_content`.
    pub thinking: bool,
    /// The decision: whether noevia-core may send `tools` to this model.
    pub send_tools: bool,
}

/// Why a template was not analysed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapsError {
    TooLarge,
}

#[derive(Debug, PartialEq)]
enum Tok {
    Ident { name: String, attr: bool },
    Lit(String),
    Punct(char),
}

/// Read one string literal starting after `quote`; returns its text. Unterminated literals run
/// to the end of the input.
fn literal(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, quote: char) -> String {
    let mut out = String::new();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(match n {
                    'n' => '\n',
                    't' => '\t',
                    other => other,
                });
            }
        } else if c == quote {
            break;
        } else {
            out.push(c);
        }
    }
    out
}

/// The code tokens of every `{{ }}` / `{% %}` tag, in order.
fn tokens(template: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            continue;
        }
        let close = match chars.peek() {
            Some('{') => '}',
            Some('%') => '%',
            Some('#') => {
                chars.next();
                // Comment: skip to `#}`.
                let mut prev = ' ';
                for d in chars.by_ref() {
                    if prev == '#' && d == '}' {
                        break;
                    }
                    prev = d;
                }
                continue;
            }
            _ => continue,
        };
        chars.next();
        let mut last_sig = ' ';
        while let Some(d) = chars.next() {
            if d == close && chars.peek() == Some(&'}') {
                chars.next();
                break;
            }
            if d == '\'' || d == '"' {
                out.push(Tok::Lit(literal(&mut chars, d)));
                last_sig = '"';
            } else if d.is_ascii_alphabetic() || d == '_' {
                let mut name = String::from(d);
                while let Some(&n) = chars.peek() {
                    if n.is_ascii_alphanumeric() || n == '_' {
                        name.push(n);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push(Tok::Ident {
                    name,
                    attr: last_sig == '.',
                });
                last_sig = 'a';
            } else if !d.is_whitespace() {
                out.push(Tok::Punct(d));
                last_sig = d;
            }
        }
        out.push(Tok::Punct(';'));
    }
    out
}

/// Analyse a template. A blank one is `known: false`.
pub fn analyze(template: &str) -> Result<Caps, CapsError> {
    if template.len() > MAX_TEMPLATE_BYTES {
        return Err(CapsError::TooLarge);
    }
    if template.trim().is_empty() {
        return Ok(Caps {
            send_tools: true,
            ..Caps::default()
        });
    }
    let toks = tokens(template);
    let mut caps = Caps {
        known: true,
        system_role: true,
        ..Caps::default()
    };
    let mut raise_messages: Vec<String> = Vec::new();
    let mut alternation_check = false;
    for (i, tok) in toks.iter().enumerate() {
        match tok {
            Tok::Ident { name, attr } => match name.as_str() {
                "tools" if !attr => caps.tools = true,
                "tool_calls" => caps.tool_calls = true,
                "enable_thinking" | "reasoning_content" => caps.thinking = true,
                "raise_exception" if !attr => {
                    caps.raises = true;
                    if let (Some(Tok::Punct('(')), Some(Tok::Lit(msg))) =
                        (toks.get(i + 1), toks.get(i + 2))
                    {
                        raise_messages.push(msg.to_ascii_lowercase());
                    }
                }
                "index0" if *attr && matches!(toks.get(i + 1), Some(Tok::Punct('%'))) => {
                    // `loop.index0 % 2`: the Gemma/Mistral alternation test.
                    alternation_check = true;
                }
                _ => {}
            },
            Tok::Lit(s) => match s.as_str() {
                "tool_calls" => caps.tool_calls = true,
                "tool" | "ipython" => caps.tool_role = true,
                "enable_thinking" | "reasoning_content" => caps.thinking = true,
                _ => {}
            },
            Tok::Punct(_) => {}
        }
    }
    for m in &raise_messages {
        if m.contains("alternate") {
            caps.strict_alternation = true;
        }
        if m.contains("system")
            && (m.contains("not supported")
                || m.contains("not allowed")
                || m.contains("unsupported"))
        {
            caps.system_role = false;
        }
    }
    if caps.raises && alternation_check {
        caps.strict_alternation = true;
    }
    caps.send_tools = caps.tools || !caps.raises;
    Ok(caps)
}

/// `{"known":…,"tools":…,…,"sendTools":…}` or `{"error":"too_large"}`.
pub fn reply_json(result: &Result<Caps, CapsError>) -> String {
    match result {
        Ok(c) => serde_json::json!({
            "known": c.known,
            "tools": c.tools,
            "toolCalls": c.tool_calls,
            "toolRole": c.tool_role,
            "systemRole": c.system_role,
            "strictAlternation": c.strict_alternation,
            "raises": c.raises,
            "thinking": c.thinking,
            "sendTools": c.send_tools,
        })
        .to_string(),
        Err(CapsError::TooLarge) => "{\"error\":\"too_large\"}".to_owned(),
    }
}

/// Autotune's serving check (noevia#1003): the outcome of one realistic chat request (the app's
/// message and tool shape, sent the way chat would send it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub passed: bool,
    /// The failure kind (`None` when passed).
    pub kind: Option<Kind>,
    /// A sanitised reason (empty when passed).
    pub reason: String,
}

/// Pass when the reply is a 2xx JSON body whose first choice has a message with text content or
/// tool calls; otherwise fail with the classified upstream error.
pub fn serving_verdict(status: u32, body: &str) -> Verdict {
    if (200..300).contains(&status) {
        let ok = serde_json::from_str::<Value>(provider_error::cut_bytes(
            body,
            provider_error::MAX_BODY_BYTES,
        ))
        .ok()
        .and_then(|v| {
            let msg = v
                .get("choices")?
                .as_array()?
                .first()?
                .get("message")?
                .clone();
            let text = msg.get("content").and_then(Value::as_str).is_some();
            let calls = msg
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty());
            Some(text || calls)
        })
        .unwrap_or(false);
        return if ok {
            Verdict {
                passed: true,
                kind: None,
                reason: String::new(),
            }
        } else {
            Verdict {
                passed: false,
                kind: Some(Kind::Other),
                reason: "The engine answered without a chat message.".to_owned(),
            }
        };
    }
    let c = classify(status, body);
    Verdict {
        passed: false,
        kind: Some(c.kind),
        reason: c.reason,
    }
}

/// `{"passed":…,"kind":"…"|null,"reason":"…"}`.
pub fn verdict_json(v: &Verdict) -> String {
    serde_json::json!({
        "passed": v.passed,
        "kind": v.kind.map(Kind::as_str),
        "reason": v.reason,
    })
    .to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn blank_and_large() {
        let c = analyze("  ").unwrap();
        assert!(!c.known && c.send_tools);
        assert_eq!(
            analyze(&"x".repeat(MAX_TEMPLATE_BYTES + 1)),
            Err(CapsError::TooLarge)
        );
    }

    #[test]
    fn text_comments_literals_and_attributes_do_not_count() {
        let c = analyze("tools {# {{ tools }} #} {{ 'tools' }} {{ message.tools }} {{ \"}}\" }}")
            .unwrap();
        assert!(c.known && !c.tools);
        let c = analyze("{{ \"}}\" }}{% if tools %}{% endif %}").unwrap();
        assert!(c.tools);
    }

    #[test]
    fn raise_without_tools_blocks_tools() {
        let c = analyze("{{ raise_exception('Conversation roles must alternate') }}").unwrap();
        assert!(c.raises && c.strict_alternation && !c.send_tools);
        let c = analyze("{% if tools %}{{ raise_exception('x') }}{% endif %}").unwrap();
        assert!(c.send_tools);
    }

    #[test]
    fn verdicts() {
        assert!(serving_verdict(200, r#"{"choices":[{"message":{"content":"ok"}}]}"#).passed);
        assert!(
            serving_verdict(
                200,
                r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"a"}]}}]}"#
            )
            .passed
        );
        let v = serving_verdict(200, r#"{"choices":[]}"#);
        assert_eq!((v.passed, v.kind), (false, Some(Kind::Other)));
        let v = serving_verdict(
            400,
            r#"{"error":{"message":"Unable to generate parser for this template."}}"#,
        );
        assert_eq!(v.kind, Some(Kind::TemplateOrToolsUnsupported));
    }
}
