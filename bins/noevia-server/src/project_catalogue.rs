//! Track R14 step 4: the project catalogue (core server/project-catalogue.cjs) composed by this
//! process instead of read from the `project-catalogue.json` core publishes.
//!
//! What a project create, its configuration, its instruction skills and a source upload validate
//! against ([`Catalogue`]: the toolbox ids a project may select, every toolbox id the server knows,
//! the default selection and the project icon ids) is core index.cjs's `current()` over
//! `allToolboxes()`, written through `catalogueOf`. Its toolbox ids are already this process's:
//! [`mcp_discovery::read`] composes `allToolboxes()` from its own MCP discovery and the chat facts core
//! publishes (built-in boxes, connector boxes, defaults), the same composition the toolbox picker and
//! the permitted view answer from. With `NOEVIA_RUST_PROJECT_CATALOGUE=1` (read once) every reader of
//! the catalogue takes it from there ([`of`]); unset or any other value, the file as today.
//!
//! Not ready: when [`mcp_discovery::read`] cannot prove its inputs current (the chat facts are not
//! published yet, discovery has not answered), the native catalogue is `None`, which each reader
//! answers as it answers a missing file today (a create waits up to its 10 s, then 503 "has not
//! published"). The file is never consulted while the switch is on, so a stale file cannot hide a
//! fault of the native composition. The readers that write a project (create, import, uploads, sync,
//! a research report) wait up to 10 s for it, as a create waited for the file: a first upload made
//! without a catalogue would never get the project documents default. A request that does not read
//! the catalogue (a configuration patch without `icon`/`toolboxes`, a report's target lookup) does
//! not compose it ([`Source::unread`]).
//!
//! Composing touches core's demand marker (`snapshot-demand`), as every reader of the chat facts in
//! this process does: core then publishes its snapshots (chat facts, RAG status, auto roles) for the
//! next minute. A background job or report save can therefore wake that publishing; it is the same
//! cost core's own per-request catalogue publish had, and it ends with the demand.
//!
//! The icon ids are `Object.keys(contracts/project-icons.json)`: the file is core's, copied byte for
//! byte into noevia-rs `contracts/project-icons.json` (its sha256 is checked against the recording).
//! Recorded proof: tests/fixtures/project-catalogue-node.v1.json (tools/gen-project-catalogue-fixtures.cjs
//! over core's toolboxes.cjs, mcp-servers.cjs, toolbox-chat-facts.cjs and project-catalogue.cjs).
use crate::{mcp_discovery, App};
use server_projects::lifecycle::Catalogue;
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// core contracts/project-icons.json, byte for byte.
const ICONS_JSON: &str = include_str!("../../../contracts/project-icons.json");

/// `NOEVIA_RUST_PROJECT_CATALOGUE=1`, read once.
pub fn native() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("NOEVIA_RUST_PROJECT_CATALOGUE").is_ok_and(|v| v == "1"))
}

/// `Object.keys(require('../contracts/project-icons.json'))`.
fn icons() -> &'static [String] {
    static ICONS: OnceLock<Vec<String>> = OnceLock::new();
    ICONS.get_or_init(|| match js_json::wtf16::parse(ICONS_JSON) {
        Ok(js_json::JValue::Obj(items)) => items.into_iter().map(|(k, _)| k).collect(),
        _ => Vec::new(),
    })
}

/// core index.cjs `current()` (`selectable: sanitizeToolboxes(ids) || []`, `known: ids`, `defaults:
/// DEFAULT_TOOLBOXES`, `icons`) over the composed `allToolboxes()`. `sanitizeToolboxes` keeps each id
/// once, in first-seen order, without the connector boxes; `known` keeps every box's id, repeats
/// included.
pub fn of(cat: &mcp_discovery::Catalogue) -> Catalogue {
    let known: Vec<String> = cat.boxes.iter().map(|b| b.id.clone()).collect();
    let connectors: HashSet<&str> = cat.connector_boxes.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    let selectable = known
        .iter()
        .filter(|id| !connectors.contains(id.as_str()) && seen.insert(id.as_str()))
        .cloned()
        .collect();
    Catalogue {
        selectable,
        defaults: cat.defaults.clone(),
        icons: icons().to_vec(),
        known: Some(known),
    }
}

/// Where a request's catalogue comes from, settled before its blocking work starts.
pub enum Source {
    /// core's published `project-catalogue.json` (switch unset).
    File,
    /// This process's composition, `None` when not ready.
    Native(Option<Catalogue>),
}

impl Source {
    /// The catalogue's source for one request; with the switch on, composed now.
    pub async fn get(app: &Arc<App>) -> Source {
        if !native() {
            return Source::File;
        }
        Source::Native(compose(app).await)
    }

