//! noevia-core `project-edit-target.cjs` `resolveEditTarget` and `targetDigest` over a project as
//! JSON (track R14 step 3): the stored path a project edit tool call would change, with the
//! storage account a move into the project folder is bound to, or the model-readable reason it
//! may not; and the digest a capability token carries for an approved edit. What the chat turn's
//! edit-target bridge (core `rust-chat-native.cjs` `/internal/chat/tools/edit-target`) answers.
//!
//! The rules are the project-edit-target crate's `plan` (planEdit's, in the JS's order) and the name lookup
//! is the project-file-names crate's `call`, fed exactly what core's wasm host feeds it; this module
//! adds the JS's argument handling, the #2245 waiting/unreadable split of a document refusal and
//! every message, word for word.
//!
//! Strings are `js_json::wtf16` strings (a lone surrogate held as its mapped character), so a
//! name with a lone surrogate is quoted as `JSON.stringify` quotes it. [`Lookup::Unsupported`]
//! stands for what core would answer from a shape it never stores (a truthy non-string
//! `projectFolder`, `reservedFolder` or `source`, an argument object with its own `toString`):
//! the caller then asks core.
#![forbid(unsafe_code)]

use js_json::{wtf16, JValue};
use project_edit_target::{plan, Attachment, File, Refused, Request, Work};
use sha2::{Digest, Sha256};

/// What the lookup answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// `{ path, account }`: the stored path, and the storage account a move is bound to.
    Target {
        path: String,
        account: Option<String>,
    },
    /// `{ error }`, the model-readable reason.
    Refused(String),
    /// A shape core never stores: ask core.
    Unsupported,
}

/// A wtf16 string as UTF-16 code units.
fn units(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        match wtf16::lone_unit(c) {
            Some(u) => out.push(u),
            None => out.extend_from_slice(c.encode_utf16(&mut [0; 2])),
        }
    }
    out
}

/// UTF-16 code units as a wtf16 string.
fn from_units(u: &[u16]) -> String {
    char::decode_utf16(u.iter().copied())
        .map(|r| {
            r.unwrap_or_else(|e| wtf16::lone_char(e.unpaired_surrogate()).unwrap_or('\u{fffd}'))
        })
        .collect()
}

