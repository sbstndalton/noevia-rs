//! The native chat preparation in shadow (track R8, slice P7a). For an account the switch names,
//! a turn core handed to the native executor (chat_native.rs) is prepared a second time here, from
//! the same request and the account's own files, with `chat_prepare::shadow`; the native messages
//! are compared with core's `prepared` frame and one line says whether they are the same. Nothing
//! uses them: the turn runs from core's frame as before, and the shadow never delays it (it runs on
//! a blocking thread after the turn has started, at most [`MAX_RUNNING`] at a time, the rest skipped).
//!
//! The line holds no text of the turn: `chat prepare shadow: match`, `skip <reason>`,
//! `failed <reason>` or `diff field=<messages|chat|turn> message=<i> unit=<u> ours=<n> core=<n>` (see `chat_prepare::shadow`).
//!
//! Switch: `NOEVIA_CHAT_PREPARE` (config.rs): unset, empty or `node` is off (the default);
//! `shadow` every account the executor runs; `shadow:<account id>,...` those. It only matters for
//! turns `NOEVIA_CHAT_EXECUTOR` sends to the native executor. A bad value refuses startup.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use chat_prepare::shadow::{compare, Catalogue, Settings, ShadowTurn, Verdict};
use chat_prepare::toolboxes::Toolbox;
use chat_prepare::tools_mode::{is_public_tool_name, PRIVATE_CORE_TOOLS};
use js_json::{wtf16, JValue};
use server_features::Feature;

use crate::App;

/// index.cjs HISTORY_CAP.
const HISTORY_CAP: usize = 1000;
/// Shadows running at once; one more is skipped (`skip busy`).
pub const MAX_RUNNING: usize = 2;
static RUNNING: AtomicUsize = AtomicUsize::new(0);

/// Which accounts' turns are prepared in shadow.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Switch {
    /// Off (the default).
    #[default]
    Node,
    Accounts(Vec<String>),
    All,
}

impl Switch {
    /// `NOEVIA_CHAT_PREPARE`: unset, empty or `node`; `shadow`; `shadow:<id>,<id>`.
    pub fn parse(value: Option<&str>) -> Result<Switch, String> {
        let v = value.map(str::trim).unwrap_or("");
        match v {
            "" | "node" => Ok(Switch::Node),
            "shadow" => Ok(Switch::All),
            _ => match v.strip_prefix("shadow:") {
                Some(list) => {
                    let ids: Vec<String> = list
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                    if ids.is_empty()
                        || ids.iter().any(|id| {
                            id.len() > 64
                                || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        })
                    {
                        Err("NOEVIA_CHAT_PREPARE: shadow:<account id>,... needs account ids".into())
                    } else {
                        Ok(Switch::Accounts(ids))
                    }
                }
                None => Err(
                    "NOEVIA_CHAT_PREPARE must be node, shadow or shadow:<account id>,...".into(),
                ),
            },
        }
    }

    pub fn names(&self, user: &str) -> bool {
        match self {
            Switch::Node => false,
            Switch::All => true,
            Switch::Accounts(ids) => ids.iter().any(|id| id == user),
        }
    }
}

/// Whether this account's handed-off turns are prepared in shadow.
pub(crate) fn wanted(app: &Arc<App>, user: &str) -> bool {
    app.config.chat_prepare.names(user)
}

/// Prepares the turn in shadow and logs the verdict, on a blocking thread; returns at once.
/// `body` is the request body core was sent, `frame` core's `prepared` frame.
pub(crate) fn spawn(app: Arc<App>, user_dir: PathBuf, body: Bytes, frame: JValue) {
    /// One running shadow, counted until it is dropped (also when the blocking task never runs).
    struct Slot;
    impl Drop for Slot {
        fn drop(&mut self) {
            RUNNING.fetch_sub(1, Ordering::AcqRel);
        }
    }
    let slot = Slot;
    if RUNNING.fetch_add(1, Ordering::AcqRel) >= MAX_RUNNING {
        drop(slot);
        eprintln!("chat prepare shadow: skip busy");
        return;
    }
    drop(tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || {
            // allToolboxes(), as the front's catalogue stands (None: not current).
            let catalogue = quiet_catalogue(&app);
            // index.cjs: chatFramingEnabled() and brainContext.enabled() (brainContext and chatFraming).
            let framing = crate::code_routes::feature(&app, Feature::ChatFraming);
            let flags = Flags {
                chat_framing: Some(framing),
                brain_context: Some(
                    framing && crate::code_routes::feature(&app, Feature::BrainContext),
                ),
            };
            let boxes = catalogue.map(|c| toolboxes(&c.boxes));
            let settings = settings_rows(&app);
            let verdict = run(
                &user_dir,
                &body,
                &frame,
                &server_time_zone(),
                flags,
                boxes.as_deref(),
                settings.as_ref(),
            );
            drop(slot);
            eprintln!("chat prepare shadow: {}", verdict.line());
        })
        .await;
    }));
}

