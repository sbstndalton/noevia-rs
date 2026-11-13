//! A turn's image parts and its vision pass (slice P2c): what noevia-core's `server/chat.cjs` does
//! with a project's attached images (`projectImages`, ~866-914 at the pinned core) and, after the
//! stream opened, what it puts on the wire for them or asks a Vision role to describe (~1461-1547,
//! `describeImagesNow` ~783-860). In chat.cjs's order:
//! 1. [`images`], before routing: the attached assets (`body.turnImageIds`, #1512; all without
//!    it) as `image_url` parts, the loaded, missing and unattached names, the `[assets]` lines.
//!    [`Images::reading`] is the status line's `readingImages` ([`crate::reroute::status_event`]).
//! 2. [`Pass`], after the meta event and the status line: the missing-images system message, rule
//!    3 of the external provider, the unattached names, the Code route note (#1604), then (not on
//!    `compactOnly`) the Vision role's description (#1741: on the local default provider, on a
//!    turn that stays on the box; cached in [`Descriptions`]; a miss said on the reply, #1790),
//!    else the answering model's probe: the parts on the last user message, or the blind note.
//!    Every seam is the caller's ([`Seam`], events included), asked in Node's order.
//! 3. [`generating_event`], [`reply_warning`]: the status and warning after it.
//!    [`Seen::image_warning`] is also the prepared frame's `skills.imageWarning`.
//!
//! Preconditions (later slices): multimodal retrieval off (P2b keeps such a turn in Node), no
//! Inference machines, no routing modes (`offBoxTurn` is [`crate::offer::off_box`] without a cloud
//! route).
//!
//! Recorded in `tests/fixtures/node-vision.v1.json`, written by `tests/record_vision.cjs` (whole
//! turns through `createChatHandler`, the seams stubbed). Named differences, in the fixture's
//! `differences` and tested:
//! - An asset id that is not a plain file name ([`plain_asset_name`]) is a missing image and is
//!   never read; Node reads `path.join(assetDir, id)`, which for `x/../img-1` or `../x` is another
//!   file in or beyond the project's asset directory. Ids are `img-<hex>` (uploads.cjs), so only a
//!   hand-edited record differs, and the port never reads outside the directory.
//! - The `[vision] describe failed:` line of a body that is not JSON says [`NOT_JSON`] where Node
//!   writes V8's `JSON.parse` message (the empty body's is V8's).
//!
//! Node behaviour kept: the notes go on every system message, the missing-images one included; a
//! `mime` whose `String()` throws makes an image loaded and missing; `${...}` of an object with
//! its own `toString` throws ([`Halt::Thrown`]); Node changes the last user message in place, so
//! its `msgs` entry holds the parts too.

use crate::roles::strict_eq;
pub use crate::roles::Halt;
use crate::sources::string_of;
use crate::{js, names, Thrown};
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};

/// `VISION_DESCRIBE_TIMEOUT_MS`: the describe request's own deadline.
pub const DESCRIBE_TIMEOUT_MS: u64 = 180_000;
/// `visionStageTimeoutMs`'s default: the whole describe step (lease and reply), #1790.
pub const STAGE_TIMEOUT_MS: u64 = 120_000;
/// `VISION_DESCRIBE_MAX_TOKENS`.
pub const DESCRIBE_MAX_TOKENS: u32 = 1536;
/// `readCappedJson(r, 4 * 1024 * 1024)`: a longer describe reply is a failure.
pub const REPLY_CAP: usize = 4 * 1024 * 1024;
/// How long a description is kept (`until: Date.now() + 300000`).
pub const DESCRIPTION_TTL_MS: f64 = 300_000.0;
/// The most descriptions kept (`visionDescriptions.size > 64` drops the oldest).
pub const DESCRIPTIONS_MAX: usize = 64;
/// chat.cjs `CODE_ROUTE_NOTE` (#1604).
pub const CODE_ROUTE_NOTE: &str = "For commands and code: before answering, check that it does exactly what was asked and nothing more (for example, when files are asked for, directories must not appear in the output). When asked why something can mislead or fail, name the concrete mechanism in one sentence.";
/// The `[vision] describe failed:` message of a body that is not JSON (see the named difference).
pub const NOT_JSON: &str = "the describe reply is not JSON";
const PROMPT: [&str; 3] = [
    "Describe these images in detail, so someone who cannot see them can answer questions about them.",
    "Transcribe any text exactly, including numbers, labels and headings. If text is unclear, say so rather than guessing.",
    "Do not answer the question yourself; only describe.",
];

