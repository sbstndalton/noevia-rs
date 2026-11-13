//! noevia-core's `server/tool-route-notes.cjs` `toolRouteNotes` (#1520/#1521/#1786, slice P6a):
//! what the model is told about the chat's tools when the message depends on them. Only notes;
//! nothing here offers a tool. The patterns are non-Unicode `/i` JS regexes, translated as
//! [`crate::tools_mode::js_regex`] says.

use std::sync::OnceLock;

use regex::Regex;

use crate::tools_mode::{automatic_boxes_for, compile};

/// `CONNECTORS`: label, box id, tool prefix and the pattern that names it.
const CONNECTORS: [(&str, &str, &str, &str); 2] = [
    (
        "Nextcloud Files",
        "nextcloud-files",
        "nc_webdav_",
        r"\bnextcloud\b",
    ),
    (
        "Google Drive",
        "gdrive",
        "drive_",
        r"\bgoogle\s+drive\b|\bmy\s+drive\b|\bthe\s+drive\b",
    ),
];
const WEB_BOX: &str = "web-search";
/// `ABOUT_FILES`.
const ABOUT_FILES: &str = r"\b(files?|folders?|documents?|docs?|pdfs?|spreadsheets?|sheets?|slides?|notes?|attachments?|uploads?|uploaded|saved|stored|drive|nextcloud|webdav|dav)\b";
/// `TIME_SENSITIVE`.
const TIME_SENSITIVE: &str = r"\b(latest|newest|current(?:ly)?|today|tonight|right now|this (?:week|month|year)|as of|up[- ]to[- ]date|recent(?:ly)?|news|weather|exchange rate|stock price)\b";

/// The JS sources, for the recording's check against the pinned module.
pub fn sources() -> [&'static str; 4] {
    [
        CONNECTORS[0].3,
        CONNECTORS[1].3,
        ABOUT_FILES,
        TIME_SENSITIVE,
    ]
}

struct Patterns {
    named: [Regex; 2],
    about_files: Regex,
    time_sensitive: Regex,
}

fn patterns() -> Option<&'static Patterns> {
    static P: OnceLock<Option<Patterns>> = OnceLock::new();
    P.get_or_init(|| {
        Some(Patterns {
            named: [compile(CONNECTORS[0].3)?, compile(CONNECTORS[1].3)?],
            about_files: compile(ABOUT_FILES)?,
            time_sensitive: compile(TIME_SENSITIVE)?,
        })
    })
    .as_ref()
}

/// What `toolRouteNotes` reads.
pub struct NotesInput<'a> {
    pub message: &'a str,
    /// `offered`: the box ids this request carries.
    pub offered: &'a [String],
    /// `onServer`: every box id this server offers.
    pub on_server: &'a [String],
    /// `automatic`: Tools: Automatic.
    pub automatic: bool,
    /// `localModel`.
    pub local_model: bool,
    /// `gateNone`: the tool gate ran and picked no tool.
    pub gate_none: bool,
}

/// `toolRouteNotes(input)`: the note for the system text, `''` when none applies (and, were a
/// constant pattern ever refused, `''` too; the recorded cases would fail).
pub fn tool_route_notes(n: &NotesInput<'_>) -> String {
    let Some(p) = patterns() else {
        return String::new();
    };
    let text = n.message.to_ascii_lowercase();
    let on = |id: &str| n.offered.iter().any(|o| o == id);
    let mut notes: Vec<String> = Vec::new();
    let on_connectors: Vec<_> = CONNECTORS.iter().filter(|c| on(c.1)).collect();
    for (k, (label, id, prefix, _)) in CONNECTORS.iter().enumerate() {
        if !p.named.get(k).is_some_and(|re| re.is_match(&text)) {
            continue;
        }
        if on(id) {
            notes.push(format!("The user named {label}. Answer that request with the {label} tools ({prefix}*), and do not search another storage in its place."));
        } else {
            let how = if !n.automatic {
                "turn it on under + > Tools, or connect it under Settings > Connectors if it is not connected yet"
            } else if !n.local_model && *id != "gdrive" {
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
            .map(|c| format!("{} ({}* tools)", c.0, c.2))
            .collect();
        notes.push(format!("Connectors in this chat, and the tools that belong to each: {}. Tools from one connector do not read another's files.", list.join("; ")));
    }
    if n.gate_none
        && !on_connectors.is_empty()
        && notes.is_empty()
        && !p.about_files.is_match(&text)
    {
        let labels: Vec<&str> = on_connectors.iter().map(|c| c.0).collect();
        notes.push(format!("This message does not ask about the person's stored files, so answer it directly from what you know: do not search {} for it. A label, code or ID in the message (such as a ticket number or a test tag) is not a file to look up. Use those tools only if the person asks about a file or folder of theirs.", labels.join(" or ")));
    }
    let needs_web = if n.automatic {
        automatic_boxes_for(n.message).contains(&WEB_BOX)
    } else {
        p.time_sensitive.is_match(&text)
    };
    if !on(WEB_BOX) && needs_web {
        let exists = n.on_server.iter().any(|o| o == WEB_BOX);
        notes.push(if exists && !n.automatic {
            "Web search is off for this chat, so you cannot check current information. For a question about the latest or current state of something, say that you cannot verify it right now, give what you knew at your training cutoff and say it may be out of date, and suggest the user turn Web search on under + > Tools to check. Do not say that you cannot browse the internet in general, and do not present an older answer as the latest."
        } else if exists {
            "Web search could not be used for this message, so you cannot check current information. For a question about the latest or current state of something, say that you cannot verify it right now, give what you knew at your training cutoff and say it may be out of date. Do not say that you cannot browse the internet in general, and do not present an older answer as the latest."
        } else {
            "Web search is not available on this server, so you cannot check current information. For a question about the latest or current state of something, say that you cannot verify it right now, give what you knew at your training cutoff and say it may be out of date. Do not present an older answer as the latest."
        }.to_string());
    }
    notes.join(" ")
}
