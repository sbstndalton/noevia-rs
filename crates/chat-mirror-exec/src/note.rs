//! The vault note: names, folders, the path guard, the frontmatter and the body, byte for byte as
//! noevia-core `chat-vault-mirror.cjs` (safeName, folderFor, insideMirror, frontmatter,
//! renderNote) and `chat-export.cjs` (chatMarkdown) write them.

use crate::brain;
use crate::jsx;
use js_json::{wtf16, JValue};

pub const ROOT: &str = "Chats";
pub const INBOX: &str = "Inbox";
/// A note without a brain; with one it is [`brain::BRAIN_SCHEMA`].
pub const BRAIN_SCHEMA: u32 = 0;
/// The companion's editor limit is 512 KiB.
pub const MAX_NOTE_BYTES: usize = 500 * 1024;
const MAX_NAME: usize = 100;

/// Why a note could not be rendered (where JS throws).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderError(pub &'static str);

fn reserved(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    matches!(l.as_str(), "con" | "prn" | "aux" | "nul")
        || ((l.starts_with("com") || l.starts_with("lpt"))
            && l.len() == 4
            && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit))
}

fn nfc(s: &str) -> String {
    // Lone surrogates are held as private-use characters, which NFC leaves alone (as JS does).
    icu_normalizer::ComposingNormalizerBorrowed::new_nfc()
        .normalize(s)
        .into_owned()
}

fn strip_dots_spaces(s: &str, leading: bool) -> &str {
    let f = |c: char| c == '.' || jsx::is_space(c);
    if leading {
        s.trim_start_matches(f)
    } else {
        s.trim_end_matches(f)
    }
}

/// `safeName(text, fallback)`: `text` is `String(text ?? '')` already.
pub fn safe_name(text: &str, fallback: &str) -> String {
    let mut cleaned = String::with_capacity(text.len());
    for c in nfc(text).chars() {
        let bad = matches!(u32::from(c), 0..=0x1f | 0x7f)
            || matches!(
                c,
                '\\' | '/'
                    | ':'
                    | '*'
                    | '?'
                    | '"'
                    | '<'
                    | '>'
                    | '|'
                    | '#'
                    | '^'
                    | '['
                    | ']'
                    | '%'
                    | '{'
                    | '}'
            );
        cleaned.push(if bad { ' ' } else { c });
    }
    // .replace(/\s+/g, ' ')
    let mut collapsed = String::with_capacity(cleaned.len());
    let mut in_space = false;
    for c in cleaned.chars() {
        if jsx::is_space(c) {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(c);
            in_space = false;
        }
    }
    let t = jsx::trim(&collapsed);
    let t = strip_dots_spaces(strip_dots_spaces(t, true), false);
    let mut name = t.to_owned();
    if jsx::len(&name) > MAX_NAME {
        name = strip_dots_spaces(&jsx::slice_to(&name, MAX_NAME), false).to_owned();
    }
    if name.is_empty() || name == "." || name == ".." {
        name = fallback.to_owned();
    }
    if reserved(&name) {
        name = format!("{name} chat");
    }
    name
}

/// `folderFor(projectName)`.
pub fn folder_for(project: Option<&str>) -> String {
    match project {
        None => format!("{ROOT}/{INBOX}"),
        Some(p) => {
            let name = safe_name(p, "Project");
            if name.to_lowercase() == INBOX.to_lowercase() {
                format!("{ROOT}/{name} (project)")
            } else {
                format!("{ROOT}/{name}")
            }
        }
    }
}

/// `insideMirror(path)`: `Chats/<one folder>/<one .md file>`, no traversal.
pub fn inside_mirror(path: &JValue) -> bool {
    let Some(path) = path.as_str() else {
        return false;
    };
    let parts: Vec<&str> = path.split('/').collect();
    let [root, folder, file] = parts.as_slice() else {
        return false;
    };
    *root == ROOT
        && [folder, file].iter().all(|p| {
            !p.is_empty()
                && **p != "."
                && **p != ".."
                && !p.starts_with('.')
                && !p.chars().any(|c| c == '\\' || u32::from(c) <= 0x1f)
        })
        && file.ends_with(".md")
}

/// `JSON.stringify(String(value))`.
pub fn yaml_string(value: &str) -> String {
    wtf16::stringify(&JValue::Str(value.to_owned())).unwrap_or_default()
}

fn iso_of(v: &JValue) -> Result<Option<String>, RenderError> {
    match v {
        JValue::Num(n) if n.is_finite() && *n > 0.0 => jsx::iso(*n)
            .map(Some)
            .ok_or(RenderError("invalid time value")),
        _ => Ok(None),
    }
}

fn js_string(v: &JValue) -> Result<String, RenderError> {
    js_json::to_js_string(v).map_err(|_| RenderError("cannot convert to string"))
}

/// `frontmatter({ chat, projectName, brain })`.
pub fn frontmatter(
    chat: &JValue,
    project: Option<&str>,
    with_brain: bool,
) -> Result<String, RenderError> {
    let frame = chat.get("frame");
    let frame = if frame.type_of() == "object" && frame.truthy() {
        Some(frame)
    } else {
        None
    };
    let tags: Vec<String> = match frame.map(|f| f.get("tags")) {
        Some(JValue::Arr(items)) => items
            .iter()
            .filter_map(|t| t.as_str().filter(|s| !s.is_empty()).map(yaml_string))
            .collect(),
        _ => Vec::new(),
    };
    let id = js_string(chat.get("id"))?;
    let created = match iso_of(chat.get("createdAt"))? {
        Some(s) => s,
        None => iso_of(chat.get("updatedAt"))?.unwrap_or_else(|| "null".into()),
    };
    let updated = iso_of(chat.get("updatedAt"))?.unwrap_or_else(|| "null".into());
    let kind = match frame.map(|f| f.get("kind")) {
        Some(k) if k.truthy() => yaml_string(&js_string(k)?),
        _ => "null".into(),
    };
    let lines = [
        "---".to_owned(),
        format!("noevia_id: {}", yaml_string(&id)),
        format!("created: {created}"),
        format!("updated: {updated}"),
        format!(
            "project: {}",
            project.map_or_else(|| "null".into(), yaml_string)
        ),
        format!("kind: {kind}"),
        format!("tags: [{}]", tags.join(", ")),
        format!(
            "brain_schema: {}",
            if with_brain {
                brain::BRAIN_SCHEMA
            } else {
                BRAIN_SCHEMA
            }
        ),
        "---".to_owned(),
        String::new(),
    ];
    Ok(lines.join("\n"))
}

