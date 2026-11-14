//! Node shims and route tables (council p2, pool item 8): what is left of core's ownership refusal stubs once the Rust
//! front pins a route natively.
//!
//! Core's `server/index.cjs` runs about 65 refusal checks before any of its own routes (`require('./rust-X.cjs')
//! .refuseOwned(req, res, …)`). Each answers a fixed 503 (or 405) for a request the Rust front owns, so a stale route
//! table or a direct loopback call can never make two processes answer the same route. Once the front answers every
//! request a stub refuses, that stub is unreachable through the front: it can only fire for a bypass of the front,
//! which the supervisor's pairing check already prevents. This crate holds:
//!
//! * the recording of every stub (`fixtures/node-refusal-stubs.json`, made by `tools/gen-node-refusal-stubs.cjs` from
//!   a clean core export: every routes.toml path, parameters filled, under all seven methods, with and without a
//!   trailing slash, and with each switch the stub reads set);
//! * [`classify`]: given what the front answers natively (the caller passes the front's own dispatch), a verdict per
//!   stub: delete the file, delete only its refusal function, keep (switch-gated, or still reached = [`Verdict::Live`]);
//! * [`PinTable`]: the exact Node reply (status, headers, body bytes) for each refusal the front still forwards, so a
//!   later front step can answer those natively and make the stub dead too. Nothing calls it in production yet;
//! * [`delete_list`]: the safe delete list as Markdown, with the pairing lines in noevia's supervisor and assembler a
//!   deletion must change in the same step.
//!
//! Council p5 (sbstndalton/noevia#2700, #2713) added what a deletion train needs before it trusts a verdict: each
//! stub's exported `OWNED_ROUTES` (an owned route without a recorded refusal is [`Verdict::Incomplete`]), the core
//! lines outside the stubs that still name a refused route ([`Verdict::GuardsHandler`]: a direct request to Node
//! would reach that handler once the stub is gone), any switch the file names keeps it ([`Verdict::SwitchGated`]), and
//! [`Manifest`]: the front's own `noevia-server --owned-routes` list as a [`Front`], so the verdicts can be recomputed
//! from the binary a train ships instead of from a test inside noevia-server.
//!
//! It has no I/O and no production caller: an inert leaf crate.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// The recording, compiled in.
pub const FIXTURE: &str = include_str!("../fixtures/node-refusal-stubs.json");

/// The seven methods every probe was asked with (core's request chain sees no others from the front).
pub const METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// The whole recording.
#[derive(Debug, Clone, Deserialize)]
pub struct Recording {
    pub generator: String,
    /// The core commit the stubs were read from.
    pub core: String,
    /// The noevia commit the pairing references were read from.
    #[serde(default)]
    pub noevia: String,
    pub methods: Vec<String>,
    /// SHA-256 of the noevia-rs contracts/http/routes.toml the probes were made from.
    #[serde(default)]
    pub routes_toml_sha256: Option<String>,
    /// The ids every route parameter was filled with (a plain one and a UUID).
    #[serde(default)]
    pub fills: Vec<String>,
    /// How many distinct paths were probed.
    pub probes: usize,
    pub stubs: Vec<Stub>,
}

