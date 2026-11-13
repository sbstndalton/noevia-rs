//! What code-acp.cjs builds before a connection: the agent's environment, the sandbox endpoint,
//! the harness-specific session metadata, the claude-code harness's pinned settings, and what an
//! agent says about itself at `initialize` (code-meta.cjs `readAgent`).

use crate::Error;
use js_json::{wtf16, JValue};

/// The egress proxy a task with network is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proxy {
    pub url: String,
    pub no_proxy: Option<String>,
}

fn set(items: &mut Vec<(String, JValue)>, key: &str, value: Option<String>) {
    match value {
        Some(v) => wtf16::set(items, key, JValue::Str(v)),
        None => items.retain(|(k, _)| k != key),
    }
}

/// code-acp.cjs `agentEnv`: nothing inherited but `PATH` and `LANG` (given here, read from the
/// server's own environment by the caller); `HOME` the task's private home; the proxy variables;
/// the caller's own; then `CURL_HOME` and `WGETRC` pinned so neither tool reads config the agent
/// wrote. In JS property order.
pub fn agent_env(
    base_path: Option<&str>,
    base_lang: Option<&str>,
    env: &[(String, String)],
    proxy: Option<&Proxy>,
    home: Option<&str>,
) -> JValue {
    let get = |k: &str| {
        env.iter()
            .rev()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    let truthy = |v: Option<String>| v.filter(|s| !s.is_empty());
    let mut out: Vec<(String, JValue)> = Vec::new();
    set(&mut out, "PATH", base_path.map(str::to_string));
    set(
        &mut out,
        "HOME",
        truthy(home.map(str::to_string)).or_else(|| truthy(get("HOME"))),
    );
    set(&mut out, "TMPDIR", truthy(get("TMPDIR")));
    set(&mut out, "LANG", base_lang.map(str::to_string));
    if let Some(p) = proxy {
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            set(&mut out, key, Some(p.url.clone()));
        }
        set(
            &mut out,
            "NO_PROXY",
            Some(p.no_proxy.clone().unwrap_or_default()),
        );
    }
    for (k, v) in env {
        set(&mut out, k, Some(v.clone()));
    }
    set(&mut out, "CURL_HOME", Some("/nonexistent".into()));
    set(&mut out, "WGETRC", Some("/dev/null".into()));
    JValue::Obj(out)
}

/// code-acp.cjs `splitEndpoint`: `host:port` or `[v6]:port`.
pub fn split_endpoint(endpoint: &str) -> Result<(String, u16), Error> {
    let bad = || {
        Error::with_status(
            500,
            format!("CODE_HARNESS_ENDPOINT should be host:port, not \"{endpoint}\""),
        )
    };
    let (host, port) = if let Some(rest) = endpoint.strip_prefix('[') {
        let (host, port) = rest.split_once("]:").ok_or_else(bad)?;
        if host.is_empty() || host.contains(']') {
            return Err(bad());
        }
        (host, port)
    } else {
        let (host, port) = endpoint.split_once(':').ok_or_else(bad)?;
        if host.is_empty() || port.contains(':') {
            return Err(bad());
        }
        (host, port)
    };
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    // Node then refuses a port past 65535 when it connects; refused here instead.
    let port: u16 = port.parse().map_err(|_| bad())?;
    Ok((host.to_string(), port))
}

/// code-acp.cjs `sessionMetadata`: `{ noevia: { permission } }` and, for the claude-code harness,
/// its adapter's options with noevia's pinned settings inline. `None` when there is nothing.
pub fn session_metadata(
    permission: Option<&JValue>,
    harness: Option<&str>,
    settings: Option<&JValue>,
) -> Result<Option<JValue>, Error> {
    let mut meta: Vec<(String, JValue)> = Vec::new();
    if let Some(p) = permission.filter(|p| p.truthy()) {
        meta.push(("noevia".into(), JValue::obj([("permission", p.clone())])));
    }
    if harness == Some("claude-code") {
        let Some(settings) = settings.filter(|s| matches!(s, JValue::Obj(_))) else {
            return Err(Error::with_status(
                409,
                "Claude Code needs noevia\u{2019}s pinned session settings.",
            ));
        };
        let strings =
            |list: &[&str]| JValue::Arr(list.iter().map(|s| JValue::Str((*s).into())).collect());
        let options = JValue::obj([
            ("settingSources", JValue::Arr(Vec::new())),
            ("strictMcpConfig", JValue::Bool(true)),
            ("allowDangerouslySkipPermissions", JValue::Bool(false)),
            ("plugins", JValue::Arr(Vec::new())),
            (
                "tools",
                strings(&[
                    "Read",
                    "Edit",
                    "Write",
                    "NotebookEdit",
                    "Bash",
                    "Glob",
                    "Grep",
                    "WebFetch",
                    "WebSearch",
                ]),
            ),
            (
                "extraArgs",
                JValue::obj([("disable-slash-commands", JValue::Null)]),
            ),
            ("settings", settings.clone()),
        ]);
        meta.push(("claudeCode".into(), JValue::obj([("options", options)])));
    }
    Ok((!meta.is_empty()).then_some(JValue::Obj(meta)))
}