/// What chat.cjs holds of the project's images before routing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Images {
    /// `attachedImages`: one `{ type: 'image_url', image_url: { url } }` part per loaded image.
    pub attached: Vec<JValue>,
    /// `loadedImageNames`, `missingImages`, `unattachedImageNames`: the assets' `name` values as
    /// stored (`String()` is applied where Node quotes or joins them).
    pub loaded_names: Vec<JValue>,
    pub missing_names: Vec<JValue>,
    pub unattached_names: Vec<JValue>,
    /// The `[assets] ... but its bytes are missing` lines, in order.
    pub lines: Vec<String>,
}

impl Images {
    /// `attachedImages.length > 0` before the pass (`readingImages`, part of P5b's `hasImages`).
    pub fn reading(&self) -> bool {
        !self.attached.is_empty()
    }
}

/// Whether the port reads an asset id as a file of the asset directory (the named difference).
pub fn plain_asset_name(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\', '\0'])
}

/// `Buffer.toString('base64')`.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            let sextet = ((n >> (18 - 6 * i)) & 63) as usize;
            let c = ALPHABET.get(sextet).map_or('=', |c| char::from(*c));
            out.push(if i <= chunk.len() { c } else { '=' });
        }
    }
    out
}

/// `v.key` of a value Node reads a property of: `null` and `undefined` throw.
fn field<'v>(v: &'v JValue, key: &str) -> Result<&'v JValue, Thrown> {
    (!v.is_nullish()).then(|| v.get(key)).ok_or(Thrown)
}

/// chat.cjs's `projectImages`, `namedImageIds`, `unattachedImageNames`, `attachedImages`,
/// `missingImages` and `loadedImageNames` (multimodal retrieval off). `project` is the turn's
/// record (`null` without one), `body` the request body. `read(id)` is the seam: the bytes of the
/// file `id` in the project's asset directory (`currentWorkspace().assetDir(project.id)`), `None`
/// when it cannot be read; it is asked only for a [`plain_asset_name`], once per attached asset in
/// order.
pub fn images(
    project: &JValue,
    body: &JValue,
    read: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
) -> Result<Images, Thrown> {
    let named = match body.get("turnImageIds") {
        JValue::Arr(ids) => Some(ids.as_slice()),
        _ => None,
    };
    let list: &[JValue] = match project.truthy().then(|| project.get("assets")) {
        Some(JValue::Arr(list)) => list,
        Some(v) if v.truthy() => {
            // `.filter` of a value that is not a list: reached when the turn names its images or
            // its `length` is truthy.
            let length = match v {
                JValue::Str(_) => true,
                other => other.get("length").truthy(),
            };
            if named.is_some() || length {
                return Err(Thrown);
            }
            &[]
        }
        _ => &[],
    };
    let has = |id: &JValue| named.is_some_and(|ids| ids.iter().any(|x| strict_eq(x, id)));
    let mut out = Images::default();
    let mut chosen = Vec::with_capacity(list.len());
    for asset in list {
        if named.is_none() || has(field(asset, "id")?) {
            chosen.push(asset);
        }
    }
    if named.is_some() {
        for asset in list {
            if !has(field(asset, "id")?) {
                out.unattached_names.push(field(asset, "name")?.clone());
            }
        }
    }
    for asset in chosen {
        let id = field(asset, "id")?;
        let name = asset.get("name");
        let bytes = id
            .as_str()
            .filter(|id| plain_asset_name(id))
            .and_then(&mut *read);
        let mut loaded = false;
        if let Some(bytes) = bytes {
            out.loaded_names.push(name.clone());
            if let Ok(mime) = string_of(asset.get("mime")) {
                let url = format!("data:{mime};base64,{}", base64(&bytes));
                out.attached.push(JValue::obj([
                    ("type", JValue::from("image_url")),
                    ("image_url", JValue::obj([("url", JValue::Str(url))])),
                ]));
                loaded = true;
            }
        }
        if !loaded {
            out.missing_names.push(name.clone());
            out.lines.push(format!(
                "[assets] {} is listed on project {} but its bytes are missing",
                string_of(id)?,
                string_of(project.get("id"))?
            ));
        }
    }
    Ok(out)
}

