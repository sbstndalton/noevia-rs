//! In-app storage file tools (noevia#2759, first slice): the `storage-files` box definition, strict
//! argument parsing, the Diary-folder refusal for calls bound for an external provider, and
//! execution through `server-storage-reads` (`protocol::execute` over any [`Transport`]).
//!
//! Inert: nothing depends on this crate yet. Every tool is a read; the output and every message
//! are built from the requested paths, the listing and the read engine's fixed public failure
//! texts only, never from the connection's base URL, user name or secret.
#![forbid(unsafe_code)]

use js_json::JValue;
use server_storage_reads::protocol::{self, Connection, Kind, Operation, Transport};
use server_storage_reads::Failure;
use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::time::Duration;

/// The box id in the tool-box manifest.
pub const BOX_ID: &str = "storage-files";
/// The tool server the box belongs to (noevia's own, in-app tools).
pub const SERVER: &str = "noevia";

/// The longest string argument, in UTF-8 bytes (the storage path rule's own bound is 500 UTF-16
/// units).
pub const MAX_ARGUMENT_BYTES: usize = 500;
/// The longest arguments object text accepted (three escaped strings fit well within it).
pub const MAX_ARGUMENTS_TEXT: usize = 8 * 1024;
/// The longest extension, in characters.
pub const MAX_EXTENSION_CHARS: usize = 16;
/// `storage_find_files`: the most folder listings one search makes.
pub const MAX_FOLDERS: usize = 40;
/// `storage_find_files`: the most matches one search returns.
pub const MAX_MATCHES: usize = 50;
/// `storage_find_files`: the deepest folder listed, counted from the start folder (0).
pub const MAX_DEPTH: usize = 6;
/// `storage_find_files`: no new listing starts once this much time has passed since the search
/// began. It does not cut a listing already running, which keeps the read engine's own 31 s
/// budget, so one search can take up to about 91 s.
pub const FIND_DEADLINE: Duration = Duration::from_secs(60);

/// What a refused private path answers.
pub const PRIVATE_REFUSAL: &str = "This folder is private and cannot be used here.";

/// One tool of the box: its name, its model-facing description and its JSON Schema (object text).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: &'static str,
    pub write: bool,
}

pub const LIST_FOLDER: &str = "storage_list_folder";
pub const READ_FILE: &str = "storage_read_file";
pub const FIND_FILES: &str = "storage_find_files";

/// The box's tools. Descriptions are terse: they are sent with every request.
pub const TOOLS: &[Tool] = &[
    Tool {
        name: LIST_FOLDER,
        description: "List a folder in the person's connected storage. Paths are relative to the storage root.",
        input_schema: r#"{"type":"object","properties":{"path":{"type":"string","description":"Folder path; empty or omitted for the storage root."}},"additionalProperties":false}"#,
        write: false,
    },
    Tool {
        name: READ_FILE,
        description: "Read a text file from the person's connected storage. Paths are relative to the storage root.",
        input_schema: r#"{"type":"object","properties":{"path":{"type":"string","description":"File path, e.g. \"Notes/plan.md\"."}},"required":["path"],"additionalProperties":false}"#,
        write: false,
    },
    Tool {
        name: FIND_FILES,
        description: "Find files in the person's connected storage by name and/or extension, searching subfolders. Paths are relative to the storage root.",
        input_schema: r#"{"type":"object","properties":{"name":{"type":"string","description":"Part of the file name."},"extension":{"type":"string","description":"File extension, e.g. \"md\"."},"folder":{"type":"string","description":"Folder to search from; empty or omitted for the storage root."}},"additionalProperties":false}"#,
        write: false,
    },
];

