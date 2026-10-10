//! noevia-core's `server/project-edit-target.cjs` `planEdit` in Rust, exported from
//! `dav-parse.wasm` (PROJECT_EDIT_TARGET_IMPL): which stored file a project edit tool
//! (`project_append_file`, `project_replace_text`) may change, the plain name the write path is
//! handed, and the stored path the edit writes (shown on the approval card and pinned for the
//! call, noevia#648, #687). The host only ever uses this port to refuse more: it computes the JS
//! plan first and an edit goes ahead only when the port gives the identical plan.
//!
//! The host has already resolved the model's name to one file of the requesting project
//! (project-file-names.cjs, itself confirmed under PROJECT_FILE_NAMES_IMPL). It sends that file's
//! index, the stored names of all the project's files (a non-string name or a missing entry as
//! `null`), the project's `projectFolder` and `reservedFolder` (strings or `null`; the empty string
//! counts as none, as in the JS), whether a storage account is connected, and the file's projection:
//! its `source` (string or `null`), its `attachment` (`null`, or its `state` and `group`, each a
//! string or `null`) and whether it has a `document`.
//!
//! [`plan`] transcribes the JS rule by rule, in the JS's order:
//!
//! 1. a connected upload (`projectFolder`, an attachment and `source === projectFolder`) must be
//!    stored at exactly `<projectFolder>/<classify(base)>/<base>`; it writes `base`;
//! 2. otherwise a file synced from an attached folder, or one with a `/` in its name, is refused;
//!    with storage connected a plain-named file is moved to `<folder>/Text/<name>` (`adopt`;
//!    `folder` is `projectFolder || reservedFolder`), refused when there is no folder, the name is
//!    not a Text type, or another file already has that stored name;
//! 3. an original kept in its stored format, an unreadable source, an extracted document, and text
//!    read only in part or in another encoding are refused.
//!
//! `classify` is upload-sniff's (the code noevia-core's uploads.cjs already runs). A name with a
//! lone surrogate is classified as its U+FFFD form, as the host's `uploadClassify` sends it. The
//! port is stricter than the JS only for requests over [`MAX_INPUT_BYTES`] or past the work budget
//! (`too_large`), which the host refuses. Linear in the request, no panics.

#![forbid(unsafe_code)]

use prompt_framing::js::lossy;
use prompt_framing::json::{self, Value};

/// The largest request [`call`] accepts (the op byte and the JSON).
pub const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024 + 1;
/// Work units a request may use per byte of its size (plus [`WORK_FLOOR`]).
pub const WORK_PER_BYTE: u64 = 8;
/// Work units every request may use, whatever its size.
pub const WORK_FLOOR: u64 = 1 << 16;

const JSON_CAP: usize = 4;

/// Why the port gives no answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Input,
    TooLarge,
}

impl Refusal {
    /// The refusal reply.
    pub fn json(self) -> &'static str {
        match self {
            Refusal::Input => r#"{"error":"input"}"#,
            Refusal::TooLarge => r#"{"error":"too_large"}"#,
        }
    }
}

type R<T> = Result<T, Refusal>;

/// Work left for one request.
#[derive(Debug)]
pub struct Work {
    left: u64,
}

impl Work {
    /// A budget of `units`.
    pub fn new(units: u64) -> Self {
        Work { left: units }
    }

    /// The budget for a request of `bytes` bytes.
    pub fn for_request(bytes: usize) -> Self {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        Work::new(
            bytes
                .saturating_mul(WORK_PER_BYTE)
                .saturating_add(WORK_FLOOR),
        )
    }

    /// Charge `n` units (and one for the step itself).
    pub fn charge(&mut self, n: usize) -> R<()> {
        let n = u64::try_from(n).unwrap_or(u64::MAX).saturating_add(1);
        if n > self.left {
            self.left = 0;
            return Err(Refusal::TooLarge);
        }
        self.left -= n;
        Ok(())
    }
}

/// A file's `attachment`, as far as the edit rules read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// `attachment.state` when it is a string.
    pub state: Option<Vec<u16>>,
    /// `attachment.group` when it is a string.
    pub group: Option<Vec<u16>>,
}

/// The resolved file's projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    /// `file.source` when it is a non-empty string (`None`: falsy).
    pub source: Option<Vec<u16>>,
    /// `file.attachment` when truthy.
    pub attachment: Option<Attachment>,
    /// `!!file.document`.
    pub document: bool,
}

