//! One chat as a Markdown file (`GET /api/chats/{chatId}/export`, #1471): core routes/chat-lists.cjs's
//! body and file name. The chat is crates/server-projects `chat_download::find`'s; the HTTP answer is
//! bins/noevia-server project_routes.rs.
//!
//! - The body is chat-export.cjs `chatMarkdown(chat, history)` ([`crate::markdown::render_as_node`]) as
//!   `Buffer.from(text, 'utf8')` (a lone surrogate is EF BF BD).
//! - The name is `noevia-chat-<slug>-<id>.md`, where the slug is
//!   `String(chat.title || 'untitled-chat')` NFKD-normalised, every character outside `\x20-\x7e`
//!   dropped, lower-cased, each run of anything but `[a-z0-9]` one `-`, leading and trailing `-`
//!   trimmed, cut to 60 characters (after the trim, so it may end in `-`), else `untitled-chat`. This
//!   is not the conversation export's slug, which keeps the base letters of every script.
use crate::markdown;
use js_json::JValue;
use unicode_normalization::UnicodeNormalization;

/// Why Node would have thrown (its 500).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// `chatMarkdown` throws, or this port's bounds refuse it ([`markdown::Error`]).
    Markdown(markdown::Error),
    /// `String(chat.title)` throws (a title whose own `toString` is not callable).
    Title,
}

/// The download: its file name and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub name: String,
    pub body: Vec<u8>,
}

/// The slug part of the file name.
pub fn slug(title: &JValue) -> Result<String, Error> {
    let text = if title.truthy() {
        js_json::to_js_string(title).map_err(|_| Error::Title)?
    } else {
        "untitled-chat".to_owned()
    };
    let mut out = String::new();
    let mut gap = false;
    for c in text.nfkd().filter(|c| (' '..='~').contains(c)) {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if gap && !out.is_empty() {
                out.push('-');
            }
            gap = false;
            out.push(c);
        } else {
            gap = true;
        }
    }
    // `^-+|-+$` are gone (no leading run is written, a trailing one is pending only); then
    // `.slice(0, 60)` on what is all ASCII by now.
    out.truncate(60);
    Ok(if out.is_empty() {
        "untitled-chat".to_owned()
    } else {
        out
    })
}

/// `chatMarkdown(chat, history)` and the file name for the chat whose reduced id is `id`.
pub fn download(chat: &JValue, id: &str, history: &JValue) -> Result<Download, Error> {
    let text = markdown::render_as_node(chat, history).map_err(Error::Markdown)?;
    let name = format!("noevia-chat-{}-{id}.md", slug(chat.get("title"))?);
    Ok(Download {
        name,
        body: crate::assemble::utf8_like_buffer(text).into_bytes(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn s(v: &str) -> String {
        slug(&JValue::Str(v.into())).unwrap()
    }

    #[test]
    fn slug_as_node_writes_it() {
        assert_eq!(s("Café Ünïcode — notes"), "cafe-unicode-notes");
        assert_eq!(s("  --Hello, World!--  "), "hello-world");
        assert_eq!(s("!!!"), "untitled-chat");
        assert_eq!(s(""), "untitled-chat");
        assert_eq!(s("日本語"), "untitled-chat");
        assert_eq!(s("ﬁle №5"), "file-no5");
        assert_eq!(slug(&JValue::Num(42.0)).unwrap(), "42");
        assert_eq!(slug(&JValue::Null).unwrap(), "untitled-chat");
        // Cut after the trim: the 60th character may be a dash.
        let long = format!("{} {}", "a".repeat(59), "b".repeat(10));
        assert_eq!(s(&long), format!("{}-", "a".repeat(59)));
    }
}
