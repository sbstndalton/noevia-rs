//! Conversation export assembly (#1315): core `server/chat-export.cjs` `buildExport`, ported as
//! behaviour on top of [`crate::zip_store`] and [`crate::markdown::render`].
//!
//! The caller supplies the tenant's free chats and projects (as `ws.freeChats || []` and
//! `ws.projects || []` would be) and reads one chat history per call of `read_history`, exactly
//! where Node calls `readHistory(chat.id)`. No filesystem, clock, route, audit or switch lives
//! here: this is the export's bytes, not its endpoint.
//!
//! Byte compatibility: entry names (slug, id part, collision suffixes), the per-chat Markdown,
//! `README.md`, and `conversations.json` exactly as `JSON.stringify(…, null, 2)` indented by four
//! spaces, with `reasoning` and the variant fields (`answers`, `answer`, `branches`, `branch`,
//! #1517) left out of every history entry. Limits as Node: payload bytes and entries, checked in
//! Node's order, so the same input is refused with the same class.
//!
//! Narrower admission, as refusals ([`Error::Unsupported`]), never as different bytes: a list
//! (free chats, projects, a project's chats) that is truthy but not an array; a `now` outside
//! years 0..=9999; and the per-chat Markdown and ZIP bounds of the two foundations.
use crate::markdown;
use js_json::JValue;
use std::collections::{HashMap, HashSet};
use unicode_normalization::UnicodeNormalization;

