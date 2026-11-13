//! The pure half of noevia-core's `server/gdrive-tools.cjs` (the Google Drive toolbox), in Rust.
//!
//! What is here, each with the Node function it ports:
//!
//! - [`Tool`], [`TOOLS`], [`Tool::label`], [`Tool::is_write`]: the seven tools in the order and
//!   words Settings lists them (`TOOLS`, `READS`, `LABELS`; the shared fixture
//!   `tests/fixtures/gdrive-tools-list.v1.json` is byte-identical to noevia-core's).
//! - [`plan_write`]: `describeTarget`, without the Drive metadata lookup. It answers which file
//!   an approval card names, or the refusal, before anything is fetched or written.
//! - [`shape_read`]: the reply of `drive_read_file` (whole, or cut to the chat's cap with
//!   `[truncated]`), and whether the reply held the whole file.
//! - [`ReadGate`] and [`check_update`]: the #659 rule that `drive_update_file` replaces a file
//!   only when the same file was read in full in the same exchange, within 30 minutes.
//! - the reply strings (`file_line`, `found_reply`, ...), copied from the Node template strings.
//!
//! What is NOT here: any network call, the Drive API client, the token (`gdrive.rs` in
//! noevia-rs owns it), the account's connection state and the approval card itself. Those are
//! the caller's, and they stay on the Node side until the release that wires this in.
//!
//! Lengths are UTF-16 code units, as in JavaScript. A cut that would fall inside a surrogate
//! pair is moved back to the previous character, so the Rust result can be one unit shorter than
//! the JS one in that case. Untrusted text (a file name, a file id) is only ever echoed through
//! [`json_string`], which escapes the way `JSON.stringify` does.

#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};

/// `READ_TTL_MS` in gdrive-tools.cjs: 30 minutes.
pub const READ_TTL_MS: u64 = 30 * 60 * 1000;
/// `READ_MAX`: at most this many remembered reads, the oldest dropped first.
pub const READ_MAX: usize = 2000;
/// The longest Drive id the tools accept (`DRIVE_ID`).
pub const MAX_ID_UNITS: usize = 200;
/// The longest name a create target keeps (`.slice(0, 255)`).
pub const MAX_TARGET_UNITS: usize = 255;

pub const NO_MATCHES: &str =
    "No matching files. noevia only sees files it created or was given in this Drive.";
pub const NO_FILES: &str =
    "No files yet. noevia only sees files it created or was given in this Drive.";
pub const NOT_CONNECTED: &str =
    "ERROR: Google Drive is not connected for this account. Connect it in Settings → Connectors.";
pub const NOT_READ_IN_FULL: &str = "ERROR: drive_update_file replaces the whole file, so it only runs on a file read in full with drive_read_file in this same reply (a read from an earlier message does not count). Read it again first. Nothing was changed.";
pub const READ_PARTLY: &str = "ERROR: only part of this file was read, so replacing it would drop the rest. Nothing was changed.";
pub const UNREACHABLE: &str = "Google Drive could not be reached.";
pub const TRUNCATED_MARKER: &str = "\n[truncated]";
const NOT_A_FILE_ID_TAIL: &str = " is not a Google Drive file id. The Google Drive tools only change files in the connected Google Drive; to change a file uploaded to this project, use the project file tools (project_append_file or project_replace_text)";

/// The seven Drive tools, in the order Settings lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    SearchFiles,
    ReadFile,
    GetMetadata,
    ListRecent,
    CreateFile,
    UpdateFile,
    TrashFile,
}

/// The seven tools, in Settings order (`TOOLS` in gdrive-tools.cjs).
pub const TOOLS: [Tool; 7] = [
    Tool::SearchFiles,
    Tool::ReadFile,
    Tool::GetMetadata,
    Tool::ListRecent,
    Tool::CreateFile,
    Tool::UpdateFile,
    Tool::TrashFile,
];