/// The handler's `visionDescriptions` Map: a description per key, kept [`DESCRIPTION_TTL_MS`],
/// at most [`DESCRIPTIONS_MAX`] (the oldest key goes first; setting a key again keeps its place).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Descriptions {
    entries: Vec<(String, String, f64)>,
}

impl Descriptions {
    /// `cached && cached.until > Date.now()`.
    pub fn get(&self, key: &str, now_ms: f64) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _, until)| k == key && *until > now_ms)
            .map(|(_, text, _)| text.as_str())
    }

    /// `set(key, { text, until: now + 300000 })`, then the bound.
    pub fn set(&mut self, key: &str, text: &str, now_ms: f64) {
        let until = now_ms + DESCRIPTION_TTL_MS;
        match self.entries.iter_mut().find(|(k, _, _)| k == key) {
            Some(entry) => {
                entry.1 = text.to_string();
                entry.2 = until;
            }
            None => self
                .entries
                .push((key.to_string(), text.to_string(), until)),
        }
        if self.entries.len() > DESCRIPTIONS_MAX {
            self.entries.remove(0);
        }
    }

    /// The keys in the Map's order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _, _)| k.as_str())
    }
}

/// What the pass reads of the turn besides its seams.
pub struct PassTurn<'a> {
    pub images: Images,
    /// `wire` as the route left it (system message, replayed history, the user's message).
    pub wire: Vec<JValue>,
    /// `sys` is not empty (always, with the time part).
    pub sys: bool,
    pub compact_only: bool,
    /// `externalProvider` (`egress.isExternalProvider(provider)`).
    pub external: bool,
    /// `offBoxTurn(...)` ([`crate::offer::off_box`]).
    pub off_box: bool,
    /// `routedRole === 'code'`.
    pub code_route: bool,
    /// The answering provider row (`provider`).
    pub provider: &'a JValue,
    pub default_provider_id: &'a str,
    /// The answering `model`.
    pub model: &'a JValue,
    /// `upstreamHeaders` (`providerHeaders(provider)`), the probe's headers.
    pub upstream_headers: &'a JValue,
    /// `message` (`body.message`), the question the description is asked for.
    pub message: &'a str,
    /// `currentWorkspace().userId` and `project.id` (in the description's cache key).
    pub user_id: &'a str,
    pub project_id: &'a JValue,
    /// `visionStageTimeoutMs` ([`STAGE_TIMEOUT_MS`] by default).
    pub stage_timeout_ms: f64,
    /// `typeof modelManager.acquireLease === 'function'`, `!!modelManager.recordEvidence`.
    pub lease_hook: bool,
    pub evidence_hook: bool,
    /// `Date.now()` when the description cache is read.
    pub now_ms: f64,
}

/// What the pass asks the caller, in Node's order.
#[derive(Debug, Clone, PartialEq)]
pub enum Seam {
    /// `autoRoles()`: [`Reply::AutoRoles`].
    AutoRoles,
    /// `getProvider(DEFAULT_PROVIDER_ID)`: [`Reply::Provider`].
    GetProvider(String),
    /// `providerHeaders(row)`: [`Reply::Headers`].
    ProviderHeaders(JValue),
    /// `modelManager.acquireLease(visionModel, { signal: stageSignal })`: [`Reply::Lease`].
    AcquireLease(JValue),
    /// `fetch(url, { method: 'POST', headers, body, ... })` of the description (through the
    /// lease's fetch when there is one; `body` is the JSON text): [`Reply::Described`].
    Describe {
        url: String,
        headers: JValue,
        body: String,
    },
    /// `stageLease.release()`: [`Reply::Released`].
    ReleaseLease,
    /// `send(event)`: [`Reply::Sent`].
    Send(JValue),
    /// `visionProbe(provider.baseUrl, upstreamHeaders, model)`: [`Reply::Probe`].
    Probe {
        base_url: JValue,
        headers: JValue,
        model: JValue,
    },
    /// `modelManager.recordEvidence(model, evidence)`, not awaited: [`Reply::Recorded`].
    RecordEvidence { model: JValue, evidence: JValue },
}

