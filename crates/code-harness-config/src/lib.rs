//! The harness's own configuration files, ported from noevia-core `server/code-harness-config.cjs`
//! (spec-agent-execution §3).
//!
//! A coding harness takes its permissions and its model endpoint from files it treats as
//! authoritative, so noevia writes those files before the agent exists: the gate (every edit,
//! command and fetch asks) and the one endpoint and model it may use. A harness whose permissions
//! cannot be pinned is refused.
//!
//! - [`config_for`]: the `opencode.json` object of the `opencode` harness.
//! - [`pin_files_for`]: every file a harness needs, relative to the task's working directory
//!   ([`Base::Cwd`]) or its private home ([`Base::Home`]), byte for byte what Node writes (key
//!   order, two-space pretty printing, number formatting, the embedded gate scripts), or the
//!   refusal (status 409 and Node's message) for codex, unknown harnesses, a missing model and an
//!   endpoint that is not `http(s)://`.
//! - [`write_harness_config`] / [`write_pinned`]: the symlink-safe write (no link in any directory
//!   component, no link or directory at the target, a fresh `O_CREAT|O_EXCL` file mode `0o600`,
//!   new directories mode `0o700`) and the hand-over to the harness user through an injectable
//!   `lchown`.
//! - [`cwd_pin_paths`]: the pinned paths that land in the working tree and must never be committed.
//!
//! Proved against fixtures recorded from the Node module (`tools/gen-fixtures.cjs`, replayed by
//! `tests/fixtures.rs`). Inputs are Rust strings, so the lone UTF-16 surrogates a JS string can
//! hold are not representable; every other input behaves as in Node, including JS `trim()`
//! (which strips U+FEFF but not U+0085, unlike `str::trim`).
//!
//! Regenerating: `node tools/gen-fixtures.cjs <noevia-core>/server` rewrites
//! `tests/fixtures/code-harness-config.v1.json` (deterministic; the sha256 of the Node source it
//! exercised is in the fixture's `source` field). Synthetic inputs only.
//!
//! Named differences from Node: none. Two Node behaviours are kept on purpose and covered by the
//! fixture: a missing working directory is refused with the same "needs a private home directory"
//! message as a missing home, and a later file's refusal leaves the earlier files written (the
//! `claude-code` harness writes its working-tree file before it finds the home missing).
#![forbid(unsafe_code)]

mod gates;
#[cfg(unix)]
mod write;

pub use gates::{DSH_GATE, PI_GATE};
use js_json::JValue;
#[cfg(unix)]
pub use write::{
    lchown, write_harness_config, write_pinned, Owner, PinError, Record, WriteRequest,
};

/// Every action class asks; there is no "never ask" (`PERMISSION`).
pub const PERMISSION: [(&str, &str); 3] = [("edit", "ask"), ("bash", "ask"), ("webfetch", "ask")];
/// The `claude-code` harness's `ask` rules (`EDIT_TOOLS`).
pub const EDIT_TOOLS: [&str; 6] = [
    "Edit",
    "Write",
    "NotebookEdit",
    "Bash",
    "WebFetch",
    "WebSearch",
];
/// The `pi` harness tools that are plain reads, still confined to the working directory by the gate.
pub const PI_READ_TOOLS: [&str; 4] = ["read", "grep", "find", "ls"];
/// The `deepseek` harness tools that do not ask: plain reads and its own to-do/goal bookkeeping.
pub const DSH_NO_ASK_TOOLS: [&str; 6] = [
    "read",
    "read_image",
    "glob",
    "grep",
    "todo_write",
    "get_goal",
];
/// Shipped `deepseek` harness profile entries the patch disables (telemetry, hosted services,
/// self-configuration, repository-supplied instructions and skills).
pub const DSH_DISABLED: [&str; 12] = [
    "session-telemetry-otel",
    "session-log-deepseek",
    "web-search-deepseek",
    "llm-deepseek",
    "deepseek-account",
    "plugin-manager",
    "config-editor",
    "settings",
    "agent-instructions",
    "skill-filesystem",
    "tool-skill",
    "skill",
];

/// The default context and output token limits (Node's parameter defaults).
pub const DEFAULT_CONTEXT_TOKENS: f64 = 32768.0;
pub const DEFAULT_OUTPUT_TOKENS: f64 = 4096.0;

/// The status Node attaches to every refusal here.
pub const CONFLICT: u16 = 409;

