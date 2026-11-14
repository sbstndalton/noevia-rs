//! Track R14 step 7: core's toolbox chat facts (toolbox-chat-facts.cjs `buildFacts`, published as
//! `toolbox-chat-facts.json`) built by this process, for the toolbox catalogue [`crate::mcp_discovery::read`]
//! composes (the permitted view, the connectors page, the admin directory listing, the project catalogue).
//!
//! Behind `NOEVIA_RUST_CHAT_FACTS=1` (read once; unset or any other value: core's file, as today). On,
//! the catalogue no longer needs core to publish: the built-in boxes are core's own definitions
//! (contracts/builtin-toolboxes.json, written by tools/gen-chat-facts-fixtures.cjs from core's
//! toolboxes.cjs, kiwix.cjs and gdrive-tools.cjs), offered as core index.cjs offers them:
//! - `core` always; `offline-wikipedia` when the `kiwix` feature is on and `KIWIX_URL` is a plain
//!   http(s) origin; `gdrive` always (a connector box, kept whatever `ENABLED_TOOLBOXES` says); each other
//!   box only when `ENABLED_TOOLBOXES` offers it (mcp-servers `toolboxOffered`, the discovery handle's);
//! - `defaults` `["core"]`, `connectorBoxes` `["gdrive"]`;
//! - `harnessEnabled`: the `codeHarness` feature now (features.cjs `enabled`, code_routes.rs `feature`);
//! - `hasRepositories`: `CODE_REPOS` names a usable repository (code-harness `parse_repos`, as core's code
//!   service parses it once at its start: here once, at the first build).
//!
//! The picker (`GET /api/toolboxes`) keeps core's file whatever the switch says: its token estimates are
//! built from core's prefill measurements, which only the chat process has (prefill.cjs, fed by the chat
//! stream). The facts built here carry an empty measurement set.
//!
//! The `kiwix` flag is a construction flag (it needs a restart): core reads `selected().value` once at
//! its start. This process takes that very value from core's feature registration (boot facts), so a
//! setting changed since core's start shows nowhere until core restarts; while no registration can be
//! read it keeps the last value read (none yet: not ready), and with no feature context at all (no
//! core registers here) it reads the setting's selected value at each build. Named difference: a `KIWIX_URL` core cannot parse stops core's start;
//! here it only means no Offline Wikipedia box.
//! `KIWIX_URL` and `CODE_REPOS` are this process's environment and filesystem, the same container as
//! core's.
//!
//! Recorded proof: tests/fixtures/chat-facts-node.v1.json (12 configurations through core's own boxes and
//! buildFacts); the test module below replays them through [`build`].
use crate::App;
use js_json::{wtf16, JValue};
use std::sync::{Arc, OnceLock};

/// core contracts copy: the three built-in boxes, in core's offer order.
const BOXES_JSON: &str = include_str!("../../../contracts/builtin-toolboxes.json");
/// toolboxes.cjs `TOOL_PREFILL_TARGET_MS`.
const PREFILL_TARGET_MS: f64 = 14000.0;
const CORE: &str = "core";
const KIWIX: &str = "offline-wikipedia";
/// toolboxes.cjs `CONNECTOR_BOXES`.
const CONNECTORS: [&str; 1] = ["gdrive"];

/// `NOEVIA_RUST_CHAT_FACTS=1`, read once.
pub fn native() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("NOEVIA_RUST_CHAT_FACTS").is_ok_and(|v| v == "1"))
}

/// The built-in boxes of the contract file (empty if it cannot be read, which its test rules out).
fn boxes() -> &'static [JValue] {
    static BOXES: OnceLock<Vec<JValue>> = OnceLock::new();
    BOXES.get_or_init(|| match wtf16::parse(BOXES_JSON) {
        Ok(doc) => match doc.get("boxes") {
            JValue::Arr(items) => items.clone(),
            _ => Vec::new(),
        },
        Err(_) => Vec::new(),
    })
}

/// What decides the facts, read from the deployment.
pub struct Inputs<'a> {
    /// The Offline Wikipedia box exists (core: `featureConstruction.kiwix && KIWIX_URL`, and the URL
    /// accepted by createKiwixTools).
    pub kiwix: bool,
    /// `toolboxOffered(id)` over `ENABLED_TOOLBOXES`.
    pub offered: &'a dyn Fn(&str) -> bool,
    pub harness_enabled: bool,
    pub has_repositories: bool,
}

