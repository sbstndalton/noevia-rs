//! Low- and high-context profiles per model (sbstndalton/noevia#1079).
//!
//! Auto-tune can tune a model two ways. **Fast** keeps today's prompt time limit and writes the
//! model's own `models.ini` section. **Long** allows a much longer prompt fill (up to 1800 s), so
//! the context search goes as far as the memory budget allows, and writes a second section,
//! `[<model>-long]`, that loads the same weights with its own context, KV cache and batch values.
//! The llama.cpp router serves each section as its own model id; it keeps one chat model resident,
//! so choosing the other profile reloads the weights.
//!
//! Three pure decisions, one JSON entry point ([`run_json`], dav-parse.wasm's `long_profile`):
//!
//! 1. **`pairs`**: which router rows are a model's long profile. A row `L` is the long profile of
//!    row `B` exactly when `L`'s id is a valid preset name ending in [`SUFFIX`], `B`'s id is `L`'s
//!    without the suffix (not empty, and not itself ending in the suffix: a long profile has no
//!    long profile of its own), both rows exist and both name the same, non-empty model file. The
//!    file comparison is what makes a pair: an unrelated model that merely ends in `-long` and
//!    loads other weights is never paired. Reply `{"pairs":[{"base":…,"long":…}]}` in byte order
//!    of `base`.
//! 2. **`section`**: the `models.ini` text with `[<base>-long]` appended for a Long tune to start
//!    from. Its lines are the base section's own non-blank lines (trimmed), in order, except
//!    `load-on-startup` and `alias` (two entries must not both start at boot, nor share a name).
//!    A base section without its own model line gets `model = <model>` from the router (its
//!    download keys, `hf-repo` and the like, are then left out, so the copy never downloads); one
//!    without an `mmproj` line gets `mmproj = <mmproj>` when the router names one (and leaves out
//!    `mmproj-url`). Nothing before the new header changes: the reply is the input text, a
//!    newline when it did not end in one, a blank line and the new section, and
//!    [`preset_reload::check`] must find every existing section, `[*]` and the preamble unchanged,
//!    so the router re-reads it without unloading anything. Refusals, checked in this order:
//!    `invalid_id` (the base or the new id is not a preset name of at most [`MAX_ID_BYTES`]),
//!    `is_long` (the base ends in the suffix), `ambiguous` (preset-reload's grammar finds a
//!    duplicate section, a header it reads differently, or a line it would reject), `no_base`,
//!    `exists` (the long section is already there), `no_model` (no model line and no file from the
//!    router), `bad_path` (a file it would write is padded, has a control character, or is
//!    longer than [`MAX_PATH_BYTES`]) and `too_large` (the new text passes [`MAX_FILE_BYTES`]).
//!    Reply `{"ok":true,"id":…,"text":…}` or `{"ok":false,"reason":…}`.
//! 3. **`pick`**: which router entry serves a chat. Profile `high` serves a base model's long
//!    profile (`high`); a model that is a long profile already stays (`is_long`); one without a
//!    long profile stays (`no_long`). Profile `low` serves the base model, so a long profile maps
//!    back to its base (`low`). Reply `{"model":…,"long":bool,"reason":…}`.
//!
//! A preset name is what noevia's preset editor accepts: 1 to [`MAX_ID_BYTES`] bytes of ASCII
//! letters, digits, `_`, `.`, `/`, `:` and `-`.
//!
//! Input is untrusted JSON with exactly the keys named below; anything malformed, with unknown
//! keys, duplicate ids or over a cap is refused with a fixed error code. Nothing here panics.

use serde_json::{json, Map, Value};

