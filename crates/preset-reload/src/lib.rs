//! Can the llama.cpp router re-read `models.ini` without touching a running model?
//! (sbstndalton/noevia#1012)
//!
//! The router (`llama-server --models-preset`) reads `models.ini` at start and again on
//! `GET /models?reload=1`. A reload keeps every running model whose effective preset is unchanged
//! and **unloads** every running model whose preset changed or whose section was removed. That
//! unload would cut off any client using the model, including clients that do not pass through
//! noevia's maintenance gate. So noevia reloads with models loaded only when this crate says the
//! reload cannot change a loaded model's preset.
//!
//! [`check`] compares two texts of `models.ini`: `baseline` (the text the router last read, as
//! recorded by noevia right after a reload it made) and `current` (the file now). A loaded model's
//! preset is built from its own section cascaded over the global `[*]` section, plus the lines
//! before the first header. The reload is safe when, for every loaded model, its own section, the
//! `[*]` section and the preamble are line-for-line identical in both texts.
//!
//! The comparison is deliberately stricter than the router's parser, never looser: it compares
//! trimmed lines (comment lines and inline comments included), so a comment edit counts as a
//! change. Anything the router would read differently from a plain `[name]` header (a duplicate
//! header, which the router resets; a name with `:`, which it canonicalises; padding inside the
//! brackets; an indented header; a `[default]` section, which the router merges with the
//! preamble; any whitespace but space/tab in a header, or anything but a comment after `]`; a
//! line the router's grammar would reject) makes the verdict [`Verdict::Ambiguous`], which is
//! never safe. Grammar: llama.cpp `common/preset.cpp` lines 186-217 at eafe15a5e
//! (`header-line ::= "[" ws section-name "]" eol`, `ws ::= [ \t]*`, `eol ::= ws comment? newline`).
//!
//! A reload never starts a model (`tools/server/server-models.cpp` lines 960-985 add new
//! sections as unloaded; load-on-startup is honoured only on the first load, lines 845-861), so
//! `load-on-startup` needs no special case. A sleeping model counts as running for the reload
//! (`server-models.h` lines 94-95): callers must include sleeping models in `loaded`.
//!
//! Input is untrusted JSON (see [`check_json`]); anything malformed or over a cap is refused with
//! a fixed error code and never echoed. Nothing here panics.

use serde_json::{json, Map, Value};

/// Requests longer than this are refused: two files at the model manager's 1 MiB editor limit,
/// the loaded ids and the JSON escaping around them.
pub const MAX_INPUT_BYTES: usize = 5 * 1024 * 1024;
/// Each text at most this long (`MODELS_INI_MAX_BYTES` in the model manager).
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
/// At most this many loaded models.
pub const MAX_LOADED: usize = 64;
/// A loaded model id at most this long.
pub const MAX_ID_BYTES: usize = 512;

/// The router's name for the lines before the first header (`COMMON_PRESET_DEFAULT_NAME`).
const DEFAULT_NAME: &str = "default";
const GLOBAL_NAME: &str = "*";

/// Why a text could not be compared safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ambiguity {
    /// The same section header appears twice in one text.
    DuplicateSection,
    /// A header the router reads differently from its plain text (padding, `:`, `[default]`,
    /// an empty name, an unclosed bracket, an indented header, any whitespace other than space or
    /// tab in or around it, anything but a `;`/`#` comment after `]`).
    Header,
    /// A non-header line the router's grammar does not accept as a key, comment or blank line
    /// (e.g. a byte-order mark or other leading character), so it would reject the whole file.
    Line,
}

impl Ambiguity {
    pub fn code(self) -> &'static str {
        match self {
            Ambiguity::DuplicateSection => "duplicate_section",
            Ambiguity::Header => "header",
            Ambiguity::Line => "line",
        }
    }
}

/// The verdict for one reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// No loaded model's preset can change: reloading now keeps every loaded model.
    Unchanged,
    /// These loaded models would be unloaded by a reload (in the order given).
    Changed(Vec<String>),
    /// One of the texts cannot be compared safely.
    Ambiguous(Ambiguity),
}

impl Verdict {
    pub fn safe(&self) -> bool {
        matches!(self, Verdict::Unchanged)
    }
}

/// A refused request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckError {
    TooLarge,
    Input,
}

impl CheckError {
    pub fn code(self) -> &'static str {
        match self {
            CheckError::TooLarge => "too_large",
            CheckError::Input => "input",
        }
    }
}