/// buildFacts over core's built-in boxes as index.cjs wires them (without core's `version`/`kind`
/// header, which only the file carries).
pub fn build(inputs: &Inputs<'_>) -> JValue {
    let builtin: Vec<JValue> = boxes()
        .iter()
        .filter(|b| {
            let id = b.get("id").as_str().unwrap_or_default();
            (id != KIWIX || inputs.kiwix) && (CONNECTORS.contains(&id) || (inputs.offered)(id))
        })
        .cloned()
        .collect();
    let strs =
        |items: &[&str]| JValue::Arr(items.iter().map(|s| JValue::Str((*s).into())).collect());
    JValue::obj([
        ("builtin", JValue::Arr(builtin)),
        ("defaults", strs(&[CORE])),
        ("connectorBoxes", strs(&CONNECTORS)),
        ("harnessEnabled", JValue::Bool(inputs.harness_enabled)),
        ("hasRepositories", JValue::Bool(inputs.has_repositories)),
        (
            "prefill",
            JValue::obj([
                ("targetMs", JValue::Num(PREFILL_TARGET_MS)),
                ("models", JValue::Obj(Vec::new())),
            ]),
        ),
    ])
}

/// kiwix.cjs createKiwixTools's check: `new URL(KIWIX_URL)` is http(s), without user, password or query.
pub fn kiwix_url_ok(raw: &str) -> bool {
    match url::Url::parse(raw) {
        Ok(u) => {
            matches!(u.scheme(), "http" | "https")
                && u.username().is_empty()
                && u.password().is_none()
                && u.query().is_none_or(str::is_empty)
        }
        Err(_) => false,
    }
}

/// features.cjs `selected(name).value`: the value core reads once at its start for a construction flag
/// (`featureConstruction.kiwix` = `state.get('kiwix').boot`): the env override, else the typed admin
/// setting (or its declared legacy row), else the default. Availability does not enter it. `None`: the
/// setting could not be read (core would not start then).
fn selected_value(app: &App, feature: server_features::Feature) -> Option<bool> {
    let env = server_features::Feature::ALL
        .iter()
        .position(|f| *f == feature)
        .and_then(|i| app.config.feature_context_env.get(i).copied().flatten());
    let stored = match env {
        Some(_) => None,
        None => app
            .writes
            .writer()
            .ok()?
            .read(|r| server_account::feature_context::startup::selected(r, feature))
            .ok()?,
    };
    let text = stored.map(|on| if on { "true" } else { "false" });
    Some(
        server_features::evaluate_with_context(
            feature,
            env,
            text,
            server_features::Availability::Unknown,
            None,
        )
        .value,
    )
}

/// core's own `kiwix` construction value, as core registered it at its start (the feature context's
/// boot facts, `restart[1]`; crates/server-account features.rs reads it the same way), with the
/// controller it was read from. `None` while no registration can be read.
fn registered_kiwix(
    app: &App,
    controller: &Arc<server_account::feature_context::Controller>,
) -> Option<bool> {
    let view = app
        .writes
        .writer()
        .ok()?
        .read(|r| controller.view(r))
        .ok()?;
    view.facts.restart.get(1).copied()
}

/// Whether the Offline Wikipedia box exists here: the `kiwix` construction value and a `KIWIX_URL`
/// createKiwixTools accepts.
/// - With a feature context (core registers with this process): core's registered value; while no
///   registration can be read (before core's first one, a lease that expired or was released, a
///   settings edit before core's next renewal) the last value read from this controller; with none
///   read yet, not ready (`None`), so a setting changed since core's start never shows early.
/// - Without one (no core registers here): the setting's selected value.
fn kiwix_box(app: &App) -> Option<bool> {
    // The last registered value, per controller (one per App).
    static LAST: std::sync::Mutex<Option<(usize, bool)>> = std::sync::Mutex::new(None);
    let url = std::env::var("KIWIX_URL").unwrap_or_default();
    if url.is_empty() || !kiwix_url_ok(&url) {
        return Some(false);
    }
    let Some(layer) = app.feature_context.as_ref() else {
        return selected_value(app, server_features::Feature::Kiwix);
    };
    let controller = layer.controller();
    let key = Arc::as_ptr(&controller) as usize;
    if let Some(v) = registered_kiwix(app, &controller) {
        if let Ok(mut last) = LAST.lock() {
            *last = Some((key, v));
        }
        return Some(v);
    }
    LAST.lock()
        .ok()
        .and_then(|l| *l)
        .filter(|(k, _)| *k == key)
        .map(|(_, v)| v)
}