impl Tool {
    /// The name the model calls the tool by.
    pub fn name(self) -> &'static str {
        match self {
            Tool::SearchFiles => "drive_search_files",
            Tool::ReadFile => "drive_read_file",
            Tool::GetMetadata => "drive_get_metadata",
            Tool::ListRecent => "drive_list_recent",
            Tool::CreateFile => "drive_create_file",
            Tool::UpdateFile => "drive_update_file",
            Tool::TrashFile => "drive_trash_file",
        }
    }

    /// The name of a tool call, or `None` for a name that is not one of the seven.
    pub fn from_name(name: &str) -> Option<Tool> {
        TOOLS.into_iter().find(|t| t.name() == name)
    }

    /// The words Settings → Connectors uses (`LABELS`).
    pub fn label(self) -> &'static str {
        match self {
            Tool::SearchFiles => "Search files",
            Tool::ReadFile => "Read file content",
            Tool::GetMetadata => "Get file metadata",
            Tool::ListRecent => "List recent files",
            Tool::CreateFile => "Create file",
            Tool::UpdateFile => "Update file",
            Tool::TrashFile => "Trash file",
        }
    }

    /// `true` for the three tools that change Drive (`READS` is the complement).
    pub fn is_write(self) -> bool {
        matches!(self, Tool::CreateFile | Tool::UpdateFile | Tool::TrashFile)
    }
}

/// What an approval card names for a call (`describeTarget`), before any lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WritePlan {
    /// A read, or a call with nothing to name (`null` in the JS). The caller shows no target.
    NoTarget,
    /// `drive_create_file` with a name: the card names the new file (`kind: 'drive-new'`).
    NewFile { target: String },
    /// `drive_update_file` or `drive_trash_file` with a Drive id. The caller fetches the file's
    /// metadata and names it as `"<name> (id <id>)"` (`kind: 'drive'`), see [`existing_target`].
    ExistingFile { file_id: String },
    /// Refused before the card, nothing written: the text the model is shown.
    Refused(String),
}

/// `describeTarget(user, name, args)` for the arguments already parsed. `name` and `file_id`
/// are the call's string arguments, `None` when absent or not a string.
pub fn plan_write(tool: Tool, name: Option<&str>, file_id: Option<&str>) -> WritePlan {
    match tool {
        Tool::CreateFile => {
            let trimmed = name.map(js_trim).unwrap_or("");
            if trimmed.is_empty() {
                WritePlan::NoTarget
            } else {
                WritePlan::NewFile {
                    target: slice_units(trimmed, MAX_TARGET_UNITS).to_owned(),
                }
            }
        }
        Tool::UpdateFile | Tool::TrashFile => {
            let id = file_id.map(js_trim).unwrap_or("");
            if is_drive_id(id) {
                WritePlan::ExistingFile {
                    file_id: id.to_owned(),
                }
            } else {
                let shown = slice_units(id, MAX_ID_UNITS);
                WritePlan::Refused(format!("{}{NOT_A_FILE_ID_TAIL}", json_string(shown)))
            }
        }
        Tool::SearchFiles | Tool::ReadFile | Tool::GetMetadata | Tool::ListRecent => {
            WritePlan::NoTarget
        }
    }
}

/// `"<name> (id <id>)"`, the target text an existing-file card shows.
pub fn existing_target(name: &str, id: &str) -> String {
    format!("{name} (id {id})")
}

/// `DRIVE_ID` = `/^[\w-]{1,200}$/`: ASCII letters, digits, `_` and `-`, 1 to 200 of them.
pub fn is_drive_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_UNITS
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// What a Drive file's metadata looks like to the reply strings (`line` and friends). An empty
/// string counts as absent, as it does in the JS truthiness test.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileLine {
    pub name: String,
    pub id: String,
    pub mime_type: Option<String>,
    pub size: Option<String>,
    pub modified_time: Option<String>,
    pub web_view_link: Option<String>,
}

