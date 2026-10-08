//! Two small pure leaves of noevia-core in Rust, exported from `dav-parse.wasm`
//! (POLICY_LEAVES_IMPL):
//!
//! - [`auth_tokens`]: `resolveAuthTokens(env)` of `server/auth-tokens.cjs` (#294): the trimmed
//!   DIARY_AUTH_TOKEN and UI_AUTH_TOKEN (no fallback between them), LEGACY_AUTH_COMPAT, and the
//!   two startup warnings. Strings are UTF-16 code units, trimmed as `String.prototype.trim`.
//! - [`mode`] and [`check_set`]: `server/tool-policy.cjs`'s per-account, per-tool decision and
//!   the validation `set()` runs before it writes. The database stays in the JS.
//!
//! Security: the decision is never weaker than the JS's (a stored mode the JS does not know is
//! `block` here, where the JS would hand the unknown string back), writes are never `allow`,
//! and no refusal or fault reply carries any input, so a token can never be echoed by an error.

#![forbid(unsafe_code)]

/// Units per string (a token, a stored mode, a value) before the request is refused.
pub const MAX_UNITS: usize = 65_536;
/// Tools in one `set()` before the request is refused.
pub const MAX_TOOLS: usize = 65_536;
/// The largest request either call accepts.
pub const MAX_INPUT_BYTES: usize = 3 * (5 + 2 * MAX_UNITS) + 1;

pub const DIARY_WARNING: &str = "WARNING: Set DIARY_AUTH_TOKEN to protect the internal diary connection. Browser accounts remain authenticated.";
pub const LEGACY_WARNING: &str = "WARNING: LEGACY_AUTH_COMPAT is true but UI_AUTH_TOKEN is empty; the legacy bearer sign-in has no token to check requests against.";

const TOO_LARGE: &str = r#"{"error":"too_large"}"#;
const BAD_INPUT: &str = r#"{"error":"input"}"#;

/// ECMAScript WhiteSpace and LineTerminator, which `trim()` removes (not U+0085, unlike
/// Rust's `str::trim`; U+FEFF included).
pub fn is_js_space(u: u16) -> bool {
    matches!(
        u,
        0x09..=0x0d | 0x20 | 0xa0 | 0x1680 | 0x2000..=0x200a | 0x2028 | 0x2029 | 0x202f | 0x205f
            | 0x3000 | 0xfeff
    )
}

/// `String.prototype.trim` over UTF-16 code units.
pub fn js_trim(s: &[u16]) -> &[u16] {
    let start = s.iter().position(|u| !is_js_space(*u)).unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|u| !is_js_space(*u))
        .map_or(start, |i| i + 1);
    s.get(start..end).unwrap_or(&[])
}

/// The result of `resolveAuthTokens`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthTokens {
    pub diary_token: Vec<u16>,
    pub ui_auth_token: Vec<u16>,
    pub legacy_compat: bool,
    pub warnings: Vec<&'static str>,
}

/// `resolveAuthTokens(env)`. `diary` and `ui` are `String(env.X || '')` (the host's coercion);
/// `compat` is LEGACY_AUTH_COMPAT when it is a string, `None` otherwise.
pub fn auth_tokens(diary: &[u16], ui: &[u16], compat: Option<&[u16]>) -> AuthTokens {
    let diary_token = js_trim(diary).to_vec();
    let ui_auth_token = js_trim(ui).to_vec();
    let true_units: [u16; 4] = [0x74, 0x72, 0x75, 0x65];
    let legacy_compat = compat == Some(&true_units[..]);
    let mut warnings = Vec::new();
    if diary_token.is_empty() {
        warnings.push(DIARY_WARNING);
    }
    if legacy_compat && ui_auth_token.is_empty() {
        warnings.push(LEGACY_WARNING);
    }
    AuthTokens {
        diary_token,
        ui_auth_token,
        legacy_compat,
        warnings,
    }
}

