//! The replayed history of a turn: chat.cjs `handleChat`'s inline filter, `HISTORY_CAP` cut and
//! map of `body.history` ([`map_history`]), then its `normalizeReplayHistory` ([`normalize`]), which
//! makes the roles alternate for strict chat templates, folds tool results into the assistant turn
//! they belong to (framed as untrusted data), tells the model about applied (#658) and declined
//! (#666) writes, marks abandoned prompts (#1448) and carries a failed reply's public tool results
//! (#1692, #1700); off the box (#1703) it carries only what a client may not smuggle out.

use crate::js;
use js_json::JValue;

/// The replay's cut of a tool result.
pub const REPLAY_TOOL_RESULT_MAX: usize = 500;
/// The cut of an applied write's target and arguments.
pub const APPLIED_ARG_MAX: usize = 300;
/// The cut chat-sources.cjs `sanitizeFailure` makes (a non-empty trimmed failure stays non-empty).
pub const STORED_FAILURE_MAX: usize = 1000;
pub const FAILED_REPLY_TOOLS_NOTE: &str = "(The reply to the message above ended before its answer. These tool results came back for it and can be used.)";
pub const ABANDONED_TURN_NOTE: &str = "(No reply was kept for the message above: it was stopped or failed before an answer. It is not an open request. Answer only the newest message unless the user asks about it.)";
const DO_NOT_REPEAT: &str = "Do not run it again for the same change; if the user asks for it again, say it is already done unless they clearly want a second, separate change.";

/// One entry of the mapped history, as chat.cjs's map makes it: `{ role, content }`, plus the
/// tool's `name` for a tool or function entry, `applied` (with an optional `target` and `args`) or
/// `declined` for a tool write the client reports.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Entry {
    pub role: String,
    pub content: String,
    pub name: Option<String>,
    pub applied: bool,
    pub declined: bool,
    pub target: Option<String>,
    pub args: Option<String>,
}

/// The tool-name rules a replay applies; chat.cjs builds them from the server's toolboxes.
#[derive(Clone, Copy)]
pub struct ReplayOptions<'a> {
    /// `carryOrphanTool(name)`: a positively public tool (tools-mode.cjs `isPublicToolName`).
    pub carry_orphan_tool: &'a dyn Fn(&str) -> bool,
    /// The request leaves this box (#1703).
    pub off_box: bool,
    /// `knownTool(name)`: a tool some server toolbox registers (#1704).
    pub known_tool: &'a dyn Fn(&str) -> bool,
}

fn str_of<'a>(h: &'a JValue, key: &str) -> Option<&'a str> {
    h.get(key).as_str()
}

/// chat-sources.cjs `isFailedTurn(entry)`: a saved failed reply (#2116), a record for the person.
pub fn is_failed_turn(h: &JValue) -> bool {
    matches!(h, JValue::Obj(_))
        && str_of(h, "role") == Some("assistant")
        && str_of(h, "failure").is_some_and(|f| !js::slice(js::trim(f), STORED_FAILURE_MAX).is_empty())
}

fn applied_entry(h: &JValue) -> bool {
    str_of(h, "role") == Some("tool")
        && h.get("applied") == &JValue::Bool(true)
        && str_of(h, "name").is_some()
        && str_of(h, "content").is_some()
}

fn declined_entry(h: &JValue) -> bool {
    str_of(h, "role") == Some("tool")
        && h.get("declined") == &JValue::Bool(true)
        && h.get("applied") != &JValue::Bool(true)
        && str_of(h, "name").is_some()
        && str_of(h, "content").is_some()
}

/// chat.cjs's `mappedHistory`: of `body.history` (anything but an array is none), the user,
/// assistant, tool and function entries with text (or an applied write, whose result may be
/// empty), without saved failed replies, the last `cap` of them (`slice(-0)` keeps all), mapped
/// to [`Entry`].
pub fn map_history(history: &JValue, cap: usize) -> Vec<Entry> {
    let JValue::Arr(items) = history else {
        return Vec::new();
    };
    let kept: Vec<&JValue> = items
        .iter()
        .filter(|h| {
            h.truthy()
                && matches!(
                    str_of(h, "role"),
                    Some("user" | "assistant" | "tool" | "function")
                )
                && (str_of(h, "content").is_some_and(|c| !c.is_empty()) || applied_entry(h))
        })
        .filter(|h| !is_failed_turn(h))
        .collect();
    let from = if cap == 0 {
        0
    } else {
        kept.len().saturating_sub(cap)
    };
    kept.get(from..)
        .unwrap_or_default()
        .iter()
        .map(|h| map_entry(h))
        .collect()
}