/// The toolbox catalogue as it stands, read without the side effects of
/// [`crate::mcp_discovery::read`]: no touch of the demand marker (which wakes core's publishers),
/// no discovery refresh and no wait. The facts file as published (a stale one is refused), then
/// discovery's current snapshot. `None` when either is not current: a history that needs the
/// catalogue is then skipped.
fn quiet_catalogue(app: &Arc<App>) -> Option<crate::mcp_discovery::Catalogue> {
    let dir = app.config.data_dir.clone()?;
    let file = dir.join(crate::mcp_discovery::FACTS_FILE);
    let snap = crate::snapshot::read(
        &file,
        crate::mcp_discovery::FACTS_KIND,
        crate::identity::now_ms(),
    )
    .ok()?;
    let facts = wtf16::parse(std::str::from_utf8(&snap.raw).ok()?).ok()?;
    let handle = crate::mcp_discovery_backend::handle(app)?;
    let discovered = handle.source.current();
    if discovered.generation == 0 {
        return None;
    }
    crate::mcp_discovery::compose(
        &facts,
        &discovered,
        &|id| handle.offered(id),
        &handle.nextcloud_origins,
    )
}

/// chat.cjs's two settings reads (`SELECT value FROM settings WHERE key=...`, then `?.value`):
/// `reasoning_effort_default` and `auto_sampling_presets_enabled`, each `undefined` without a row,
/// `null` for SQL NULL. `None` when either cannot be read (the shadow then leaves those fields out).
fn settings_rows(app: &App) -> Option<(JValue, JValue)> {
    // better-sqlite3's `.get()?.value`: a string for TEXT, a number for INTEGER or REAL, null for
    // NULL (a BLOB is a Buffer: unknown here).
    let read = |key: &str| -> Option<JValue> {
        let row = app
            .writes
            .writer()
            .ok()?
            .read(|r| {
                r.row(
                    "SELECT typeof(value), CAST(value AS TEXT) FROM settings WHERE key=?1",
                    [key],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                )
            })
            .ok()?;
        match row {
            None => Some(JValue::Undefined),
            Some((kind, text)) => match (kind.as_str(), text) {
                ("null", _) => Some(JValue::Null),
                ("text", Some(t)) => Some(JValue::from(t)),
                ("integer" | "real", Some(t)) => t.parse::<f64>().ok().map(JValue::Num),
                _ => None,
            },
        }
    };
    Some((
        read("reasoning_effort_default")?,
        read("auto_sampling_presets_enabled")?,
    ))
}

/// The catalogue's boxes as the replay's tool questions read them: each box's id and its tools
/// (`{ type: 'function', function: { name } }`). The catalogue keeps at most 500 tools of a box and
/// 128 units of a name; past that a tool is missed, which can only show as a diff.
fn toolboxes(boxes: &[crate::mcp_discovery::BoxInfo]) -> Vec<Toolbox> {
    boxes
        .iter()
        .map(|b| Toolbox {
            id: b.id.clone(),
            label: String::new(),
            source: String::new(),
            in_app: false,
            server: None,
            tools: b
                .tools
                .iter()
                .map(|t| {
                    js_json::JValue::Obj(vec![
                        ("type".into(), JValue::from("function")),
                        (
                            "function".into(),
                            JValue::Obj(vec![("name".into(), JValue::from(t.name.as_str()))]),
                        ),
                    ])
                })
                .collect(),
            reads: Vec::new(),
        })
        .collect()
}

/// The feature flags the preparation reads (`None`: unknown).
#[derive(Debug, Clone, Copy, Default)]
pub struct Flags {
    pub chat_framing: Option<bool>,
    pub brain_context: Option<bool>,
}