/// One request.
#[derive(Clone, Debug)]
pub struct Request {
    /// `project.projectFolder` when it is a non-empty string.
    pub folder: Option<Vec<u16>>,
    /// `project.reservedFolder` when it is a non-empty string.
    pub reserved: Option<Vec<u16>>,
    /// A storage account is connected (`typeof account === 'string' && !!account`).
    pub connected: bool,
    /// The resolved file's index in `names`.
    pub index: usize,
    /// Every entry of `project.files`: its name when it is a string.
    pub names: Vec<Option<Vec<u16>>>,
    /// The resolved file.
    pub file: File,
}

/// Why the JS refuses the edit (the first rule that fails, in the JS's order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// A connected upload not stored at its upload path.
    UploadPath,
    /// A file synced from an attached folder.
    Synced,
    /// A plain (not connected-upload) file whose name has a `/`.
    Path,
    /// Storage connected, but the project has no storage folder.
    NoFolder,
    /// Storage connected, but the name is not a Text type.
    NotText,
    /// Storage connected, and another file already has the adopt path.
    Taken,
    /// An original kept in its stored format.
    Original,
    /// An unreadable source or an extracted document.
    Document,
    /// Text read only in part or in another encoding.
    Partial,
}

impl Refused {
    /// The code in the reply.
    pub fn code(self) -> &'static str {
        match self {
            Refused::UploadPath => "upload_path",
            Refused::Synced => "synced",
            Refused::Path => "path",
            Refused::NoFolder => "no_folder",
            Refused::NotText => "not_text",
            Refused::Taken => "taken",
            Refused::Original => "original",
            Refused::Document => "document",
            Refused::Partial => "partial",
        }
    }
}

/// The edit plan: the plain name the write path is handed, the stored path the edit writes, and
/// whether the edit moves a plain-named file into the project folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub write: Vec<u16>,
    pub target: Vec<u16>,
    pub adopt: bool,
}

const SLASH: u16 = b'/' as u16;

fn eq_ascii(s: &[u16], a: &str) -> bool {
    s.len() == a.len() && s.iter().zip(a.bytes()).all(|(&c, b)| c == u16::from(b))
}

/// upload-sniff `classify(name)`, over code units (a lone surrogate as U+FFFD).
pub fn classify(name: &[u16]) -> &'static str {
    upload_sniff::classify(&lossy(name))
}

fn join(parts: &[&[u16]]) -> Vec<u16> {
    let mut out = Vec::with_capacity(parts.iter().map(|p| p.len()).sum());
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

/// `planEdit` for the resolved file (see the crate docs).
pub fn plan(req: &Request, work: &mut Work) -> R<Result<Plan, Refused>> {
    let Some(Some(name)) = req.names.get(req.index) else {
        return Err(Refusal::Input);
    };
    let name = name.as_slice();
    work.charge(name.len().saturating_mul(4))?;
    let file = &req.file;
    let upload = match (&req.folder, &file.attachment, &file.source) {
        (Some(folder), Some(_), Some(source)) => folder == source,
        _ => false,
    };
    let (mut write, mut target, mut adopt) = (name.to_vec(), name.to_vec(), false);
    if upload {
        let folder = req.folder.as_deref().unwrap_or_default();
        let cut = name
            .iter()
            .rposition(|&c| c == SLASH)
            .map_or(0, |i| i.saturating_add(1));
        let base = name.get(cut..).unwrap_or_default();
        let group: Vec<u16> = classify(base).encode_utf16().collect();
        let expected = join(&[folder, &[SLASH], &group, &[SLASH], base]);
        if name != expected.as_slice() {
            return Ok(Err(Refused::UploadPath));
        }
        write = base.to_vec();
        target = name.to_vec();
    } else {
        if file.source.is_some() {
            return Ok(Err(Refused::Synced));
        }
        if name.contains(&SLASH) {
            return Ok(Err(Refused::Path));
        }
        if req.connected {
            let Some(folder) = req.folder.as_deref().or(req.reserved.as_deref()) else {
                return Ok(Err(Refused::NoFolder));
            };
            if classify(name) != "Text" {
                return Ok(Err(Refused::NotText));
            }
            let moved = join(&[folder, &[SLASH], &[0x54, 0x65, 0x78, 0x74], &[SLASH], name]);
            for (j, other) in req.names.iter().enumerate() {
                let Some(other) = other else { continue };
                work.charge(other.len().min(moved.len()))?;
                if j != req.index && other.as_slice() == moved.as_slice() {
                    return Ok(Err(Refused::Taken));
                }
            }
            target = moved;
            adopt = true;
        }
    }
    let state = file.attachment.as_ref().map(|a| a.state.as_deref());
    if matches!(state, Some(Some(s)) if eq_ascii(s, "stored")) {
        return Ok(Err(Refused::Original));
    }
    // isUnreadable: a stored Text/Documents original (refused above) or a failed document (below).
    if file.document {
        return Ok(Err(Refused::Document));
    }
    if let Some(state) = state {
        let ready = matches!(state, Some(s) if eq_ascii(s, "ready"));
        if !ready || classify(&write) != "Text" {
            return Ok(Err(Refused::Partial));
        }
    }
    Ok(Ok(Plan {
        write,
        target,
        adopt,
    }))
}

/// A string or `null`; the empty string is falsy, so it reads as `None`.
fn truthy_string(v: &Value) -> R<Option<Vec<u16>>> {
    match v {
        Value::Null => Ok(None),
        Value::Str(s) if s.is_empty() => Ok(None),
        Value::Str(s) => Ok(Some(s.clone())),
        _ => Err(Refusal::Input),
    }
}

fn opt_string(v: &Value) -> R<Option<Vec<u16>>> {
    match v {
        Value::Null => Ok(None),
        Value::Str(s) => Ok(Some(s.clone())),
        _ => Err(Refusal::Input),
    }
}

fn boolean(v: &Value) -> R<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        _ => Err(Refusal::Input),
    }
}