/// `line(f)`: `- <name> (id <id>) · <mime or file>[ · <size> bytes][ · changed <time>]`.
pub fn file_line(f: &FileLine) -> String {
    let mime = non_empty(f.mime_type.as_deref()).unwrap_or("file");
    let size = non_empty(f.size.as_deref())
        .map(|s| format!(" · {s} bytes"))
        .unwrap_or_default();
    let changed = non_empty(f.modified_time.as_deref())
        .map(|t| format!(" · changed {t}"))
        .unwrap_or_default();
    format!("- {} (id {}) · {mime}{size}{changed}", f.name, f.id)
}

/// The reply of `drive_search_files`.
pub fn found_reply(files: &[FileLine]) -> String {
    if files.is_empty() {
        return NO_MATCHES.to_owned();
    }
    let lines: Vec<String> = files.iter().map(file_line).collect();
    format!("Found {}:\n{}", files.len(), lines.join("\n"))
}

/// The reply of `drive_list_recent`.
pub fn recent_reply(files: &[FileLine]) -> String {
    if files.is_empty() {
        return NO_FILES.to_owned();
    }
    let lines: Vec<String> = files.iter().map(file_line).collect();
    lines.join("\n")
}

/// The reply of `drive_get_metadata`: the line, then the link when there is one.
pub fn metadata_reply(f: &FileLine) -> String {
    let line = file_line(f);
    match non_empty(f.web_view_link.as_deref()) {
        Some(link) => format!("{line}\nLink: {link}"),
        None => line,
    }
}

/// The reply of `drive_create_file`.
pub fn created_reply(name: &str, id: &str, link: Option<&str>) -> String {
    match non_empty(link) {
        Some(link) => format!("Created {name} (id {id}): {link}"),
        None => format!("Created {name} (id {id})"),
    }
}

/// The reply of `drive_update_file`.
pub fn updated_reply(name: &str, id: &str) -> String {
    format!("Updated {name} (id {id}).")
}

/// The reply of `drive_trash_file`.
pub fn trashed_reply(name: &str) -> String {
    format!("Moved {name} to the Drive trash.")
}

/// The `ERROR:` reply for a failed call: the public message, or the generic one when there is
/// none (`error.publicMessage || 'Google Drive could not be reached.'`).
pub fn error_reply(public_message: Option<&str>) -> String {
    format!(
        "ERROR: {}",
        non_empty(public_message).unwrap_or(UNREACHABLE)
    )
}

/// What a `drive_read_file` reply holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadReply {
    /// The string the model receives.
    pub text: String,
    /// `true` when `text` holds the whole file (the read gate may then allow an update).
    pub complete: bool,
}

/// The reply of `drive_read_file` (the `drive_read_file` case of `execute`). `text` is the file
/// body as Drive returned it, `truncated` says Drive or the reader cut it, and `cap` is the
/// chat's tool-result cap. The header counts against the cap. When the file is not complete,
/// the reply is cut to the cap, and the cut keeps room for [`TRUNCATED_MARKER`].
pub fn shape_read(name: &str, text: &str, truncated: bool, cap: usize) -> ReadReply {
    let header = format!("{name}:\n");
    let whole = format!("{header}{text}");
    if !truncated && units(&whole) <= cap {
        return ReadReply {
            text: whole,
            complete: true,
        };
    }
    let budget = cap
        .saturating_sub(units(&header))
        .saturating_sub(units(TRUNCATED_MARKER));
    ReadReply {
        text: format!("{header}{}{TRUNCATED_MARKER}", slice_units(text, budget)),
        complete: false,
    }
}

/// `scopeOf`: the exchange when there is one (a non-empty key), else the chat.
pub fn scope_key(exchange: Option<&str>, chat: Option<&str>) -> String {
    match exchange.filter(|e| !e.is_empty()) {
        Some(e) => format!("x:{e}"),
        None => format!("c:{}", chat.unwrap_or("")),
    }
}

/// One remembered read of a Drive file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadRecord {
    pub version: String,
    pub complete: bool,
    pub at_ms: u64,
    seq: u64,
}

/// The per-account, per-exchange read record (#659). `remember` and `last_read` take the time
/// in milliseconds, so the TTL is testable without a clock.
#[derive(Debug, Default)]
pub struct ReadGate {
    reads: HashMap<String, ReadRecord>,
    order: VecDeque<(String, u64)>,
    next_seq: u64,
}

