//! `POST /api/projects/{projectId}/config` (sbstndalton/noevia#2191 round 9, capability
//! `rust-project-config`): core routes/projects.cjs's project configuration, field for field.
//!
//! In Node's order: the id percent-decoded (a bad escape is the URIError, 500); the body read up to
//! 1 MiB (413) and parsed (`invalid JSON`, 400); `getProject` (the self-heal, saved; 404 `no such
//! project`); then the patch, the first refusal winning: the icon and colour, the reasoning effort,
//! the context profile, the name, goal and instructions (clipped), the model (a local provider's
//! model must be able to answer a chat), the pin, the storage folders (each through
//! `safeRelativePath`, at most 10), the archive flag, the modes, the shared context, the routing,
//! the routing mode, the tools mode, the provider (it must be registered), the sampling override,
//! the toolboxes (only ids the server offers), the memories (at most 50 of 500 characters) and the
//! uploads (the list as the client sends it, which holds no text for what the project already
//! has). A patch that leaves every field as stored is a read: no write, no timestamp
//! (`{ ok: true, unchanged: true }`, #1886). Otherwise `updatedAt` moves, every project is
//! reconciled, the new texts are written before the projects.json that points at them, and the
//! RAG index follows through the user's index outbox (an `index` job per new or changed upload,
//! an `unindex` job per removed one: core's executor re-reads the list and skips what was listed
//! again, #1919, #1914). The documents and originals of a source that left are pruned once the
//! list without it is on disk.
//!
//! What the route needs of the outside is passed in [`Env`]: core's published catalogue (the project
//! icons, the toolbox ids), whether a provider is registered, and whether the engine can serve a
//! model as a chat model.
//!
//! Differences from Node (sbstndalton/noevia-core `contracts`, README "round 9"): Node's
//! `ensureRolesLoaded()` when a project is set to Auto routing warmed the role models in an engine
//! without a native router; the models now load on the first turn (the native router, the only
//! one deployed, never did it). A `null` body is Node's TypeError, a 500, as before.

use crate::lifecycle::{self, Catalogue};
use crate::{chats, files, js, outbox, source_limits, sources, store, Ctx, Reply};
use js_json::{wtf16, JValue};
use std::path::Path;

/// http.cjs readBody's default limit.
pub const BODY_LIMIT: usize = 1024 * 1024;
/// contracts/project-limits.json `nameMaxLength`.
const NAME_MAX: usize = lifecycle::NAME_MAX;
/// reasoning-effort.cjs EFFORTS.
const EFFORTS: [&str; 3] = ["default", "low", "high"];
/// routing-modes.cjs MODES / WHEN_SENSITIVE.
const ROUTING_MODES: [&str; 3] = ["local", "cloud", "hybrid"];
const WHEN_SENSITIVE: [&str; 2] = ["ask", "local"];
/// sampling-presets.cjs SAMPLING_KEYS.
const SAMPLING_KEYS: [&str; 4] = ["temperature", "top_p", "top_k", "repeat_penalty"];
/// shared-context.cjs RECEIVERS.
const RECEIVERS: [&str; 2] = ["chat", "code"];
const SAVE_FAILED: &str = "could not save the project; nothing was changed";

/// What the route reads from outside the request and the project.
pub struct Env<'a> {
    /// core's published catalogue (None while it is unpublished: a patch that needs it is a 503).
    pub catalogue: Option<&'a Catalogue>,
    /// `DEFAULT_PROVIDER_ID`.
    pub default_provider: &'a str,
    /// Whether `getProvider(id)` finds a registered provider of that id (the default one, a shared
    /// row or the account's own); `lemonade` is the default's old name and is not asked.
    pub provider_known: &'a dyn Fn(&str) -> bool,
    /// `isChatGenerationModel(name, labels)` for the model as the local engine serves it.
    pub chat_model: &'a dyn Fn(&str) -> bool,
}

/// How the closure ended.
enum Done {
    Unchanged,
    Saved {
        /// Names an `index` job is queued for.
        index: Vec<String>,
        /// Names an `unindex` job is queued for.
        gone: Vec<String>,
        /// Whether the sources' documents and originals are pruned afterwards.
        prune: bool,
    },
}

fn not_published() -> Reply {
    Reply::error(
        503,
        "noevia-core has not published its toolbox catalogue yet; try again shortly.",
    )
}

fn typed_str(v: &JValue) -> Option<&str> {
    v.as_str()
}

fn is_defined(v: &JValue) -> bool {
    !matches!(v, JValue::Undefined)
}

/// `isDeepStrictEqual(a, b)` for JSON data: object keys in any order, arrays in order.
pub fn deep_equal(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Obj(x), JValue::Obj(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.iter().any(|(k2, v2)| k == k2 && deep_equal(v, v2)))
        }
        (JValue::Arr(x), JValue::Arr(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| deep_equal(p, q))
        }
        (JValue::Num(x), JValue::Num(y)) => x == y || (x.is_nan() && y.is_nan()),
        _ => a == b,
    }
}

/// `delete project[key]`.
fn remove(project: &mut JValue, key: &str) {
    if let JValue::Obj(items) = project {
        items.retain(|(k, _)| k != key);
    }
}

/// `project[key] = value` (replaced in place or appended).
fn put(project: &mut JValue, key: &str, value: JValue) {
    store::set(project, key, value);
}