/// One refusal check in core's request chain.
#[derive(Debug, Clone, Deserialize)]
pub struct Stub {
    /// Repository path in noevia-core, e.g. `server/rust-chat-search.cjs`.
    pub file: String,
    /// The exported function index.cjs calls (`refuseOwned`, `refuseOwnedRoute`, …).
    #[serde(rename = "fn")]
    pub function: String,
    /// The `(req, res, pathname, json)` shape inside handleRequestScoped rather than `(req, res, { … })`.
    #[serde(default)]
    pub pathname_arg: bool,
    pub lines: usize,
    /// Deployment switches the file names.
    #[serde(default)]
    pub env: Vec<String>,
    /// Every core file that names this one (`./rust-x.cjs`), tests included.
    #[serde(default)]
    pub importers: Vec<String>,
    /// Other core files that name the file in a quoted string (manifests, allowlists, preload lists, generators): a
    /// non-test one keeps the file.
    #[serde(default)]
    pub string_refs: Vec<String>,
    /// Other core files that name it only in a comment or prose: a deletion edits them, they keep nothing alive.
    #[serde(default)]
    pub mentions: Vec<String>,
    /// What index.cjs takes from the file (`require('./x.cjs').name`, destructured names); the refusal function and
    /// anything else index.cjs still calls.
    #[serde(default)]
    pub index_uses: Vec<String>,
    /// `file:line` in noevia's build/web-supervisor.sh and deploy/tools/assemble-release.sh naming this file.
    #[serde(default)]
    pub pairing: Vec<String>,
    /// What it refuses with every switch unset.
    #[serde(default)]
    pub refusals: Vec<Refusal>,
    /// Recordings under a switch value that changed the answer.
    #[serde(default)]
    pub switched: Vec<Switched>,
    /// The stub's exported `OWNED_ROUTES` (`ALL /p` or `METHOD /p`, routes.toml spelling); `None` when it exports none.
    #[serde(default)]
    pub owned_routes: Option<Vec<String>>,
    /// Per refused routes.toml pattern, the `file:line`s outside the stubs in core's server/ that still name it (a
    /// handler or matcher). Over-reported on purpose: the line only has to contain the pattern's literal pieces.
    #[serde(default)]
    pub behind: BTreeMap<String, Vec<String>>,
    /// The recorder could not load the file.
    #[serde(default)]
    pub error: Option<String>,
}

/// One refused request.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Refusal {
    pub method: String,
    /// The concrete probe path.
    pub path: String,
    /// The routes.toml pattern the probe was made from (`None` for the extra page paths).
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The body as Node wrote it (JSON.stringify bytes).
    #[serde(default)]
    pub body_text: Option<String>,
    /// The same body parsed, for reading.
    #[serde(default)]
    pub body: serde_json::Value,
    /// The stub threw or answered asynchronously.
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Switched {
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub refusals: Vec<Refusal>,
}

/// Parses the compiled-in recording.
pub fn embedded() -> Result<Recording, serde_json::Error> {
    serde_json::from_str(FIXTURE)
}

/// Whether the Rust front answers `method path` itself (with every deployment switch off), rather than forwarding it
/// to Node. The caller supplies the front's own decision; this crate never re-implements its route matcher.
pub trait Front {
    fn answers(&self, method: &str, path: &str) -> bool;
}

impl<F: Fn(&str, &str) -> bool> Front for F {
    fn answers(&self, method: &str, path: &str) -> bool {
        self(method, path)
    }
}

/// What may happen to one stub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// A dedicated `rust-*.cjs` stub, no switch, every refusal answered by the front, and no production file but
    /// index.cjs imports it: the file, its index.cjs call, its tests and its pairing lines can go together.
    DeleteFile,
    /// Every refusal is answered by the front, but other production code imports the file (shared helpers or route
    /// sets) or it is a function inside a live file: only the refusal function and its index.cjs call can go.
    DeleteFunction { other_importers: Vec<String> },
    /// It reads a deployment switch: what it refuses depends on the deployment. Keep until the switch is retired.
    SwitchGated,
    /// The front forwards some refused request to Node, so the stub still answers in production. Keep; the leaks are
    /// the pin candidates ([`PinTable`]).
    Live { leaks: Vec<(String, String)> },
    /// Nothing recorded (the recorder failed or the stub refuses nothing it could ask). Keep.
    Unrecorded,
    /// The stub's `OWNED_ROUTES` names routes it was never seen refusing (a parameter shape or method the probes
    /// missed): the recording is partial for this stub. Keep until it is recorded.
    Incomplete { missing: Vec<String> },
    /// Every refusal is answered by the front, but core still has a handler (or matcher) for a refused route outside
    /// the stubs: deleting the stub alone would let a direct request to Node's port run it. Delete the handler first
    /// (or in the same step), then re-record.
    GuardsHandler { handlers: Vec<String> },
}

