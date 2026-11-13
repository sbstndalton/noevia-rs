//! `server/tool-route-notes.cjs` (#1520, #1521, #1786): what the model is told about this chat's
//! tools when the message depends on them. A request that names a storage connector goes to that
//! connector's tools (and a named connector that is off is said to be off); a time-sensitive
//! question with Web search off says the answer cannot be verified now; a message that is not
//! about stored files, when the tool gate picked no tool, is answered directly. Only notes are
//! produced: nothing here offers a tool or changes what the gate allows.

use super::js_regex::Regex;
use super::{test, units, Pattern};
use std::sync::LazyLock;

/// What `toolRouteNotes` reads.
#[derive(Debug, Clone, Copy)]
pub struct RouteInput<'a> {
    /// The user's message for this turn (Node reads a non-string as `''`).
    pub message: &'a str,
    /// The box ids this request carries (`requestToolboxes`).
    pub offered: &'a [String],
    /// The box ids this server offers at all (`allToolboxes`).
    pub on_server: &'a [String],
    /// Tools: Automatic (#1550). Default `false`.
    pub automatic: bool,
    /// Automatic only: the answering model is local. Default `true`.
    pub local_model: bool,
    /// #1786: the tool gate ran on this request and picked no tool. Default `false`.
    pub gate_none: bool,
    /// Automatic only: whether `tools-mode.cjs` `automaticBoxesFor(message)` includes
    /// `web-search` (the caller's tools-mode decision; a named difference, see the module root).
    pub automatic_web: bool,
}

impl Default for RouteInput<'_> {
    fn default() -> Self {
        RouteInput {
            message: "",
            offered: &[],
            on_server: &[],
            automatic: false,
            local_model: true,
            gate_none: false,
            automatic_web: false,
        }
    }
}

struct Connector {
    label: &'static str,
    box_id: &'static str,
    prefix: &'static str,
    named: &'static Pattern,
}

static NAMED_NEXTCLOUD: Pattern = LazyLock::new(|| Regex::new(r"\bnextcloud\b", true).ok());
static NAMED_DRIVE: Pattern =
    LazyLock::new(|| Regex::new(r"\bgoogle\s+drive\b|\bmy\s+drive\b|\bthe\s+drive\b", true).ok());
/// `ABOUT_FILES`.
static ABOUT_FILES: Pattern = LazyLock::new(|| {
    Regex::new(
        r"\b(files?|folders?|documents?|docs?|pdfs?|spreadsheets?|sheets?|slides?|notes?|attachments?|uploads?|uploaded|saved|stored|drive|nextcloud|webdav|dav)\b",
        true,
    )
    .ok()
});
/// `TIME_SENSITIVE`.
static TIME_SENSITIVE: Pattern = LazyLock::new(|| {
    Regex::new(
        r"\b(latest|newest|current(?:ly)?|today|tonight|right now|this (?:week|month|year)|as of|up[- ]to[- ]date|recent(?:ly)?|news|weather|exchange rate|stock price)\b",
        true,
    )
    .ok()
});

static CONNECTORS: [Connector; 2] = [
    Connector {
        label: "Nextcloud Files",
        box_id: "nextcloud-files",
        prefix: "nc_webdav_",
        named: &NAMED_NEXTCLOUD,
    },
    Connector {
        label: "Google Drive",
        box_id: "gdrive",
        prefix: "drive_",
        named: &NAMED_DRIVE,
    },
];
const WEB_BOX: &str = "web-search";

pub(super) fn patterns_compile() -> bool {
    [
        &NAMED_NEXTCLOUD,
        &NAMED_DRIVE,
        &ABOUT_FILES,
        &TIME_SENSITIVE,
    ]
    .iter()
    .all(|p| p.is_some())
}

/// `toolRouteNotes(input)`: the note for the system text, or `""` when none applies.
pub fn tool_route_notes(input: &RouteInput<'_>) -> String {
    let text = units(input.message);
    let on = |id: &str| input.offered.iter().any(|o| o == id);
    let exists = |id: &str| input.on_server.iter().any(|o| o == id);
    let mut notes: Vec<String> = Vec::new();
    let on_connectors: Vec<&Connector> = CONNECTORS.iter().filter(|c| on(c.box_id)).collect();
    for c in &CONNECTORS {
        if !test(c.named, &text) {
            continue;
        }
        let label = c.label;
        if on(c.box_id) {
            notes.push(format!("The user named {label}. Answer that request with the {label} tools ({}*), and do not search another storage in its place.", c.prefix));
        } else {
            let how = if !input.automatic {
                "turn it on under + > Tools, or connect it under Settings > Connectors if it is not connected yet"
            } else if !input.local_model && c.box_id != "gdrive" {
                "your own files are only offered to a local model; choose a local model to use it"
            } else {
                "connect it under Settings > Connectors, or ask the administrator to offer it on this server"
            };
            notes.push(format!("The user named {label}, but {label} is not available in this chat ({how}). Do not use another storage's tools in its place: say that {label} is not available in this chat."));
        }
    }
    if !notes.is_empty() && !on_connectors.is_empty() {
        let list: Vec<String> = on_connectors
            .iter()
            .map(|c| format!("{} ({}* tools)", c.label, c.prefix))
            .collect();
        notes.push(format!("Connectors in this chat, and the tools that belong to each: {}. Tools from one connector do not read another's files.", list.join("; ")));
    }
    if input.gate_none
        && !on_connectors.is_empty()
        && notes.is_empty()
        && !test(&ABOUT_FILES, &text)
    {
        let labels: Vec<&str> = on_connectors.iter().map(|c| c.label).collect();
        notes.push(format!("This message does not ask about the person's stored files, so answer it directly from what you know: do not search {} for it. A label, code or ID in the message (such as a ticket number or a test tag) is not a file to look up. Use those tools only if the person asks about a file or folder of theirs.", labels.join(" or ")));
    }
    let needs_web = if input.automatic {
        input.automatic_web
    } else {
        test(&TIME_SENSITIVE, &text)
    };
    if !on(WEB_BOX) && needs_web {
        let note = if exists(WEB_BOX) && !input.automatic {
            "Web search is off for this chat, so you cannot check current information. For a question about the latest or current state of something, say that you cannot verify it right now, give what you knew at your training cutoff and say it may be out of date, and suggest the user turn Web search on under + > Tools to check. Do not say that you cannot browse the internet in general, and do not present an older answer as the latest."
        } else if exists(WEB_BOX) {
            "Web search could not be used for this message, so you cannot check current information. For a question about the latest or current state of something, say that you cannot verify it right now, give what you knew at your training cutoff and say it may be out of date. Do not say that you cannot browse the internet in general, and do not present an older answer as the latest."
        } else {
            "Web search is not available on this server, so you cannot check current information. For a question about the latest or current state of something, say that you cannot verify it right now, give what you knew at your training cutoff and say it may be out of date. Do not present an older answer as the latest."
        };
        notes.push(note.to_string());
    }
    notes.join(" ")
}