/// A refused lease: `String(err?.code || err?.status || 'error')`, and whether the stage deadline
/// had passed (`stageTimeout.aborted`) and the client had left (`chatSignal.signal.aborted`).
#[derive(Debug, Clone, PartialEq)]
pub struct LeaseRefusal {
    pub code: String,
    pub stage_timeout: bool,
    pub client_left: bool,
}

/// What the describe request came to.
#[derive(Debug, Clone, PartialEq)]
pub enum DescribeAnswer {
    /// A reply that is not `ok` (its body is not read).
    Status(u16),
    /// An `ok` reply's whole body (read up to one byte past [`REPLY_CAP`]).
    Body(Vec<u8>),
    /// The request or the body read threw (`String(err.message || err)`; the deadline passed?).
    Threw {
        message: String,
        stage_timeout: bool,
    },
}

/// The caller's answer to the last [`Seam`]; [`Reply::Start`] for the first step.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Start,
    AutoRoles(JValue),
    /// The row and the caller's `egress.isExternalProvider(row)` (read for the default id only).
    Provider {
        row: JValue,
        external: bool,
    },
    Headers(JValue),
    Lease(Result<(), LeaseRefusal>),
    /// The answer, whether the client had left by then, and `Date.now()` (the description's start).
    Described {
        answer: DescribeAnswer,
        client_left: bool,
        now_ms: f64,
    },
    Released,
    Sent,
    Probe(JValue),
    Recorded,
}

/// The wire after the pass and `visionWarning` (also the frame's `skills.imageWarning`).
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    pub wire: Vec<JValue>,
    pub image_warning: String,
}

/// One step of [`Pass`].
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Ask(Seam),
    /// The wire or why there is none, and the `[vision]` lines written, in order.
    Done {
        seen: Result<Seen, Halt>,
        lines: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Start,
    Roles,
    Provider,
    Headers,
    Lease,
    Describe,
    Release,
    Failure,
    Probe,
    Evidence,
    Over,
}

/// The vision pass, step by step (see the module doc).
pub struct Pass<'a> {
    t: PassTurn<'a>,
    cache: &'a mut Descriptions,
    stage: Stage,
    warning: String,
    vision_model: JValue,
    row: JValue,
    headers: JValue,
    key: String,
    leased: bool,
    /// `describeFailure`: `Some(true)` for 'timeout', `Some(false)` for 'failed'.
    failure: Option<bool>,
    client_left: bool,
    described: Option<String>,
    vision: JValue,
    lines: Vec<String>,
}

impl<'a> Pass<'a> {
    /// `cache` is the handler's `visionDescriptions`.
    pub fn new(t: PassTurn<'a>, cache: &'a mut Descriptions) -> Self {
        Pass {
            t,
            cache,
            stage: Stage::Start,
            warning: String::new(),
            vision_model: JValue::Undefined,
            row: JValue::Undefined,
            headers: JValue::Undefined,
            key: String::new(),
            leased: false,
            failure: None,
            client_left: false,
            described: None,
            vision: JValue::Undefined,
            lines: Vec::new(),
        }
    }

    pub fn step(&mut self, reply: Reply) -> Step {
        let stage = std::mem::replace(&mut self.stage, Stage::Over);
        match self.advance(stage, reply) {
            Ok(step) => step,
            Err(halt) => Step::Done {
                seen: Err(halt),
                lines: std::mem::take(&mut self.lines),
            },
        }
    }

