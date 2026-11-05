//! The saved history a turn replays (core chat.cjs `createChatHandler`'s history mapping and
//! `normalizeReplayHistory`, #658, #666, #1448, #1692, #1700, #1703, #1704, #2116).
use crate::text::{as_str, clip, frame, get, set, spread, string, trim};
use crate::Thrown;
use js_json::{wtf16, JValue};
use std::collections::HashSet;

const REPLAY_TOOL_RESULT_MAX: usize = 500;
const APPLIED_ARG_MAX: usize = 300;
/// core chat-sources.cjs `STORED_FAILURE_MAX`.
const STORED_FAILURE_MAX: usize = 1000;
/// #1692: the lead of a turn that holds only the tool results of a reply that ended before its answer.
pub const FAILED_REPLY_TOOLS_NOTE: &str = "(The reply to the message above ended before its answer. These tool results came back for it and can be used.)";
/// #1448: between a prompt that got no reply and the next turn.
pub const ABANDONED_TURN_NOTE: &str = "(No reply was kept for the message above: it was stopped or failed before an answer. It is not an open request. Answer only the newest message unless the user asks about it.)";

/// core tools-mode.cjs `PRIVATE_CORE_TOOLS`.
pub const PRIVATE_CORE_TOOLS: [&str; 1] = ["read_project_file"];
/// core tools-mode.cjs `HEALTH_PUBLIC_BOXES`.
const HEALTH_PUBLIC_BOXES: [&str; 2] = ["web-search", "offline-wikipedia"];
/// core tools-mode.cjs `CLOUD_SAFE_BOXES`.
const CLOUD_SAFE_BOXES: [&str; 4] = ["core", "web-search", "web-crawl", "offline-wikipedia"];

fn is_str(v: &JValue, s: &str) -> bool {
    as_str(v) == Some(s)
}

fn tool_role(entry: &JValue) -> bool {
    is_str(get(entry, "role"), "tool") || is_str(get(entry, "role"), "function")
}

/// `!!b && (b.tools || []).some((t) => t?.function?.name === name)`.
fn offers(b: &JValue, name: &str) -> bool {
    let tools = get(b, "tools");
    let list: &[JValue] = match tools {
        JValue::Arr(a) => a,
        _ => &[],
    };
    b.truthy() && list.iter().any(|t| is_str(get(get(t, "function"), "name"), name))
}

fn boxes(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(a) => a,
        _ => &[],
    }
}

/// core tools-mode.cjs `isPublicToolName(name, boxes)` (#1592, #1700): offered by a search or
/// Wikipedia box, and not by a box of private data.
pub fn is_public_tool_name(name: &JValue, toolboxes: &JValue) -> bool {
    let Some(name) = as_str(name) else {
        return false;
    };
    if name.is_empty() || PRIVATE_CORE_TOOLS.contains(&name) {
        return false;
    }
    let list = boxes(toolboxes);
    let id = |b: &JValue| as_str(get(b, "id")).map(str::to_owned);
    let private = |b: &JValue| !id(b).is_some_and(|i| CLOUD_SAFE_BOXES.contains(&i.as_str()));
    !list.iter().any(|b| b.truthy() && private(b) && offers(b, name))
        && list.iter().any(|b| {
            b.truthy()
                && id(b).is_some_and(|i| HEALTH_PUBLIC_BOXES.contains(&i.as_str()))
                && offers(b, name)
        })
}

/// chat.cjs `knownTool` (#1704): a tool name some server toolbox registers, never a client's free text.
pub fn is_known_tool(name: &JValue, toolboxes: &JValue) -> bool {
    let Some(name) = as_str(name) else {
        return false;
    };
    PRIVATE_CORE_TOOLS.contains(&name) || boxes(toolboxes).iter().any(|b| offers(b, name))
}