/// One shadow preparation: the account's files read as core reads them.
/// `boxes`: `allToolboxes()` (`None`: unknown, so a history that needs it is skipped).
pub fn run(
    user_dir: &Path,
    body: &[u8],
    frame: &JValue,
    time_zone: &str,
    flags: Flags,
    boxes: Option<&[Toolbox]>,
    settings: Option<&(JValue, JValue)>,
) -> Verdict {
    let Some(body) = std::str::from_utf8(body)
        .ok()
        .and_then(|t| wtf16::parse(t).ok())
    else {
        return Verdict::Skip("body");
    };
    let read = |name: &str| std::fs::read(user_dir.join(name)).ok();
    let instructions = read("account-instructions.json");
    let memory = read("account-memory.json");
    // freeChats(): the list as stored (read as core reads a JSON file); no file is the empty list,
    // an unreadable one `None` (the shadow then skips a turn with chat framing on).
    let free_chats = match std::fs::read(user_dir.join(server_projects::chats::FREE_CHATS_FILE)) {
        Ok(bytes) => chat_prepare::js::parse_file(&bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(JValue::Arr(Vec::new())),
        Err(_) => None,
    };
    // chat.cjs replayOptions: knownTool (a tool some box registers, the project reader always) and
    // carryOrphanTool (tools-mode.cjs isPublicToolName).
    let known = |name: &str| {
        PRIVATE_CORE_TOOLS.contains(&name)
            || boxes.is_some_and(|b| b.iter().any(|b| b.offers(name)))
    };
    let carry = |name: &str| is_public_tool_name(Some(name), boxes.unwrap_or_default());
    // getProject(body.projectId): the account's projects.json as the store reads it.
    let project_id = body.get("projectId").as_str();
    let projects = project_id.and_then(|_| {
        server_projects::store::read_json(&user_dir.join(server_projects::store::PROJECTS_FILE))
            .ok()
            .flatten()
    });
    let project = match (project_id, projects.as_ref().map(|p| p.get("projects"))) {
        (Some(id), Some(JValue::Arr(list))) => {
            list.iter().find(|p| p.get("id").as_str() == Some(id))
        }
        _ => None,
    };
    compare(&ShadowTurn {
        body: &body,
        project,
        frame,
        instructions: instructions.as_deref(),
        memory: memory.as_deref(),
        free_chats: free_chats.as_ref(),
        time_zone,
        chat_framing: flags.chat_framing,
        brain_context: flags.brain_context,
        history_cap: HISTORY_CAP,
        titles: true,
        workspace: true,
        settings: settings.map(|(effort_default, sampling_presets)| Settings {
            effort_default,
            sampling_presets,
        }),
        catalogue: boxes.map(|_| Catalogue {
            known_tool: &known,
            carry_orphan_tool: &carry,
        }),
    })
}

/// `serverTimeZone()`: what Node's ICU resolves for the process, as measured with Node 22 (the N5
/// builder, 2026-11-14): a set `TZ` alone decides (a leading `:` and a `posix/` or `right/` prefix
/// dropped; an absolute path or an unknown name gives `undefined`, so `UTC`; an empty one
/// `Etc/Unknown`, which the time part states as `UTC`); unset, where the `/etc/localtime` link
/// finally points, else `/etc/timezone`, else `UTC`. Then V8's canonical `UTC` for the UTC and GMT
/// aliases. A link zone ICU maps to its canonical name (`US/Eastern` to `America/New_York`) is not
/// mapped here: the shadow then reports a diff in the system message.
pub fn server_time_zone() -> String {
    let tz = std::env::var_os("TZ").map(|v| v.to_string_lossy().into_owned());
    let localtime = std::fs::canonicalize("/etc/localtime")
        .ok()
        .and_then(|p| p.to_str().map(str::to_string));
    let timezone = std::fs::read_to_string("/etc/timezone").ok();
    let zone = zone_from(tz.as_deref(), localtime.as_deref(), timezone.as_deref());
    if chat_prepare::time::zone(&zone).is_some() {
        zone
    } else {
        "UTC".into()
    }
}

/// The names V8 resolves to `UTC` (ICU's `Etc/UTC` and `Etc/GMT` and their links).
const UTC_ALIASES: [&str; 18] = [
    "UTC",
    "Etc/UTC",
    "GMT",
    "Etc/GMT",
    "UCT",
    "Etc/UCT",
    "Universal",
    "Etc/Universal",
    "Zulu",
    "Etc/Zulu",
    "GMT0",
    "Etc/GMT0",
    "GMT+0",
    "Etc/GMT+0",
    "GMT-0",
    "Etc/GMT-0",
    "Greenwich",
    "Etc/Greenwich",
];

fn zone_from(tz: Option<&str>, localtime: Option<&str>, timezone: Option<&str>) -> String {
    let zone = match tz {
        Some(tz) => {
            let name = tz.strip_prefix(':').unwrap_or(tz);
            let name = name
                .strip_prefix("posix/")
                .or_else(|| name.strip_prefix("right/"))
                .unwrap_or(name);
            if name.is_empty() || name.starts_with('/') {
                "UTC".to_string()
            } else {
                name.to_string()
            }
        }
        None => localtime
            .and_then(|l| l.rsplit_once("zoneinfo/").map(|(_, z)| z.to_string()))
            .or_else(|| {
                timezone
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "UTC".into()),
    };
    if UTC_ALIASES.iter().any(|a| a.eq_ignore_ascii_case(&zone)) {
        "UTC".into()
    } else {
        zone
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_reads_off_shadow_and_accounts() {
        assert_eq!(Switch::parse(None), Ok(Switch::Node));
        assert_eq!(Switch::parse(Some(" node ")), Ok(Switch::Node));
        assert_eq!(Switch::parse(Some("shadow")), Ok(Switch::All));
        assert_eq!(
            Switch::parse(Some("shadow:qa-1, qa-2")),
            Ok(Switch::Accounts(vec!["qa-1".into(), "qa-2".into()]))
        );
        for bad in ["native", "shadow:", "shadow:qa 1", "shadow:qa/1", "on"] {
            assert!(Switch::parse(Some(bad)).is_err(), "{bad}");
        }
        let s = Switch::parse(Some("shadow:qa-1")).unwrap();
        assert!(s.names("qa-1") && !s.names("qa-2"));
        assert!(!Switch::Node.names("qa-1"));
    }

    #[test]
    fn the_zone_is_read_as_icu_reads_it() {
        // Node 22 on the N5 builder (TZ=<value> node -p "Intl.DateTimeFormat().resolvedOptions().timeZone").
        assert_eq!(
            zone_from(Some("Europe/Berlin"), None, None),
            "Europe/Berlin"
        );
        assert_eq!(zone_from(Some(":Asia/Tokyo"), None, None), "Asia/Tokyo");
        assert_eq!(
            zone_from(Some("posix/Europe/Berlin"), None, None),
            "Europe/Berlin"
        );
        assert_eq!(
            zone_from(Some("right/Europe/Berlin"), None, None),
            "Europe/Berlin"
        );
        // undefined, then serverTimeZone's 'UTC'; an empty TZ is Etc/Unknown, stated as UTC.
        assert_eq!(
            zone_from(Some("/usr/share/zoneinfo/America/New_York"), None, None),
            "UTC"
        );
        assert_eq!(
            zone_from(Some(""), Some("/usr/share/zoneinfo/Europe/Paris"), None),
            "UTC"
        );
        assert_eq!(zone_from(Some("Etc/GMT+5"), None, None), "Etc/GMT+5");
        // TZ unset: the stock image links /etc/localtime to Etc/UTC, which V8 calls UTC.
        assert_eq!(
            zone_from(None, Some("/usr/share/zoneinfo/Etc/UTC"), None),
            "UTC"
        );
        assert_eq!(
            zone_from(
                None,
                Some("/usr/share/zoneinfo/Europe/Paris"),
                Some("Etc/UTC")
            ),
            "Europe/Paris"
        );
        assert_eq!(
            zone_from(None, None, Some("Australia/Adelaide\n")),
            "Australia/Adelaide"
        );
        assert_eq!(zone_from(None, Some("/etc/elsewhere"), None), "UTC");
        assert_eq!(zone_from(None, None, None), "UTC");
        for alias in [
            "Etc/GMT",
            "GMT",
            "UCT",
            "Etc/Universal",
            "Zulu",
            "GMT0",
            "Greenwich",
            "etc/utc",
        ] {
            assert_eq!(zone_from(Some(alias), None, None), "UTC", "{alias}");
        }
    }

    #[test]
    fn an_unreadable_body_is_skipped() {
        let dir = std::env::temp_dir();
        let frame = JValue::Null;
        let flags = Flags::default();
        assert_eq!(
            run(&dir, b"{not json", &frame, "UTC", flags, None, None),
            Verdict::Skip("body")
        );
        assert_eq!(
            run(&dir, b"{}", &frame, "UTC", flags, None, None),
            Verdict::Skip("frame")
        );
    }

    /// Recorded free-chat hand-offs (crates/chat-prepare's node-shadow.v1.json) prepared from the files
    /// as core's workspace holds them: the account's instructions and memory and the free list.
    #[test]
    fn recorded_hand_offs_match_from_the_account_s_files() {
        let f = wtf16::parse(include_str!(
            "../../../crates/chat-prepare/tests/fixtures/node-shadow.v1.json"
        ))
        .unwrap();
        let JValue::Arr(turns) = f.get("turns") else {
            panic!("no turns")
        };
        let base = std::env::temp_dir().join(format!("qa-shadow-{}", std::process::id()));
        let (mut matched, mut framed) = (0, 0);
        for (i, t) in turns.iter().enumerate() {
            let frame = t.get("prepared");
            let input = t.get("input");
            if !frame.is_object() || t.get("threw").as_str().is_some() {
                continue;
            }
            let dir = base.join(i.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            // What Node's fs.writeFileSync wrote (a lone surrogate as U+FFFD).
            let write = |name: &str, v: &JValue| {
                if let Some(text) = v.as_str() {
                    let text: String = text
                        .chars()
                        .map(|c| {
                            if wtf16::lone_unit(c).is_some() {
                                '\u{FFFD}'
                            } else {
                                c
                            }
                        })
                        .collect();
                    std::fs::write(dir.join(name), text).unwrap();
                }
            };
            write("account-instructions.json", input.get("instructions"));
            if let JValue::Arr(projects) = input.get("projects") {
                let file = JValue::Obj(vec![("projects".into(), JValue::Arr(projects.clone()))]);
                std::fs::write(
                    dir.join(server_projects::store::PROJECTS_FILE),
                    wtf16::stringify(&file).unwrap(),
                )
                .unwrap();
            }
            write("account-memory.json", input.get("memory"));
            if input.get("freeChats").is_object()
                || matches!(input.get("freeChats"), JValue::Arr(_))
            {
                let list = wtf16::stringify(input.get("freeChats")).unwrap();
                std::fs::write(dir.join(server_projects::chats::FREE_CHATS_FILE), list).unwrap();
            }
            let body = wtf16::stringify(input.get("body")).unwrap();
            let flags = Flags {
                chat_framing: input.get("chatFraming").as_bool(),
                brain_context: Some(false),
            };
            // The recorded allToolboxes() (ids and tool names), as the catalogue gives them.
            let boxes: Vec<Toolbox> = match input.get("toolboxes") {
                JValue::Arr(list) => list
                    .iter()
                    .map(|b| Toolbox {
                        id: b.get("id").as_str().unwrap().to_string(),
                        label: String::new(),
                        source: String::new(),
                        in_app: false,
                        server: None,
                        tools: match b.get("tools") {
                            JValue::Arr(names) => names
                                .iter()
                                .map(|n| {
                                    JValue::Obj(vec![
                                        ("type".into(), JValue::from("function")),
                                        (
                                            "function".into(),
                                            JValue::Obj(vec![("name".into(), n.clone())]),
                                        ),
                                    ])
                                })
                                .collect(),
                            _ => Vec::new(),
                        },
                        reads: Vec::new(),
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let tz = input.get("tz").as_str().unwrap();
            let rows = (
                input
                    .get("settings")
                    .get("reasoning_effort_default")
                    .clone(),
                input
                    .get("settings")
                    .get("auto_sampling_presets_enabled")
                    .clone(),
            );
            let verdict = run(
                &dir,
                body.as_bytes(),
                frame,
                tz,
                flags,
                Some(&boxes),
                Some(&rows),
            );
            let name = t.get("name").as_str().unwrap_or_default();
            // The recorded turns made to be skipped (what is not ported yet) say why; all others match.
            let skipped = [
                ("project_files_skipped", "files"),
                ("project_shared_context_skipped", "shared-context"),
                ("project_assets_skipped", "assets"),
                ("free_chat_context_project_skipped", "context-project"),
            ];
            if let Some((_, reason)) = skipped.iter().find(|(n, _)| *n == name) {
                assert_eq!(verdict, Verdict::Skip(reason), "{name}");
                continue;
            }
            assert_eq!(verdict, Verdict::Match, "{name}");
            // Without the catalogue and the settings rows: a history that needs the catalogue is skipped, and
            // a turn whose compared fields are all core's says the settings were not read.
            let without = run(&dir, body.as_bytes(), frame, tz, flags, None, None);
            assert!(
                matches!(without, Verdict::Skip("tool-history" | "settings")),
                "{}: {}",
                t.get("name").as_str().unwrap_or_default(),
                without.line()
            );
            if verdict == Verdict::Match {
                matched += 1;
                let system = wtf16::stringify(frame.get("messages")).unwrap_or_default();
                framed += usize::from(system.contains("Chat frame (set by the user)"));
            }
        }
        let _ = std::fs::remove_dir_all(&base);
        assert!(
            matched >= 150 && framed >= 5,
            "{matched} matched, {framed} framed"
        );
    }
}
