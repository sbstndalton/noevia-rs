//! The file part of the route, before the child is asked anything: the system routing role check
//! (the services ini module's role check), the section's model file (`helpers._resolve_section_gguf`), its header
//! summary (`gguf_meta.summarize_path`) and its size (the folder's model parts when it sits in one).
use super::render;
use crate::draft_heads::{normal, stat, stat_file};
use crate::model_detail::{hit_read_bound, is_file, stems_present, Watched};
use crate::model_updates::{companion, exists, inside, listing, read_ini, Stop};
use crate::sections::role::{is_system_routing_role, LEGACY_SYSTEM_ROUTING_ROLE};
use crate::sections::{access::Root, ini, Budget, Error as WalkError};
use std::path::{Path, PathBuf};

/// The legacy role key: the deprecated name/path rule matches it as a name prefix, and the route's
/// own answer (its wording and the `laya_section` code) is spelled with it whichever key the roles
/// file uses. The roles-file check accepts the role name and this key (`sections::role`).
const LEGACY_ROLE: &str = LEGACY_SYSTEM_ROUTING_ROLE;
/// Largest roles file read (Python reads any size; a larger one refuses).
const ROLES_CAP: u64 = 1024 * 1024;

/// What the file part found.
pub(super) enum Part {
    /// The route's answer already (200): the routing role, no model file, an unreadable header.
    Answer(Vec<u8>),
    Located(Box<Located>),
}

/// The section's model file, read.
pub(super) struct Located {
    /// The canonical models folder the file was found in.
    pub base: PathBuf,
    /// `model_rel`: the container path the backend server is given, or "" for a flat layout.
    pub model_rel: String,
    /// `rel`: `ini.section_file_rel(name)`.
    pub rel: Option<String>,
    pub file_size: u128,
    pub summary: gguf::Json,
}

fn error_body(message: &str, code: Option<&str>) -> Vec<u8> {
    let mut out = String::from("{\"error\":");
    render::string(&mut out, message);
    if let Some(code) = code {
        out.push_str(",\"code\":");
        render::string(&mut out, code);
    }
    out.push('}');
    out.into_bytes()
}

/// The answer for a section with the system routing role (the wording is the route's own).
fn routing_answer(name: &str) -> Vec<u8> {
    let mut role = LEGACY_ROLE.to_owned();
    if let Some(first) = role.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    error_body(
        &format!(
            "'{name}' is the {role} routing model. {role} runs as its own service, so llama.cpp \
             auto-configuration does not apply to it."
        ),
        Some(&format!("{LEGACY_ROLE}_section")),
    )
}

/// `re.match(r"^<role>(?:[_.-]|$)", part, re.I)`; `$` also matches before one final newline.
fn routing_name(part: &str) -> bool {
    let role = LEGACY_ROLE.as_bytes();
    let bytes = part.as_bytes();
    if !bytes
        .get(..role.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(role))
    {
        return false;
    }
    matches!(
        bytes.get(role.len()..),
        Some([] | [b'\n'] | [b'_' | b'.' | b'-', ..])
    )
}

/// A small file read without blocking on a FIFO: `None` when missing; the error kind otherwise.
fn read_small(path: &Path, cap: u64) -> Result<Option<Vec<u8>>, Stop> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // Any other OSError reading the file: `read_roles` logs it and uses no roles.
        Err(_) => return Ok(Some(Vec::new())),
    };
    let meta = file.metadata().map_err(|_| Stop::Refused)?;
    if meta.is_dir() {
        return Ok(Some(Vec::new()));
    }
    if !meta.is_file() {
        return Err(Stop::Refused);
    }
    let mut bytes = Vec::new();
    file.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Stop::Refused)?;
    if bytes.len() as u64 > cap {
        return Err(Stop::Refused);
    }
    Ok(Some(bytes))
}

/// `ini.read_roles()`: section id -> lower-cased role (a missing or unusable file is no roles;
/// text that is not UTF-8 raises, as `read_text` does).
fn read_roles(path: &Path) -> Result<Vec<(String, String)>, Stop> {
    use model_files::json::Value;
    let Some(bytes) = read_small(path, ROLES_CAP)? else {
        return Ok(Vec::new());
    };
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8(bytes).map_err(|_| Stop::Failed)?;
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let Ok(Value::Obj(pairs)) = model_files::json::parse(&text) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in pairs {
        // dict(): a repeated key keeps its first place and its last value.
        let value = match &v {
            Value::Str(s) if !ini::strip(s).is_empty() => Some(ini::strip(s).to_lowercase()),
            _ => None,
        };
        let pos = out.iter().position(|(key, _)| *key == k);
        match (pos, value) {
            (Some(i), Some(val)) => {
                if let Some(slot) = out.get_mut(i) {
                    slot.1 = val;
                }
            }
            (Some(i), None) => {
                out.remove(i);
            }
            (None, Some(val)) => out.push((k, val)),
            (None, None) => {}
        }
    }
    Ok(out)
}