fn read_key(user_id: &str, scope: &str, file_id: &str) -> String {
    format!("{user_id}\u{0}{scope}\u{0}{file_id}")
}

impl ReadGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// `remember`: records the version a reply read. Nothing is recorded without a version or a
    /// file id. A repeat moves the entry to the newest end, and the oldest entries go when there
    /// are more than [`READ_MAX`].
    pub fn remember(
        &mut self,
        user_id: &str,
        scope: &str,
        file_id: &str,
        version: Option<&str>,
        complete: bool,
        now_ms: u64,
    ) {
        let Some(version) = version else { return };
        if file_id.is_empty() {
            return;
        }
        let key = read_key(user_id, scope, file_id);
        self.reads.remove(&key);
        self.next_seq += 1;
        let seq = self.next_seq;
        self.reads.insert(
            key.clone(),
            ReadRecord {
                version: version.to_owned(),
                complete,
                at_ms: now_ms,
                seq,
            },
        );
        self.order.push_back((key, seq));
        while self.reads.len() > READ_MAX {
            let Some((old, old_seq)) = self.order.pop_front() else {
                break;
            };
            if self.reads.get(&old).is_some_and(|r| r.seq == old_seq) {
                self.reads.remove(&old);
            }
        }
        if self.order.len() > 2 * READ_MAX {
            let reads = &self.reads;
            self.order
                .retain(|(k, s)| reads.get(k).is_some_and(|r| r.seq == *s));
        }
    }

    /// `lastRead`: the record for this user, scope and file, while it is within the TTL.
    pub fn last_read(
        &self,
        user_id: &str,
        scope: &str,
        file_id: &str,
        now_ms: u64,
    ) -> Option<&ReadRecord> {
        let hit = self.reads.get(&read_key(user_id, scope, file_id))?;
        (now_ms.saturating_sub(hit.at_ms) <= READ_TTL_MS).then_some(hit)
    }
}

/// The read-before-update decision for `drive_update_file` (the check at the top of its case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheck {
    /// The id is not a Drive id. It falls through to the Drive client, which refuses it by name,
    /// with no version to expect.
    NotDriveId,
    /// The same file was read in full in this scope: write only if Drive still has this version.
    ExpectVersion(String),
}

/// `drive_update_file`'s gate: `Err` carries the exact refusal text, and nothing is written.
pub fn check_update(
    gate: &ReadGate,
    user_id: &str,
    scope: &str,
    file_id: Option<&str>,
    now_ms: u64,
) -> Result<UpdateCheck, &'static str> {
    let id = file_id.map(js_trim).unwrap_or("");
    if !is_drive_id(id) {
        return Ok(UpdateCheck::NotDriveId);
    }
    let Some(seen) = gate.last_read(user_id, scope, id, now_ms) else {
        return Err(NOT_READ_IN_FULL);
    };
    if !seen.complete {
        return Err(READ_PARTLY);
    }
    Ok(UpdateCheck::ExpectVersion(seen.version.clone()))
}

/// `String.prototype.trim`, with the ECMAScript whitespace set (not `str::trim`, which also
/// removes U+0085 and keeps U+FEFF).
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

fn is_js_space(c: char) -> bool {
    matches!(
        c as u32,
        0x09..=0x0d
            | 0x20
            | 0xa0
            | 0x1680
            | 0x2000..=0x200a
            | 0x2028
            | 0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    )
}

/// `JSON.stringify(s)` for a string: quotes, backslashes and control characters escaped, all
/// else as is.
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The length of `s` in UTF-16 code units, as JavaScript counts it.
pub fn units(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The longest prefix of `s` that is at most `max` UTF-16 units, cut on a character boundary.
pub fn slice_units(s: &str, max: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        used += c.len_utf16();
        if used > max {
            return s.get(..i).unwrap_or("");
        }
    }
    s
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.filter(|v| !v.is_empty())
}