/// The suffix of a long profile's id.
pub const SUFFIX: &str = "-long";
/// Requests longer than this are refused ([`LongError::TooLarge`]).
pub const MAX_INPUT_BYTES: usize = 3 * 1024 * 1024;
/// A `models.ini` text, in or out, at most this long (the model manager's editor limit).
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
/// A preset name at most this long (noevia's preset editor).
pub const MAX_ID_BYTES: usize = 200;
/// A router row id (any text) at most this long.
pub const MAX_ROW_ID_BYTES: usize = 512;
/// A model or projector path at most this long.
pub const MAX_PATH_BYTES: usize = 4096;
/// At most this many router rows, or pairs.
pub const MAX_ROWS: usize = 512;

/// Keys never copied into a long section.
const DROP_ALWAYS: &[&str] = &["load-on-startup", "alias", "a", "LLAMA_ARG_ALIAS"];
const MODEL_KEYS: &[&str] = &["model", "m", "LLAMA_ARG_MODEL"];
const MMPROJ_KEYS: &[&str] = &["mmproj", "mm", "LLAMA_ARG_MMPROJ"];
/// Left out when the long section gets the router's model file instead.
const MODEL_SOURCE_KEYS: &[&str] = &[
    "hf-repo",
    "hf",
    "hfr",
    "hf-file",
    "hff",
    "LLAMA_ARG_HF_REPO",
    "LLAMA_ARG_HF_FILE",
    "model-url",
    "mu",
    "LLAMA_ARG_MODEL_URL",
    "docker-repo",
    "dr",
    "LLAMA_ARG_DOCKER_REPO",
];
/// Left out when the long section gets the router's projector file instead.
const MMPROJ_SOURCE_KEYS: &[&str] = &["mmproj-url", "mmu", "LLAMA_ARG_MMPROJ_URL"];

/// A refused request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LongError {
    Input,
    TooLarge,
}

impl LongError {
    pub fn code(self) -> &'static str {
        match self {
            LongError::Input => "input",
            LongError::TooLarge => "too_large",
        }
    }
}

/// One router row: its id and the model file it loads, when known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub model: Option<String>,
}

/// A model and its long profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    pub base: String,
    pub long: String,
}

/// Why a long section could not be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    InvalidId,
    IsLong,
    Ambiguous,
    NoBase,
    Exists,
    NoModel,
    BadPath,
    TooLarge,
    /// The reload check found an existing section changed (never expected; fails closed).
    Unsafe,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Refusal::InvalidId => "invalid_id",
            Refusal::IsLong => "is_long",
            Refusal::Ambiguous => "ambiguous",
            Refusal::NoBase => "no_base",
            Refusal::Exists => "exists",
            Refusal::NoModel => "no_model",
            Refusal::BadPath => "bad_path",
            Refusal::TooLarge => "too_large",
            Refusal::Unsafe => "unsafe",
        }
    }
}

/// The chat's context profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Low,
    High,
}

/// Which entry serves a chat, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pick {
    pub model: String,
    pub long: bool,
    pub reason: &'static str,
}

/// Whether `id` is a preset name noevia's editor accepts.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b':' | b'-'))
}

/// The long profile's id for `base`.
pub fn long_id(base: &str) -> String {
    format!("{base}{SUFFIX}")
}

/// The base id of a long profile id, or `None` when `id` cannot be one.
fn base_of(id: &str) -> Option<&str> {
    if !valid_id(id) {
        return None;
    }
    let base = id.strip_suffix(SUFFIX)?;
    if base.is_empty() || base.ends_with(SUFFIX) {
        return None;
    }
    Some(base)
}