/// The bytes `hash.update(text, 'utf8')` hashes: a lone surrogate as U+FFFD.
fn utf8(s: &str) -> String {
    s.chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

fn stringify(s: &str) -> String {
    wtf16::stringify(&JValue::Str(s.to_owned())).unwrap_or_default()
}

/// quoted-name.cjs `quoteName`: `JSON.stringify(name)` with the line and direction controls escaped.
pub fn quote_name(name: &str) -> String {
    let mut out = String::new();
    for c in stringify(name).chars() {
        let raw = matches!(c as u32, 0x85 | 0x200E | 0x200F | 0x2028 | 0x2029 | 0x202A..=0x202E | 0x2066..=0x2069);
        if raw {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// `targetDigest(storedPath, account)`.
pub fn target_digest(path: &str, account: Option<&str>) -> String {
    let mut text = format!("noevia-edit-target\0{path}");
    if let Some(account) = account.filter(|a| !a.is_empty()) {
        text.push('\0');
        text.push_str(account);
    }
    format!("{:x}", Sha256::digest(utf8(&text).as_bytes()))
}

/// A field read as a string when truthy: `Ok(None)` falsy, `Err` a truthy non-string.
fn truthy_str(v: &JValue) -> Result<Option<&str>, ()> {
    match v {
        JValue::Str(s) if s.is_empty() => Ok(None),
        JValue::Str(s) => Ok(Some(s)),
        v if !v.truthy() => Ok(None),
        _ => Err(()),
    }
}

/// `JSON.stringify(typeof raw === 'string' ? raw.slice(0, 200) : String(raw ?? ''))`.
fn shown(raw: &JValue) -> Option<String> {
    Some(stringify(&match raw {
        JValue::Str(s) => wtf16::slice(s, 200),
        v if v.is_nullish() => String::new(),
        v => js_json::to_js_string(v).ok()?,
    }))
}

/// `String(file.content || '').trim()` is empty.
fn blank(content: &str) -> bool {
    prompt_framing::js::trim(&units(content)).is_empty()
}

/// project-file-names.cjs `resolveProjectFile(project, raw)`: the index in `project.files` of the
/// one file, or the error.
fn resolve_file(files: &[JValue], raw: &JValue) -> Result<Result<usize, String>, ()> {
    // `files.filter((f) => f && typeof f.name === 'string')`, keeping each one's index.
    let named: Vec<(usize, &str)> = files
        .iter()
        .enumerate()
        .filter_map(|(i, f)| f.get("name").as_str().map(|n| (i, n)))
        .collect();
    let shown = shown(raw).ok_or(())?;
    let unverified = || {
        Ok(Err(format!(
            "{shown} could not be confirmed as exactly one project file, so nothing was read or changed. Use a name exactly as listed."
        )))
    };
    // The wasm host's call: op 1 and `JSON.stringify([names, raw === undefined ? null : raw])`.
    let names = JValue::Arr(
        named
            .iter()
            .map(|(_, n)| JValue::Str((*n).to_owned()))
            .collect(),
    );
    let raw = if matches!(raw, JValue::Undefined) {
        JValue::Null
    } else {
        raw.clone()
    };
    let Some(body) = wtf16::stringify(&JValue::Arr(vec![names, raw])) else {
        return Err(());
    };
    let mut input = vec![1u8];
    input.extend_from_slice(body.as_bytes());
    let (status, reply) = project_file_names::call(&input);
    if status != 0 {
        return unverified();
    }
    let Ok(reply) = wtf16::parse(&String::from_utf8_lossy(&reply)) else {
        return unverified();
    };
    if let Some(i) = reply.get("file").as_num() {
        return match named.get(i as usize) {
            Some((at, _)) => Ok(Ok(*at)),
            None => unverified(),
        };
    }
    match reply.get("code").as_str() {
        Some("invalid") => Ok(Err(format!(
            "{shown} is not a usable project file name: {}. Use a name exactly as listed.",
            reply.get("reason").as_str().unwrap_or_default()
        ))),
        Some("ambiguous") => {
            let JValue::Arr(candidates) = reply.get("candidates") else {
                return unverified();
            };
            let mut list = Vec::new();
            for c in candidates {
                let Some((_, name)) = c.as_num().and_then(|i| named.get(i as usize)) else {
                    return unverified();
                };
                list.push(*name);
            }
            Ok(Err(format!(
                "{shown} matches more than one project file: {}. Pass the full name exactly as listed.",
                list.join(", ")
            )))
        }
        Some("missing") => {
            let names: Vec<&str> = named.iter().map(|(_, n)| *n).collect();
            let names = if names.is_empty() {
                "(none attached)".to_owned()
            } else {
                names.join(", ")
            };
            Ok(Err(format!(
                "no project file named {shown}. Available: {names}"
            )))
        }
        _ => unverified(),
    }
}

/// `resolveEditTarget(project, rawArgs, { storageAccount })`. `project` is getProject's answer
/// (`None`: the chat is not in a project), `account` the connected storage account (or `None`),
/// and `content` reads a source's text as `file.content` does (workspace_view's `text`).
pub fn resolve_edit_target(
    project: Option<&JValue>,
    raw_args: &str,
    account: Option<&str>,
    content: &dyn Fn(&JValue) -> String,
) -> Lookup {
    let Some(project) = project else {
        return Lookup::Refused(
            "this chat is not in a project, so there are no project files to edit.".into(),
        );
    };
    let args = match wtf16::parse(if raw_args.is_empty() { "{}" } else { raw_args }) {
        Ok(v) => v,
        Err(wtf16::Refusal::Invalid) => {
            return Lookup::Refused("the tool arguments were not valid JSON.".into())
        }
        Err(wtf16::Refusal::Unsupported) => return Lookup::Unsupported,
    };
    if !matches!(args, JValue::Obj(_)) {
        return Lookup::Refused("the tool arguments must be a JSON object.".into());
    }
    match plan_edit(project, args.get("name"), account, content) {
        Ok(v) => v,
        Err(()) => Lookup::Unsupported,
    }
}

/// `planEdit(project, raw, { storageAccount })` as resolveEditTarget answers it.
fn plan_edit(
    project: &JValue,
    raw: &JValue,
    account: Option<&str>,
    content: &dyn Fn(&JValue) -> String,
) -> Result<Lookup, ()> {
    let files: &[JValue] = match project.get("files") {
        JValue::Arr(items) => items,
        _ => &[],
    };
    let at = match resolve_file(files, raw)? {
        Ok(at) => at,
        Err(message) => return Ok(Lookup::Refused(message)),
    };
    let Some(file) = files.get(at) else {
        return Err(());
    };
    let name = file.get("name").as_str().ok_or(())?;
    let account = account.filter(|a| !a.is_empty());
    let attachment = match file.get("attachment") {
        v if !v.truthy() => None,
        a => Some(Attachment {
            state: a.get("state").as_str().map(units),
            group: a.get("group").as_str().map(units),
        }),
    };
    let request = Request {
        folder: truthy_str(project.get("projectFolder"))?.map(units),
        reserved: truthy_str(project.get("reservedFolder"))?.map(units),
        connected: account.is_some(),
        index: at,
        names: files
            .iter()
            .map(|f| f.get("name").as_str().map(units))
            .collect(),
        file: File {
            source: truthy_str(file.get("source"))?.map(units),
            attachment,
            document: file.get("document").truthy(),
        },
    };
    let answer = plan(&request, &mut Work::new(u64::MAX)).map_err(|_| ())?;
    let q = format!("\"{name}\"");
    let refused = |m: String| Ok(Lookup::Refused(m));
    let folder = request
        .folder
        .as_deref()
        .or(request.reserved.as_deref())
        .map(from_units)
        .unwrap_or_default();
    match answer {
        Ok(p) => Ok(Lookup::Target {
            path: from_units(&p.target),
            account: if p.adopt { account.map(str::to_owned) } else { None },
        }),
        Err(Refused::UploadPath) => refused(format!("{q} is not stored where this project's uploads are kept, so it cannot be edited in place. Nothing was changed.")),
        Err(Refused::Synced) => {
            let source = file.get("source").as_str().unwrap_or_default();
            refused(format!("{q} comes from the attached folder \"{source}\" and is kept in sync from there. Edit it in that folder instead."))
        }
        Err(Refused::Path) => refused(format!("{q} is stored under a folder path; editing it with tools isn't supported yet. Use project_create_file to write a new file.")),
        Err(Refused::NoFolder) => refused(format!("{q} is kept only in noevia and this project has no storage folder yet, so it cannot be edited in place. Nothing was changed. Add it again from the project's Sources to store it in the project folder.")),
        Err(Refused::NotText) => refused(format!("{q} is not a text file type that can be stored in the project folder, so it cannot be edited in place. Nothing was changed.")),
        Err(Refused::Taken) => {
            let target = format!("{folder}/Text/{name}");
            refused(format!("\"{target}\" is already a separate file in this project, so {q} cannot be moved there. Nothing was changed. Edit \"{target}\" instead."))
        }
        Err(Refused::Original) => refused(format!("{q} is stored in its original format and has no editable text.")),
        Err(Refused::Document) => {
            // source-readability.cjs: isWaiting, then isUnreadable (a stored original was refused
            // above), then the document refusal.
            let state = file.get("document").get("state").as_str();
            let empty = blank(&content(file));
            if state == Some("queued") && empty {
                refused(format!("{} is waiting to be read (document reading is not available yet), so it cannot be edited yet. Nothing was changed.", quote_name(name)))
            } else if matches!(state, Some("failed" | "queued")) && empty {
                refused(format!("{} could not be read when it was added, so it cannot be edited. Nothing was changed.", quote_name(name)))
            } else {
                refused(format!("{q} is an extracted document, not an editable text file."))
            }
        }
        Err(Refused::Partial) => refused(format!("{q} was only read in part or in another encoding, so writing it back would lose content. Nothing was changed.")),
    }
}