fn index(v: &Value, len: usize) -> R<usize> {
    match v {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Value::Num(n) if n.is_finite() && n.fract() == 0.0 && *n >= 0.0 && *n < len as f64 => {
            Ok(*n as usize)
        }
        _ => Err(Refusal::Input),
    }
}

/// The request `[folder, reserved, connected, index, names, [source, attachment, document]]`.
pub fn request(args: &[Value], work: &mut Work) -> R<Request> {
    let [folder, reserved, connected, idx, names, file] = args else {
        return Err(Refusal::Input);
    };
    let Value::Arr(names) = names else {
        return Err(Refusal::Input);
    };
    let mut list = Vec::with_capacity(names.len());
    for n in names {
        let n = opt_string(n)?;
        work.charge(n.as_ref().map_or(0, Vec::len))?;
        list.push(n);
    }
    let Value::Arr(file) = file else {
        return Err(Refusal::Input);
    };
    let [source, attachment, document] = file.as_slice() else {
        return Err(Refusal::Input);
    };
    let attachment = match attachment {
        Value::Null => None,
        Value::Arr(a) => match a.as_slice() {
            [state, group] => Some(Attachment {
                state: opt_string(state)?,
                group: opt_string(group)?,
            }),
            _ => return Err(Refusal::Input),
        },
        _ => return Err(Refusal::Input),
    };
    Ok(Request {
        folder: truthy_string(folder)?,
        reserved: truthy_string(reserved)?,
        connected: boolean(connected)?,
        index: index(idx, list.len())?,
        names: list,
        file: File {
            source: truthy_string(source)?,
            attachment,
            document: boolean(document)?,
        },
    })
}

/// The reply text for [`plan`]'s answer.
pub fn reply(answer: &Result<Plan, Refused>) -> Vec<u8> {
    let mut out = Vec::new();
    match answer {
        Ok(p) => {
            out.extend_from_slice(b"{\"plan\":{\"write\":");
            json::push_str(&mut out, &p.write);
            out.extend_from_slice(b",\"target\":");
            json::push_str(&mut out, &p.target);
            out.extend_from_slice(if p.adopt {
                b",\"adopt\":true}}"
            } else {
                b",\"adopt\":false}}"
            });
        }
        Err(r) => {
            out.extend_from_slice(b"{\"refused\":");
            json::push_ascii(&mut out, r.code());
            out.push(b'}');
        }
    }
    out
}