/// The pairs among `rows` (ids unique); see the crate docs.
pub fn pairs(rows: &[Row]) -> Vec<Pair> {
    let model_of = |id: &str| -> Option<&str> {
        let row = rows.iter().find(|r| r.id == id)?;
        row.model.as_deref().filter(|m| !m.is_empty())
    };
    let mut out: Vec<Pair> = rows
        .iter()
        .filter_map(|row| {
            let base = base_of(&row.id)?;
            let mine = model_of(&row.id)?;
            let theirs = model_of(base)?;
            (mine == theirs).then(|| Pair {
                base: base.to_owned(),
                long: row.id.clone(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.base.as_bytes().cmp(b.base.as_bytes()));
    out
}

fn is_ws(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Lines split the router's way (`\r\n`, `\n` or a lone `\r`), as preset-reload splits them.
fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n').flat_map(|l| {
        let l = l.strip_suffix('\r').unwrap_or(l);
        l.split('\r')
    })
}

/// The name in a trimmed `[name]` header line, or `None` when preset-reload reads it as
/// ambiguous.
fn header_name(line: &str) -> Option<&str> {
    let inner = line.strip_prefix('[')?;
    let close = inner.find(']')?;
    let name = inner.get(..close)?;
    let rest = inner.get(close + 1..)?.trim_start_matches(is_ws);
    let ok_rest = rest.is_empty() || rest.starts_with(';') || rest.starts_with('#');
    let ok_name = !name.is_empty()
        && name == name.trim_matches(is_ws)
        && !name.chars().any(|c| c.is_whitespace() || c.is_control())
        && !name.contains(':')
        && name != "default";
    (ok_rest && ok_name).then_some(name)
}

/// The sections of `text` as (name, trimmed non-blank body lines), or `None` when preset-reload's
/// grammar finds it ambiguous.
fn sections(text: &str) -> Option<Vec<(&str, Vec<&str>)>> {
    let mut out: Vec<(&str, Vec<&str>)> = Vec::new();
    for raw in lines(text) {
        let line = raw.trim_matches(is_ws);
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if !raw.starts_with('[') {
                return None;
            }
            let name = header_name(line)?;
            if out.iter().any(|(n, _)| *n == name) {
                return None;
            }
            out.push((name, Vec::new()));
            continue;
        }
        if !line.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == ';' || c == '#')
        {
            return None;
        }
        if let Some((_, body)) = out.last_mut() {
            body.push(line);
        }
    }
    Some(out)
}

/// A key line's key: the text before `=` (or the whole line), padding and leading `-` removed.
/// `None` for a comment line.
fn key_of(line: &str) -> Option<&str> {
    if line.starts_with(';') || line.starts_with('#') {
        return None;
    }
    let key = line.split('=').next().unwrap_or(line);
    Some(key.trim_matches(is_ws).trim_start_matches('-'))
}

fn good_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= MAX_PATH_BYTES
        && p == p.trim()
        && !p.chars().any(|c| c.is_control())
}

/// `text` with `[<base>-long]` appended; see the crate docs.
pub fn section(
    text: &str,
    base: &str,
    model: Option<&str>,
    mmproj: Option<&str>,
) -> Result<(String, String), Refusal> {
    let id = long_id(base);
    if !valid_id(base) || !valid_id(&id) {
        return Err(Refusal::InvalidId);
    }
    if base.ends_with(SUFFIX) {
        return Err(Refusal::IsLong);
    }
    let all = sections(text).ok_or(Refusal::Ambiguous)?;
    let body = all
        .iter()
        .find(|(n, _)| *n == base)
        .map(|(_, b)| b)
        .ok_or(Refusal::NoBase)?;
    if all.iter().any(|(n, _)| *n == id) {
        return Err(Refusal::Exists);
    }
    let has = |keys: &[&str]| body.iter().any(|l| key_of(l).is_some_and(|k| keys.contains(&k)));
    let model = model.filter(|m| !m.is_empty());
    let mmproj = mmproj.filter(|m| !m.is_empty());
    let add_model = if has(MODEL_KEYS) {
        None
    } else {
        Some(model.ok_or(Refusal::NoModel)?)
    };
    let add_mmproj = if has(MMPROJ_KEYS) { None } else { mmproj };
    if add_model.is_some_and(|p| !good_path(p)) || add_mmproj.is_some_and(|p| !good_path(p)) {
        return Err(Refusal::BadPath);
    }
    let mut copied: Vec<String> = Vec::with_capacity(body.len() + 2);
    for line in body {
        if let Some(k) = key_of(line) {
            if DROP_ALWAYS.contains(&k)
                || (add_model.is_some() && MODEL_SOURCE_KEYS.contains(&k))
                || (add_mmproj.is_some() && MMPROJ_SOURCE_KEYS.contains(&k))
            {
                continue;
            }
        }
        copied.push((*line).to_owned());
    }
    if let Some(p) = add_model {
        copied.push(format!("model = {p}"));
    }
    if let Some(p) = add_mmproj {
        copied.push(format!("mmproj = {p}"));
    }
    let mut out = String::with_capacity(text.len() + 256);
    out.push_str(text);
    if !(text.is_empty() || text.ends_with('\n') || text.ends_with('\r')) {
        out.push('\n');
    }
    if !text.is_empty() {
        out.push('\n');
    }
    out.push('[');
    out.push_str(&id);
    out.push_str("]\n");
    for line in &copied {
        out.push_str(line);
        out.push('\n');
    }
    if out.len() > MAX_FILE_BYTES {
        return Err(Refusal::TooLarge);
    }
    let names: Vec<String> = all.iter().map(|(n, _)| (*n).to_owned()).collect();
    if !preset_reload::check(text, &out, &names).safe() {
        return Err(Refusal::Unsafe);
    }
    Ok((id, out))
}