    fn advance(&mut self, stage: Stage, reply: Reply) -> Result<Step, Halt> {
        match (stage, reply) {
            (Stage::Start, Reply::Start) => self.start(),
            (Stage::Roles, Reply::AutoRoles(roles)) => {
                // `roles && roles.vision`.
                self.vision_model = if roles.truthy() {
                    roles.get("vision").clone()
                } else {
                    roles
                };
                if self.vision_model.truthy() && !self.t.off_box {
                    let id = self.t.default_provider_id.to_string();
                    return Ok(self.ask(Stage::Provider, Seam::GetProvider(id)));
                }
                Ok(self.probe())
            }
            (Stage::Provider, Reply::Provider { row, external }) => {
                let default = JValue::from(self.t.default_provider_id);
                if row.truthy() && strict_eq(row.get("id"), &default) && !external {
                    self.row = row.clone();
                    return Ok(self.ask(Stage::Headers, Seam::ProviderHeaders(row)));
                }
                Ok(self.probe())
            }
            (Stage::Headers, Reply::Headers(headers)) => self.describe(headers),
            (Stage::Lease, Reply::Lease(Ok(()))) => {
                self.leased = true;
                let request = self.request()?;
                Ok(self.ask(Stage::Describe, request))
            }
            (Stage::Lease, Reply::Lease(Err(refused))) => {
                self.client_left = refused.client_left;
                let why = if refused.stage_timeout {
                    "timeout"
                } else {
                    &refused.code
                };
                let line = format!("{} not admitted: {why}", string_of(&self.vision_model)?);
                self.fail(refused.stage_timeout, line);
                self.after_describe()
            }
            (
                Stage::Describe,
                Reply::Described {
                    answer,
                    client_left,
                    now_ms,
                },
            ) => {
                self.client_left = client_left;
                self.answered(answer, now_ms)?;
                if self.leased {
                    return Ok(self.ask(Stage::Release, Seam::ReleaseLease));
                }
                self.after_describe()
            }
            (Stage::Release, Reply::Released) => self.after_describe(),
            (Stage::Failure, Reply::Sent) => self.after_failure(),
            (Stage::Probe, Reply::Probe(vision)) => self.probed(vision),
            (Stage::Evidence, Reply::Recorded) => self.finish_probe(),
            _ => Err(Halt::OutOfOrder),
        }
    }

    fn ask(&mut self, stage: Stage, seam: Seam) -> Step {
        self.stage = stage;
        Step::Ask(seam)
    }

    fn done(&mut self) -> Result<Step, Halt> {
        Ok(Step::Done {
            seen: Ok(Seen {
                wire: std::mem::take(&mut self.t.wire),
                image_warning: std::mem::take(&mut self.warning),
            }),
            lines: std::mem::take(&mut self.lines),
        })
    }

    /// `visionWarning += `${visionWarning ? ' ' : ''}${text}``.
    fn warn(&mut self, text: &str) {
        if !self.warning.is_empty() {
            self.warning.push(' ');
        }
        self.warning.push_str(text);
    }

    /// `wire.map((m) => (m.role === 'system' ? { ...m, content: `${m.content}\n\n${note}` } : m))`,
    /// then with `prepend` (`!sys`, or no system message for the `wire.some(...)` form) the note
    /// as a system message in front.
    fn note(&mut self, note: &str, prepend: bool) -> Result<(), Thrown> {
        for m in self.t.wire.iter_mut().filter(|m| is_system(m)) {
            let content = format!("{}\n\n{note}", string_of(m.get("content"))?);
            if let JValue::Obj(items) = m {
                wtf16::set(items, "content", JValue::Str(content));
            }
        }
        if prepend {
            self.t
                .wire
                .insert(0, js::message("system", note.to_string()));
        }
        Ok(())
    }

    /// The `wire.some((m) => m.role === 'system') ? append : prepend` form.
    fn note_somewhere(&mut self, note: &str) -> Result<(), Thrown> {
        let some = self.t.wire.iter().any(is_system);
        self.note(note, !some)
    }