/// One text, split into the preamble and named sections of trimmed, non-blank lines.
struct Sections<'a> {
    preamble: Vec<&'a str>,
    sections: Vec<(&'a str, Vec<&'a str>)>,
}

impl<'a> Sections<'a> {
    fn get(&self, name: &str) -> Option<&[&'a str]> {
        self.sections
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, body)| body.as_slice())
    }
}

fn is_ws(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Lines split the router's way: `\r\n`, `\n` or a lone `\r`. llama.cpp (eafe15a5e)
/// `common/preset.cpp:186` defines `newline ::= "\r\n" / "\n" / "\r"` and every other rule
/// (comment :192, value :201-202, blank-line :214) ends at it, so a lone `\r` is a line break
/// there. This is not Rust's `str::lines()`, which leaves a lone `\r` inside the line (noevia#1043).
fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n').flat_map(|l| {
        let l = l.strip_suffix('\r').unwrap_or(l);
        l.split('\r')
    })
}

fn split(text: &str) -> Result<Sections<'_>, Ambiguity> {
    let mut out = Sections {
        preamble: Vec::new(),
        sections: Vec::new(),
    };
    let mut current: Option<usize> = None;
    for raw in lines(text) {
        let line = raw.trim_matches(is_ws);
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if !raw.starts_with('[') {
                return Err(Ambiguity::Header);
            }
            let name = header_name(line)?;
            if out.sections.iter().any(|(n, _)| *n == name) {
                return Err(Ambiguity::DuplicateSection);
            }
            out.sections.push((name, Vec::new()));
            current = Some(out.sections.len() - 1);
            continue;
        }
        // llama.cpp common/preset.cpp (eafe15a5e) lines 186-217: a kv-line starts with an ident
        // `[a-zA-Z_]`, a comment-line with `;` or `#` after optional space/tab.
        if !line.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == ';' || c == '#')
        {
            return Err(Ambiguity::Line);
        }
        let body = match current.and_then(|i| out.sections.get_mut(i)) {
            Some((_, body)) => body,
            None => &mut out.preamble,
        };
        body.push(line);
    }
    Ok(out)
}

/// The name in a `[name]` header line (already trimmed), or why it is ambiguous.
fn header_name(line: &str) -> Result<&str, Ambiguity> {
    let inner_and_rest = line.strip_prefix('[').ok_or(Ambiguity::Header)?;
    let close = inner_and_rest.find(']').ok_or(Ambiguity::Header)?;
    let name = inner_and_rest.get(..close).ok_or(Ambiguity::Header)?;
    let rest = inner_and_rest
        .get(close + 1..)
        .ok_or(Ambiguity::Header)?
        .trim_start_matches(is_ws);
    if !(rest.is_empty() || rest.starts_with(';') || rest.starts_with('#')) {
        return Err(Ambiguity::Header);
    }
    if name.is_empty()
        || name != name.trim_matches(is_ws)
        || name.chars().any(|c| c.is_whitespace() || c.is_control())
        || name.contains(':')
        || name == DEFAULT_NAME
    {
        return Err(Ambiguity::Header);
    }
    Ok(name)
}

/// Whether reloading `current` keeps every model in `loaded`, given the router last read
/// `baseline`.
pub fn check(baseline: &str, current: &str, loaded: &[String]) -> Verdict {
    if loaded.is_empty() {
        return Verdict::Unchanged;
    }
    let (before, after) = match (split(baseline), split(current)) {
        (Ok(b), Ok(a)) => (b, a),
        (Err(e), _) | (_, Err(e)) => return Verdict::Ambiguous(e),
    };
    let shared_changed =
        before.preamble != after.preamble || before.get(GLOBAL_NAME) != after.get(GLOBAL_NAME);
    let mut changed: Vec<String> = Vec::new();
    for id in loaded {
        if (shared_changed || before.get(id) != after.get(id)) && !changed.contains(id) {
            changed.push(id.clone());
        }
    }
    if changed.is_empty() {
        Verdict::Unchanged
    } else {
        Verdict::Changed(changed)
    }
}