/// What a task asks to be pinned. `None` and `Some("")` behave alike for the strings (Node's
/// `String(v || '')` and `apiKey || 'none'`); `None` tokens take the defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PinOptions<'a> {
    pub harness: Option<&'a str>,
    pub model: Option<&'a str>,
    pub engine: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub context_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
}

/// A refusal: Node's `Error` with a `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub message: String,
}

impl Refusal {
    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: CONFLICT,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refusal {}

/// Where a pinned file is placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    /// The task's working directory.
    Cwd,
    /// The task's private HOME.
    Home,
}

impl Base {
    pub fn as_str(self) -> &'static str {
        match self {
            Base::Cwd => "cwd",
            Base::Home => "home",
        }
    }
}

/// One pinned file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinFile {
    pub base: Base,
    pub path: &'static str,
    pub content: String,
}

/// Every file a harness needs pinned, and what the job event records about it. The permission is
/// always [`PERMISSION`] ([`permission_json`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    pub harness: &'static str,
    pub files: Vec<PinFile>,
    pub model: String,
    pub endpoint: String,
}

/// The `opencode` harness's config: the file name and its JSON object.
#[derive(Debug, Clone, PartialEq)]
pub struct OpencodeConfig {
    pub name: &'static str,
    pub json: JValue,
}

/// [`PERMISSION`] as a fresh JSON object.
pub fn permission_json() -> JValue {
    JValue::obj(PERMISSION.iter().map(|(k, v)| (*k, JValue::from(*v))))
}

/// JavaScript's `String.prototype.trim()`: WhiteSpace and LineTerminator code points.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| {
        matches!(
            c,
            '\u{9}'
                | '\u{a}'
                | '\u{b}'
                | '\u{c}'
                | '\u{d}'
                | ' '
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2000}'
                ..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        )
    })
}

/// `Number(x) | 0` (ECMAScript ToInt32).
pub fn to_int32(x: f64) -> i32 {
    if !x.is_finite() {
        return 0;
    }
    let two32 = 4_294_967_296.0_f64;
    let m = x.trunc().rem_euclid(two32);
    let signed = if m >= 2_147_483_648.0 { m - two32 } else { m };
    // In range by construction: an integer in [-2^31, 2^31).
    signed as i32
}

fn unpinnable(id: &str) -> Refusal {
    let shown = if id.is_empty() { "unnamed" } else { id };
    Refusal::conflict(format!(
        "noevia cannot pin the permissions of the {shown} harness, so it will not run it."
    ))
}

const NO_MODEL: &str = "A coding task needs a model; none is loaded or chosen.";
const NO_ENDPOINT: &str = "This server has no model endpoint for coding tasks.";
const CODEX: &str = "noevia cannot pin the permissions of the codex harness (its commands run without asking inside its own sandbox), so it will not run it.";

fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn api_key_or_none(key: Option<&str>) -> &str {
    match key {
        Some(k) if !k.is_empty() => k,
        _ => "none",
    }
}

fn trimmed(v: Option<&str>) -> &str {
    js_trim(v.unwrap_or(""))
}

/// The `opencode` harness's `opencode.json` (`configFor`). Any harness id other than `opencode`
/// (after trimming) is refused; then a missing model; then an engine that is not `http(s)://`.
/// The engine is used as given (trimmed), not normalised to `/v1`.
pub fn config_for(o: &PinOptions<'_>) -> Result<OpencodeConfig, Refusal> {
    let id = trimmed(o.harness);
    if id != "opencode" {
        return Err(unpinnable(id));
    }
    let name = trimmed(o.model);
    let base_url = trimmed(o.engine);
    if name.is_empty() {
        return Err(Refusal::conflict(NO_MODEL));
    }
    if !is_http(base_url) {
        return Err(Refusal::conflict(NO_ENDPOINT));
    }
    let context = o.context_tokens.unwrap_or(DEFAULT_CONTEXT_TOKENS);
    let output = o.output_tokens.unwrap_or(DEFAULT_OUTPUT_TOKENS);
    let model_entry = JValue::obj([
        ("name", JValue::from(name)),
        ("tool_call", JValue::Bool(true)),
        (
            "limit",
            JValue::obj([
                ("context", JValue::Num(context)),
                ("output", JValue::Num(output)),
            ]),
        ),
    ]);
    let local = JValue::obj([
        ("npm", JValue::from("@ai-sdk/openai-compatible")),
        ("name", JValue::from("noevia engine")),
        (
            "options",
            JValue::obj([
                ("baseURL", JValue::from(base_url)),
                ("apiKey", JValue::from(api_key_or_none(o.api_key))),
            ]),
        ),
        ("models", JValue::obj([(name, model_entry)])),
    ]);
    let model = format!("local/{name}");
    let json = JValue::obj([
        ("$schema", JValue::from("https://opencode.ai/config.json")),
        ("autoupdate", JValue::Bool(false)),
        ("share", JValue::from("disabled")),
        ("provider", JValue::obj([("local", local)])),
        ("model", JValue::from(model.as_str())),
        ("small_model", JValue::from(model)),
        ("permission", permission_json()),
    ]);
    Ok(OpencodeConfig {
        name: "opencode.json",
        json,
    })
}