/// A per-tool permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mode {
    Allow,
    Ask,
    Block,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Allow => "allow",
            Mode::Ask => "ask",
            Mode::Block => "block",
        }
    }

    /// A string that is exactly one of the three names.
    pub fn parse(units: &[u16]) -> Option<Mode> {
        [Mode::Allow, Mode::Ask, Mode::Block]
            .into_iter()
            .find(|m| m.name().encode_utf16().eq(units.iter().copied()))
    }
}

/// tool-policy.cjs `mode()`: `stored` is the row's mode (`None` without a user or a row). A
/// stored string that is not a mode (the table's CHECK forbids one) is `block`; an empty one
/// is the default, as `m || 'allow'` makes it.
pub fn mode(stored: Option<&[u16]>, is_write: bool) -> Mode {
    let stored = stored.filter(|s| !s.is_empty());
    let parsed = stored.map(|s| Mode::parse(s).unwrap_or(Mode::Block));
    if parsed == Some(Mode::Block) {
        return Mode::Block;
    }
    if is_write {
        return Mode::Ask;
    }
    parsed.unwrap_or(Mode::Allow)
}

/// Why tool-policy.cjs `set()` refuses, in the order it checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetRefusal {
    /// `Choose allow, ask or block.`
    Mode,
    /// `Choose a tool.`
    Empty,
    /// `Writes always ask first, so they cannot be set to Always allow.`
    WriteAllow,
}

impl SetRefusal {
    pub fn code(self) -> &'static str {
        match self {
            SetRefusal::Mode => "mode",
            SetRefusal::Empty => "empty",
            SetRefusal::WriteAllow => "write",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            SetRefusal::Mode => "Choose allow, ask or block.",
            SetRefusal::Empty => "Choose a tool.",
            SetRefusal::WriteAllow => {
                "Writes always ask first, so they cannot be set to Always allow."
            }
        }
    }
}

/// tool-policy.cjs `set()` before it writes: `value` is the requested mode when it is a string,
/// `writes` whether each listed tool is a write. `Ok` is the mode to store.
pub fn check_set(value: Option<&[u16]>, writes: &[bool]) -> Result<Mode, SetRefusal> {
    let m = value.and_then(Mode::parse).ok_or(SetRefusal::Mode)?;
    if writes.is_empty() {
        return Err(SetRefusal::Empty);
    }
    if m == Mode::Allow && writes.iter().any(|w| *w) {
        return Err(SetRefusal::WriteAllow);
    }
    Ok(m)
}

struct Cursor<'a> {
    b: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = (self.b.get(..n)?, self.b.get(n..)?);
        self.b = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn u32(&mut self) -> Option<usize> {
        let mut a = [0u8; 4];
        a.copy_from_slice(self.take(4)?);
        usize::try_from(u32::from_le_bytes(a)).ok()
    }

    fn flag(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    /// `u8 tag (0 none, 1 string)` and for a string `u32le(n)` and n UTF-16LE units.
    fn string(&mut self) -> Result<Option<Vec<u16>>, &'static str> {
        match self.flag().ok_or(BAD_INPUT)? {
            false => Ok(None),
            true => {
                let n = self.u32().ok_or(BAD_INPUT)?;
                if n > MAX_UNITS {
                    return Err(TOO_LARGE);
                }
                let bytes = self.take(n * 2).ok_or(BAD_INPUT)?;
                Ok(Some(
                    bytes
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes(*c))
                        .collect(),
                ))
            }
        }
    }

    fn end(&self) -> Result<(), &'static str> {
        if self.b.is_empty() {
            Ok(())
        } else {
            Err(BAD_INPUT)
        }
    }
}