fn map_entry(h: &JValue) -> Entry {
    let role = str_of(h, "role").unwrap_or_default().to_string();
    let content = str_of(h, "content").unwrap_or_default().to_string();
    let name = str_of(h, "name").map(str::to_string);
    if applied_entry(h) {
        return Entry {
            role,
            content,
            name,
            applied: true,
            target: str_of(h, "target").map(str::to_string),
            args: str_of(h, "args").map(str::to_string),
            ..Entry::default()
        };
    }
    if declined_entry(h) {
        return Entry {
            role,
            content,
            name,
            declined: true,
            ..Entry::default()
        };
    }
    let tool = role == "tool" || role == "function";
    Entry {
        name: if tool { name } else { None },
        role,
        content,
        ..Entry::default()
    }
}

/// `foldToolMessage(entry)`.
fn fold_tool_message(e: &Entry) -> String {
    let label = e
        .name
        .as_deref()
        .map(|n| js::slice(n, 80))
        .unwrap_or_default();
    js::frame_untrusted(
        "tool result",
        &label,
        &js::clip(&e.content, REPLAY_TOOL_RESULT_MAX),
    )
}

/// The name a note may write: `/^[\w.-]{1,80}$/`, else 'a tool'.
fn plain_name(e: &Entry) -> &str {
    e.name
        .as_deref()
        .filter(|n| {
            (1..=80).contains(&n.len())
                && n.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        })
        .unwrap_or("a tool")
}

/// `appliedToolNote(entry)` (#658).
fn applied_tool_note(e: &Entry) -> String {
    let name = plain_name(e);
    let mut lines = Vec::new();
    if let Some(t) = e.target.as_deref().filter(|t| !t.is_empty()) {
        lines.push(format!("target: {}", js::clip(t, APPLIED_ARG_MAX)));
    }
    if let Some(a) = e.args.as_deref().filter(|a| !a.is_empty()) {
        lines.push(format!("arguments: {}", js::clip(a, APPLIED_ARG_MAX)));
    }
    lines.push(format!(
        "result: {}",
        js::clip(&e.content, REPLAY_TOOL_RESULT_MAX)
    ));
    format!(
        "Already done earlier in this chat: {name} ran after the user approved it, and it succeeded. {DO_NOT_REPEAT}\n{}",
        js::frame_untrusted("applied change", name, &lines.join("\n"))
    )
}

/// `declinedToolNote(entry)` (#666).
fn declined_tool_note(e: &Entry) -> String {
    let name = plain_name(e);
    format!(
        "Not run earlier in this chat: the user did not approve {name}, so it did not run and nothing was changed by it.\n{}",
        js::frame_untrusted("tool result", name, &js::clip(&e.content, REPLAY_TOOL_RESULT_MAX))
    )
}

/// The client's name when it is a tool a server toolbox registers (#1704), else 'a tool'.
fn known_name<'a>(e: &'a Entry, opts: &ReplayOptions<'_>) -> &'a str {
    e.name
        .as_deref()
        .filter(|n| (opts.known_tool)(n))
        .unwrap_or("a tool")
}

fn carries(e: &Entry, opts: &ReplayOptions<'_>) -> bool {
    e.name.as_deref().is_some_and(|n| (opts.carry_orphan_tool)(n))
}

fn applied_note(e: &Entry, opts: &ReplayOptions<'_>) -> String {
    if opts.off_box && !carries(e, opts) {
        // appliedSummaryNote (#1703): that a change was made, and with which tool; nothing else.
        format!(
            "Already done earlier in this chat: (a change was applied with {}). {DO_NOT_REPEAT}",
            known_name(e, opts)
        )
    } else {
        applied_tool_note(e)
    }
}