/// The box's line in the tool-box manifest: `{id, server, label, description, tools, reads}`.
pub fn manifest_line() -> String {
    let tools = TOOLS
        .iter()
        .map(|tool| JValue::from(tool.name))
        .collect::<Vec<_>>();
    let reads = TOOLS
        .iter()
        .filter(|tool| !tool.write)
        .map(|tool| JValue::from(tool.name))
        .collect::<Vec<_>>();
    let line = JValue::obj([
        ("id", JValue::from(BOX_ID)),
        ("server", JValue::from(SERVER)),
        ("label", JValue::from("Storage files")),
        (
            "description",
            JValue::from("List, read and find files in your connected storage."),
        ),
        ("tools", JValue::Arr(tools)),
        ("reads", JValue::Arr(reads)),
    ]);
    js_json::stringify(&line).unwrap_or_default()
}

/// A parsed tool call. Paths are as given (the read engine applies the storage path rule);
/// `extension` is lowercase without its leading dot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    List {
        path: String,
    },
    Read {
        path: String,
    },
    Find {
        name: Option<String>,
        extension: Option<String>,
        folder: String,
    },
}

/// A model-facing refusal or failure: short, and never naming the connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolError {
    pub message: String,
}

impl ToolError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<Failure> for ToolError {
    /// The read engine's public failure text is fixed or derived only from the requested name.
    fn from(failure: Failure) -> Self {
        Self::new(failure.message)
    }
}

/// Parse one call: `tool` is the tool name, `arguments` the JSON text of its arguments object
/// (empty text counts as `{}`).
pub fn parse(tool: &str, arguments: &str) -> Result<Call, ToolError> {
    let allowed: &[&str] = match tool {
        LIST_FOLDER | READ_FILE => &["path"],
        FIND_FILES => &["name", "extension", "folder"],
        _ => return Err(ToolError::new("Unknown storage tool.")),
    };
    if arguments.len() > MAX_ARGUMENTS_TEXT {
        return Err(ToolError::new("The arguments are too long."));
    }
    let value = if arguments.trim().is_empty() {
        JValue::Obj(Vec::new())
    } else {
        js_json::parse(arguments)
            .map_err(|_| ToolError::new("The arguments must be a JSON object."))?
    };
    let JValue::Obj(pairs) = &value else {
        return Err(ToolError::new("The arguments must be a JSON object."));
    };
    if pairs
        .iter()
        .any(|(key, _)| !allowed.contains(&key.as_str()))
    {
        let list = allowed.join(", ");
        return Err(ToolError::new(format!(
            "Unknown argument; this tool takes: {list}."
        )));
    }
    match tool {
        LIST_FOLDER => Ok(Call::List {
            path: string_arg(&value, "path")?.unwrap_or_default(),
        }),
        READ_FILE => match string_arg(&value, "path")? {
            Some(path) if !path.trim().is_empty() => Ok(Call::Read { path }),
            _ => Err(ToolError::new("path is required.")),
        },
        _ => {
            let name = string_arg(&value, "name")?.filter(|s| !s.trim().is_empty());
            let extension = match string_arg(&value, "extension")? {
                Some(raw) => extension_arg(&raw)?,
                None => None,
            };
            if name.is_none() && extension.is_none() {
                return Err(ToolError::new("Give a name, an extension, or both."));
            }
            Ok(Call::Find {
                name,
                extension,
                folder: string_arg(&value, "folder")?.unwrap_or_default(),
            })
        }
    }
}

/// An optional string argument: absent or `null` is `None`; any other non-string is refused.
fn string_arg(value: &JValue, key: &str) -> Result<Option<String>, ToolError> {
    match value.get(key) {
        JValue::Undefined | JValue::Null => Ok(None),
        JValue::Str(s) if s.len() > MAX_ARGUMENT_BYTES => Err(ToolError::new(format!(
            "{key} is too long (at most {MAX_ARGUMENT_BYTES} bytes)."
        ))),
        JValue::Str(s) => Ok(Some(s.clone())),
        _ => Err(ToolError::new(format!("{key} must be a string."))),
    }
}