/// The services ini module's role check: the system routing role from `models.roles.json` when the
/// section has a role (the role name or the legacy key, until phase 3 of noevia#2093), else the
/// deprecated name/path rule.
fn routing_section(parsed: &ini::Ini, ini_path: &Path, name: &str) -> Result<bool, Stop> {
    if name == "*" || !parsed.names().any(|n| n == name) {
        return Ok(false);
    }
    let roles = read_roles(&ini_path.with_file_name("models.roles.json"))?;
    if let Some((_, role)) = roles.iter().find(|(k, _)| k == name) {
        return Ok(is_system_routing_role(role));
    }
    let items = parsed
        .items(name, &mut Budget::default())
        .map_err(|_| Stop::Refused)?
        .unwrap_or_default();
    let value = |key: &str| {
        items
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    let model = value("model").or_else(|| value("m")).unwrap_or_default();
    let matched = std::iter::once(name)
        .chain(model.split(['\\', '/']))
        .filter(|part| !part.is_empty())
        .any(routing_name);
    if matched {
        warn_once(name);
    }
    Ok(matched)
}

/// Sections already warned about (bounded: past the cap nothing more is logged).
static WARNED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
const WARNED_CAP: usize = 64;

/// Logs the deprecated name/path rule once per section.
fn warn_once(name: &str) {
    let Ok(mut warned) = WARNED.lock() else {
        return;
    };
    if warned.len() >= WARNED_CAP || warned.iter().any(|n| n == name) {
        return;
    }
    warned.push(name.to_owned());
    eprintln!(
        "models.ini [{name:?}] is treated as a system routing section by the deprecated name/path \
         rule; mark it in models.roles.json instead"
    );
}

/// `PurePosixPath(name).suffix` (Python 3.12).
fn suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if 0 < i && i + 1 < name.len() => name.get(i..).unwrap_or_default(),
        _ => "",
    }
}

/// Opens `rel` inside the models folder, following an absolute link only when its target stays
/// inside; `None` when it does not exist.
pub(super) fn open_file(
    root: &Root,
    base: &Path,
    rel: &str,
) -> Result<Option<std::fs::File>, Stop> {
    let rel = normal(rel)?;
    let mut budget = Budget::default();
    match root.file(&rel, false, &mut budget) {
        Ok(found) => Ok(found),
        Err(_) => {
            if inside(base, &rel)?.is_none() {
                return Ok(None);
            }
            let target = std::fs::canonicalize(base.join(&rel)).map_err(|_| Stop::Refused)?;
            if !target.starts_with(base) {
                return Err(Stop::Refused);
            }
            match std::fs::File::open(&target) {
                Ok(file) if file.metadata().is_ok_and(|m| m.is_file()) => Ok(Some(file)),
                _ => Err(Stop::Refused),
            }
        }
    }
}

fn finite(value: &gguf::Value) -> bool {
    match value {
        gguf::Value::Float(f) => f.is_finite(),
        gguf::Value::List(values) => values.iter().all(finite),
        gguf::Value::ArraySummary { sample, .. } => sample.iter().all(finite),
        _ => true,
    }
}

/// The file part for section `name`.
pub(super) fn locate(
    models: &Path,
    ini_path: &Path,
    name: &str,
    open: fn(&Path) -> Result<Root, WalkError>,
) -> Result<Part, Stop> {
    locate_mode(models, ini_path, name, open, true)
}

/// Chart estimates resolve the same file, without the route's role refusal or folder sum.
#[cfg(feature = "native-backends")]
pub(super) fn locate_prediction(
    models: &Path,
    ini_path: &Path,
    name: &str,
    open: fn(&Path) -> Result<Root, WalkError>,
) -> Result<Part, Stop> {
    locate_mode(models, ini_path, name, open, false)
}