/// `sampling-presets.cjs validSamplingValue(key, value)`.
fn valid_sampling(key: &str, v: &JValue) -> bool {
    let Some(n) = v.as_num().filter(|n| n.is_finite()) else {
        return false;
    };
    match key {
        "temperature" => (0.0..=2.0).contains(&n),
        "top_p" => n > 0.0 && n <= 1.0,
        "top_k" => n.fract() == 0.0 && n >= 0.0,
        "repeat_penalty" => (0.0..=3.0).contains(&n),
        _ => false,
    }
}

/// `sanitizeExplicitSampling(raw)`: the recognised, valid keys, or none.
fn sampling(raw: &JValue) -> Option<JValue> {
    if !matches!(raw, JValue::Obj(_) | JValue::Arr(_)) {
        return None;
    }
    let out: Vec<(String, JValue)> = SAMPLING_KEYS
        .iter()
        .filter(|k| valid_sampling(k, raw.get(k)))
        .map(|k| ((*k).to_string(), raw.get(k).clone()))
        .collect();
    (!out.is_empty()).then_some(JValue::Obj(out))
}

/// project-modes `sanitize`'s refusal text aside, `sharedContext.sanitize(value)`.
fn shared_context(value: &JValue) -> Result<JValue, Reply> {
    let JValue::Obj(items) = value else {
        return Err(Reply::error(400, "sharedContext must be an object"));
    };
    if items.iter().any(|(k, _)| !RECEIVERS.contains(&k.as_str())) {
        return Err(Reply::error(
            400,
            &format!("sharedContext may only contain {}", RECEIVERS.join(", ")),
        ));
    }
    for (k, v) in items {
        if !matches!(v, JValue::Bool(_)) {
            return Err(Reply::error(
                400,
                &format!("sharedContext.{k} must be true or false"),
            ));
        }
    }
    // { ...defaults(), ...value }: chat then code, each the given value or false.
    Ok(JValue::Obj(
        RECEIVERS
            .iter()
            .map(|r| {
                (
                    (*r).to_string(),
                    match value.get(r) {
                        JValue::Bool(b) => JValue::Bool(*b),
                        _ => JValue::Bool(false),
                    },
                )
            })
            .collect(),
    ))
}

/// routing-modes `projectOverride(raw, true)`: the stored override, or None to clear it.
fn routing_override(raw: &JValue) -> Result<Option<JValue>, Reply> {
    match raw {
        JValue::Undefined | JValue::Null => Ok(None),
        JValue::Obj(_) => {
            let mode = raw
                .get("mode")
                .as_str()
                .filter(|m| ROUTING_MODES.contains(m));
            let Some(mode) = mode else {
                return Err(Reply::error(
                    400,
                    "routingMode.mode must be 'local', 'cloud' or 'hybrid'",
                ));
            };
            let when = raw.get("whenSensitive");
            if is_defined(when) && !when.as_str().is_some_and(|w| WHEN_SENSITIVE.contains(&w)) {
                return Err(Reply::error(
                    400,
                    "routingMode.whenSensitive must be 'ask' or 'local'",
                ));
            }
            let sensitive = if when.truthy() {
                when.as_str().unwrap_or("ask")
            } else {
                "ask"
            };
            Ok(Some(JValue::obj([
                ("mode", JValue::from(mode)),
                ("whenSensitive", JValue::from(sensitive)),
            ])))
        }
        _ => Err(Reply::error(400, "routingMode must be an object or null")),
    }
}

/// `patch.contextProfile`: 'high' is stored, Low (the default) is the absence of the field.
fn context_profile(project: &mut JValue, value: &JValue) -> Result<(), Reply> {
    if !is_defined(value) {
        return Ok(());
    }
    match value {
        JValue::Null => remove(project, "contextProfile"),
        JValue::Str(s) if s == "low" => remove(project, "contextProfile"),
        JValue::Str(s) if s == "high" => put(project, "contextProfile", JValue::from("high")),
        _ => return Err(Reply::error(400, "Invalid context profile")),
    }
    Ok(())
}