/// The reply for a verdict: `{"safe":bool,"reason":"unchanged"|"changed"|"ambiguous",
/// "changed":[ids],"detail":null|"duplicate_section"|"header"}`.
pub fn verdict_json(v: &Verdict) -> String {
    let (reason, changed, detail): (&str, &[String], Option<&str>) = match v {
        Verdict::Unchanged => ("unchanged", &[], None),
        Verdict::Changed(ids) => ("changed", ids.as_slice(), None),
        Verdict::Ambiguous(a) => ("ambiguous", &[], Some(a.code())),
    };
    json!({"safe": v.safe(), "reason": reason, "changed": changed, "detail": detail}).to_string()
}

/// Parse `{"baseline":"…","current":"…","loaded":["…"]}` (exactly these keys).
pub fn parse(text: &str) -> Result<(String, String, Vec<String>), CheckError> {
    if text.len() > MAX_INPUT_BYTES {
        return Err(CheckError::TooLarge);
    }
    let value: Value = serde_json::from_str(text).map_err(|_| CheckError::Input)?;
    let obj: &Map<String, Value> = value.as_object().ok_or(CheckError::Input)?;
    if obj.len() != 3 {
        return Err(CheckError::Input);
    }
    let file = |key: &str| -> Result<String, CheckError> {
        let s = obj
            .get(key)
            .and_then(Value::as_str)
            .ok_or(CheckError::Input)?;
        if s.len() > MAX_FILE_BYTES {
            return Err(CheckError::TooLarge);
        }
        Ok(s.to_owned())
    };
    let baseline = file("baseline")?;
    let current = file("current")?;
    let list = obj
        .get("loaded")
        .and_then(Value::as_array)
        .ok_or(CheckError::Input)?;
    if list.len() > MAX_LOADED {
        return Err(CheckError::TooLarge);
    }
    let mut loaded = Vec::with_capacity(list.len());
    for item in list {
        let id = item.as_str().ok_or(CheckError::Input)?;
        if id.is_empty() || id.len() > MAX_ID_BYTES {
            return Err(CheckError::Input);
        }
        loaded.push(id.to_owned());
    }
    Ok((baseline, current, loaded))
}