/// An ASCII JSON string over UTF-16 units (lone surrogates as `\uXXXX`).
pub fn write_units(out: &mut String, s: &[u16]) {
    out.push('"');
    for &u in s {
        match u {
            0x22 => out.push_str("\\\""),
            0x5c => out.push_str("\\\\"),
            0x20..=0x7e => out.push(char::from(u as u8)),
            _ => out.push_str(&format!("\\u{u:04x}")),
        }
    }
    out.push('"');
}

/// The `auth_tokens` wire call: input three strings in the [`Cursor::string`] format
/// (DIARY_AUTH_TOKEN, UI_AUTH_TOKEN, LEGACY_AUTH_COMPAT; the first two always strings). Status 0
/// replies `{"diaryToken":…,"uiAuthToken":…,"legacyCompat":bool,"warnings":[…]}`; status 1
/// `{"error":"input"|"too_large"}`, never anything of the input.
pub fn auth_call(input: &[u8]) -> (u32, String) {
    let run = || -> Result<String, &'static str> {
        if input.len() > MAX_INPUT_BYTES {
            return Err(TOO_LARGE);
        }
        let mut c = Cursor { b: input };
        let diary = c.string()?.ok_or(BAD_INPUT)?;
        let ui = c.string()?.ok_or(BAD_INPUT)?;
        let compat = c.string()?;
        c.end()?;
        let t = auth_tokens(&diary, &ui, compat.as_deref());
        let mut out = String::from("{\"diaryToken\":");
        write_units(&mut out, &t.diary_token);
        out.push_str(",\"uiAuthToken\":");
        write_units(&mut out, &t.ui_auth_token);
        out.push_str(",\"legacyCompat\":");
        out.push_str(if t.legacy_compat { "true" } else { "false" });
        out.push_str(",\"warnings\":[");
        for (i, w) in t.warnings.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let units: Vec<u16> = w.encode_utf16().collect();
            write_units(&mut out, &units);
        }
        out.push_str("]}");
        Ok(out)
    };
    match run() {
        Ok(s) => (0, s),
        Err(e) => (1, e.to_owned()),
    }
}

/// The `tool_policy` wire call. Input `1 stored(string) u8(isWrite)` for `mode()`, reply
/// `{"mode":"allow"|"ask"|"block"}`; or `2 value(string) u32le(n) n×u8(isWrite)` for `set()`,
/// reply `{"ok":true,"mode":…}` / `{"ok":false,"reason":"mode"|"empty"|"write"}`. Status 1
/// `{"error":"input"|"too_large"}`.
pub fn policy_call(input: &[u8]) -> (u32, String) {
    let run = || -> Result<String, &'static str> {
        if input.len() > MAX_INPUT_BYTES {
            return Err(TOO_LARGE);
        }
        let mut c = Cursor { b: input };
        match c.u8().ok_or(BAD_INPUT)? {
            1 => {
                let stored = c.string()?;
                let w = c.flag().ok_or(BAD_INPUT)?;
                c.end()?;
                Ok(format!(
                    "{{\"mode\":\"{}\"}}",
                    mode(stored.as_deref(), w).name()
                ))
            }
            2 => {
                let value = c.string()?;
                let n = c.u32().ok_or(BAD_INPUT)?;
                if n > MAX_TOOLS {
                    return Err(TOO_LARGE);
                }
                let writes = c
                    .take(n)
                    .ok_or(BAD_INPUT)?
                    .iter()
                    .map(|b| match b {
                        0 => Ok(false),
                        1 => Ok(true),
                        _ => Err(BAD_INPUT),
                    })
                    .collect::<Result<Vec<bool>, _>>()?;
                c.end()?;
                Ok(match check_set(value.as_deref(), &writes) {
                    Ok(m) => format!("{{\"ok\":true,\"mode\":\"{}\"}}", m.name()),
                    Err(r) => format!("{{\"ok\":false,\"reason\":\"{}\"}}", r.code()),
                })
            }
            _ => Err(BAD_INPUT),
        }
    };
    match run() {
        Ok(s) => (0, s),
        Err(e) => (1, e.to_owned()),
    }
}