fn locate_mode(
    models: &Path,
    ini_path: &Path,
    name: &str,
    open: fn(&Path) -> Result<Root, WalkError>,
    route: bool,
) -> Result<Part, Stop> {
    let parsed = read_ini(ini_path)?;
    if let Some((_, parsed)) = &parsed {
        if route && routing_section(parsed, ini_path, name)? {
            return Ok(Part::Answer(routing_answer(name)));
        }
    }
    let no_file = || {
        Part::Answer(error_body(
            &format!("No model file found for '{name}'."),
            None,
        ))
    };
    let explicit = match &parsed {
        Some((_, parsed)) => parsed
            .items(name, &mut Budget::default())
            .map_err(|_| Stop::Refused)?
            .unwrap_or_default()
            .iter()
            .rev()
            .find(|(k, _)| k == "model")
            .map(|(_, v)| ini::strip(v).to_owned())
            .unwrap_or_default(),
        None => String::new(),
    };
    // A missing models folder (or a file in its place) holds no model file for any section.
    let Some(true) = exists(models)? else {
        return Ok(no_file());
    };
    let base = std::fs::canonicalize(models).map_err(|_| Stop::Refused)?;
    let root = open(&base).map_err(|_| Stop::Refused)?;
    let checked = |rel: &str| -> Result<bool, Stop> {
        if rel.contains('\0') {
            return Ok(false); // pathlib's is_file() is False for an embedded NUL
        }
        is_file(&root, &base, rel).ok_or(Stop::Refused)
    };
    // `ini.section_file_rel(name)`.
    let mut rel: Option<String> = None;
    if !explicit.is_empty() {
        let candidate = explicit.replacen("/models/", "", 1);
        let candidate = candidate.trim_start_matches('/');
        if checked(candidate)? {
            rel = Some(candidate.to_owned());
        }
    }
    if rel.is_none() {
        let found = listing(&root, &base)?;
        rel = stems_present(&found)?.get(name).cloned();
    }
    // `helpers._resolve_section_gguf(name)`.
    let mut model_rel = String::new();
    let gguf_rel = match &rel {
        Some(r) if !explicit.is_empty() && !r.is_empty() && checked(r)? => {
            if r.contains('/') {
                model_rel.clone_from(&explicit);
            }
            r.clone()
        }
        Some(r) if r.contains('/') => {
            let folder = r.rsplit_once('/').map_or("", |(d, _)| d);
            if !crate::draft_heads::is_dir(&root, &base, folder)? {
                return Ok(no_file());
            }
            let entries = root
                .entries_unless_denied(&normal(folder)?, &mut Budget::default())
                .map_err(|_| Stop::Refused)?
                .ok_or(Stop::Failed)?; // iterdir() raises PermissionError: unhandled
            let mut ggufs: Vec<String> = Vec::new();
            for entry in entries {
                let path = format!("{}/{}", normal(folder)?, entry.name);
                if suffix(&entry.name).to_lowercase() == ".gguf" && stat_file(&root, &base, &path)?
                {
                    ggufs.push(entry.name);
                }
            }
            ggufs.sort();
            let main = ggufs
                .iter()
                .find(|n| !n.to_lowercase().contains("mmproj"))
                .or(ggufs.first());
            let Some(file) = main else {
                return Ok(no_file());
            };
            model_rel = format!("/models/{folder}/{file}");
            format!("{}/{file}", normal(folder)?)
        }
        Some(r) => r.clone(),
        None => format!("{name}.gguf"),
    };
    if !checked(&gguf_rel)? {
        return Ok(no_file());
    }
    let Some(file) = open_file(&root, &base, &gguf_rel)? else {
        return Err(Stop::Refused); // vanished: Python's FileNotFoundError text is not reproduced
    };
    let mut reader = Watched {
        inner: file,
        failed: false,
    };
    let parsed_header = gguf::read_raw_stream(&mut reader);
    if reader.failed {
        return Err(Stop::Refused); // an OSError's text is not reproduced
    }
    let raw = match parsed_header {
        Ok(raw) => raw,
        Err(e) => {
            return Ok(Part::Answer(error_body(
                &format!("Could not read the model file: {}", e.0),
                None,
            )))
        }
    };
    if hit_read_bound(&raw) || !raw.values().all(finite) {
        return Err(Stop::Refused);
    }
    // Which Python exception a summarize() failure would be is unknown here.
    let summary = gguf::summarize(&raw).map_err(|_| Stop::Refused)?;
    drop(raw);
    let mut file_size = u128::from(stat(&root, &base, &gguf_rel)?.ok_or(Stop::Refused)?.len());
    if route && rel.as_deref().is_some_and(|r| r.contains('/')) {
        if let Some(sum) = folder_parts(&root, &base, &gguf_rel)? {
            file_size = sum;
        }
    }
    Ok(Part::Located(Box::new(Located {
        base,
        model_rel,
        rel,
        file_size,
        summary,
    })))
}