/// Which entry serves `model` for `profile`, given `pairs`; see the crate docs.
pub fn pick(model: &str, profile: Profile, pairs: &[Pair]) -> Pick {
    let as_long = pairs.iter().find(|p| p.long == model);
    let as_base = pairs.iter().find(|p| p.base == model);
    match profile {
        Profile::Low => Pick {
            model: as_long.map_or_else(|| model.to_owned(), |p| p.base.clone()),
            long: false,
            reason: "low",
        },
        Profile::High => match (as_base, as_long) {
            (Some(p), _) => Pick {
                model: p.long.clone(),
                long: true,
                reason: "high",
            },
            (None, Some(_)) => Pick {
                model: model.to_owned(),
                long: true,
                reason: "is_long",
            },
            (None, None) => Pick {
                model: model.to_owned(),
                long: false,
                reason: "no_long",
            },
        },
    }
}

fn exact_keys(o: &Map<String, Value>, keys: &[&str]) -> Result<(), LongError> {
    if o.len() == keys.len() && keys.iter().all(|k| o.contains_key(*k)) {
        Ok(())
    } else {
        Err(LongError::Input)
    }
}

fn text(v: Option<&Value>, min: usize, max: usize) -> Result<&str, LongError> {
    match v.and_then(Value::as_str) {
        Some(s) if s.len() >= min && s.len() <= max => Ok(s),
        _ => Err(LongError::Input),
    }
}

fn maybe_text(v: Option<&Value>, max: usize) -> Result<Option<&str>, LongError> {
    match v {
        Some(Value::Null) => Ok(None),
        other => text(other, 0, max).map(Some),
    }
}

fn parse_pairs(v: Option<&Value>) -> Result<Vec<Pair>, LongError> {
    let list = v.and_then(Value::as_array).ok_or(LongError::Input)?;
    if list.len() > MAX_ROWS {
        return Err(LongError::Input);
    }
    let mut out: Vec<Pair> = Vec::with_capacity(list.len());
    for item in list {
        let o = item.as_object().ok_or(LongError::Input)?;
        exact_keys(o, &["base", "long"])?;
        let base = text(o.get("base"), 1, MAX_ID_BYTES)?;
        let long = text(o.get("long"), 1, MAX_ID_BYTES)?;
        if base_of(long) != Some(base) || out.iter().any(|p| p.base == base) {
            return Err(LongError::Input);
        }
        out.push(Pair {
            base: base.to_owned(),
            long: long.to_owned(),
        });
    }
    Ok(out)
}