/// The JSON entry point used by dav-parse.wasm's `preset_reload`: status 0 with the verdict, or
/// status 1 with `{"error":"too_large"|"input"}`.
pub fn check_json(text: &str) -> (u32, String) {
    match parse(text) {
        Ok((baseline, current, loaded)) => (0, verdict_json(&check(&baseline, &current, &loaded))),
        Err(e) => (1, format!("{{\"error\":\"{}\"}}", e.code())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    const BASE: &str = "version = 1\n\n[*]\ncache-ram = 1024\n\n[Synthetic-A]\nmodel = /models/a.gguf\nctx-size = 8192\n\n[Synthetic-B]\nmodel = /models/b.gguf\n";

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn adding_a_section_keeps_the_loaded_model() {
        let cur = format!("{BASE}\n[Synthetic-New]\nmodel = /models/new.gguf\nctx-size = 8192\n");
        assert_eq!(
            check(BASE, &cur, &ids(&["Synthetic-A"])),
            Verdict::Unchanged
        );
    }

    #[test]
    fn editing_another_section_keeps_the_loaded_model() {
        let cur = BASE.replace("/models/b.gguf", "/models/b2.gguf");
        assert_eq!(
            check(BASE, &cur, &ids(&["Synthetic-A"])),
            Verdict::Unchanged
        );
    }

    #[test]
    fn editing_the_loaded_section_is_a_change() {
        let cur = BASE.replace("ctx-size = 8192", "ctx-size = 16384");
        assert_eq!(
            check(BASE, &cur, &ids(&["Synthetic-A", "Synthetic-B"])),
            Verdict::Changed(ids(&["Synthetic-A"]))
        );
    }

    #[test]
    fn removing_or_renaming_the_loaded_section_is_a_change() {
        let cur = BASE.replace("[Synthetic-A]", "[Synthetic-A2]");
        assert_eq!(
            check(BASE, &cur, &ids(&["Synthetic-A"])),
            Verdict::Changed(ids(&["Synthetic-A"]))
        );
    }

    #[test]
    fn global_or_preamble_edits_change_every_loaded_model() {
        let cur = BASE.replace("cache-ram = 1024", "cache-ram = 2048");
        assert_eq!(
            check(BASE, &cur, &ids(&["Synthetic-A", "Synthetic-B"])),
            Verdict::Changed(ids(&["Synthetic-A", "Synthetic-B"]))
        );
        let cur = BASE.replace("version = 1", "version = 1\nctx-size = 4096");
        assert_eq!(
            check(BASE, &cur, &ids(&["Synthetic-B"])),
            Verdict::Changed(ids(&["Synthetic-B"]))
        );
    }

    #[test]
    fn whitespace_blank_lines_and_line_endings_do_not_count() {
        let cur = BASE
            .replace('\n', "\r\n")
            .replace("ctx-size = 8192", "  ctx-size = 8192\t");
        let cur = format!("\n\n{cur}\n\n");
        assert_eq!(
            check(BASE, &cur, &ids(&["Synthetic-A"])),
            Verdict::Unchanged
        );
    }

    #[test]
    fn a_lone_cr_is_a_line_break_like_the_router() {
        // preset.cpp:186 `newline` includes a bare "\r"; a lone-CR file means what its LF twin means.
        let cr = BASE.replace('\n', "\r");
        assert_eq!(
            check(BASE, &cr, &ids(&["Synthetic-A", "Synthetic-B"])),
            Verdict::Unchanged
        );
        let mixed = BASE.replacen('\n', "\r", 3).replacen('\n', "\r\r\n", 2);
        assert_eq!(
            check(BASE, &mixed, &ids(&["Synthetic-A", "Synthetic-B"])),
            Verdict::Unchanged
        );
        // An edit behind a lone CR (a value or comment ends there, preset.cpp:192/:201) is seen,
        // and a header after a lone CR opens a section.
        let edited = BASE
            .replace("ctx-size = 8192", "ctx-size = 16384")
            .replace('\n', "\r");
        assert_eq!(
            check(BASE, &edited, &ids(&["Synthetic-A", "Synthetic-B"])),
            Verdict::Changed(ids(&["Synthetic-A"]))
        );
        assert_eq!(
            check(
                "[a]\rk = 1\r[b]\rk = 2\r",
                "[a]\rk = 1\r[b]\rk = 3\r",
                &ids(&["b"])
            ),
            Verdict::Changed(ids(&["b"]))
        );
        assert_eq!(
            check(
                "[a]\rk = 1\r[b]\rk = 2\r",
                "[a]\rk = 1\r[b]\rk = 3\r",
                &ids(&["a"])
            ),
            Verdict::Unchanged
        );
        // An indented header after a lone CR stays ambiguous.
        assert_eq!(
            check("[a]\rk = 1 \r  [b]\r", "[a]\r", &ids(&["a"])),
            Verdict::Ambiguous(Ambiguity::Header)
        );
    }

    #[test]
    fn a_comment_edit_counts_as_a_change() {
        let cur = BASE.replace("ctx-size = 8192", "ctx-size = 8192 ; tuned");
        assert_eq!(
            check(BASE, &cur, &ids(&["Synthetic-A"])),
            Verdict::Changed(ids(&["Synthetic-A"]))
        );
    }

    #[test]
    fn a_model_absent_from_both_texts_is_unchanged() {
        assert_eq!(
            check(BASE, BASE, &ids(&["org/cached-model"])),
            Verdict::Unchanged
        );
    }

    #[test]
    fn nothing_loaded_is_always_safe() {
        assert_eq!(check("[x]\n[x]\n", "[", &[]), Verdict::Unchanged);
    }

    #[test]
    fn ambiguous_headers_are_never_safe() {
        for bad in [
            "[Synthetic-A]\n[Synthetic-A]\n",
            "[ Synthetic-A]\n",
            "[Synthetic-A ]\n",
            "  [Synthetic-A]\n",
            "[org/m:Q4_K_M]\n",
            "[default]\nctx-size = 1\n",
            "[]\n",
            "[Synthetic-A\n",
            "[Synthetic-A] trailing\n",
            "[Synthetic-A]]\n",
            "[Synthetic-A]\u{b}\n",
            "[Synthetic-A]\u{a0}\n",
            "[Synth\u{b}etic]\n",
            "[Synth etic]\n",
            "[\u{a0}Synthetic-A]\n",
            "\u{feff}[Synthetic-A]\n",
            "[Synthetic-A]\n\u{feff}ctx-size = 1\n",
            "[Synthetic-A]\n\u{b}ctx-size = 1\n",
            "[Synthetic-A]\n-ctx-size = 1\n",
            "[Synthetic-A]\n= 1\n",
        ] {
            let v = check(BASE, bad, &ids(&["Synthetic-B"]));
            assert!(matches!(v, Verdict::Ambiguous(_)), "{bad:?} gave {v:?}");
            assert!(!v.safe());
            let v = check(bad, BASE, &ids(&["Synthetic-B"]));
            assert!(matches!(v, Verdict::Ambiguous(_)), "{bad:?} gave {v:?}");
        }
        assert_eq!(
            check(BASE, "[a]\n[a]\n", &ids(&["x"])),
            Verdict::Ambiguous(Ambiguity::DuplicateSection)
        );
    }

    #[test]
    fn line_codes() {
        assert_eq!(
            check(BASE, "\u{feff}version = 1\n", &ids(&["x"])),
            Verdict::Ambiguous(Ambiguity::Line)
        );
        assert_eq!(
            check(
                "[a]\t# note\n[b];x\n",
                "[a]\t# note\n[b];x\n[c]\n",
                &ids(&["a"])
            ),
            Verdict::Unchanged
        );
    }

    #[test]
    fn load_on_startup_is_an_ordinary_option() {
        let cur = format!("{BASE}[Synthetic-New]\nload-on-startup = true\n");
        assert_eq!(
            check(BASE, &cur, &ids(&["Synthetic-A"])),
            Verdict::Unchanged
        );
        let cur = BASE.replace("ctx-size = 8192", "ctx-size = 8192\nload-on-startup = true");
        assert_eq!(
            check(BASE, &cur, &ids(&["Synthetic-A"])),
            Verdict::Changed(ids(&["Synthetic-A"]))
        );
    }

    #[test]
    fn header_with_trailing_comment_is_plain() {
        let cur = BASE.replace("[Synthetic-B]", "[Synthetic-B] ; second");
        assert_eq!(
            check(BASE, &cur, &ids(&["Synthetic-A"])),
            Verdict::Unchanged
        );
    }

    #[test]
    fn json_entry_point() {
        let req = json!({"baseline": BASE, "current": BASE, "loaded": ["Synthetic-A"]}).to_string();
        let (status, reply) = check_json(&req);
        assert_eq!(status, 0);
        let v: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(
            v,
            json!({"safe": true, "reason": "unchanged", "changed": [], "detail": null})
        );

        let cur = BASE.replace("8192", "4096");
        let req = json!({"baseline": BASE, "current": cur, "loaded": ["Synthetic-A"]}).to_string();
        let v: Value = serde_json::from_str(&check_json(&req).1).unwrap();
        assert_eq!(
            v,
            json!({"safe": false, "reason": "changed", "changed": ["Synthetic-A"], "detail": null})
        );

        let req = json!({"baseline": BASE, "current": "[a]\n[a]", "loaded": ["x"]}).to_string();
        let v: Value = serde_json::from_str(&check_json(&req).1).unwrap();
        assert_eq!(
            v,
            json!({"safe": false, "reason": "ambiguous", "changed": [], "detail": "duplicate_section"})
        );
    }

    #[test]
    fn refusals_are_fixed_codes() {
        for bad in [
            "",
            "null",
            "[]",
            "{\"baseline\":\"\",\"current\":\"\"}",
            "{\"baseline\":\"\",\"current\":\"\",\"loaded\":[1]}",
            "{\"baseline\":\"\",\"current\":\"\",\"loaded\":[\"\"]}",
            "{\"baseline\":1,\"current\":\"\",\"loaded\":[]}",
            "{\"baseline\":\"\",\"current\":\"\",\"loaded\":[],\"x\":1}",
        ] {
            assert_eq!(
                check_json(bad),
                (1, "{\"error\":\"input\"}".to_owned()),
                "{bad}"
            );
        }
        let many: Vec<String> = (0..=MAX_LOADED).map(|i| format!("m{i}")).collect();
        let req = json!({"baseline": "", "current": "", "loaded": many}).to_string();
        assert_eq!(
            check_json(&req),
            (1, "{\"error\":\"too_large\"}".to_owned())
        );
        let big = "x".repeat(MAX_FILE_BYTES + 1);
        let req = json!({"baseline": big, "current": "", "loaded": []}).to_string();
        assert_eq!(
            check_json(&req),
            (1, "{\"error\":\"too_large\"}".to_owned())
        );
        assert_eq!(
            check_json(&" ".repeat(MAX_INPUT_BYTES + 1)),
            (1, "{\"error\":\"too_large\"}".to_owned())
        );
    }
}