/// core chat-sources.cjs `isFailedTurn` (#2116): a saved failed reply is a record for the person.
pub fn is_failed_turn(entry: &JValue) -> bool {
    matches!(entry, JValue::Obj(_))
        && is_str(get(entry, "role"), "assistant")
        && as_str(get(entry, "failure"))
            .is_some_and(|f| !wtf16::slice(trim(f), STORED_FAILURE_MAX).is_empty())
}

fn applied_entry(h: &JValue) -> bool {
    is_str(get(h, "role"), "tool")
        && get(h, "applied") == &JValue::Bool(true)
        && as_str(get(h, "name")).is_some()
        && as_str(get(h, "content")).is_some()
}

fn declined_entry(h: &JValue) -> bool {
    is_str(get(h, "role"), "tool")
        && get(h, "declined") == &JValue::Bool(true)
        && get(h, "applied") != &JValue::Bool(true)
        && as_str(get(h, "name")).is_some()
        && as_str(get(h, "content")).is_some()
}

fn obj(pairs: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

/// The history items a turn replays, as `createChatHandler` maps the client's `history`: kept
/// roles with text (or an applied write), saved failed replies dropped, the last `cap` items
/// (`slice(-cap)`), each rebuilt with only the fields the replay reads.
pub fn map_history(history: &JValue, cap: i64) -> Vec<JValue> {
    let kept: Vec<&JValue> = boxes(history)
        .iter()
        .filter(|h| {
            h.truthy()
                && ["user", "assistant", "tool", "function"]
                    .iter()
                    .any(|r| is_str(get(h, "role"), r))
                && (as_str(get(h, "content")).is_some_and(|c| !c.is_empty()) || applied_entry(h))
        })
        .filter(|h| !is_failed_turn(h))
        .collect();
    // `slice(-cap)`: -0 is 0 (everything); a negative cap counts from the start.
    let len = kept.len() as i64;
    let start = if cap == 0 {
        0
    } else if cap > 0 {
        (len - cap).max(0)
    } else {
        (-cap).min(len)
    };
    kept.into_iter()
        .skip(usize::try_from(start).unwrap_or(0))
        .map(|h| {
            if applied_entry(h) {
                let mut o = vec![
                    ("role", JValue::Str("tool".into())),
                    ("content", get(h, "content").clone()),
                    ("name", get(h, "name").clone()),
                    ("applied", JValue::Bool(true)),
                ];
                if as_str(get(h, "target")).is_some() {
                    o.push(("target", get(h, "target").clone()));
                }
                if as_str(get(h, "args")).is_some() {
                    o.push(("args", get(h, "args").clone()));
                }
                obj(o)
            } else if declined_entry(h) {
                obj(vec![
                    ("role", JValue::Str("tool".into())),
                    ("content", get(h, "content").clone()),
                    ("name", get(h, "name").clone()),
                    ("declined", JValue::Bool(true)),
                ])
            } else if tool_role(h) && as_str(get(h, "name")).is_some() {
                obj(vec![
                    ("role", get(h, "role").clone()),
                    ("content", get(h, "content").clone()),
                    ("name", get(h, "name").clone()),
                ])
            } else {
                obj(vec![
                    ("role", get(h, "role").clone()),
                    ("content", get(h, "content").clone()),
                ])
            }
        })
        .collect()
}

/// What decides how tool entries replay (`normalizeReplayHistory`'s options).
pub struct ReplayOptions<'a> {
    /// `carryOrphanTool(name)`: a positively public tool ([`is_public_tool_name`]).
    pub carry_orphan_tool: &'a dyn Fn(&JValue) -> bool,
    /// The request leaves this box (#1703).
    pub off_box: bool,
    /// `knownTool(name)` ([`is_known_tool`]).
    pub known_tool: &'a dyn Fn(&JValue) -> bool,
}

fn safe_name(entry: &JValue) -> String {
    match as_str(get(entry, "name")) {
        Some(n)
            if (1..=80).contains(&n.len())
                && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-') =>
        {
            n.to_owned()
        }
        _ => "a tool".to_owned(),
    }
}