fn run(input: &str) -> Result<Value, LongError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(LongError::TooLarge);
    }
    let v: Value = serde_json::from_str(input).map_err(|_| LongError::Input)?;
    let o = v.as_object().ok_or(LongError::Input)?;
    match o.get("op").and_then(Value::as_str) {
        Some("pairs") => {
            exact_keys(o, &["op", "rows"])?;
            let list = o
                .get("rows")
                .and_then(Value::as_array)
                .ok_or(LongError::Input)?;
            if list.len() > MAX_ROWS {
                return Err(LongError::Input);
            }
            let mut rows: Vec<Row> = Vec::with_capacity(list.len());
            for item in list {
                let r = item.as_object().ok_or(LongError::Input)?;
                exact_keys(r, &["id", "model"])?;
                let id = text(r.get("id"), 1, MAX_ROW_ID_BYTES)?;
                let model = maybe_text(r.get("model"), MAX_PATH_BYTES)?;
                if rows.iter().any(|x| x.id == id) {
                    return Err(LongError::Input);
                }
                rows.push(Row {
                    id: id.to_owned(),
                    model: model.map(str::to_owned),
                });
            }
            let found: Vec<Value> = pairs(&rows)
                .into_iter()
                .map(|p| json!({"base": p.base, "long": p.long}))
                .collect();
            Ok(json!({ "pairs": found }))
        }
        Some("section") => {
            exact_keys(o, &["op", "text", "base", "model", "mmproj"])?;
            let file = text(o.get("text"), 0, usize::MAX)?;
            if file.len() > MAX_FILE_BYTES {
                return Err(LongError::TooLarge);
            }
            let base = text(o.get("base"), 1, MAX_ROW_ID_BYTES)?;
            let model = maybe_text(o.get("model"), MAX_PATH_BYTES)?;
            let mmproj = maybe_text(o.get("mmproj"), MAX_PATH_BYTES)?;
            Ok(match section(file, base, model, mmproj) {
                Ok((id, out)) => json!({"ok": true, "id": id, "text": out}),
                Err(r) => json!({"ok": false, "reason": r.code()}),
            })
        }
        Some("pick") => {
            exact_keys(o, &["op", "model", "profile", "pairs"])?;
            let model = text(o.get("model"), 1, MAX_ROW_ID_BYTES)?;
            let profile = match o.get("profile").and_then(Value::as_str) {
                Some("low") => Profile::Low,
                Some("high") => Profile::High,
                _ => return Err(LongError::Input),
            };
            let list = parse_pairs(o.get("pairs"))?;
            let p = pick(model, profile, &list);
            Ok(json!({"model": p.model, "long": p.long, "reason": p.reason}))
        }
        _ => Err(LongError::Input),
    }
}