/// The model name and the `/v1` endpoint (`endpointFor`): trailing slashes dropped, `/v1`
/// appended unless already there.
fn endpoint_for(o: &PinOptions<'_>) -> Result<(String, String), Refusal> {
    let name = trimmed(o.model);
    let base_url = trimmed(o.engine).trim_end_matches('/');
    if name.is_empty() {
        return Err(Refusal::conflict(NO_MODEL));
    }
    if !is_http(base_url) {
        return Err(Refusal::conflict(NO_ENDPOINT));
    }
    let endpoint = if base_url.ends_with("/v1") {
        base_url.to_owned()
    } else {
        format!("{base_url}/v1")
    };
    Ok((name.to_owned(), endpoint))
}

fn strings(items: &[&str]) -> JValue {
    JValue::Arr(items.iter().map(|s| JValue::from(*s)).collect())
}

/// The `claude-code` harness's security settings (`claudeSecuritySettings`): bypass and auto modes
/// off, every edit, command and fetch asks, MCP tools denied, sandboxed commands not
/// auto-approved, hooks and hosted connectors and syncs off.
pub fn claude_security_settings() -> JValue {
    JValue::obj([
        (
            "permissions",
            JValue::obj([
                ("defaultMode", JValue::from("default")),
                ("disableBypassPermissionsMode", JValue::from("disable")),
                ("disableAutoMode", JValue::from("disable")),
                ("blockReadsOutsideWorkingDirectories", JValue::Bool(true)),
                ("allow", JValue::Arr(Vec::new())),
                ("deny", strings(&["mcp__*"])),
                ("ask", strings(&EDIT_TOOLS)),
            ]),
        ),
        (
            "sandbox",
            JValue::obj([("autoAllowBashIfSandboxed", JValue::Bool(false))]),
        ),
        ("disableAllHooks", JValue::Bool(true)),
        ("disableClaudeAiConnectors", JValue::Bool(true)),
        ("syncClaudeAiSkills", JValue::Bool(false)),
        ("syncClaudeAiPlugins", JValue::Bool(false)),
    ])
}

fn claude_settings(name: &str, base_url: &str, api_key: Option<&str>) -> JValue {
    let mut items = match claude_security_settings() {
        JValue::Obj(items) => items,
        _ => Vec::new(),
    };
    // The harness appends /v1/messages itself.
    let base = base_url.strip_suffix("/v1").unwrap_or(base_url);
    items.push((
        "env".to_owned(),
        JValue::obj([
            ("ANTHROPIC_BASE_URL", JValue::from(base)),
            ("ANTHROPIC_API_KEY", JValue::from(api_key_or_none(api_key))),
            ("ANTHROPIC_MODEL", JValue::from(name)),
            ("DISABLE_AUTOUPDATER", JValue::from("1")),
            ("DISABLE_UPDATES", JValue::from("1")),
            (
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
                JValue::from("1"),
            ),
            ("DISABLE_TELEMETRY", JValue::from("1")),
        ]),
    ));
    JValue::Obj(items)
}

/// `JSON.stringify(String(v))`, which YAML reads as a double-quoted scalar.
fn q(s: &str) -> String {
    let mut out = String::new();
    js_json::quote(s, &mut out);
    out
}