impl Verdict {
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::DeleteFile => "delete file",
            Verdict::DeleteFunction { .. } => "delete function",
            Verdict::SwitchGated => "keep (switch)",
            Verdict::Live { .. } => "keep (live)",
            Verdict::Unrecorded => "keep (unrecorded)",
            Verdict::Incomplete { .. } => "keep (partly recorded)",
            Verdict::GuardsHandler { .. } => "delete with handler",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Assessment {
    pub file: String,
    pub function: String,
    pub lines: usize,
    pub refusals: usize,
    pub pairing: Vec<String>,
    /// Comment or prose mentions a deletion edits.
    pub mentions: Vec<String>,
    pub verdict: Verdict,
}

fn is_test_file(f: &str) -> bool {
    f.ends_with(".test.cjs")
        || f.starts_with("tests/")
        || f.contains("/fixtures/")
        || f.ends_with(".probe.cjs")
}

/// Documentation and change notes: naming a file there does not keep it alive.
fn is_doc(f: &str) -> bool {
    f.ends_with(".md") || f.starts_with("docs/")
}

/// Production files other than the stub itself that need it once its refusal function is gone: every other
/// non-test importer, and index.cjs when it uses anything of the file besides the refusal function.
pub fn other_importers(stub: &Stub) -> Vec<String> {
    let mut out: Vec<String> = stub
        .importers
        .iter()
        .chain(stub.string_refs.iter().filter(|f| !is_doc(f)))
        .filter(|f| **f != stub.file && f.as_str() != "server/index.cjs" && !is_test_file(f))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    let extra: Vec<&str> = stub
        .index_uses
        .iter()
        .map(String::as_str)
        .filter(|u| *u != stub.function)
        .collect();
    if !extra.is_empty() {
        out.insert(0, format!("server/index.cjs ({})", extra.join(", ")));
    }
    out
}

/// A file whose only job is refusals: core names them `server/rust-*.cjs`. features.cjs or routes/*.cjs are live
/// files that happen to hold a refusal function.
pub fn is_dedicated(stub: &Stub) -> bool {
    stub.file.starts_with("server/rust-")
}

/// The refused requests the front still forwards to Node.
pub fn leaks(stub: &Stub, front: &dyn Front) -> Vec<(String, String)> {
    stub.refusals
        .iter()
        .filter(|r| r.error.is_none() && !front.answers(&r.method, &r.path))
        .map(|r| (r.method.clone(), r.path.clone()))
        .collect()
}

/// A pattern in the assembler's normal form: every `{…}` is `{}` (deploy/tools/assemble-release.sh
/// `project_routes_core`, `sed 's/{[^}]*}/{}/g'`), so two spellings of one parameter compare equal.
pub fn normalised(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut rest = pattern;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        match rest[start..].find('}') {
            Some(len) => {
                out.push_str("{}");
                rest = &rest[start + len + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// One `OWNED_ROUTES` entry as `(method or "ALL", normalised pattern)`; `None` for a malformed entry.
pub fn owned_entry(entry: &str) -> Option<(&str, String)> {
    let (method, pattern) = entry.split_once(' ')?;
    (pattern.starts_with('/') && !method.is_empty()).then(|| (method, normalised(pattern)))
}

/// The stub's `OWNED_ROUTES` entries it was never recorded refusing: `METHOD /p` needs a refusal of that method on a
/// probe of `/p`, `ALL /p` one of every method. A malformed entry is missing too.
pub fn unrecorded_owned(stub: &Stub) -> Vec<String> {
    let Some(owned) = &stub.owned_routes else {
        return Vec::new();
    };
    let refused: BTreeSet<(&str, String)> = stub
        .refusals
        .iter()
        .filter(|r| r.error.is_none())
        .filter_map(|r| Some((r.method.as_str(), normalised(r.pattern.as_deref()?))))
        .collect();
    owned
        .iter()
        .filter(|entry| match owned_entry(entry) {
            None => true,
            Some(("ALL", p)) => !METHODS.iter().all(|m| refused.contains(&(*m, p.clone()))),
            Some((m, p)) => !refused.contains(&(m, p)),
        })
        .cloned()
        .collect()
}

/// The core lines behind the stub's refused routes (see [`Stub::behind`]), deduplicated and sorted.
pub fn handlers_behind(stub: &Stub) -> Vec<String> {
    let set: BTreeSet<&String> = stub.behind.values().flatten().collect();
    set.into_iter().cloned().collect()
}

/// One verdict per stub, in core's request-chain order.
pub fn classify(rec: &Recording, front: &dyn Front) -> Vec<Assessment> {
    rec.stubs
        .iter()
        .map(|s| {
            let verdict = if s.error.is_some() || s.refusals.iter().any(|r| r.error.is_some()) {
                Verdict::Unrecorded
            } else if !s.switched.is_empty() || !s.env.is_empty() {
                // sbstndalton/noevia#2713 item 3: a named switch keeps the stub, whether or not the recorded values
                // changed its answer (the PinTable skips the same stubs).
                Verdict::SwitchGated
            } else if s.refusals.is_empty() {
                Verdict::Unrecorded
            } else {
                let leaks = leaks(s, front);
                let others = other_importers(s);
                let missing = unrecorded_owned(s);
                let handlers = handlers_behind(s);
                if !missing.is_empty() {
                    Verdict::Incomplete { missing }
                } else if !leaks.is_empty() {
                    Verdict::Live { leaks }
                } else if !handlers.is_empty() {
                    Verdict::GuardsHandler { handlers }
                } else if is_dedicated(s) && others.is_empty() {
                    Verdict::DeleteFile
                } else {
                    Verdict::DeleteFunction {
                        other_importers: others,
                    }
                }
            };
            Assessment {
                file: s.file.clone(),
                function: s.function.clone(),
                lines: s.lines,
                refusals: s.refusals.len(),
                pairing: s.pairing.clone(),
                mentions: s.mentions.clone(),
                verdict,
            }
        })
        .collect()
}

/// A reply as the stub wrote it. Node's response also carries `publicHeaders(res)` (CSP, HSTS, X-Frame-Options, …)
/// and `X-Noevia-API: 1` on `/api` paths, set outside the stub and not recorded here (sbstndalton/noevia#2713 item
/// 5): a step that pins replies from this table must add the front's own public headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    /// Lower-case names, sorted.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Where a refusal applies: method, routes.toml pattern, and whether the path had a trailing slash.
pub type PinKey = (String, String, bool);

/// The Node refusal replies for the requests the front still forwards only to be refused, keyed by method,
/// routes.toml pattern and trailing slash. A front that answered these itself (behind its own switch) would make the
/// remaining refusal stubs dead. Switch-gated stubs are left out: their answer depends on the deployment.
#[derive(Debug, Clone, Default)]
pub struct PinTable {
    pins: BTreeMap<PinKey, Reply>,
    /// Keys two stubs (or two probes of one pattern) answered differently: never pinned.
    conflicts: BTreeSet<PinKey>,
}

impl PinTable {
    /// Built from what the first matching stub in core's order answers for each forwarded request.
    pub fn leaked(rec: &Recording, front: &dyn Front) -> PinTable {
        let mut table = PinTable::default();
        let mut taken: BTreeSet<(String, String)> = BTreeSet::new();
        for s in &rec.stubs {
            if s.error.is_some() || !s.switched.is_empty() || !s.env.is_empty() {
                continue;
            }
            for r in &s.refusals {
                if r.error.is_some() || front.answers(&r.method, &r.path) {
                    continue;
                }
                // Core's chain stops at the first stub that refuses: a later stub never sees this request.
                if !taken.insert((r.method.clone(), r.path.clone())) {
                    continue;
                }
                let Some(pattern) = r.pattern.clone() else {
                    continue;
                };
                let key = (
                    r.method.clone(),
                    pattern,
                    r.path.len() > 1 && r.path.ends_with('/'),
                );
                let reply = reply_of(r);
                if table.conflicts.contains(&key) {
                    continue;
                }
                match table.pins.get(&key) {
                    Some(have) if *have != reply => {
                        table.pins.remove(&key);
                        table.conflicts.insert(key);
                    }
                    Some(_) => {}
                    None => {
                        table.pins.insert(key, reply);
                    }
                }
            }
        }
        table
    }

    pub fn lookup(&self, method: &str, pattern: &str, trailing_slash: bool) -> Option<&Reply> {
        self.pins
            .get(&(method.to_string(), pattern.to_string(), trailing_slash))
    }

    pub fn len(&self) -> usize {
        self.pins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }

    pub fn keys(&self) -> impl Iterator<Item = &PinKey> {
        self.pins.keys()
    }

    pub fn conflicts(&self) -> &BTreeSet<PinKey> {
        &self.conflicts
    }
}

/// The reply bytes of one recorded refusal. Without the raw text (older recordings) the parsed body is written back
/// with serde_json, whose key order can differ from JSON.stringify's: [`Refusal::body_text`] is the exact one.
pub fn reply_of(r: &Refusal) -> Reply {
    let body = match &r.body_text {
        Some(t) => t.as_bytes().to_vec(),
        None => serde_json::to_vec(&r.body).unwrap_or_default(),
    };
    Reply {
        status: r.status,
        headers: r
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        body,
    }
}

/// Totals for the report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub delete_file: usize,
    pub delete_file_lines: usize,
    pub delete_function: usize,
    /// Deletable once the named core handler goes too ([`Verdict::GuardsHandler`]).
    pub with_handler: usize,
    pub keep: usize,
}

pub fn totals(list: &[Assessment]) -> Totals {
    let mut t = Totals::default();
    for a in list {
        match a.verdict {
            Verdict::DeleteFile => {
                t.delete_file += 1;
                t.delete_file_lines += a.lines;
            }
            Verdict::DeleteFunction { .. } => t.delete_function += 1,
            Verdict::GuardsHandler { .. } => t.with_handler += 1,
            _ => t.keep += 1,
        }
    }
    t
}

/// The safe delete list as Markdown.
pub fn delete_list(rec: &Recording, list: &[Assessment]) -> String {
    let t = totals(list);
    let mut out = String::new();
    let _ = writeln!(out, "# Node refusal stubs: safe delete list");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Recorded from noevia-core `{}` (pairing lines from noevia `{}`, routes.toml sha256 `{}`) by `{}`; verdicts from \
         the front's own dispatch with every switch off (`crates/route-pins`). {} stubs: {} files can go ({} lines), {} \
         refusal functions can go from files that stay, {} can go only with the core handler they guard, {} stay.",
        short(&rec.core),
        short(&rec.noevia),
        short(rec.routes_toml_sha256.as_deref().unwrap_or("unknown")),
        rec.generator,
        list.len(),
        t.delete_file,
        t.delete_file_lines,
        t.delete_function,
        t.with_handler,
        t.keep
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "A deletion removes the file (or function), its `index.cjs` call, the core tests that only test the refusal, and \
         changes the listed supervisor/assembler pairing lines in the same train (they read the stub's marker or route \
         set). Delete nothing whose verdict is keep."
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| Stub | Function | Lines | Refusals | Verdict | Pairing lines / why |"
    );
    let _ = writeln!(out, "|---|---|---:|---:|---|---|");
    for a in list {
        let why = match &a.verdict {
            Verdict::DeleteFile if a.mentions.is_empty() => {
                format!("pairing: {}", none_or(&a.pairing))
            }
            Verdict::DeleteFile => {
                format!(
                    "pairing: {}; comments in: {}",
                    none_or(&a.pairing),
                    a.mentions.join(", ")
                )
            }
            Verdict::DeleteFunction { other_importers } if other_importers.is_empty() => {
                format!("live file; pairing: {}", none_or(&a.pairing))
            }
            Verdict::DeleteFunction { other_importers } => {
                format!(
                    "imported by {}; pairing: {}",
                    other_importers.join(", "),
                    none_or(&a.pairing)
                )
            }
            Verdict::SwitchGated => "reads a deployment switch".to_string(),
            Verdict::Live { leaks } => format!(
                "front forwards {}: {}",
                leaks.len(),
                leaks
                    .iter()
                    .take(4)
                    .map(|(m, p)| format!("{m} {p}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Verdict::Unrecorded => "nothing recorded".to_string(),
            Verdict::Incomplete { missing } => {
                format!("OWNED_ROUTES not recorded: {}", missing.join(", "))
            }
            Verdict::GuardsHandler { handlers } => format!(
                "core still names the route at {}; pairing: {}",
                handlers.join(", "),
                none_or(&a.pairing)
            ),
        };
        let _ = writeln!(
            out,
            "| `{}` | `{}` | {} | {} | {} | {} |",
            a.file,
            a.function,
            a.lines,
            a.refusals,
            a.verdict.label(),
            why
        );
    }
    out
}

/// The front's owned-route manifest (`noevia-server --owned-routes`: one `METHOD pattern` per line), as a [`Front`]
/// over a recording: a probe is answered when the front lists its method on the probe's routes.toml pattern. Trailing
/// slash probes are never answered (the manifest lists patterns, not the matcher's slash tolerance), so they show as
/// [`Verdict::Live`] leaks: the conservative direction.
#[derive(Debug, Clone, Default)]
pub struct Manifest {
    pairs: BTreeSet<(String, String)>,
}

impl Manifest {
    /// Parses the manifest text; a line that is not `METHOD /pattern` is an error (a truncated or foreign file must
    /// not read as "the front owns nothing").
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let mut pairs = BTreeSet::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            match line.split_once(' ') {
                Some((m, p))
                    if METHODS.contains(&m) && p.starts_with('/') && !p.contains(char::is_whitespace) =>
                {
                    pairs.insert((m.to_string(), normalised(p)));
                }
                _ => return Err(format!("line {}: not `METHOD /pattern`: {line:?}", i + 1)),
            }
        }
        if pairs.is_empty() {
            return Err("empty manifest".to_string());
        }
        Ok(Manifest { pairs })
    }

    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// Whether the front lists `method` on `pattern` (any parameter spelling).
    pub fn owns(&self, method: &str, pattern: &str) -> bool {
        self.pairs
            .contains(&(method.to_string(), normalised(pattern)))
    }

    /// This manifest as a [`Front`] for `rec`'s probes.
    pub fn front<'a>(&'a self, rec: &'a Recording) -> ManifestFront<'a> {
        let mut pattern_of = BTreeMap::new();
        for s in &rec.stubs {
            for r in s
                .refusals
                .iter()
                .chain(s.switched.iter().flat_map(|w| w.refusals.iter()))
            {
                if let Some(p) = &r.pattern {
                    pattern_of.entry(r.path.clone()).or_insert_with(|| p.clone());
                }
            }
        }
        ManifestFront {
            manifest: self,
            pattern_of,
        }
    }

    /// `OWNED_ROUTES` entries (`stub file`, entry) the front does not list: a stub refusing a route nobody answers.
    /// `ALL /p` needs every method listed.
    pub fn unowned_claims(&self, rec: &Recording) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for s in &rec.stubs {
            for entry in s.owned_routes.iter().flatten() {
                let owned = match owned_entry(entry) {
                    None => false,
                    Some(("ALL", p)) => METHODS.iter().all(|m| self.owns(m, &p)),
                    Some((m, p)) => self.owns(m, &p),
                };
                if !owned {
                    out.push((s.file.clone(), entry.clone()));
                }
            }
        }
        out
    }
}

/// [`Manifest::front`].
#[derive(Debug, Clone)]
pub struct ManifestFront<'a> {
    manifest: &'a Manifest,
    pattern_of: BTreeMap<String, String>,
}

impl Front for ManifestFront<'_> {
    fn answers(&self, method: &str, path: &str) -> bool {
        if path.len() > 1 && path.ends_with('/') {
            return false;
        }
        self.pattern_of
            .get(path)
            .is_some_and(|p| self.manifest.owns(method, p))
    }
}

fn none_or(v: &[String]) -> String {
    if v.is_empty() {
        "none".to_string()
    } else {
        v.join(", ")
    }
}

fn short(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

#[cfg(test)]
mod tests;