/// The `files` patch: the uploads as the client lists them. Returns the names to index.
fn apply_files(
    dir: &Path,
    project: &mut JValue,
    patch_files: &[JValue],
) -> (Vec<String>, Vec<String>) {
    let prev: Vec<JValue> = match project.get("files") {
        JValue::Arr(items) => items.clone(),
        _ => Vec::new(),
    };
    let project_id = project.get("id").as_str().unwrap_or("").to_string();
    // Folder-derived files belong to the sync, not to this patch.
    let from_folders: Vec<JValue> = prev
        .iter()
        .filter(|f| f.truthy() && f.get("source").truthy())
        .cloned()
        .collect();
    let own =
        |p: &JValue, name: &str| !p.get("source").truthy() && p.get("name").as_str() == Some(name);
    // #1806: the client lists sources without their text; an entry with no `content` names an upload
    // this project already holds, kept as it is; with no such upload it is dropped.
    let room = source_limits::limits()
        .sources
        .saturating_sub(from_folders.len());
    let mut uploads: Vec<JValue> = Vec::new();
    for f in patch_files {
        let Some(name) = f.get("name").as_str() else {
            continue;
        };
        if !f.truthy() || f.get("source").truthy() {
            continue;
        }
        let content = f.get("content");
        let existing = prev.iter().find(|p| own(p, name));
        let kept =
            matches!(content, JValue::Str(_)) || (!is_defined(content) && existing.is_some());
        if !kept {
            continue;
        }
        if uploads.len() >= room {
            break;
        }
        match (content, existing) {
            (JValue::Str(_), _) => {}
            (_, Some(e)) => {
                uploads.push(e.clone());
                continue;
            }
            _ => continue,
        }
        let text = content.as_str().unwrap_or("");
        if let Some(e) =
            existing.filter(|e| e.get("document").truthy() || e.get("attachment").truthy())
        {
            uploads.push(e.clone());
            continue;
        }
        let mut next = vec![
            ("name".to_string(), JValue::Str(wtf16::slice(name, 200))),
            (
                "content".to_string(),
                JValue::Str(wtf16::slice(text, 200_000)),
            ),
        ];
        // Published provenance is kept only while the exact copied bytes survive.
        if let Some(e) = existing {
            if e.get("skillOrigin").truthy()
                && sources::text(dir, &project_id, e).ok().as_deref() == Some(text)
            {
                next.push(("skillOrigin".to_string(), e.get("skillOrigin").clone()));
            }
        }
        uploads.push(JValue::Obj(next));
    }
    let mut next_files = from_folders;
    next_files.extend(uploads);
    let next_names: Vec<&str> = next_files
        .iter()
        .filter_map(|f| f.get("name").as_str())
        .collect();
    // RAG bookkeeping: the chunks of removed sources go, new or changed ones are indexed.
    let gone: Vec<String> = prev
        .iter()
        .filter_map(|p| p.get("name").as_str())
        .filter(|n| !next_names.contains(n))
        .map(str::to_string)
        .collect();
    let mut index: Vec<String> = Vec::new();
    for next in &next_files {
        let Some(name) = next.get("name").as_str() else {
            continue;
        };
        // `new Map(prevFiles.map(...))`: a later entry of the same name wins.
        let before = prev
            .iter()
            .rev()
            .find(|p| p.get("name").as_str() == Some(name));
        let changed = match before {
            None => true,
            Some(b) => !same_text(dir, &project_id, b, next),
        };
        if changed && !index.iter().any(|i| i == name) {
            index.push(name.to_string());
        }
    }
    // Images of sources that left go too.
    let kept_assets: Vec<JValue> = match project.get("assets") {
        JValue::Arr(items) => items
            .iter()
            .filter(|a| {
                let s = a.get("sourceName");
                !s.truthy() || s.as_str().is_some_and(|s| next_names.contains(&s))
            })
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    put(project, "files", JValue::Arr(next_files));
    put(project, "assets", JValue::Arr(kept_assets));
    (index, gone)
}

/// A source whose text is in its own file (a 64-hex pointer and no text of its own).
fn external(f: &JValue) -> Option<&str> {
    let r = f.get("textRef").as_str()?;
    (matches!(f.get("content"), JValue::Undefined)
        && r.len() == 64
        && r.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
    .then_some(r)
}

/// source-text.cjs `sameText(a, b)`.
fn same_text(dir: &Path, project: &str, a: &JValue, b: &JValue) -> bool {
    if let (Some(x), Some(y)) = (external(a), external(b)) {
        return x == y;
    }
    let read = |f: &JValue| sources::text(dir, project, f).unwrap_or_default();
    read(a) == read(b)
}

/// The patch applied to the (healed) stored project, in Node's order. `Err` is the answer to send.
fn patch_project(
    dir: &Path,
    env: &Env<'_>,
    stored: &JValue,
    patch: &JValue,
) -> Result<(JValue, Vec<String>, Vec<String>), Reply> {
    let mut project = stored.clone();
    // projectAppearance (`Object.hasOwn(patch, ...)`, a null patch is the TypeError).
    if matches!(patch, JValue::Null) {
        return Err(Reply::internal());
    }
    let wants_icon = lifecycle::has_own(patch, "icon");
    let look = if wants_icon || lifecycle::has_own(patch, "color") {
        let icons: Vec<String> = match (env.catalogue, wants_icon) {
            (Some(c), _) => c.icons.clone(),
            (None, true) => return Err(not_published()),
            (None, false) => Vec::new(),
        };
        lifecycle::appearance(patch, &icons)?
    } else {
        Vec::new()
    };
    let effort = patch.get("reasoningEffort");
    if !matches!(effort, JValue::Undefined | JValue::Null)
        && !effort.as_str().is_some_and(|e| EFFORTS.contains(&e))
    {
        return Err(Reply::error(400, "Invalid reasoning effort"));
    }
    for (k, v) in look {
        put(&mut project, &k, v);
    }
    match effort {
        JValue::Null => remove(&mut project, "reasoningEffort"),
        JValue::Undefined => {}
        v => put(&mut project, "reasoningEffort", v.clone()),
    }
    // #1079: Context: Low (the default, stored as absent) or High.
    context_profile(&mut project, patch.get("contextProfile"))?;
    if let Some(name) = typed_str(patch.get("name")).filter(|n| !js::trim(n).is_empty()) {
        put(
            &mut project,
            "name",
            JValue::Str(wtf16::slice(js::trim(name), NAME_MAX)),
        );
    }
    if let Some(goal) = typed_str(patch.get("goal")) {
        put(&mut project, "goal", JValue::Str(wtf16::slice(goal, 2000)));
    }
    if let Some(text) = typed_str(patch.get("instructions")) {
        put(
            &mut project,
            "instructions",
            JValue::Str(wtf16::slice(text, 8000)),
        );
    }
    let model = typed_str(patch.get("model")).filter(|m| !m.is_empty());
    if let Some(model) = model {
        // Embedding, reranking and Laya (the internal routing model) cannot answer a chat prompt:
        // pinning one would save and then send every message to it (#409). A model of another
        // provider is not in the local catalogue and is never judged.
        let asked = typed_str(patch.get("provider")).filter(|p| !p.is_empty());
        let effective = asked
            .or_else(|| typed_str(stored.get("provider")).filter(|p| !p.is_empty()))
            .unwrap_or(env.default_provider);
        if (effective == env.default_provider || effective == "lemonade")
            && !(env.chat_model)(model)
        {
            return Err(Reply::error(
                400,
                &format!(
                    "{model} is an embedding, reranking or routing model and cannot be used as a chat model — pick a chat model instead."
                ),
            ));
        }
        put(&mut project, "model", JValue::Str(model.to_string()));
    }
    // Picking a model is a manual choice even when only `model` was sent (#384).
    if model.is_some() && !matches!(patch.get("routing"), JValue::Str(_)) {
        put(&mut project, "routing", JValue::from("manual"));
        put(&mut project, "routingChosen", JValue::Bool(true));
    }
    if let JValue::Bool(b) = patch.get("pinned") {
        put(&mut project, "pinned", JValue::Bool(*b));
    }
    if let JValue::Arr(folders) = patch.get("sourceFolders") {
        let safe: Vec<JValue> = folders
            .iter()
            .filter_map(|f| f.as_str())
            .filter(|f| !js::trim(f).is_empty())
            .map(|f| storage_path::safe_relative_path(f).unwrap_or_default())
            .filter(|f| !f.is_empty())
            .take(10)
            .map(JValue::Str)
            .collect();
        put(&mut project, "sourceFolders", JValue::Arr(safe));
    }
    if let JValue::Bool(b) = patch.get("archived") {
        put(&mut project, "archived", JValue::Bool(*b));
    }
    if is_defined(patch.get("modes")) {
        let modes = lifecycle::modes(patch.get("modes"))?;
        put(
            &mut project,
            "modes",
            JValue::Arr(modes.into_iter().map(JValue::Str).collect()),
        );
    }
    if is_defined(patch.get("sharedContext")) {
        put(
            &mut project,
            "sharedContext",
            shared_context(patch.get("sharedContext"))?,
        );
    }
    if let JValue::Str(routing) = patch.get("routing") {
        if routing != "auto" && routing != "manual" {
            return Err(Reply::error(400, "routing must be 'auto' or 'manual'"));
        }
        put(&mut project, "routing", JValue::Str(routing.clone()));
        // A person's own choice, never the template default (#352).
        put(&mut project, "routingChosen", JValue::Bool(true));
    }
    // #1007: this project's own routing mode; null goes back to the account's.
    if is_defined(patch.get("routingMode")) {
        match routing_override(patch.get("routingMode"))? {
            Some(v) => put(&mut project, "routingMode", v),
            None => remove(&mut project, "routingMode"),
        }
    }
    // #1006: tools picked automatically or by hand from `toolboxes`.
    if is_defined(patch.get("toolsMode")) {
        match patch.get("toolsMode").as_str() {
            Some(m @ ("auto" | "manual")) => put(&mut project, "toolsMode", JValue::from(m)),
            _ => return Err(Reply::error(400, "toolsMode must be 'auto' or 'manual'")),
        }
    }
    if let Some(provider) = typed_str(patch.get("provider")).filter(|p| !p.is_empty()) {
        // An explicit selection must match a registered id (or the legacy lemonade alias).
        let known = provider == env.default_provider
            || provider == "lemonade"
            || (env.provider_known)(provider);
        if !known {
            return Err(Reply::error(400, "no such provider"));
        }
        put(&mut project, "provider", JValue::from(provider));
    }
    // Explicit sampling override (#194): null clears it; an unusable value clears it too.
    match patch.get("sampling") {
        JValue::Undefined => {}
        JValue::Null => remove(&mut project, "sampling"),
        raw => match sampling(raw) {
            Some(v) => put(&mut project, "sampling", v),
            None => remove(&mut project, "sampling"),
        },
    }
    if is_defined(patch.get("toolboxes")) {
        let Some(catalogue) = env.catalogue else {
            return Err(not_published());
        };
        let Some(boxes) = catalogue.sanitize(patch.get("toolboxes")) else {
            return Err(Reply::error(
                400,
                "toolboxes must be an array of toolbox ids",
            ));
        };
        put(
            &mut project,
            "toolboxes",
            JValue::Arr(boxes.into_iter().map(JValue::Str).collect()),
        );
    }
    if let JValue::Arr(memories) = patch.get("memories") {
        let kept: Vec<JValue> = memories
            .iter()
            .filter_map(|m| m.as_str())
            .filter(|m| !js::trim(m).is_empty())
            .map(|m| JValue::Str(wtf16::slice(js::trim(m), 500)))
            .take(50)
            .collect();
        put(&mut project, "memories", JValue::Arr(kept));
    }
    let (mut index, mut gone) = (Vec::new(), Vec::new());
    if let JValue::Arr(patch_files) = patch.get("files") {
        let (i, g) = apply_files(dir, &mut project, patch_files);
        index = i;
        gone = g;
    }
    Ok((project, index, gone))
}

/// POST /api/projects/{projectId}/config. `body` was read up to [`BODY_LIMIT`] (`over` past it).
pub fn post(ctx: Ctx<'_>, raw: &str, body: &[u8], over: bool, env: &Env<'_>) -> Reply {
    let Some(id) = js::decode_uri_component(raw) else {
        return Reply::internal();
    };
    if over {
        return Reply::error(413, "Request exceeds size limit");
    }
    let Ok(patch) = wtf16::parse(&String::from_utf8_lossy(body)) else {
        return chats::invalid_json();
    };
    let dir = match store::user_dir(ctx.data_dir, ctx.user_id) {
        Ok(d) => d,
        Err(e) => return chats::store_failure(&e),
    };
    let done = store::update(&dir, ctx.switch, |projects| {
        let Ok(at) = chats::locate(projects, &id) else {
            return (Err(Reply::internal()), false);
        };
        let Some(at) = at else {
            return (Err(Reply::error(404, "no such project")), false);
        };
        let Some(p) = projects.get_mut(at) else {
            return (Err(Reply::internal()), false);
        };
        // getProject: the self-heal and the reconcile, saved even when the patch is refused.
        let healed = match chats::get_project_effects(p) {
            Ok(effects) => {
                let changed = effects.saved.is_some();
                *p = effects.memory;
                changed
            }
            Err(_) => return (Err(Reply::internal()), false),
        };
        let stored = p.clone();
        let (mut project, index, gone) = match patch_project(&dir, env, &stored, &patch) {
            Ok(v) => v,
            Err(reply) => return (Err(reply), healed),
        };
        // #1886: a patch that leaves every field as stored is a read.
        if deep_equal(&project, &stored) {
            return (Ok(Done::Unchanged), healed);
        }
        put(&mut project, "updatedAt", JValue::Num(ctx.now_ms as f64));
        // The new texts are on disk before the projects.json that points at them.
        if let Err(reply) = lifecycle::externalize_reply(&dir, &mut project) {
            return (Err(reply), healed);
        }
        let prune = matches!(patch.get("files"), JValue::Arr(_));
        *p = project;
        if !lifecycle::reconcile_all(projects) {
            return (Err(Reply::internal()), false);
        }
        (Ok(Done::Saved { index, gone, prune }), true)
    });
    match done {
        Ok(Ok(Done::Unchanged)) => Reply::json(200, "{\"ok\":true,\"unchanged\":true}".into()),
        Ok(Ok(Done::Saved { index, gone, prune })) => {
            queue(&dir, &id, &index, &gone, ctx.now_ms);
            if prune {
                if let Err(reply) = prune_after(ctx, &dir, &id) {
                    return reply;
                }
            }
            Reply::json(200, "{\"ok\":true}".into())
        }
        Ok(Err(reply)) => reply,
        Err(e) => chats::save_failure(&e, SAVE_FAILED),
    }
}

/// The index outbox's jobs for the sources that changed or left (best effort: the list is saved,
/// and a failure only delays the index until the next change of that source).
fn queue(dir: &Path, id: &str, index: &[String], gone: &[String], now_ms: i64) {
    let jobs: Vec<outbox::Job<'_>> = gone
        .iter()
        .map(|name| outbox::Job {
            kind: "unindex",
            project: id,
            file: Some(name),
            payload: None,
        })
        .chain(index.iter().map(|name| outbox::Job {
            kind: "index",
            project: id,
            file: Some(name),
            payload: None,
        }))
        .collect();
    if jobs.is_empty() {
        return;
    }
    if let Err(e) = outbox::enqueue(dir, &jobs, now_ms) {
        eprintln!("noevia-server: index outbox write failed for a project config: {e:?}");
    }
}

/// document-sources.cjs and uploads.cjs `prune` against the list as saved (#1914: only once it is
/// on disk).
fn prune_after(ctx: Ctx<'_>, dir: &Path, id: &str) -> Result<(), Reply> {
    let done = store::update(dir, ctx.switch, |projects| {
        let Some(p) = store::find_mut(projects, id) else {
            return (false, false);
        };
        files::prune_documents(dir, p);
        files::prune_uploads(dir, p, ctx.now_ms);
        (true, true)
    });
    match done {
        Ok(_) => Ok(()),
        Err(e) => Err(chats::save_failure(&e, SAVE_FAILED)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    const USER: &str = "01234567-89ab-4cde-8fab-0123456789ab";

    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new(projects: &str) -> Fixture {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join("users").join(USER);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("projects.json"), projects).unwrap();
            Fixture { root }
        }

        fn ctx(&self) -> Ctx<'_> {
            Ctx {
                data_dir: self.root.path(),
                user_id: USER,
                switch: server_store::RustProjects::owned(),
                now_ms: 1_700_000_000_000,
            }
        }

        fn project(&self) -> JValue {
            let text = std::fs::read_to_string(
                self.root
                    .path()
                    .join("users")
                    .join(USER)
                    .join("projects.json"),
            )
            .unwrap();
            match wtf16::parse(&text).unwrap().get("projects") {
                JValue::Arr(a) => a[0].clone(),
                _ => panic!(),
            }
        }

        fn raw(&self) -> String {
            std::fs::read_to_string(
                self.root
                    .path()
                    .join("users")
                    .join(USER)
                    .join("projects.json"),
            )
            .unwrap()
        }

        fn jobs(&self) -> Vec<(String, String, Option<String>)> {
            let dir = self.root.path().join("users").join(USER);
            outbox::jobs(&dir)
                .unwrap_or_default()
                .into_iter()
                .map(|j| (j.kind, j.project, j.file))
                .collect()
        }
    }

    fn catalogue() -> Catalogue {
        Catalogue {
            selectable: vec!["core".into(), "project-docs".into()],
            defaults: vec!["core".into()],
            icons: vec!["folder".into(), "book".into()],
            known: None,
        }
    }

    fn env<'a>(
        c: &'a Catalogue,
        provider: &'a dyn Fn(&str) -> bool,
        chat: &'a dyn Fn(&str) -> bool,
    ) -> Env<'a> {
        Env {
            catalogue: Some(c),
            default_provider: "default",
            provider_known: provider,
            chat_model: chat,
        }
    }

    const BASE: &str = r#"{"projects":[{"id":"p1","name":"Base","goal":"g","instructions":"i","pinned":false,"archived":false,"sourceFolders":[],"memories":[],"files":[],"routing":"auto","modes":["chat"],"toolboxes":["core"],"toolsMode":"auto","chats":[],"createdAt":1,"updatedAt":2}]}"#;

    fn run(f: &Fixture, patch: &str) -> (u16, String) {
        let c = catalogue();
        let known = |p: &str| p == "cloud-1";
        let chat = |m: &str| !m.contains("embed");
        let r = post(
            f.ctx(),
            "p1",
            patch.as_bytes(),
            false,
            &env(&c, &known, &chat),
        );
        match r.body {
            crate::Body::Json(t) => (r.status, t),
            crate::Body::Bytes { bytes, .. } => (r.status, String::from_utf8(bytes).unwrap()),
        }
    }

    #[test]
    fn an_unchanged_patch_is_a_read() {
        let f = Fixture::new(BASE);
        let before = f.raw();
        let (status, body) = run(
            &f,
            r#"{"name":"Base","goal":"g","pinned":false,"sourceFolders":[],"routing":"auto"}"#,
        );
        // `routing` is a choice: routingChosen is new, so this one is saved...
        assert_eq!((status, body.as_str()), (200, r#"{"ok":true}"#));
        assert_ne!(f.raw(), before);
        let after = f.raw();
        // ...and asking for the same again changes nothing.
        let (status, body) = run(
            &f,
            r#"{"name":"Base","goal":"g","pinned":false,"sourceFolders":[],"routing":"auto"}"#,
        );
        assert_eq!(
            (status, body.as_str()),
            (200, r#"{"ok":true,"unchanged":true}"#)
        );
        assert_eq!(f.raw(), after, "no write and no timestamp");
        let (_, empty) = run(&f, "{}");
        assert_eq!(empty, r#"{"ok":true,"unchanged":true}"#);
    }

    #[test]
    fn a_change_saves_with_a_new_timestamp_and_the_clips_are_cores() {
        let f = Fixture::new(BASE);
        let long = "n".repeat(300);
        let (status, _) = run(
            &f,
            &format!(
                r#"{{"name":"  {long}  ","goal":"{}","instructions":"{}","pinned":true,"archived":true,"memories":["  a  ","","b",5]}}"#,
                "g".repeat(2100),
                "i".repeat(8100)
            ),
        );
        assert_eq!(status, 200);
        let p = f.project();
        assert_eq!(p.get("name").as_str().map(wtf16::len), Some(120));
        assert_eq!(p.get("goal").as_str().map(wtf16::len), Some(2000));
        assert_eq!(p.get("instructions").as_str().map(wtf16::len), Some(8000));
        assert_eq!(p.get("pinned"), &JValue::Bool(true));
        assert_eq!(p.get("archived"), &JValue::Bool(true));
        assert_eq!(p.get("updatedAt"), &JValue::Num(1_700_000_000_000.0));
        assert_eq!(wtf16::stringify(p.get("memories")).unwrap(), r#"["a","b"]"#);
        // A blank name leaves the name.
        run(&f, r#"{"name":"   "}"#);
        assert_eq!(f.project().get("name").as_str().map(wtf16::len), Some(120));
    }

    #[test]
    fn each_refusal_is_cores_and_nothing_is_saved() {
        let f = Fixture::new(BASE);
        let before = f.raw();
        for (patch, status, message) in [
            ("not json", 400, "invalid JSON"),
            (r#"{"icon":"nope"}"#, 400, "Choose a supported project icon."),
            (r#"{"color":"red"}"#, 400, "Project color must be default or a six-digit hex color."),
            (r#"{"reasoningEffort":"max"}"#, 400, "Invalid reasoning effort"),
            (r#"{"contextProfile":"huge"}"#, 400, "Invalid context profile"),
            (r#"{"modes":["chat","x"]}"#, 400, "modes may only contain chat, cowork, code"),
            (r#"{"modes":"chat"}"#, 400, "modes must be an array"),
            (r#"{"sharedContext":[]}"#, 400, "sharedContext must be an object"),
            (r#"{"sharedContext":{"x":true}}"#, 400, "sharedContext may only contain chat, code"),
            (r#"{"sharedContext":{"chat":"yes"}}"#, 400, "sharedContext.chat must be true or false"),
            (r#"{"routing":"sometimes"}"#, 400, "routing must be 'auto' or 'manual'"),
            (r#"{"routingMode":"cloud"}"#, 400, "routingMode must be an object or null"),
            (r#"{"routingMode":{"mode":"far"}}"#, 400, "routingMode.mode must be 'local', 'cloud' or 'hybrid'"),
            (r#"{"routingMode":{"mode":"local","whenSensitive":null}}"#, 400, "routingMode.whenSensitive must be 'ask' or 'local'"),
            (r#"{"toolsMode":"x"}"#, 400, "toolsMode must be 'auto' or 'manual'"),
            (r#"{"provider":"nobody"}"#, 400, "no such provider"),
            (r#"{"toolboxes":"core"}"#, 400, "toolboxes must be an array of toolbox ids"),
            (r#"{"model":"embed-small"}"#, 400, "embed-small is an embedding, reranking or routing model and cannot be used as a chat model — pick a chat model instead."),
            (r#"null"#, 500, "Internal error"),
        ] {
            let (s, body) = run(&f, patch);
            assert_eq!((s, wtf16::parse(&body).unwrap().get("error").as_str().map(str::to_owned)), (status, Some(message.to_string())), "{patch}");
        }
        assert_eq!(f.raw(), before);
        let c = catalogue();
        let (known, chat) = (|_: &str| false, |_: &str| true);
        assert_eq!(
            post(f.ctx(), "nope", b"{}", false, &env(&c, &known, &chat)).status,
            404
        );
        assert_eq!(
            post(f.ctx(), "p1", b"{}", true, &env(&c, &known, &chat)).status,
            413
        );
        assert_eq!(
            post(f.ctx(), "%E0%A4%A", b"{}", false, &env(&c, &known, &chat)).status,
            500
        );
    }

    #[test]
    fn a_model_is_a_manual_choice_unless_routing_is_sent_and_a_cloud_model_is_not_judged() {
        let f = Fixture::new(BASE);
        assert_eq!(run(&f, r#"{"model":"chat-1"}"#).0, 200);
        let p = f.project();
        assert_eq!(p.get("model").as_str(), Some("chat-1"));
        assert_eq!(p.get("routing").as_str(), Some("manual"));
        assert_eq!(p.get("routingChosen"), &JValue::Bool(true));
        assert_eq!(run(&f, r#"{"model":"chat-2","routing":"auto"}"#).0, 200);
        assert_eq!(f.project().get("routing").as_str(), Some("auto"));
        // On another provider the local catalogue says nothing.
        assert_eq!(
            run(&f, r#"{"model":"embed-x","provider":"cloud-1"}"#).0,
            200
        );
        assert_eq!(f.project().get("provider").as_str(), Some("cloud-1"));
        // The stored provider decides when the patch names none.
        assert_eq!(run(&f, r#"{"model":"embed-y"}"#).0, 200);
        assert_eq!(
            run(&f, r#"{"model":"embed-y","provider":"lemonade"}"#).0,
            400
        );
        assert_eq!(run(&f, r#"{"provider":"lemonade"}"#).0, 200);
    }

    #[test]
    fn the_overrides_are_stored_and_null_clears_them() {
        let f = Fixture::new(BASE);
        let (s, _) = run(
            &f,
            r##"{"routingMode":{"mode":"hybrid"},"sampling":{"temperature":0.5,"top_k":3,"bad":1,"top_p":0},"reasoningEffort":"high","contextProfile":"high","sharedContext":{"code":true},"toolsMode":"manual","toolboxes":["core","core","nope","project-docs"],"icon":"book","color":"#ABCDEF","sourceFolders":["a/b","../x","  ","/abs","c"]}"##,
        );
        assert_eq!(s, 200);
        let p = f.project();
        assert_eq!(
            wtf16::stringify(p.get("routingMode")).unwrap(),
            r#"{"mode":"hybrid","whenSensitive":"ask"}"#
        );
        assert_eq!(
            wtf16::stringify(p.get("sampling")).unwrap(),
            r#"{"temperature":0.5,"top_k":3}"#
        );
        assert_eq!(p.get("reasoningEffort").as_str(), Some("high"));
        assert_eq!(p.get("contextProfile").as_str(), Some("high"));
        assert_eq!(
            wtf16::stringify(p.get("sharedContext")).unwrap(),
            r#"{"chat":false,"code":true}"#
        );
        assert_eq!(
            wtf16::stringify(p.get("toolboxes")).unwrap(),
            r#"["core","project-docs"]"#
        );
        assert_eq!(p.get("color").as_str(), Some("#abcdef"));
        assert_eq!(p.get("icon").as_str(), Some("book"));
        assert_eq!(
            wtf16::stringify(p.get("sourceFolders")).unwrap(),
            r#"["a/b","c"]"#
        );
        let (s, _) = run(
            &f,
            r#"{"routingMode":null,"sampling":null,"reasoningEffort":null,"contextProfile":"low"}"#,
        );
        assert_eq!(s, 200);
        let p = f.project();
        for gone in [
            "routingMode",
            "sampling",
            "reasoningEffort",
            "contextProfile",
        ] {
            assert!(matches!(p.get(gone), JValue::Undefined), "{gone}");
        }
        // An unusable sampling value clears it as well.
        run(&f, r#"{"sampling":{"temperature":1}}"#);
        run(&f, r#"{"sampling":{"temperature":9}}"#);
        assert!(matches!(f.project().get("sampling"), JValue::Undefined));
    }

    fn with_uploads() -> String {
        r#"{"projects":[{"id":"p1","name":"Base","files":[
            {"name":"keep.md","content":"kept"},
            {"name":"drop.md","content":"dropped"},
            {"name":"folder.md","content":"f","source":"shared/docs"},
            {"name":"doc.pdf","content":"","document":{"version":"v"}}],
          "assets":[{"id":"a1","sourceName":"drop.md"},{"id":"a2","sourceName":"keep.md"},{"id":"a3"}]}]}"#.to_string()
    }

    #[test]
    fn the_upload_list_keeps_what_has_no_text_in_the_patch_and_queues_the_index() {
        let f = Fixture::new(&with_uploads());
        let (s, _) = run(
            &f,
            r#"{"files":[{"name":"keep.md"},{"name":"new.md","content":"fresh"},{"name":"unknown.md"},{"name":"doc.pdf","content":"ignored"},{"name":"folder.md","content":"x","source":"s"},"junk",null]}"#,
        );
        assert_eq!(s, 200);
        let p = f.project();
        let names: Vec<String> = match p.get("files") {
            JValue::Arr(a) => a
                .iter()
                .map(|f| f.get("name").as_str().unwrap().to_string())
                .collect(),
            _ => panic!(),
        };
        // Folder-derived first, then the uploads in the patch's order; the unknown name is dropped.
        assert_eq!(names, vec!["folder.md", "keep.md", "new.md", "doc.pdf"]);
        let assets = wtf16::stringify(p.get("assets")).unwrap();
        assert_eq!(
            assets,
            r#"[{"id":"a2","sourceName":"keep.md"},{"id":"a3"}]"#
        );
        // The new text is in its own file and the project file points at it.
        let new = match p.get("files") {
            JValue::Arr(a) => a[2].clone(),
            _ => panic!(),
        };
        assert!(
            new.get("textRef").as_str().is_some()
                && matches!(new.get("content"), JValue::Undefined)
        );
        let mut jobs = f.jobs();
        jobs.sort();
        assert_eq!(
            jobs,
            vec![
                (
                    "index".to_string(),
                    "p1".to_string(),
                    Some("new.md".to_string())
                ),
                (
                    "unindex".to_string(),
                    "p1".to_string(),
                    Some("drop.md".to_string())
                ),
            ]
        );
    }

    #[test]
    fn an_edit_of_an_upload_with_the_same_text_queues_nothing_but_a_new_text_does() {
        let f = Fixture::new(&with_uploads());
        let (s, _) = run(
            &f,
            r#"{"files":[{"name":"keep.md","content":"kept"},{"name":"drop.md","content":"dropped"},{"name":"doc.pdf"}]}"#,
        );
        assert_eq!(s, 200);
        assert_eq!(f.jobs(), vec![], "same texts: nothing to index");
        let (s, _) = run(
            &f,
            r#"{"files":[{"name":"keep.md","content":"edited"},{"name":"drop.md"},{"name":"doc.pdf"}]}"#,
        );
        assert_eq!(s, 200);
        assert_eq!(
            f.jobs(),
            vec![(
                "index".to_string(),
                "p1".to_string(),
                Some("keep.md".to_string())
            )]
        );
    }

    #[test]
    fn a_published_skill_keeps_its_provenance_only_while_its_bytes_survive() {
        let origin = r#""skillOrigin":{"kind":"published","digest":"d"}"#;
        let f = Fixture::new(&format!(
            r#"{{"projects":[{{"id":"p1","name":"B","files":[{{"name":"s/SKILL.md","content":"same",{origin}}}]}}]}}"#
        ));
        run(&f, r#"{"files":[{"name":"s/SKILL.md","content":"same"}]}"#);
        let kept = f.project();
        let file = match kept.get("files") {
            JValue::Arr(a) => a[0].clone(),
            _ => panic!(),
        };
        assert!(
            file.get("skillOrigin").truthy(),
            "unchanged bytes keep the origin"
        );
        run(
            &f,
            r#"{"files":[{"name":"s/SKILL.md","content":"edited"}]}"#,
        );
        let file = match f.project().get("files") {
            JValue::Arr(a) => a[0].clone(),
            _ => panic!(),
        };
        assert!(
            matches!(file.get("skillOrigin"), JValue::Undefined),
            "edited bytes lose it"
        );
    }

    #[test]
    fn the_account_deleted_meanwhile_is_410_and_nothing_is_recreated() {
        let f = Fixture::new(BASE);
        std::fs::remove_dir_all(f.root.path().join("users").join(USER)).unwrap();
        let (s, body) = run(&f, r#"{"pinned":true}"#);
        assert_eq!(s, 410, "{body}");
        assert!(!f.root.path().join("users").join(USER).exists());
    }
}