/// `String(value || '')`.
fn or_empty(v: &JValue) -> Result<String, Thrown> {
    if v.truthy() {
        string(v)
    } else {
        Ok(String::new())
    }
}

fn fold_tool_message(entry: &JValue) -> Result<String, Thrown> {
    let text = string(get(entry, "content"))?;
    let label = as_str(get(entry, "name")).map_or(String::new(), |n| wtf16::slice(n, 80));
    Ok(frame("tool result", &label, &clip(&text, REPLAY_TOOL_RESULT_MAX)))
}

fn applied_tool_note(entry: &JValue) -> Result<String, Thrown> {
    let name = safe_name(entry);
    let mut lines = Vec::new();
    if let Some(t) = as_str(get(entry, "target")).filter(|t| !t.is_empty()) {
        lines.push(format!("target: {}", clip(t, APPLIED_ARG_MAX)));
    }
    if let Some(a) = as_str(get(entry, "args")).filter(|a| !a.is_empty()) {
        lines.push(format!("arguments: {}", clip(a, APPLIED_ARG_MAX)));
    }
    lines.push(format!(
        "result: {}",
        clip(&or_empty(get(entry, "content"))?, REPLAY_TOOL_RESULT_MAX)
    ));
    Ok(format!(
        "Already done earlier in this chat: {name} ran after the user approved it, and it succeeded. Do not run it again for the same change; if the user asks for it again, say it is already done unless they clearly want a second, separate change.\n{}",
        frame("applied change", &name, &lines.join("\n"))
    ))
}

fn declined_tool_note(entry: &JValue) -> Result<String, Thrown> {
    let name = safe_name(entry);
    let text = or_empty(get(entry, "content"))?;
    Ok(format!(
        "Not run earlier in this chat: the user did not approve {name}, so it did not run and nothing was changed by it.\n{}",
        frame("tool result", &name, &clip(&text, REPLAY_TOOL_RESULT_MAX))
    ))
}

fn summary_name(entry: &JValue, known: &dyn Fn(&JValue) -> bool) -> String {
    let n = get(entry, "name");
    match as_str(n) {
        Some(s) if known(n) => s.to_owned(),
        _ => "a tool".to_owned(),
    }
}

fn applied_summary_note(entry: &JValue, known: &dyn Fn(&JValue) -> bool) -> String {
    format!("Already done earlier in this chat: (a change was applied with {}). Do not run it again for the same change; if the user asks for it again, say it is already done unless they clearly want a second, separate change.", summary_name(entry, known))
}

fn declined_summary_note(entry: &JValue, known: &dyn Fn(&JValue) -> bool) -> String {
    format!("Not run earlier in this chat: the user did not approve {}, so it did not run and nothing was changed by it.", summary_name(entry, known))
}

/// `last.content = `${last.content}\n\n${note}``.
fn append(last: &mut JValue, note: &str) -> Result<(), Thrown> {
    let was = string(get(last, "content"))?;
    set(last, "content", JValue::Str(format!("{was}\n\n{note}")));
    Ok(())
}

fn message(role: &str, content: String) -> JValue {
    obj(vec![
        ("role", JValue::Str(role.into())),
        ("content", JValue::Str(content)),
    ])
}