/// chat-export.cjs `failed(m)`.
fn failed(m: &JValue) -> bool {
    m.get("role").as_str() == Some("assistant")
        && m.get("failure")
            .as_str()
            .is_some_and(|f| !jsx::trim(f).is_empty())
}

/// `String(m.content || '').trim()`.
pub fn content_text(m: &JValue) -> Result<String, RenderError> {
    let c = m.get("content");
    let s = if c.truthy() {
        js_string(c)?
    } else {
        String::new()
    };
    Ok(jsx::trim(&s).to_owned())
}

/// chat-export.cjs `chatMarkdown(chat, history)`. `title` and `updated_at` are the chat's fields.
pub fn chat_markdown(
    title: &JValue,
    updated_at: &JValue,
    history: &[JValue],
) -> Result<String, RenderError> {
    let title = if title.truthy() {
        js_string(title)?
    } else {
        "Untitled chat".into()
    };
    let mut lines: Vec<String> = vec![format!("# {title}"), String::new()];
    if updated_at.truthy() {
        // `new Date(updatedAt)`: a number is the time; anything else goes through ToPrimitive
        // and, as text, Date's parser.
        let ms = match updated_at {
            JValue::Num(n) => *n,
            JValue::Bool(true) => 1.0,
            JValue::Str(text) => jsx::parse_date(text),
            other => jsx::parse_date(&js_string(other)?),
        };
        let at = jsx::iso(ms).ok_or(RenderError("invalid time value"))?;
        lines.push(format!("_Last updated {at}_"));
        lines.push(String::new());
    }
    for m in history {
        if m.is_nullish() {
            return Err(RenderError("cannot read properties of null"));
        }
        let role = m.get("role").as_str();
        if role != Some("user") && role != Some("assistant") {
            continue;
        }
        let text = content_text(m)?;
        let body = if failed(m) {
            if text.is_empty() {
                "_(The reply failed; no answer was written.)_".to_owned()
            } else {
                format!("{text}\n\n_(The reply failed before it finished.)_")
            }
        } else if text.is_empty() {
            "_(no text)_".to_owned()
        } else {
            text
        };
        lines.push(format!(
            "## {}",
            if role == Some("user") {
                "You"
            } else {
                "Assistant"
            }
        ));
        lines.push(String::new());
        lines.push(body);
        lines.push(String::new());
        let calls = m.get("toolCalls");
        let tools: Vec<String> = match calls {
            JValue::Arr(items) => {
                let mut out = Vec::new();
                for t in items {
                    let name = if t.truthy() { t.get("name") } else { t };
                    if name.truthy() {
                        out.push(js_string(name)?);
                    }
                }
                out
            }
            c if !c.truthy() => Vec::new(),
            _ => return Err(RenderError("toolCalls.map is not a function")),
        };
        if !tools.is_empty() {
            lines.push(format!("_Tools used: {}_", tools.join(", ")));
            lines.push(String::new());
        }
    }
    Ok(lines.join("\n"))
}

/// `renderNote({ chat, projectName, history, linkNames, brain })`. `brain` is the stored brain
/// (validated again here, as JS does).
pub fn render_note(
    chat: &JValue,
    project: Option<&str>,
    history: &[JValue],
    link_names: &[String],
    brain: Option<&JValue>,
) -> Result<String, RenderError> {
    let usable = brain.and_then(|b| brain::validate(b).ok());
    let mut head = frontmatter(chat, project, usable.is_some())?;
    if let Some(b) = &usable {
        head.push_str(&brain::render_markdown(b));
    }
    let links = if link_names.is_empty() {
        String::new()
    } else {
        let items: Vec<String> = link_names.iter().map(|n| format!("- [[{n}]]")).collect();
        format!("\n## Links\n\n{}\n", items.join("\n"))
    };
    let mut body = chat_markdown(chat.get("title"), chat.get("updatedAt"), history)?;
    let fixed = jsx::byte_len(&head) + jsx::byte_len(&links);
    let budget =
        i64::try_from(MAX_NOTE_BYTES).unwrap_or(0) - i64::try_from(fixed).unwrap_or(i64::MAX) - 200;
    if i64::try_from(jsx::byte_len(&body)).unwrap_or(i64::MAX) > budget {
        let keep = usize::try_from(budget.max(0)).unwrap_or(0);
        let bytes = jsx::utf8(&body);
        let cut = bytes.get(..keep.min(bytes.len())).unwrap_or(&[]);
        let text = String::from_utf8_lossy(cut);
        body = text.trim_end_matches('\u{FFFD}').to_owned();
        body.push_str(
            "\n\n_(This chat is longer than a note can hold here; the rest is in noevia.)_\n",
        );
    }
    Ok(format!("{head}{body}{links}"))
}

/// The note's name: the last path part without `.md`.
pub fn note_name(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.strip_suffix(".md").unwrap_or(file).to_owned()
}