/// The `deepseek` harness profile patch, as YAML (`dshPatch`). Every string goes through JSON
/// quoting; the token limits through `Number(x) | 0`.
pub fn dsh_patch(
    name: &str,
    base_url: &str,
    api_key: Option<&str>,
    context_tokens: f64,
    output_tokens: f64,
) -> String {
    let mut lines: Vec<String> = [
        "# Written by noevia for this task. Replaces nothing but the entries it names.",
        "- id: llm-pi-ai",
        "  config:",
        "    providers:",
        "      noevia:",
        "        displayName: \"noevia engine\"",
        "        api: openai-completions",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    lines.push(format!("        baseURL: {}", q(base_url)));
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        lines.push("        headers:".to_owned());
        lines.push(format!(
            "          Authorization: {}",
            q(&format!("Bearer {key}"))
        ));
    }
    lines.push("        models:".to_owned());
    lines.push(format!("          - id: {}", q(name)));
    lines.push(format!("            name: {}", q(name)));
    lines.push(format!(
        "            contextWindow: {}",
        to_int32(context_tokens)
    ));
    lines.push(format!(
        "            maxTokens: {}",
        to_int32(output_tokens)
    ));
    lines.push("            reasoningEfforts: false".to_owned());
    for entry in ["agent-default-model", "acp"] {
        lines.push(format!("- id: {entry}"));
        lines.push("  config:".to_owned());
        lines.push("    provider: noevia".to_owned());
        lines.push(format!("    model: {}", q(name)));
    }
    for line in [
        "- id: approval",
        "  config:",
        "    policy: ask",
        "- id: sandbox-policy",
        "  config:",
        "    mode: workspace-write",
        "    workspaceRoot: !!js process.cwd()",
    ] {
        lines.push(line.to_owned());
    }
    for id in DSH_DISABLED {
        lines.push(format!("- id: {id}"));
        lines.push("  disabled: true".to_owned());
    }
    for line in [
        "- insert:",
        "    - id: noevia-gate",
        "      name: ./noevia-gate.mjs",
    ] {
        lines.push(line.to_owned());
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// `JSON.stringify(value, null, 2) + '\n'`.
fn json_file(v: &JValue) -> String {
    let mut out = js_json::stringify_pretty(v).unwrap_or_default();
    out.push('\n');
    out
}

/// Every file a harness needs pinned (`pinFilesFor`). Harness ids are trimmed and exact
/// (`opencode`, `claude-code`, `qwen-code`, `pi`, `deepseek`); `codex` and every other id are
/// refused before the model and endpoint are looked at.
pub fn pin_files_for(o: &PinOptions<'_>) -> Result<Pinned, Refusal> {
    let id = trimmed(o.harness);
    let context = o.context_tokens.unwrap_or(DEFAULT_CONTEXT_TOKENS);
    let output = o.output_tokens.unwrap_or(DEFAULT_OUTPUT_TOKENS);
    let file = |base, path, content| PinFile {
        base,
        path,
        content,
    };
    match id {
        "opencode" => {
            let config = config_for(o)?;
            let model = config
                .json
                .get("model")
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let endpoint = config
                .json
                .get("provider")
                .get("local")
                .get("options")
                .get("baseURL")
                .as_str()
                .unwrap_or_default()
                .to_owned();
            Ok(Pinned {
                harness: "opencode",
                files: vec![file(Base::Cwd, config.name, json_file(&config.json))],
                model,
                endpoint,
            })
        }
        "claude-code" => {
            let (name, endpoint) = endpoint_for(o)?;
            let settings = json_file(&claude_settings(&name, &endpoint, o.api_key));
            Ok(Pinned {
                harness: "claude-code",
                files: vec![
                    file(Base::Cwd, ".claude/settings.local.json", settings.clone()),
                    file(Base::Home, ".claude/settings.json", settings),
                ],
                model: name,
                endpoint,
            })
        }
        "codex" => Err(Refusal::conflict(CODEX)),
        "qwen-code" => {
            let (name, endpoint) = endpoint_for(o)?;
            let settings = json_file(&JValue::obj([
                (
                    "tools",
                    JValue::obj([("approvalMode", JValue::from("default"))]),
                ),
                (
                    "security",
                    JValue::obj([(
                        "auth",
                        JValue::obj([("selectedType", JValue::from("openai"))]),
                    )]),
                ),
                (
                    "model",
                    JValue::obj([
                        ("name", JValue::from(name.as_str())),
                        (
                            "generationConfig",
                            JValue::obj([("contextWindowSize", JValue::Num(context))]),
                        ),
                    ]),
                ),
                (
                    "modelProviders",
                    JValue::obj([(
                        "openai",
                        JValue::Arr(vec![JValue::obj([
                            ("id", JValue::from(name.as_str())),
                            ("name", JValue::from(name.as_str())),
                            ("baseUrl", JValue::from(endpoint.as_str())),
                            ("envKey", JValue::from("NOEVIA_ENGINE_KEY")),
                        ])]),
                    )]),
                ),
                (
                    "env",
                    JValue::obj([(
                        "NOEVIA_ENGINE_KEY",
                        JValue::from(api_key_or_none(o.api_key)),
                    )]),
                ),
                (
                    "general",
                    JValue::obj([("enableAutoUpdate", JValue::Bool(false))]),
                ),
                (
                    "privacy",
                    JValue::obj([("usageStatisticsEnabled", JValue::Bool(false))]),
                ),
            ]));
            Ok(Pinned {
                harness: "qwen-code",
                files: vec![
                    file(Base::Cwd, ".qwen/settings.json", settings.clone()),
                    file(Base::Home, ".qwen/settings.json", settings),
                ],
                model: name,
                endpoint,
            })
        }
        "pi" => {
            let (name, endpoint) = endpoint_for(o)?;
            let models = JValue::obj([(
                "providers",
                JValue::obj([(
                    "noevia",
                    JValue::obj([
                        ("baseUrl", JValue::from(endpoint.as_str())),
                        ("api", JValue::from("openai-completions")),
                        ("apiKey", JValue::from(api_key_or_none(o.api_key))),
                        (
                            "models",
                            JValue::Arr(vec![JValue::obj([
                                ("id", JValue::from(name.as_str())),
                                ("name", JValue::from(name.as_str())),
                                ("contextWindow", JValue::Num(context)),
                                ("maxTokens", JValue::Num(output)),
                                ("reasoning", JValue::Bool(false)),
                                ("input", strings(&["text"])),
                            ])]),
                        ),
                    ]),
                )]),
            )]);
            let settings = JValue::obj([
                ("defaultProvider", JValue::from("noevia")),
                ("defaultModel", JValue::from(name.as_str())),
                ("defaultProjectTrust", JValue::from("never")),
                ("enableInstallTelemetry", JValue::Bool(false)),
            ]);
            Ok(Pinned {
                harness: "pi",
                files: vec![
                    file(Base::Home, ".pi/agent/models.json", json_file(&models)),
                    file(Base::Home, ".pi/agent/settings.json", json_file(&settings)),
                    file(
                        Base::Home,
                        ".pi/agent/extensions/noevia-gate.js",
                        PI_GATE.to_owned(),
                    ),
                ],
                model: name,
                endpoint,
            })
        }
        "deepseek" => {
            let (name, endpoint) = endpoint_for(o)?;
            let patch = dsh_patch(&name, &endpoint, o.api_key, context, output);
            Ok(Pinned {
                harness: "deepseek",
                files: vec![
                    file(Base::Home, ".dsh/profiles/acp/cordis.patch.yml", patch),
                    file(
                        Base::Home,
                        ".dsh/profiles/acp/noevia-gate.mjs",
                        DSH_GATE.to_owned(),
                    ),
                ],
                model: name,
                endpoint,
            })
        }
        other => Err(unpinnable(other)),
    }
}

/// The pinned files a harness writes into the working tree, which must never be committed
/// (`cwdPinPaths`); empty for a harness that is refused.
pub fn cwd_pin_paths(harness: Option<&str>) -> Vec<&'static str> {
    let probe = PinOptions {
        harness,
        model: Some("m"),
        engine: Some("http://engine.invalid/v1"),
        ..PinOptions::default()
    };
    match pin_files_for(&probe) {
        Ok(p) => p
            .files
            .iter()
            .filter(|f| f.base == Base::Cwd)
            .map(|f| f.path)
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gates_embed_the_tool_lists() {
        let pi = js_json::stringify(&strings(&PI_READ_TOOLS)).unwrap_or_default();
        assert!(PI_GATE.contains(&format!("const READ = new Set({pi});\n")));
        let dsh = js_json::stringify(&strings(&DSH_NO_ASK_TOOLS)).unwrap_or_default();
        assert!(DSH_GATE.contains(&format!("const NO_ASK = new Set({dsh});\n")));
    }

    #[test]
    fn to_int32_wraps_like_js() {
        assert_eq!(to_int32(2_147_483_653.0), -2_147_483_643);
        assert_eq!(to_int32(4_294_967_297.0), 1);
        assert_eq!(to_int32(-1.5), -1);
        assert_eq!(to_int32(f64::NAN), 0);
        assert_eq!(to_int32(1e21), -559_939_584);
    }

    #[test]
    fn js_trim_is_not_str_trim() {
        assert_eq!(js_trim("\u{feff} a \u{3000}"), "a");
        assert_eq!(js_trim("\u{85}a"), "\u{85}a");
    }
}
