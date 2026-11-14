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
}

impl Verdict {
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::DeleteFile => "delete file",
            Verdict::DeleteFunction { .. } => "delete function",
            Verdict::SwitchGated => "keep (switch)",
            Verdict::Live { .. } => "keep (live)",
            Verdict::Unrecorded => "keep (unrecorded)",
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
    f.ends_with(".test.cjs") || f.starts_with("tests/") || f.contains("/fixtures/") || f.ends_with(".probe.cjs")
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
    let extra: Vec<&str> =
        stub.index_uses.iter().map(String::as_str).filter(|u| *u != stub.function).collect();
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

/// One verdict per stub, in core's request-chain order.
pub fn classify(rec: &Recording, front: &dyn Front) -> Vec<Assessment> {
    rec.stubs
        .iter()
        .map(|s| {
            let verdict = if s.error.is_some() || s.refusals.iter().any(|r| r.error.is_some()) {
                Verdict::Unrecorded
            } else if !s.switched.is_empty() || (s.refusals.is_empty() && !s.env.is_empty()) {
                Verdict::SwitchGated
            } else if s.refusals.is_empty() {
                Verdict::Unrecorded
            } else {
                let leaks = leaks(s, front);
                let others = other_importers(s);
                if !leaks.is_empty() {
                    Verdict::Live { leaks }
                } else if is_dedicated(s) && others.is_empty() {
                    Verdict::DeleteFile
                } else {
                    Verdict::DeleteFunction { other_importers: others }
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

/// A reply exactly as Node wrote it.
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
                let Some(pattern) = r.pattern.clone() else { continue };
                let key = (r.method.clone(), pattern, r.path.len() > 1 && r.path.ends_with('/'));
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
        self.pins.get(&(method.to_string(), pattern.to_string(), trailing_slash))
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
    Reply { status: r.status, headers: r.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect(), body }
}

/// Totals for the report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub delete_file: usize,
    pub delete_file_lines: usize,
    pub delete_function: usize,
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
        "Recorded from noevia-core `{}` (pairing lines from noevia `{}`) by `{}`; verdicts from the front's own dispatch \
         with every switch off (`crates/route-pins`). {} stubs: {} files can go ({} lines), {} refusal functions can go \
         from files that stay, {} stay.",
        short(&rec.core),
        short(&rec.noevia),
        rec.generator,
        list.len(),
        t.delete_file,
        t.delete_file_lines,
        t.delete_function,
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
    let _ = writeln!(out, "| Stub | Function | Lines | Refusals | Verdict | Pairing lines / why |");
    let _ = writeln!(out, "|---|---|---:|---:|---|---|");
    for a in list {
        let why = match &a.verdict {
            Verdict::DeleteFile if a.mentions.is_empty() => format!("pairing: {}", none_or(&a.pairing)),
            Verdict::DeleteFile => {
                format!("pairing: {}; comments in: {}", none_or(&a.pairing), a.mentions.join(", "))
            }
            Verdict::DeleteFunction { other_importers } if other_importers.is_empty() => {
                format!("live file; pairing: {}", none_or(&a.pairing))
            }
            Verdict::DeleteFunction { other_importers } => {
                format!("imported by {}; pairing: {}", other_importers.join(", "), none_or(&a.pairing))
            }
            Verdict::SwitchGated => "reads a deployment switch".to_string(),
            Verdict::Live { leaks } => format!(
                "front forwards {}: {}",
                leaks.len(),
                leaks.iter().take(4).map(|(m, p)| format!("{m} {p}")).collect::<Vec<_>>().join(", ")
            ),
            Verdict::Unrecorded => "nothing recorded".to_string(),
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