/// The JSON entry point used by dav-parse.wasm's `long_profile`: status 0 with the reply, or
/// status 1 with `{"error":"input"|"too_large"}`. The request is
/// `{"op":"pairs","rows":[{"id":…,"model":…|null}]}`,
/// `{"op":"section","text":…,"base":…,"model":…|null,"mmproj":…|null}` or
/// `{"op":"pick","model":…,"profile":"low"|"high","pairs":[{"base":…,"long":…}]}`.
pub fn run_json(input: &str) -> (u32, String) {
    match run(input) {
        Ok(v) => (0, v.to_string()),
        Err(e) => (1, format!("{{\"error\":\"{}\"}}", e.code())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    const INI: &str = "version = 1\n\n[*]\ncache-ram = 1024\n\n[Synthetic-A]\nmodel = /models/a.gguf\nmmproj = /models/a-mmproj.gguf\nctx-size = 8192\nload-on-startup = true\n\n[Synthetic-B]\nctx-size = 4096\n";

    fn row(id: &str, model: Option<&str>) -> Row {
        Row {
            id: id.to_owned(),
            model: model.map(str::to_owned),
        }
    }

    #[test]
    fn pairs_need_the_same_file() {
        let rows = [
            row("Synthetic-A", Some("/models/a.gguf")),
            row("Synthetic-A-long", Some("/models/a.gguf")),
            row("Other-long", Some("/models/x.gguf")),
            row("Other", Some("/models/y.gguf")),
            row("Lonely-long", Some("/models/z.gguf")),
        ];
        assert_eq!(
            pairs(&rows),
            vec![Pair {
                base: "Synthetic-A".into(),
                long: "Synthetic-A-long".into()
            }]
        );
    }

    #[test]
    fn a_long_profile_has_no_long_profile() {
        let rows = [row("m-long", Some("/f")), row("m-long-long", Some("/f"))];
        assert!(pairs(&rows).is_empty());
    }

    #[test]
    fn section_copies_the_base_and_keeps_every_other_section() {
        let (id, out) = section(INI, "Synthetic-A", Some("/models/a.gguf"), None).unwrap();
        assert_eq!(id, "Synthetic-A-long");
        assert!(out.starts_with(INI));
        assert!(out.ends_with(
            "\n\n[Synthetic-A-long]\nmodel = /models/a.gguf\nmmproj = /models/a-mmproj.gguf\nctx-size = 8192\n"
        ));
        assert!(preset_reload::check(
            INI,
            &out,
            &["Synthetic-A".into(), "Synthetic-B".into()]
        )
        .safe());
    }

    #[test]
    fn section_adds_the_router_file_when_the_base_has_none() {
        let (_, out) = section(INI, "Synthetic-B", Some("/models/b.gguf"), Some("/models/b-p.gguf"))
            .unwrap();
        assert!(out.ends_with(
            "[Synthetic-B-long]\nctx-size = 4096\nmodel = /models/b.gguf\nmmproj = /models/b-p.gguf\n"
        ));
        assert_eq!(section(INI, "Synthetic-B", None, None), Err(Refusal::NoModel));
        assert_eq!(
            section(INI, "Synthetic-B", Some(" /x"), None),
            Err(Refusal::BadPath)
        );
    }

    #[test]
    fn section_refusals() {
        assert_eq!(section(INI, "bad name", None, None), Err(Refusal::InvalidId));
        assert_eq!(section(INI, "x-long", None, None), Err(Refusal::IsLong));
        assert_eq!(section(INI, "Missing", None, None), Err(Refusal::NoBase));
        assert_eq!(
            section("[a]\nmodel = /f\n[a]\n", "a", None, None),
            Err(Refusal::Ambiguous)
        );
        assert_eq!(
            section("[a]\nmodel = /f\n[a-long]\n", "a", None, None),
            Err(Refusal::Exists)
        );
    }

    #[test]
    fn pick_follows_the_profile() {
        let p = [Pair {
            base: "a".into(),
            long: "a-long".into(),
        }];
        assert_eq!(pick("a", Profile::High, &p).model, "a-long");
        assert_eq!(pick("a-long", Profile::High, &p).reason, "is_long");
        assert_eq!(pick("b", Profile::High, &p).reason, "no_long");
        assert_eq!(pick("a-long", Profile::Low, &p).model, "a");
        assert_eq!(pick("a", Profile::Low, &p).model, "a");
    }

    #[test]
    fn json_shapes() {
        assert_eq!(run_json("{").0, 1);
        assert_eq!(run_json(r#"{"op":"nope"}"#).0, 1);
        assert_eq!(
            run_json(r#"{"op":"pick","model":"a","profile":"high","pairs":[]}"#),
            (
                0,
                r#"{"long":false,"model":"a","reason":"no_long"}"#.to_owned()
            )
        );
        assert_eq!(
            run_json(r#"{"op":"pick","model":"a","profile":"high","pairs":[{"base":"a","long":"b-long"}]}"#).0,
            1
        );
    }
}