/// core chat.cjs `normalizeReplayHistory(mapped, newMessage, options)`: strict templates need
/// alternating roles that start with a user turn; tool entries cannot replay as they are, so they
/// fold into the assistant turn they belong to as framed blocks (#658 applied, #666 declined, #1692
/// a failed reply's public results, #1703 off the box only what a client cannot smuggle out).
pub fn normalize_replay_history(
    mapped: &[JValue],
    new_message: &JValue,
    o: &ReplayOptions<'_>,
) -> Result<Vec<JValue>, Thrown> {
    let applied_note = |e: &JValue| -> Result<String, Thrown> {
        if o.off_box && !(o.carry_orphan_tool)(get(e, "name")) {
            Ok(applied_summary_note(e, o.known_tool))
        } else {
            applied_tool_note(e)
        }
    };
    let declined_note = |e: &JValue| -> Result<String, Thrown> {
        if o.off_box {
            Ok(declined_summary_note(e, o.known_tool))
        } else {
            declined_tool_note(e)
        }
    };
    let mut out: Vec<JValue> = Vec::new();
    let mut carriers: HashSet<usize> = HashSet::new();
    let mut leading: Vec<String> = Vec::new();
    for entry in mapped {
        // `entry.role` on null or undefined throws.
        if entry.is_nullish() {
            return Err(Thrown);
        }
        let last_at = out.len().checked_sub(1);
        let last_role = last_at.and_then(|i| out.get(i)).map(|l| get(l, "role").clone());
        let last_assistant = last_role.as_ref().is_some_and(|r| is_str(r, "assistant"));
        if tool_role(entry) && get(entry, "applied") == &JValue::Bool(true) {
            let note = applied_note(entry)?;
            match out.last_mut() {
                Some(last) if last_assistant => append(last, &note)?,
                Some(_) => out.push(message("assistant", note)),
                None => leading.push(note),
            }
            continue;
        }
        if tool_role(entry) && get(entry, "declined") == &JValue::Bool(true) {
            let note = declined_note(entry)?;
            match out.last_mut() {
                Some(last) if last_assistant => append(last, &note)?,
                Some(_) => out.push(message("assistant", note)),
                None => {}
            }
            continue;
        }
        if tool_role(entry) {
            let name = get(entry, "name");
            if last_assistant && last_at.is_some_and(|i| carriers.contains(&i)) {
                if (o.carry_orphan_tool)(name) {
                    let folded = fold_tool_message(entry)?;
                    if let Some(last) = out.last_mut() {
                        append(last, &folded)?;
                    }
                }
                continue;
            }
            if last_assistant {
                if !o.off_box || (o.carry_orphan_tool)(name) {
                    let folded = fold_tool_message(entry)?;
                    if let Some(last) = out.last_mut() {
                        append(last, &folded)?;
                    }
                }
            } else if last_at.is_some() && (o.carry_orphan_tool)(name) {
                let folded = fold_tool_message(entry)?;
                carriers.insert(out.len());
                out.push(message(
                    "assistant",
                    format!("{FAILED_REPLY_TOOLS_NOTE}\n\n{folded}"),
                ));
            }
            continue;
        }
        let role = get(entry, "role");
        if last_assistant && is_str(role, "assistant") {
            let content = string(get(entry, "content"))?;
            if let Some(last) = out.last_mut() {
                append(last, &content)?;
            }
        } else if is_str(role, "user") && last_role.as_ref().is_some_and(|r| is_str(r, "user")) {
            out.push(message("assistant", ABANDONED_TURN_NOTE.into()));
            out.push(spread(entry));
        } else {
            out.push(spread(entry));
        }
    }
    let drop = out
        .iter()
        .take_while(|m| is_str(get(m, "role"), "assistant"))
        .count();
    out.drain(..drop);
    if let Some(m) = as_str(new_message) {
        if out.last().is_some_and(|l| is_str(get(l, "role"), "user")) {
            out.push(message("assistant", ABANDONED_TURN_NOTE.into()));
        }
        out.push(message("user", m.to_owned()));
    }
    if !leading.is_empty() && out.first().is_some_and(|f| is_str(get(f, "role"), "user")) {
        if let Some(first) = out.first_mut() {
            let was = string(get(first, "content"))?;
            let mut copy = spread(first);
            set(&mut copy, "content", JValue::Str(format!("{}\n\n{was}", leading.join("\n\n"))));
            *first = copy;
        }
    }
    Ok(out)
}