    /// [`Source::get`], waiting up to `wait` for a native catalogue that is not ready yet (what
    /// [`Catalogue::read_waiting`] does for the file). Each attempt is cut at the deadline, so the wait
    /// never runs past `wait` by more than a poll.
    pub async fn waiting(app: &Arc<App>, wait: Duration) -> Source {
        if !native() {
            return Source::File;
        }
        let until = Instant::now() + wait;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if let Ok(Some(c)) = tokio::time::timeout(left, compose(app)).await {
                return Source::Native(Some(c));
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Source::Native(None);
            }
            tokio::time::sleep(left.min(Duration::from_millis(200))).await;
        }
    }

    /// The source for a request that does not read the catalogue: the file as today (switch unset;
    /// nothing changes), nothing composed (switch on).
    pub fn unread() -> Source {
        if native() {
            Source::Native(None)
        } else {
            Source::File
        }
    }

    /// The catalogue (blocking: the file is read here).
    pub fn read(&self, data_dir: &Path) -> Option<Catalogue> {
        match self {
            Source::File => Catalogue::read(data_dir),
            Source::Native(c) => c.clone(),
        }
    }

    /// [`Source::read`], the file waited for up to `wait` (a native one was waited for in
    /// [`Source::waiting`]).
    pub fn read_waiting(&self, data_dir: &Path, wait: Duration) -> Option<Catalogue> {
        match self {
            Source::File => Catalogue::read_waiting(data_dir, wait),
            Source::Native(c) => c.clone(),
        }
    }
}

async fn compose(app: &Arc<App>) -> Option<Catalogue> {
    mcp_discovery::read(app).await.map(|c| of(&c))
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
    use crate::mcp_discovery_actor::Snapshot;
    use js_json::{wtf16, JValue};

    fn fixture() -> JValue {
        wtf16::parse(include_str!(
            "../tests/fixtures/project-catalogue-node.v1.json"
        ))
        .unwrap()
    }

    fn strings(v: &JValue) -> Vec<String> {
        match v {
            JValue::Arr(items) => items
                .iter()
                .map(|x| x.as_str().unwrap().to_owned())
                .collect(),
            other => panic!("not a list: {other:?}"),
        }
    }

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// Every recorded case: core's chat facts and discovered boxes, composed as `mcp_discovery::read`
    /// composes them (the operator's ENABLED_TOOLBOXES judged by mcp-servers, as the discovery handle
    /// does), give core's catalogue.
    #[test]
    fn replays_core_catalogue() {
        let doc = fixture();
        let JValue::Arr(cases) = doc.get("cases") else {
            panic!("no cases")
        };
        assert!(cases.len() >= 14);
        for case in cases {
            let name = case.get("name").as_str().unwrap();
            let enabled = case
                .get("enabled")
                .as_str()
                .map(|e| mcp_servers::parse_toolboxes(Some(&units(e))));
            let enabled = enabled.unwrap_or(None);
            let offered = |id: &str| mcp_servers::toolbox_offered(enabled.as_deref(), &units(id));
            let JValue::Arr(boxes) = case.get("mcpBoxes") else {
                panic!("{name}: no boxes")
            };
            let snap = Snapshot {
                generation: 1,
                enabled: true,
                boxes: boxes.clone(),
                ..Snapshot::default()
            };
            let facts = case.get("node").get("facts");
            let composed = mcp_discovery::compose(facts, &snap, &offered, &[])
                .unwrap_or_else(|| panic!("{name}: not composed"));
            let got = of(&composed);
            let want = wtf16::parse(case.get("node").get("catalogue").as_str().unwrap()).unwrap();
            assert_eq!(want.get("version").as_num(), Some(1.0), "{name}");
            assert_eq!(
                got.selectable,
                strings(want.get("selectable")),
                "{name}: selectable"
            );
            assert_eq!(got.known, Some(strings(want.get("known"))), "{name}: known");
            assert_eq!(
                got.defaults,
                strings(want.get("defaults")),
                "{name}: defaults"
            );
            assert_eq!(got.icons, strings(want.get("icons")), "{name}: icons");
        }
    }

    /// The copied icon file is core's, byte for byte, and its keys are the recorded ones in order.
    #[test]
    fn icons_are_cores() {
        use sha2::Digest;
        let doc = fixture();
        let want = doc
            .get("sourcesRead")
            .get("contracts/project-icons.json")
            .as_str()
            .unwrap()
            .to_owned();
        let got: String = sha2::Sha256::digest(ICONS_JSON.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            got, want,
            "contracts/project-icons.json is not core's recorded file"
        );
        assert_eq!(icons(), strings(doc.get("icons")).as_slice());
        assert!(!icons().is_empty());
    }

    /// The file a `Source::File` reads is core's publication, untouched by the native path.
    #[test]
    fn file_source_reads_the_published_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Source::File.read(dir.path()), None);
        std::fs::write(
            dir.path().join("project-catalogue.json"),
            r#"{"version":1,"selectable":["core"],"defaults":["core"],"icons":["folder"],"known":["core","gdrive"]}"#,
        )
        .unwrap();
        let c = Source::File.read(dir.path()).unwrap();
        assert_eq!(c.selectable, vec!["core".to_owned()]);
        assert_eq!(c.known, Some(vec!["core".to_owned(), "gdrive".to_owned()]));
        // A native source never falls back to the file.
        assert_eq!(Source::Native(None).read(dir.path()), None);
        assert_eq!(
            Source::Native(None).read_waiting(dir.path(), Duration::from_millis(1)),
            None
        );
    }
}