/// The sizes of the model parts beside `gguf_rel` (every `.gguf` that is not a companion), or
/// `None` where Python's `OSError` keeps the single file's size.
fn folder_parts(root: &Root, base: &Path, gguf_rel: &str) -> Result<Option<u128>, Stop> {
    let rel = normal(gguf_rel)?;
    let folder = rel.rsplit_once('/').map_or(".", |(d, _)| d);
    let Some(entries) = root
        .entries_unless_denied(folder, &mut Budget::default())
        .map_err(|_| Stop::Refused)?
    else {
        return Ok(None);
    };
    let mut sum: u128 = 0;
    for entry in entries {
        let path = if folder == "." {
            entry.name.clone()
        } else {
            format!("{folder}/{}", entry.name)
        };
        let Some(meta) = stat(root, base, &path)? else {
            continue;
        };
        if meta.is_file()
            && suffix(&entry.name).to_lowercase() == ".gguf"
            && !companion(&entry.name)
        {
            sum = sum.saturating_add(u128::from(meta.len()));
        }
    }
    Ok(Some(sum))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;

    /// `routing_section` for `name` in a models folder built from `ini_text` and `roles`.
    fn routing(ini_text: &str, roles: Option<&str>, name: &str) -> bool {
        let dir = tempfile::tempdir().unwrap();
        let ini_path = dir.path().join("models.ini");
        std::fs::write(&ini_path, ini_text).unwrap();
        if let Some(roles) = roles {
            std::fs::write(dir.path().join("models.roles.json"), roles).unwrap();
        }
        let parsed = ini::Ini::parse(ini_text, &mut Budget::default()).unwrap();
        matches!(routing_section(&parsed, &ini_path, name), Ok(true))
    }

    const PLAIN: &str = "[a]\nctx-size = 1\n";

    #[test]
    fn the_role_name_is_the_system_routing_role() {
        assert!(routing(PLAIN, Some(r#"{"a": "system-routing"}"#), "a"));
        // The reader strips and lower-cases the value, as for the legacy key.
        assert!(routing(PLAIN, Some(r#"{"a": " System-Routing "}"#), "a"));
    }

    #[test]
    fn the_legacy_key_is_still_the_system_routing_role() {
        let legacy = format!(r#"{{"a": "{LEGACY_SYSTEM_ROUTING_ROLE}"}}"#);
        assert!(routing(PLAIN, Some(&legacy), "a"));
    }

    #[test]
    fn an_unrelated_role_is_not_the_system_routing_role() {
        for role in ["chat", "system-routing-x", "routing", "system_routing"] {
            let roles = format!(r#"{{"a": "{role}"}}"#);
            assert!(!routing(PLAIN, Some(&roles), "a"), "{role}");
        }
        // Another section's role does not carry over, and no roles file is no role.
        assert!(!routing(
            "[a]\n\n[b]\n",
            Some(r#"{"b": "system-routing"}"#),
            "a"
        ));
        assert!(!routing(PLAIN, None, "a"));
    }

    #[test]
    fn a_role_decides_over_the_name_rule_and_the_rule_still_covers_a_section_without_one() {
        let named = format!("[{LEGACY_ROLE}-x]\nctx-size = 1\n");
        let name = format!("{LEGACY_ROLE}-x");
        assert!(routing(&named, None, &name));
        let chat = format!(r#"{{"{name}": "chat"}}"#);
        assert!(!routing(&named, Some(&chat), &name));
        let routed = format!(r#"{{"{name}": "system-routing"}}"#);
        assert!(routing(&named, Some(&routed), &name));
    }

    #[test]
    fn the_answer_keeps_the_legacy_error_code() {
        // The error code is the route's own contract, whichever key the roles file uses.
        let body = String::from_utf8(routing_answer("a")).unwrap();
        assert!(body.contains(&format!(r#""code":"{LEGACY_ROLE}_section""#)));
    }
}