    fn start(&mut self) -> Result<Step, Halt> {
        let missing = &self.t.images.missing_names;
        if !missing.is_empty() {
            self.warning = format!(
                "Images were not read because their stored files are missing: {}. Re-upload them.",
                quoted(missing)?
            );
            let message = format!("{} Do not guess their contents.", self.warning);
            self.t.wire.insert(0, js::message("system", message));
        }
        if self.t.external && !self.t.images.attached.is_empty() {
            self.t.images.attached.clear();
            let label = match self.t.provider.get("label") {
                l if l.truthy() => string_of(l)?,
                _ => "an external provider".to_string(),
            };
            self.warn(&format!(
                "Project images were not sent: images are never sent to {label} automatically."
            ));
            let unsent = format!("This project has image sources ({}) that were not sent to this external provider. Say so if asked about them; do not guess at their contents.", quoted(&self.t.images.loaded_names)?);
            self.note(&unsent, !self.t.sys)?;
        }
        if !self.t.images.unattached_names.is_empty() {
            let note = format!("This project has image sources ({}) that are not attached to this message. Say so if asked about them; do not guess at their contents.", quoted(&self.t.images.unattached_names)?);
            self.note_somewhere(&note)?;
        }
        if self.t.code_route && !self.t.compact_only {
            self.note_somewhere(CODE_ROUTE_NOTE)?;
        }
        if !self.t.images.attached.is_empty() && !self.t.compact_only {
            return Ok(self.ask(Stage::Roles, Seam::AutoRoles));
        }
        self.done()
    }

    /// `describeImagesNow`: the cache, then the lease or the request.
    fn describe(&mut self, headers: JValue) -> Result<Step, Halt> {
        let key = JValue::Arr(vec![
            JValue::from(self.t.user_id),
            self.row.get("baseUrl").clone(),
            self.vision_model.clone(),
            self.t.project_id.clone(),
            headers.clone(),
            JValue::Arr(self.t.images.attached.clone()),
            JValue::from(self.t.message),
        ]);
        let text = wtf16::stringify(&key).unwrap_or_default();
        self.key = format!("{:x}", Sha256::digest(text.as_bytes()));
        self.headers = headers;
        if let Some(text) = self.cache.get(&self.key, self.t.now_ms) {
            self.described = Some(text.to_string());
            return self.after_describe();
        }
        if self.t.lease_hook {
            return Ok(self.ask(Stage::Lease, Seam::AcquireLease(self.vision_model.clone())));
        }
        let request = self.request()?;
        Ok(self.ask(Stage::Describe, request))
    }

    /// The describe request: url, headers and body.
    fn request(&self) -> Result<Seam, Thrown> {
        let base = string_of(self.row.get("baseUrl"))?;
        let base = base.trim_end_matches('/');
        let url = format!(
            "{}/v1/chat/completions",
            base.strip_suffix("/v1").unwrap_or(base)
        );
        let mut items = vec![("Content-Type".to_string(), JValue::from("application/json"))];
        if let JValue::Obj(extra) = &self.headers {
            for (k, v) in extra {
                wtf16::set(&mut items, k, v.clone());
            }
        }
        let question = js::slice(self.t.message, 500);
        let prompt = format!(
            "{} The question that will be asked is: {question}",
            PROMPT.join(" ")
        );
        let mut content = vec![text_part(prompt)];
        content.extend(self.t.images.attached.iter().cloned());
        let body = JValue::obj([
            ("model", self.vision_model.clone()),
            ("max_tokens", JValue::Num(f64::from(DESCRIBE_MAX_TOKENS))),
            (
                "chat_template_kwargs",
                JValue::obj([("enable_thinking", JValue::Bool(false))]),
            ),
            (
                "messages",
                JValue::Arr(vec![JValue::obj([
                    ("role", JValue::from("user")),
                    ("content", JValue::Arr(content)),
                ])]),
            ),
        ]);
        Ok(Seam::Describe {
            url,
            headers: JValue::Obj(items),
            body: wtf16::stringify(&body).unwrap_or_default(),
        })
    }

    /// `describeFailure` ('timeout' or 'failed') and its `[vision]` line.
    fn fail(&mut self, timeout: bool, line: String) {
        self.failure = Some(timeout);
        if !line.is_empty() {
            self.lines.push(format!("[vision] {line}"));
        }
    }

    /// What the describe answer came to: the description (kept in the cache) or a failure.
    fn answered(&mut self, answer: DescribeAnswer, now_ms: f64) -> Result<(), Thrown> {
        match answer {
            DescribeAnswer::Status(status) => {
                let model = string_of(&self.vision_model)?;
                self.fail(false, format!("{model} could not describe ({status})"));
            }
            DescribeAnswer::Body(bytes) => match description(&bytes) {
                Ok(Some(text)) => {
                    self.cache.set(&self.key, &text, now_ms);
                    self.described = Some(text);
                }
                Ok(None) => self.fail(false, String::new()),
                Err(message) => self.fail(false, format!("describe failed: {message}")),
            },
            DescribeAnswer::Threw {
                stage_timeout: true,
                ..
            } => {
                let line = format!(
                    "describe failed: no description within {} s",
                    self.stage_seconds()
                );
                self.fail(true, line);
            }
            DescribeAnswer::Threw { message, .. } => {
                self.fail(false, format!("describe failed: {message}"));
            }
        }
        Ok(())
    }