pub const FORMAT: &str = "noevia-conversations-v1";
const VARIANT_FIELDS: [&str; 4] = ["answers", "answer", "branches", "branch"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_bytes: usize,
    pub max_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_bytes: crate::MAX_PAYLOAD_BYTES,
            max_entries: crate::MAX_ENTRIES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error<E> {
    /// Node's `ExportTooLargeError` (413); see [`too_large_message`].
    TooLarge,
    /// Node throws a TypeError here (the route answers 500).
    Type,
    /// Outside this port's admission (see the module docs).
    Unsupported,
    Markdown(markdown::Error),
    Zip(crate::Error),
    /// `read_history` failed.
    History(E),
}

/// Node's public 413 message for these limits.
pub fn too_large_message(limits: Limits) -> String {
    let mb = (limits.max_bytes as f64 / (1024.0 * 1024.0)).round();
    format!(
        "Your conversations are too large to export in one file (the limit is {} MB and {} chats). Delete conversations you no longer need and try again.",
        js_json::number_to_string(mb),
        thousands(limits.max_entries)
    )
}

fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `String(value)`; a value with no primitive (an own `toString`) is Node's TypeError.
fn js_string<E>(v: &JValue) -> Result<String, Error<E>> {
    js_json::to_js_string(v).map_err(|_| Error::Type)
}

/// `Buffer.from(text, 'utf8')` for a string that may hold [`js_json::wtf16`] lone surrogates:
/// Node encodes a lone surrogate as U+FFFD (EF BF BD). Text without one is returned unchanged.
fn utf8_like_buffer(text: String) -> String {
    if !text.chars().any(|c| js_json::wtf16::lone_unit(c).is_some()) {
        return text;
    }
    text.chars()
        .map(|c| {
            if js_json::wtf16::lone_unit(c).is_some() {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

/// `slug(text, fallback)`.
fn slug<E>(text: &JValue, fallback: &str) -> Result<String, Error<E>> {
    let s = if text.truthy() {
        js_string(text)?
    } else {
        String::new()
    };
    let mut out = String::new();
    let mut gap = false;
    for c in s
        .nfkd()
        .filter(|c| !('\u{300}'..='\u{36f}').contains(c))
        .flat_map(char::to_lowercase)
    {
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
    // Leading and trailing runs are trimmed before the 60-unit cut (all ASCII by now).
    out.truncate(60);
    Ok(if out.is_empty() {
        fallback.to_owned()
    } else {
        out
    })
}

/// `idPart(id)`.
fn id_part<E>(id: &JValue) -> Result<String, Error<E>> {
    Ok(js_string(id)?
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect())
}

/// `list || []`, admitted only as an array.
fn list<E>(v: &JValue) -> Result<&[JValue], Error<E>> {
    match v {
        JValue::Arr(items) => Ok(items),
        v if !v.truthy() => Ok(&[]),
        _ => Err(Error::Unsupported),
    }
}

/// `exportedEntry(entry)`.
fn exported_entry(entry: &JValue) -> JValue {
    let keep = |k: &str| k != "reasoning" && !VARIANT_FIELDS.contains(&k);
    match entry {
        JValue::Obj(items) => JValue::Obj(items.iter().filter(|(k, _)| keep(k)).cloned().collect()),
        // `{ ...array }`: its index keys, in order.
        JValue::Arr(items) => JValue::Obj(
            items
                .iter()
                .enumerate()
                .map(|(i, v)| (i.to_string(), v.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn iso<E>(ms: i64) -> Result<String, Error<E>> {
    let t = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).ok_or(Error::Unsupported)?;
    if !(0..=9999).contains(&chrono::Datelike::year(&t)) {
        return Err(Error::Unsupported);
    }
    Ok(t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

struct Builder {
    limits: Limits,
    total: usize,
    names: Vec<String>,
    data: Vec<Vec<u8>>,
    taken: HashSet<String>,
    /// For a base name, the suffix its collision search resumes from: every lower one is taken,
    /// and taken names stay taken, so the result is Node's `-2`, `-3`, ... search.
    next_suffix: HashMap<String, usize>,
}

impl Builder {
    fn take<E>(&mut self, bytes: usize) -> Result<(), Error<E>> {
        self.total = self.total.saturating_add(bytes);
        if self.total > self.limits.max_bytes {
            return Err(Error::TooLarge);
        }
        Ok(())
    }

    fn add<E>(&mut self, name: String, text: String) -> Result<(), Error<E>> {
        let mut unique = name.clone();
        if self.taken.contains(&unique) {
            let stem = name.strip_suffix(".md");
            let mut n = self.next_suffix.get(&name).copied().unwrap_or(2);
            loop {
                unique = match stem {
                    Some(stem) => format!("{stem}-{n}.md"),
                    // `replace(/\.md$/, …)` leaves a name without `.md` unchanged: Node would
                    // never stop. No export name lacks it; refuse rather than spin.
                    None => return Err(Error::Unsupported),
                };
                n += 1;
                if !self.taken.contains(&unique) {
                    break;
                }
            }
            self.next_suffix.insert(name, n);
        }
        self.taken.insert(unique.clone());
        self.take(text.len())?;
        self.names.push(unique);
        self.data.push(text.into_bytes());
        Ok(())
    }
}

/// Node's `buildExport({ freeChats, projects, readHistory, now, maxBytes, maxEntries })`.
pub fn build_export<E>(
    free_chats: &JValue,
    projects: &JValue,
    mut read_history: impl FnMut(&JValue) -> Result<JValue, E>,
    now_ms: i64,
    limits: Limits,
) -> Result<Vec<u8>, Error<E>> {
    let free = list(free_chats)?;
    let projects = list(projects)?;
    // projects.reduce((n, p) => n + (p.chats || []).length, 0): a nullish project is a TypeError.
    let mut project_chats = 0usize;
    for project in projects {
        if project.is_nullish() {
            return Err(Error::Type);
        }
        project_chats = project_chats.saturating_add(list(project.get("chats"))?.len());
    }
    // Two more entries (README.md, conversations.json), checked before any history is read.
    if free.len().saturating_add(project_chats).saturating_add(2) > limits.max_entries {
        return Err(Error::TooLarge);
    }
    let mut b = Builder {
        limits,
        total: 0,
        names: Vec::new(),
        data: Vec::new(),
        taken: HashSet::new(),
        next_suffix: HashMap::new(),
    };
    let mut chat_json: Vec<String> = Vec::new();
    let mut add_chat =
        |b: &mut Builder, chat: &JValue, project: JValue| -> Result<String, Error<E>> {
            let history = read_history(chat.get("id")).map_err(Error::History)?;
            let md = markdown::render(chat, &history).map_err(|e| match e {
                markdown::Error::Type => Error::Type,
                other => Error::Markdown(other),
            })?;
            let JValue::Arr(entries) = &history else {
                return Err(Error::Type); // history.map
            };
            let title = chat.get("title");
            let updated = chat.get("updatedAt");
            let record = JValue::obj([
                ("id", chat.get("id").clone()),
                (
                    "title",
                    if title.truthy() {
                        title.clone()
                    } else {
                        JValue::Str(String::new())
                    },
                ),
                (
                    "updatedAt",
                    if updated.truthy() {
                        updated.clone()
                    } else {
                        JValue::Null
                    },
                ),
                ("project", project),
                (
                    "history",
                    JValue::Arr(entries.iter().map(exported_entry).collect()),
                ),
            ]);
            // Lossless: a lone surrogate (a title or message cut mid-emoji) is written as
            // JSON.stringify writes it, `\udxxx`; anything else is the same bytes as before.
            let json = js_json::wtf16::stringify_pretty(&record)
                .ok_or(Error::Type)?
                .replace('\n', "\n    ");
            b.take(json.len())?;
            chat_json.push(json);
            Ok(md)
        };
    for chat in free {
        if chat.is_nullish() {
            return Err(Error::Type);
        }
        let name = format!(
            "chats/{}-{}.md",
            slug(chat.get("title"), "untitled-chat")?,
            id_part(chat.get("id"))?
        );
        let md = add_chat(&mut b, chat, JValue::Null)?;
        b.add(name, utf8_like_buffer(md))?;
    }
    for project in projects {
        let fallback = id_part(project.get("id"))?;
        let folder = format!(
            "projects/{}",
            slug(
                project.get("name"),
                if fallback.is_empty() {
                    "project"
                } else {
                    &fallback
                }
            )?
        );
        for chat in list(project.get("chats"))? {
            if chat.is_nullish() {
                return Err(Error::Type);
            }
            let name = format!(
                "{folder}/{}-{}.md",
                slug(chat.get("title"), "untitled-chat")?,
                id_part(chat.get("id"))?
            );
            let tag = JValue::obj([
                ("id", project.get("id").clone()),
                ("name", project.get("name").clone()),
            ]);
            let md = add_chat(&mut b, chat, tag)?;
            b.add(name, utf8_like_buffer(md))?;
        }
    }
    let count = chat_json.len();
    let at = iso(now_ms)?;
    b.add(
        "README.md".into(),
        format!(
            "# noevia conversations\n\nExported {at}. {count} chat{}.\n\n- `chats/`: chats outside projects, one Markdown file each.\n- `projects/`: chats grouped by project.\n- `conversations.json`: everything above in one file (format {FORMAT}).\n\nThinking text is not included.\n",
            if count == 1 { "" } else { "s" }
        ),
    )?;
    let head = format!(
        "{{\n  \"format\": \"{FORMAT}\",\n  \"exportedAt\": \"{at}\",\n  \"chats\": {}",
        if count > 0 { "[\n    " } else { "[]" }
    );
    let sep = ",\n    ";
    let tail = if count > 0 { "\n  ]\n}" } else { "\n}" };
    b.take(
        head.len()
            .saturating_add(tail.len())
            .saturating_add(sep.len().saturating_mul(count.saturating_sub(1))),
    )?;
    let mut conversations = String::new();
    conversations
        .try_reserve(b.total)
        .map_err(|_| Error::Zip(crate::Error::Allocation))?;
    conversations.push_str(&head);
    for (i, json) in chat_json.iter().enumerate() {
        if i > 0 {
            conversations.push_str(sep);
        }
        conversations.push_str(json);
    }
    conversations.push_str(tail);
    b.names.push("conversations.json".into());
    b.data.push(conversations.into_bytes());
    let entries: Vec<crate::Entry<'_>> = b
        .names
        .iter()
        .zip(&b.data)
        .map(|(name, data)| crate::Entry { name, data })
        .collect();
    crate::zip_store(&entries, now_ms).map_err(Error::Zip)
}