/// code-acp.cjs `readClaudeSettings`: `<home>/.claude/settings.json`, a JSON object. The home
/// belongs to the harness user, so the two names below it are opened without following a link
/// (Node read through any link the harness placed there, and sent what it found to the agent).
pub fn read_claude_settings(home: Option<&str>) -> Result<JValue, Error> {
    use rustix::fs::{open, openat, Mode, OFlags};
    use std::io::Read;
    let home = home.filter(|h| h.starts_with('/')).ok_or_else(|| {
        Error::with_status(
            409,
            "Claude Code needs the task private HOME for its pinned settings.",
        )
    })?;
    let invalid = || {
        Error::with_status(
            409,
            "Claude Code\u{2019}s pinned settings are missing or invalid.",
        )
    };
    let dflags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let root = open(home, dflags, Mode::empty()).map_err(|_| invalid())?;
    let dir = openat(&root, ".claude", dflags | OFlags::NOFOLLOW, Mode::empty())
        .map_err(|_| invalid())?;
    let fd = openat(
        &dir,
        "settings.json",
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| invalid())?;
    let mut file = std::fs::File::from(fd);
    if !file.metadata().map_err(|_| invalid())?.is_file() {
        return Err(invalid());
    }
    let mut raw = Vec::new();
    (&mut file)
        .take(1024 * 1024 + 1)
        .read_to_end(&mut raw)
        .map_err(|_| invalid())?;
    if raw.len() > 1024 * 1024 {
        return Err(invalid());
    }
    match wtf16::parse(&String::from_utf8_lossy(&raw)) {
        Ok(v @ JValue::Obj(_)) => Ok(v),
        _ => Err(invalid()),
    }
}

/// JS `Number(v)` for the values JSON can hold.
pub(crate) fn js_number(v: &JValue) -> f64 {
    match v {
        JValue::Num(n) => *n,
        JValue::Null => 0.0,
        JValue::Bool(b) => f64::from(u8::from(*b)),
        JValue::Str(s) => string_number(s),
        JValue::Arr(items) => match items.as_slice() {
            [] => 0.0,
            [one] if !matches!(one, JValue::Arr(_) | JValue::Obj(_)) => match one {
                JValue::Null | JValue::Undefined => 0.0,
                other => js_number(&JValue::Str(
                    js_json::to_js_string(other).unwrap_or_default(),
                )),
            },
            _ => f64::NAN,
        },
        JValue::Undefined | JValue::Obj(_) => f64::NAN,
    }
}

fn string_number(s: &str) -> f64 {
    let t = crate::js_trim(s);
    if t.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = t.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return digits
                .chars()
                .filter_map(|c| c.to_digit(radix))
                .fold(0.0, |acc, d| acc * f64::from(radix) + f64::from(d));
        }
    }
    let (sign, body) = match t.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, t.strip_prefix('+').unwrap_or(t)),
    };
    if body == "Infinity" {
        return sign * f64::INFINITY;
    }
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(i) => (body.get(..i).unwrap_or(""), body.get(i + 1..)),
        None => (body, None),
    };
    let digits = mantissa.chars().filter(char::is_ascii_digit).count();
    let points = mantissa.chars().filter(|c| *c == '.').count();
    let ok = digits > 0
        && points <= 1
        && digits + points == mantissa.len()
        && exponent.is_none_or(|e| {
            let e = e.strip_prefix(['+', '-']).unwrap_or(e);
            !e.is_empty() && e.chars().all(|c| c.is_ascii_digit())
        });
    if !ok {
        return f64::NAN;
    }
    body.parse::<f64>().map_or(f64::NAN, |n| sign * n)
}

/// code-meta.cjs `readAgent`: `{ name, version, protocolVersion }`.
pub fn read_agent(result: &JValue) -> JValue {
    let info = [
        result.get("agentInfo"),
        result.get("serverInfo"),
        result.get("_meta").get("agentInfo"),
    ]
    .into_iter()
    .find(|v| v.truthy())
    .unwrap_or(&JValue::Null);
    let text = |v: &JValue, n: usize| match v {
        JValue::Str(s) => JValue::Str(wtf16::slice(s, n)),
        _ => JValue::Null,
    };
    let n = js_number(result.get("protocolVersion"));
    JValue::obj([
        ("name", text(info.get("name"), 80)),
        ("version", text(info.get("version"), 40)),
        (
            "protocolVersion",
            if n.is_finite() && n >= 0.0 {
                JValue::Num(n)
            } else {
                JValue::Null
            },
        ),
    ])
}