    fn stage_seconds(&self) -> String {
        js_json::number_to_string(self.t.stage_timeout_ms / 1000.0)
    }

    /// #1790: a description that did not come is said, unless the client left.
    fn after_describe(&mut self) -> Result<Step, Halt> {
        let Some(timeout) = self
            .failure
            .filter(|_| self.described.is_none() && !self.client_left)
        else {
            return self.after_failure();
        };
        let why = if timeout {
            format!("did not finish within {} seconds", self.stage_seconds())
        } else {
            "could not describe it".to_string()
        };
        let model = string_of(self.t.model)?;
        self.warn(&format!(
            "The vision model ({}) {why}, so {model} was given the image directly if it can read images.",
            string_of(&self.vision_model)?
        ));
        let text = format!("The vision model {why}. Trying {model} with the image instead…");
        Ok(self.ask(Stage::Failure, Seam::Send(status("readingImages", text))))
    }

    fn after_failure(&mut self) -> Result<Step, Halt> {
        let Some(text) = self.described.take() else {
            return Ok(self.probe());
        };
        let label = format!(
            "{} by {}",
            joined(&self.t.images.loaded_names)?,
            string_of(&self.vision_model)?
        );
        let note = js::frame_untrusted("image description", &label, &text);
        self.note(&note, !self.t.sys)?;
        self.t.images.attached.clear();
        self.done()
    }

    fn probe(&mut self) -> Step {
        let seam = Seam::Probe {
            base_url: self.t.provider.get("baseUrl").clone(),
            headers: self.t.upstream_headers.clone(),
            model: self.t.model.clone(),
        };
        self.ask(Stage::Probe, seam)
    }

    fn probed(&mut self, vision: JValue) -> Result<Step, Halt> {
        if vision.is_nullish() {
            return Err(Halt::Thrown);
        }
        let supported = vision.get("supported").truthy();
        let reason = vision.get("reason");
        let reason_text = || -> Result<String, Thrown> {
            if reason.truthy() {
                string_of(reason)
            } else {
                Ok(String::new())
            }
        };
        let default = JValue::from(self.t.default_provider_id);
        let evidence = strict_eq(self.t.provider.get("id"), &default)
            && self.t.evidence_hook
            && (supported || {
                let r = reason_text()?.to_ascii_lowercase();
                r.contains("projector") || r.contains("mmproj")
            });
        let limitation = if supported {
            "1×1 image accepted; not an accuracy test".to_string()
        } else if evidence {
            js::slice(&reason_text()?, 200)
        } else {
            String::new()
        };
        self.vision = vision.clone();
        if !evidence {
            return self.finish_probe();
        }
        let evidence = JValue::obj([
            ("category", JValue::from("vision")),
            (
                "result",
                JValue::from(if supported { "passed" } else { "failed" }),
            ),
            ("value", JValue::Null),
            (
                "suite",
                JValue::obj([
                    ("name", JValue::from("vision-probe")),
                    ("version", JValue::Num(1.0)),
                ]),
            ),
            ("source", JValue::from("probe")),
            ("limitations", JValue::Arr(vec![JValue::Str(limitation)])),
        ]);
        let model = self.t.model.clone();
        Ok(self.ask(Stage::Evidence, Seam::RecordEvidence { model, evidence }))
    }