/// One request: `u8(1)` and the UTF-8 JSON array
/// `[folder|null, reserved|null, connected, index, [name|null...], [source|null,
/// [state|null, group|null]|null, document]]`.
///
/// Replies `{"plan":{"write":…,"target":…,"adopt":bool}}` or `{"refused":"<code>"}` ([`Refused`]);
/// status 1 and `{"error":"input"|"too_large"}` when refused.
pub fn call(input: &[u8]) -> (u32, Vec<u8>) {
    let refuse = |r: Refusal| (1, r.json().as_bytes().to_vec());
    if input.len() > MAX_INPUT_BYTES {
        return refuse(Refusal::TooLarge);
    }
    let Some((&op, body)) = input.split_first() else {
        return refuse(Refusal::Input);
    };
    if op != 1 {
        return refuse(Refusal::Input);
    }
    let mut work = Work::for_request(input.len());
    let Some(Value::Arr(args)) = json::parse_utf8(body, JSON_CAP) else {
        return refuse(Refusal::Input);
    };
    match request(&args, &mut work).and_then(|req| plan(&req, &mut work)) {
        Ok(answer) => (0, reply(&answer)),
        Err(e) => refuse(e),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn ask(json: &str) -> (u32, String) {
        let mut input = vec![1u8];
        input.extend(json.as_bytes());
        let (status, out) = call(&input);
        (status, String::from_utf8(out).unwrap())
    }

    fn ok(json: &str) -> String {
        let (status, out) = ask(json);
        assert_eq!(status, 0, "{out}");
        out
    }

    #[test]
    fn connected_upload_at_its_path() {
        assert_eq!(
            ok(r#"["P",null,true,0,["P/Text/a.md"],["P",["ready","Text"],false]]"#),
            r#"{"plan":{"write":"a.md","target":"P/Text/a.md","adopt":false}}"#
        );
        assert_eq!(
            ok(r#"["P",null,true,0,["P/Other/a.md"],["P",["ready","Text"],false]]"#),
            r#"{"refused":"upload_path"}"#
        );
        assert_eq!(
            ok(r#"["P",null,false,0,["P/Images/a.png"],["P",["ready","Images"],false]]"#),
            r#"{"refused":"partial"}"#
        );
    }

    #[test]
    fn plain_files() {
        assert_eq!(
            ok(r#"[null,null,false,0,["a.md"],[null,null,false]]"#),
            r#"{"plan":{"write":"a.md","target":"a.md","adopt":false}}"#
        );
        assert_eq!(
            ok(r#"[null,"R",true,0,["a.md"],[null,null,false]]"#),
            r#"{"plan":{"write":"a.md","target":"R/Text/a.md","adopt":true}}"#
        );
        assert_eq!(
            ok(r#"["",null,true,0,["a.md"],[null,null,false]]"#),
            r#"{"refused":"no_folder"}"#
        );
        assert_eq!(
            ok(r#"["P",null,true,0,["a.pdf"],[null,null,false]]"#),
            r#"{"refused":"not_text"}"#
        );
        assert_eq!(
            ok(r#"["P",null,true,0,["a.md",null,"P/Text/a.md"],[null,null,false]]"#),
            r#"{"refused":"taken"}"#
        );
        assert_eq!(
            ok(r#"["P",null,true,1,["x","d/a.md"],[null,null,false]]"#),
            r#"{"refused":"path"}"#
        );
        assert_eq!(
            ok(r#"["P",null,true,0,["a.md"],["Q",null,false]]"#),
            r#"{"refused":"synced"}"#
        );
    }

    #[test]
    fn later_rules() {
        assert_eq!(
            ok(r#"[null,null,false,0,["a.md"],[null,["stored","Text"],false]]"#),
            r#"{"refused":"original"}"#
        );
        assert_eq!(
            ok(r#"[null,null,false,0,["a.md"],[null,null,true]]"#),
            r#"{"refused":"document"}"#
        );
        assert_eq!(
            ok(r#"[null,null,false,0,["a.md"],[null,["partial",null],false]]"#),
            r#"{"refused":"partial"}"#
        );
        assert_eq!(
            ok(r#"[null,null,false,0,["a.md"],[null,[null,null],false]]"#),
            r#"{"refused":"partial"}"#
        );
    }

    #[test]
    fn bad_input() {
        for body in [
            "",
            "[]",
            r#"[null,null,false,1,["a.md"],[null,null,false]]"#,
            r#"[null,null,false,0,[null],[null,null,false]]"#,
            r#"[null,null,0,0,["a.md"],[null,null,false]]"#,
            r#"[1,null,false,0,["a.md"],[null,null,false]]"#,
            r#"[null,null,false,0.5,["a.md"],[null,null,false]]"#,
            r#"[null,null,false,0,["a.md"],[null,[1,null],false]]"#,
        ] {
            assert_eq!(ask(body), (1, r#"{"error":"input"}"#.to_string()), "{body}");
        }
        assert_eq!(call(&[2, b'[', b']']).0, 1);
        assert_eq!(call(&vec![1u8; MAX_INPUT_BYTES + 1]).0, 1);
    }

    #[test]
    fn lone_surrogate_round_trips() {
        assert_eq!(
            ok(r#"["P\ud800",null,true,0,["x\udc00.md"],[null,null,false]]"#),
            r#"{"plan":{"write":"x\udc00.md","target":"P\ud800/Text/x\udc00.md","adopt":true}}"#
        );
    }
}