fn declined_note(e: &Entry, opts: &ReplayOptions<'_>) -> String {
    if opts.off_box {
        // declinedSummaryNote (#1703): our sentence only.
        format!("Not run earlier in this chat: the user did not approve {}, so it did not run and nothing was changed by it.", known_name(e, opts))
    } else {
        declined_tool_note(e)
    }
}

struct Turn {
    role: String,
    content: String,
    /// A turn made to carry a failed reply's results (#1700's `orphanCarriers`).
    carrier: bool,
}

fn turn(role: &str, content: String) -> Turn {
    Turn {
        role: role.to_string(),
        content,
        carrier: false,
    }
}

/// `normalizeReplayHistory(mapped, newMessage, options)`: `new_message` `None` is a compaction
/// (no new user turn). Every message is `{ role, content }`.
pub fn normalize(
    mapped: &[Entry],
    new_message: Option<&str>,
    opts: &ReplayOptions<'_>,
) -> Vec<JValue> {
    let mut out: Vec<Turn> = Vec::new();
    let mut leading: Vec<String> = Vec::new();
    for e in mapped {
        let tool = e.role == "tool" || e.role == "function";
        // What `last` was before this entry: none, or its role and whether it carries (#1700).
        let last = out.last().map(|t| (t.role.clone(), t.carrier));
        let last_assistant = last.as_ref().is_some_and(|(r, _)| r == "assistant");
        let add = |out: &mut Vec<Turn>, text: &str| {
            if let Some(l) = out.last_mut() {
                l.content = format!("{}\n\n{text}", l.content);
            }
        };
        if tool && e.applied {
            let note = applied_note(e, opts);
            if last_assistant {
                add(&mut out, &note);
            } else if last.is_some() {
                out.push(turn("assistant", note));
            } else {
                leading.push(note);
            }
        } else if tool && e.declined {
            if last_assistant {
                add(&mut out, &declined_note(e, opts));
            } else if last.is_some() {
                out.push(turn("assistant", declined_note(e, opts)));
            }
        } else if tool {
            let carrier = last.as_ref().is_some_and(|(_, c)| *c);
            if last_assistant && carrier {
                if carries(e, opts) {
                    add(&mut out, &fold_tool_message(e));
                }
            } else if last_assistant {
                if !opts.off_box || carries(e, opts) {
                    add(&mut out, &fold_tool_message(e));
                }
            } else if last.is_some() && carries(e, opts) {
                out.push(Turn {
                    role: "assistant".into(),
                    content: format!("{FAILED_REPLY_TOOLS_NOTE}\n\n{}", fold_tool_message(e)),
                    carrier: true,
                });
            }
        } else if last_assistant && e.role == "assistant" {
            add(&mut out, &e.content);
        } else if e.role == "user" && last.as_ref().is_some_and(|(r, _)| r == "user") {
            out.push(turn("assistant", ABANDONED_TURN_NOTE.into()));
            out.push(turn(&e.role, e.content.clone()));
        } else {
            out.push(turn(&e.role, e.content.clone()));
        }
    }
    // A slice can start mid-exchange on an assistant turn; strict templates want a user turn first.
    let first_kept = out
        .iter()
        .position(|t| t.role != "assistant")
        .unwrap_or(out.len());
    out.drain(..first_kept);
    if let Some(message) = new_message {
        if out.last().is_some_and(|t| t.role == "user") {
            out.push(turn("assistant", ABANDONED_TURN_NOTE.into()));
        }
        out.push(turn("user", message.to_string()));
    }
    if let Some(first) = out.first_mut().filter(|t| t.role == "user") {
        if !leading.is_empty() {
            first.content = format!("{}\n\n{}", leading.join("\n\n"), first.content);
        }
    }
    out.into_iter()
        .map(|t| js::message(&t.role, t.content))
        .collect()
}

/// [`map_history`] then [`normalize`]: the turn's `msgs`.
pub fn replay_history(
    history: &JValue,
    cap: usize,
    new_message: Option<&str>,
    opts: &ReplayOptions<'_>,
) -> Vec<JValue> {
    normalize(&map_history(history, cap), new_message, opts)
}