    fn finish_probe(&mut self) -> Result<Step, Halt> {
        let vision = std::mem::replace(&mut self.vision, JValue::Undefined);
        if vision.get("supported").truthy() {
            let names = quoted(&self.t.images.loaded_names)?;
            let attached = std::mem::take(&mut self.t.images.attached);
            let last = self
                .t
                .wire
                .iter_mut()
                .rev()
                .find(|m| m.get("role").as_str() == Some("user"));
            if let Some(JValue::Obj(items)) = last {
                let text = match items.iter().find(|(k, _)| k == "content") {
                    Some((_, JValue::Str(s))) => s.clone(),
                    _ => String::new(),
                };
                let mut parts = vec![text_part(format!("{text}\n\n(Attached images: {names})"))];
                parts.extend(attached);
                wtf16::set(items, "content", JValue::Arr(parts));
            }
            return self.done();
        }
        self.t.images.attached.clear();
        let reason = string_of(vision.get("reason"))?;
        self.warn(&format!("Images were not read. {reason}"));
        let blind = format!(
            "This project has image sources ({}) but image input is currently unavailable for {}: {reason}{}. Say so if asked about them; do not guess at their contents.",
            quoted(&self.t.images.loaded_names)?,
            string_of(self.t.model)?,
            if self.vision_model.truthy() { "; the configured vision model also could not describe them" } else { "" }
        );
        self.note(&blind, !self.t.sys)?;
        self.done()
    }
}

/// The description of an `ok` reply's body (`readCappedJson`, then the first choice's trimmed
/// content; `None` for no text or a reply cut at the token limit), or the message of what threw.
pub fn description(bytes: &[u8]) -> Result<Option<String>, String> {
    if bytes.len() > REPLY_CAP {
        return Err(format!("reply exceeded the {} KB limit", REPLY_CAP / 1024));
    }
    let Some(body) = js::parse_file(bytes) else {
        return Err(if bytes.is_empty() {
            "Unexpected end of JSON input".to_string()
        } else {
            NOT_JSON.to_string()
        });
    };
    // `body?.choices?.[0]`: a list's first item, an object's "0" (a string's first character has
    // no `message` either).
    let first = match body.get("choices") {
        JValue::Arr(list) => list.first().unwrap_or(&JValue::Undefined),
        other => other.get("0"),
    };
    let content = first.get("message").get("content");
    let text = if content.truthy() {
        string_of(content).map_err(|_| "Cannot convert object to primitive value".to_string())?
    } else {
        String::new()
    };
    let text = js::trim(&text);
    if text.is_empty() || first.get("finish_reason").as_str() == Some("length") {
        return Ok(None);
    }
    Ok(Some(text.to_string()))
}

/// `quotedNames(names)` (`quoteName` takes `String(name)`).
fn quoted(names: &[JValue]) -> Result<String, Thrown> {
    let texts = names.iter().map(string_of).collect::<Result<Vec<_>, _>>()?;
    Ok(names::quoted_names(&texts))
}

/// `names.join(', ')` (`null` and `undefined` join as nothing).
fn joined(names: &[JValue]) -> Result<String, Thrown> {
    let text = |n: &JValue| {
        n.is_nullish()
            .then(String::new)
            .map_or_else(|| string_of(n), Ok)
    };
    Ok(names
        .iter()
        .map(text)
        .collect::<Result<Vec<_>, _>>()?
        .join(", "))
}

fn is_system(m: &JValue) -> bool {
    m.get("role").as_str() == Some("system")
}

fn text_part(text: String) -> JValue {
    JValue::obj([("type", JValue::from("text")), ("text", JValue::Str(text))])
}

/// `{ type: 'status', id, text }`.
fn status(id: &str, text: String) -> JValue {
    let parts = [("id", JValue::from(id)), ("text", JValue::Str(text))];
    JValue::obj([("type", JValue::from("status"))].into_iter().chain(parts))
}

/// `{ type: 'status', id: 'generating', text: 'Generating response…' }`, after the pass.
pub fn generating_event() -> JValue {
    status("generating", "Generating response…".to_string())
}

/// `replyWarning()`: the image warning, the skills' history note and the coverage warning, the
/// non-empty ones joined with a space; the `warning` event is sent when it is not empty.
pub fn reply_warning(image: &str, history_note: &str, coverage: &str) -> String {
    [image, history_note, coverage]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `{ type: 'warning', text }` when `text` is not empty.
pub fn warning_event(text: &str) -> Option<JValue> {
    (!text.is_empty()).then(|| {
        JValue::obj([
            ("type", JValue::from("warning")),
            ("text", JValue::from(text)),
        ])
    })
}