/// The facts for this deployment now. `None` while MCP discovery is not configured here (the caller
/// then has no catalogue either), or while the `kiwix` value cannot be read (not ready: the caller
/// answers "being prepared"; a later build reads it again).
pub async fn facts(app: &Arc<App>) -> Option<Arc<JValue>> {
    let handle = crate::mcp_discovery_backend::handle(app)?.clone();
    let app = Arc::clone(app);
    tokio::task::spawn_blocking(move || {
        // CODE_REPOS read once, as core's code service parses it once at its start.
        static REPOS: OnceLock<bool> = OnceLock::new();
        let kiwix = kiwix_box(&app)?;
        let has_repositories = *REPOS
            .get_or_init(|| !code_harness::service::parse_repos(&app.config.code.repos).is_empty());
        let harness_enabled =
            crate::code_routes::feature(&app, server_features::Feature::CodeHarness);
        Some(Arc::new(build(&Inputs {
            kiwix,
            offered: &|id| handle.offered(id),
            harness_enabled,
            has_repositories,
        })))
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    fn fixture() -> JValue {
        wtf16::parse(include_str!("../tests/fixtures/chat-facts-node.v1.json")).unwrap()
    }

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// Every recorded configuration: the facts core's buildFacts published, built here from the same
    /// inputs (the kiwix box's existence decided by the recorded feature and URL as core decides it).
    #[test]
    fn replays_core_facts() {
        let doc = fixture();
        let JValue::Arr(cases) = doc.get("cases") else {
            panic!("no cases")
        };
        assert!(cases.len() >= 12);
        for case in cases {
            let name = case.get("name").as_str().unwrap();
            let enabled = case
                .get("enabled")
                .as_str()
                .and_then(|e| mcp_servers::parse_toolboxes(Some(&units(e))));
            let offered = |id: &str| mcp_servers::toolbox_offered(enabled.as_deref(), &units(id));
            let url = case.get("kiwixUrl").as_str().unwrap_or_default();
            let kiwix =
                case.get("kiwix").as_bool().unwrap() && !url.is_empty() && kiwix_url_ok(url);
            let got = build(&Inputs {
                kiwix,
                offered: &offered,
                harness_enabled: case.get("harness").as_bool().unwrap(),
                has_repositories: case.get("repos").as_bool().unwrap(),
            });
            assert_eq!(
                wtf16::stringify(&got).unwrap(),
                wtf16::stringify(case.get("facts")).unwrap(),
                "{name}"
            );
        }
    }

    /// The contract file is what the recorder wrote: core's three boxes, in order.
    #[test]
    fn contract_boxes_are_cores() {
        let ids: Vec<&str> = boxes()
            .iter()
            .map(|b| b.get("id").as_str().unwrap())
            .collect();
        assert_eq!(ids, ["core", "offline-wikipedia", "gdrive"]);
        let doc = fixture();
        let JValue::Arr(cases) = doc.get("cases") else {
            panic!("no cases")
        };
        let all = cases
            .iter()
            .find(|c| c.get("name").as_str() == Some("kiwix-on"))
            .unwrap();
        assert_eq!(
            wtf16::stringify(&JValue::Arr(boxes().to_vec())).unwrap(),
            wtf16::stringify(all.get("facts").get("builtin")).unwrap()
        );
    }

    /// createKiwixTools's URL check.
    #[test]
    fn kiwix_url_check() {
        for ok in [
            "http://qa-kiwix.invalid",
            "https://qa-kiwix.invalid:8443/wiki/",
            "http://10.0.0.5:8080?",
        ] {
            assert!(kiwix_url_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "qa-kiwix.invalid",
            "ftp://qa-kiwix.invalid",
            "http://user@qa-kiwix.invalid",
            "http://user:pw@qa-kiwix.invalid",
            "http://qa-kiwix.invalid/?x=1",
            "file:///srv/kiwix",
        ] {
            assert!(!kiwix_url_ok(bad), "{bad}");
        }
    }
}