/// The wanted extension, lowercase without its leading dot; empty is `None`. Matching compares
/// the text after a name's last dot, so a value with another dot, a `*` or white space (`*.md`,
/// `tar.gz`, `..md`) could never match and is refused.
fn extension_arg(raw: &str) -> Result<Option<String>, ToolError> {
    let trimmed = raw.trim();
    let bare = trimmed.strip_prefix('.').unwrap_or(trimmed);
    if bare.chars().count() > MAX_EXTENSION_CHARS
        || bare
            .chars()
            .any(|ch| matches!(ch, '/' | '\\' | '*' | '.') || ch.is_whitespace())
    {
        return Err(ToolError::new(
            "extension must be a short file extension like \"md\".",
        ));
    }
    let bare = bare.to_lowercase();
    Ok(if bare.is_empty() { None } else { Some(bare) })
}

/// A path as the private-folder check compares it: slashes trimmed, the storage path rule
/// applied, ASCII lowercase. `None` when the rule refuses the input outright.
fn normalise(path: &str) -> Option<String> {
    storage_path::safe_relative_path(path.trim_matches('/'))
        .ok()
        .map(|p| p.to_ascii_lowercase())
}

fn inside(path: &str, root: &str) -> bool {
    path.strip_prefix(root)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The private folder, as a guard: none, a normalised folder, or everything (fail closed).
enum Guard {
    Open,
    Private(String),
    Closed,
}

impl Guard {
    /// Only a WebDAV or Nextcloud connection lists paths relative to a root that contains the
    /// Diary folder. The read engine prefixes every S3 path with the corpus root (everything the
    /// tools reach is inside the Diary folder), and any other kind is not identified: closed,
    /// as provider-egress `diary_folder` treats them.
    fn new(root: Option<&str>, kind: Kind) -> Self {
        match root {
            None => Self::Open,
            Some(_) if !matches!(kind, Kind::WebDav | Kind::Nextcloud) => Self::Closed,
            Some(root) => match normalise(root) {
                Some(n) if !n.is_empty() => Self::Private(n),
                _ => Self::Closed,
            },
        }
    }
    /// Whether `path` is the private folder or inside it.
    fn blocks(&self, path: &str) -> bool {
        match self {
            Self::Open => false,
            Self::Closed => true,
            Self::Private(root) => normalise(path).is_none_or(|p| inside(&p, root)),
        }
    }
}

/// For a call bound for an external provider: a refusal when the call's path (or start folder) is
/// the Diary folder `corpus_root` or inside it, on a connection of `kind`. A `corpus_root` that
/// normalises to nothing, or a kind other than WebDAV/Nextcloud, refuses every call. A search from a folder that contains the Diary folder is not refused here: [`run`]
/// skips it (its `skip` argument).
pub fn private_refusal(call: &Call, kind: Kind, corpus_root: &str) -> Option<String> {
    let path = match call {
        Call::List { path } | Call::Read { path } => path,
        Call::Find { folder, .. } => folder,
    };
    Guard::new(Some(corpus_root), kind)
        .blocks(path)
        .then(|| PRIVATE_REFUSAL.to_owned())
}

fn units(path: &str) -> Vec<u16> {
    path.encode_utf16().collect()
}

/// Run one call over the connection. `skip`: a folder never read or descended into (the Diary
/// folder for a call bound for an external provider); a call whose own path is inside it is
/// refused, and a `skip` that normalises to nothing, or a `skip` on a connection that is not
/// WebDAV/Nextcloud, refuses every call before any request.
pub fn run(
    call: &Call,
    c: &Connection,
    t: &mut impl Transport,
    skip: Option<&str>,
) -> Result<String, ToolError> {
    let guard = Guard::new(skip, c.kind);
    match call {
        Call::List { path } => {
            if guard.blocks(path) {
                return Err(ToolError::new(PRIVATE_REFUSAL));
            }
            Ok(protocol::execute(c, &units(path), Operation::List, t)?)
        }
        Call::Read { path } => {
            if guard.blocks(path) {
                return Err(ToolError::new(PRIVATE_REFUSAL));
            }
            Ok(protocol::execute(c, &units(path), Operation::Read, t)?)
        }
        Call::Find {
            name,
            extension,
            folder,
        } => {
            if guard.blocks(folder) {
                return Err(ToolError::new(PRIVATE_REFUSAL));
            }
            let want = Want {
                name: name.as_deref().map(str::to_lowercase),
                extension: extension
                    .as_deref()
                    .map(|e| e.strip_prefix('.').unwrap_or(e).to_lowercase()),
            };
            find(c, t, folder, &want, &guard)
        }
    }
}

struct Want {
    name: Option<String>,
    extension: Option<String>,
}

impl Want {
    fn matches(&self, name: &str) -> bool {
        let name_ok = self
            .name
            .as_deref()
            .is_none_or(|fragment| name.to_lowercase().contains(fragment));
        let extension_ok = self.extension.as_deref().is_none_or(|wanted| {
            let got = protocol::extension(name);
            got.strip_prefix('.') == Some(wanted)
        });
        name_ok && extension_ok
    }
}

/// The bounded breadth-first search behind `storage_find_files`.
fn find(
    c: &Connection,
    t: &mut impl Transport,
    folder: &str,
    want: &Want,
    guard: &Guard,
) -> Result<String, ToolError> {
    let deadline = t.elapsed().checked_add(FIND_DEADLINE);
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    queue.push_back((folder.to_owned(), 0));
    let mut matches: Vec<JValue> = Vec::new();
    let mut listings = 0usize;
    let mut searched = 0usize;
    let mut complete = true;
    // Folders already queued, by their path as the read engine requests it: the path rule trims
    // a name's white space, so "Notes/ " or a root child " " would list a folder again.
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(listed_as(folder));
    'walk: while let Some((dir, depth)) = queue.pop_front() {
        let late = deadline.is_none_or(|d| t.elapsed() > d);
        if listings >= MAX_FOLDERS || late {
            complete = false;
            break;
        }
        let start = listings == 0;
        listings += 1;
        let entries = match protocol::execute(c, &units(&dir), Operation::List, t) {
            Ok(text) => match js_json::parse(&text) {
                Ok(JValue::Arr(entries)) => entries,
                _ if start => {
                    return Err(ToolError::new("storage listing could not be read"));
                }
                _ => {
                    complete = false;
                    continue;
                }
            },
            Err(failure) if start => return Err(failure.into()),
            Err(_) => {
                complete = false;
                continue;
            }
        };
        searched += 1;
        for entry in &entries {
            let name = entry.get("name").as_str().unwrap_or_default();
            let path = entry.get("path").as_str().unwrap_or_default();
            if name.is_empty() || path.is_empty() {
                continue;
            }
            if entry.get("isDir").as_bool().unwrap_or(false) {
                if guard.blocks(path) {
                    continue; // the private folder: deliberately not searched
                }
                if depth >= MAX_DEPTH {
                    complete = false;
                    continue;
                }
                if !seen.insert(listed_as(path)) {
                    continue; // the same folder under a white-space variant of its name
                }
                queue.push_back((path.to_owned(), depth + 1));
            } else if want.matches(name) {
                if matches.len() >= MAX_MATCHES {
                    complete = false;
                    break 'walk;
                }
                let size = match entry.get("size") {
                    JValue::Num(n) => JValue::Num(*n),
                    _ => JValue::Null,
                };
                matches.push(JValue::obj([("path", JValue::from(path)), ("size", size)]));
            }
        }
    }
    let out = JValue::obj([
        ("matches", JValue::Arr(matches)),
        (
            "foldersSearched",
            JValue::from(u32::try_from(searched).unwrap_or(u32::MAX)),
        ),
        ("complete", JValue::from(complete)),
    ]);
    Ok(js_json::stringify(&out).unwrap_or_default())
}

/// The folder a listing of `path` requests (the storage path rule, as the read engine applies it);
/// the path itself when the rule refuses it outright (that listing then fails on its own).
fn listed_as(path: &str) -> String {
    storage_path::safe_relative_path(path).unwrap_or_else(|_| path.to_owned())
}
