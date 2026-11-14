//! Retrieved project images for a chat turn (#1726 multimodal retrieval, the answer side), ported
//! from noevia-core `server/multimodal-rag.cjs` (`resolveHit`, `assetPlan`, the image content part)
//! and the selection block of `server/chat.cjs` that uses them (the asset plan, the images named in
//! the question, the ride-along set, the retrieved images as they are now). What the chat model
//! then gets for them is [`answer`].
//!
//! The vectors themselves are the embedding worker's (`embedding-worker` media passes, the port of
//! `mediaItems`/`syncProjectMedia`); the search that finds them is [`crate::search`]. Nothing here
//! reads an index, embeds or calls a model, and nothing calls this module yet: it is the native
//! half of chat preparation's `Stopped::Images` hand-off (track R8 owns the caller).
//!
//! Values are [`JValue`]s, as `JSON.parse` gave them to Node, so its loose reads (`a && a.id`,
//! template literals, `String(x)`) are the same here. Named differences from Node:
//! - an asset id that is not a plain file name (`chat_prepare` `plain_asset_name`, #2595) is never
//!   read: its bytes are missing, as for a file that is gone. Node's `resolveHit` joins any id to
//!   the asset directory (noevia#2802 item 1).
//! - asset ids are compared by value; Node's `Set` compares objects and arrays by reference (ids
//!   are strings everywhere the server writes them).
//! - a `null` (or non-object) entry in the project's assets is read as having no id and no name
//!   by the question-name rule, the ride-along filter and the unattached names, where chat.cjs
//!   reads `null.id` and the turn fails (noevia#2802 item 3); a non-array `assets` or `files` is empty here, where
//!   Node's `.filter`/`.find` throws.
//! - a non-string `figureFile` or asset id that Node's `path.join` would throw on: the figure is
//!   not resolved (`None`), the asset resolves with its bytes refused; Node fails the turn.
//! - a name or a template part that `String()` refuses (an object with an own `toString`, never
//!   written by the server) is `[object Object]` here; Node throws.

pub mod answer;

use std::path::{Path, PathBuf};

use js_json::{to_js_string, wtf16, JValue};
use prompt_framing::js::{includes, to_lower, trim, units};

/// A borrowable `undefined` for reads that fall back to it.
pub(crate) static UNDEFINED: JValue = JValue::Undefined;

/// `MODALITIES.image.maxBytes`: an image above this is stored but never embedded or sent.
pub const MAX_BYTES: usize = 8 * 1024 * 1024;

/// `Object.values(MODALITIES.image.mimes)`.
pub const MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Whether `mime` is one of [`MIMES`] (`Object.values(mimes).includes(mime)`, strict).
pub fn known_mime(mime: &JValue) -> bool {
    mime.as_str().is_some_and(|m| MIMES.contains(&m))
}

/// `MODALITIES.image.part(mime, bytes)`: the content part the chat model takes for an image.
pub fn part(mime: &str, bytes: &[u8]) -> JValue {
    JValue::obj([
        ("type", JValue::from("image_url")),
        (
            "image_url",
            JValue::obj([(
                "url",
                JValue::Str(format!("data:{mime};base64,{}", base64(bytes))),
            )]),
        ),
    ])
}

/// `Buffer.toString('base64')`: standard alphabet, padded.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let sym = |n: u32| char::from(ALPHABET.get((n & 63) as usize).copied().unwrap_or(b'A'));
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk.first().copied().unwrap_or(0));
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(sym(n >> 18));
        out.push(sym(n >> 12));
        out.push(if chunk.len() > 1 { sym(n >> 6) } else { '=' });
        out.push(if chunk.len() > 2 { sym(n) } else { '=' });
    }
    out
}

/// `FIGURE_FILE`: `/^[0-9a-f]{64}\.fig\d{1,3}\.(png|jpg|webp)$/`.
pub fn figure_file(name: &str) -> bool {
    let Some((hex, rest)) = name.split_at_checked(64) else {
        return false;
    };
    if !hex
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return false;
    }
    let Some(rest) = rest.strip_prefix(".fig") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=3).contains(&digits) {
        return false;
    }
    matches!(rest.get(digits..), Some(".png" | ".jpg" | ".webp"))
}

/// An id `uploads.cjs` could have minted for the asset directory: a plain file name (the same rule
/// as chat preparation's ride-along read, #2595). Anything else is never joined to a directory.
pub fn plain_asset_name(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains(['/', '\\', '\0'])
        && !id.chars().any(|c| wtf16::lone_unit(c).is_some())
}

/// `String(v)` for a template literal or a name; see the module note for the refused case.
pub fn js_str(v: &JValue) -> String {
    to_js_string(v).unwrap_or_else(|_| "[object Object]".to_owned())
}

/// `String(s).trim()` with JavaScript's white space.
pub fn js_trim(s: &str) -> String {
    String::from_utf16_lossy(trim(&units(s)))
}

/// `baseName(name)`: `String(name || '').split('/').pop()`.
pub fn base_name(name: &JValue) -> String {
    let s = if name.truthy() {
        js_str(name)
    } else {
        String::new()
    };
    s.rsplit('/').next().unwrap_or_default().to_owned()
}

/// `a === b` for values that came out of `JSON.parse`: objects and arrays are never the same
/// object, so only primitives compare equal.
pub fn strict_eq(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Undefined, JValue::Undefined) | (JValue::Null, JValue::Null) => true,
        (JValue::Bool(x), JValue::Bool(y)) => x == y,
        (JValue::Num(x), JValue::Num(y)) => x == y,
        (JValue::Str(x), JValue::Str(y)) => x == y,
        _ => false,
    }
}

/// `value || []` read as an array; a non-array is empty (see the module note).
fn list(v: &JValue) -> &[JValue] {
    match v {
        JValue::Arr(items) => items,
        _ => &[],
    }
}

/// A JavaScript `Set` of ids, in insertion order, compared by value (see the module note).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IdSet(Vec<JValue>);

impl IdSet {
    pub fn has(&self, id: &JValue) -> bool {
        self.0.iter().any(|x| same_value_zero(x, id))
    }

    pub fn add(&mut self, id: JValue) {
        if !self.has(&id) {
            self.0.push(id);
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &JValue> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<JValue> for IdSet {
    fn from_iter<I: IntoIterator<Item = JValue>>(iter: I) -> Self {
        let mut set = IdSet::default();
        for id in iter {
            set.add(id);
        }
        set
    }
}

fn same_value_zero(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Arr(_) | JValue::Obj(_), _) | (_, JValue::Arr(_) | JValue::Obj(_)) => a == b,
        _ => strict_eq(a, b),
    }
}

/// `h?.meta?.assetId`.
fn asset_id_of(hit: &JValue) -> &JValue {
    hit.get("meta").get("assetId")
}

/// `assetPlan(assets, { hits, indexed })`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AssetPlan {
    /// Assets with no vector yet: they ride along with the turn as before #1726.
    pub ride_along: IdSet,
    /// Assets retrieval returned for this question.
    pub retrieved: IdSet,
    /// Assets retrieval can see (they have a vector).
    pub indexed: IdSet,
}

/// What `onImages` handed the turn: `searchProjectWithImages`' image hits and every image row of
/// the index (`indexed`, score 0).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImageRetrieval {
    pub hits: Vec<JValue>,
    pub indexed: Vec<JValue>,
}

/// `assetPlan(assets, { hits, indexed })`: an image retrieval can see is sent only when retrieved;
/// one it cannot see yet rides along.
pub fn asset_plan(assets: &JValue, retrieval: &ImageRetrieval) -> AssetPlan {
    let indexed: IdSet = retrieval
        .indexed
        .iter()
        .map(asset_id_of)
        .filter(|id| id.truthy())
        .cloned()
        .collect();
    let retrieved: IdSet = retrieval
        .hits
        .iter()
        .map(asset_id_of)
        .filter(|id| id.truthy())
        .cloned()
        .collect();
    let ride_along: IdSet = list(assets)
        .iter()
        .filter(|a| a.truthy() && !indexed.has(a.get("id")))
        .map(|a| a.get("id").clone())
        .collect();
    AssetPlan {
        ride_along,
        retrieved,
        indexed,
    }
}

/// Where a resolved image's bytes are, read only when it is sent or captioned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bytes {
    /// A file of the project's asset or document directory.
    File(PathBuf),
    /// The id is not a plain file name: never read (the module note).
    Refused,
}

impl Bytes {
    /// `image.bytes()`: `None` where Node's `readFileSync` throws.
    pub fn read(&self) -> Option<Vec<u8>> {
        match self {
            Bytes::File(path) => std::fs::read(path).ok(),
            Bytes::Refused => None,
        }
    }
}

/// `resolveHit(...)`'s answer: a retrieved image as the project holds it now.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    /// What the model and the user are told it is called.
    pub name: String,
    /// The project source it belongs to (`file.name`).
    pub file: JValue,
    /// The hit's mime type (sent only when one of [`MIMES`]).
    pub mime: JValue,
    /// `img:` + the image bytes' sha256 (the caption cache key and the de-duplication key).
    pub hash: JValue,
    pub asset_id: Option<JValue>,
    pub score: JValue,
    pub bytes: Bytes,
    /// What was read from the image itself (Docling/OCR), empty when nothing was.
    pub text: String,
}

/// The two directories `resolveHit` reads: the project's asset directory
/// (`workspace.assetDir(project.id)`) and its document directory (`documentDir(project.id)`,
/// `None` where the caller passes no `documentDir` function).
#[derive(Clone, Copy, Debug)]
pub struct Dirs<'a> {
    pub assets: &'a Path,
    pub documents: Option<&'a Path>,
}

/// `resolveHit(workspace, project, hit, { documentDir })`: the hit resolved against the project as
/// it is now, or `None` when its source is gone or has changed since it was indexed.
pub fn resolve_hit(project: &JValue, hit: &JValue, dirs: Dirs<'_>) -> Option<Resolved> {
    let meta = hit.get("meta");
    let file = list(project.get("files"))
        .iter()
        .find(|f| f.truthy() && strict_eq(f.get("name"), hit.get("file")))?;
    let asset_id = meta.get("assetId");
    if asset_id.truthy() {
        let asset = list(project.get("assets"))
            .iter()
            .find(|a| a.truthy() && strict_eq(a.get("id"), asset_id))?;
        let expected = format!("img:{}", js_str(file.get("attachment").get("id")));
        if hit.get("hash").as_str() != Some(expected.as_str()) {
            return None;
        }
        let id = asset.get("id");
        let bytes = match id.as_str() {
            Some(id) if plain_asset_name(id) => Bytes::File(dirs.assets.join(id)),
            _ => Bytes::Refused,
        };
        let name = if asset.get("name").truthy() {
            js_str(asset.get("name"))
        } else {
            base_name(file.get("name"))
        };
        let content = file.get("content");
        let text = if content.truthy() {
            js_trim(&js_str(content))
        } else {
            String::new()
        };
        return Some(Resolved {
            name,
            file: file.get("name").clone(),
            mime: asset.get("mime").clone(),
            hash: hit.get("hash").clone(),
            asset_id: Some(id.clone()),
            score: hit.get("score").clone(),
            bytes,
            text,
        });
    }
    let figure = meta.get("figureFile");
    if figure.truthy() {
        let documents = dirs.documents?;
        if !figure_file(&js_str(figure))
            || !strict_eq(
                file.get("document").get("availableVersion"),
                meta.get("version"),
            )
        {
            return None;
        }
        // path.join takes a string only (the module note).
        let figure = figure.as_str()?;
        let text: Vec<&str> = [meta.get("caption"), meta.get("text")]
            .into_iter()
            .filter_map(JValue::as_str)
            .filter(|t| !trim(&units(t)).is_empty())
            .collect();
        return Some(Resolved {
            name: format!(
                "{} page {} figure {}",
                base_name(file.get("name")),
                js_str(meta.get("page")),
                js_str(meta.get("figure"))
            ),
            file: file.get("name").clone(),
            mime: meta.get("mime").clone(),
            hash: hit.get("hash").clone(),
            asset_id: None,
            score: hit.get("score").clone(),
            bytes: Bytes::File(documents.join(figure)),
            text: js_trim(&text.join("\n")),
        });
    }
    None
}

/// What the chat.cjs selection block decides for the turn's project images.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selection {
    /// `assetPlan`, extended by the images the question names (with retrieval on and no
    /// `turnImageIds`).
    pub plan: Option<AssetPlan>,
    /// `namedImageIds`: the assets attached the pre-#1726 way (`turnImageIds`, else the plan's
    /// ride-along set); `None` = every project image rides along.
    pub named: Option<IdSet>,
    /// Indexes into the project's assets of the images that ride along (`projectImages.filter(a
    /// => !namedImageIds || namedImageIds.has(a.id))`), in order.
    pub ride_along: Vec<usize>,
    /// `unattachedImageNames`: images neither attached nor retrieved (named to the model only).
    pub unattached_names: Vec<String>,
    /// `retrievedImages`: resolved, best first, each image once.
    pub retrieved: Vec<Resolved>,
}

/// The chat.cjs block from "A project's image sources ride along" to the end of
/// `retrievedImages`. `multimodal` is `NOEVIA_MULTIMODAL_RAG` (multimodal-rag.cjs `enabled()`),
/// `retrieval` what `onImages` handed the turn (`None` when it was not called), `turn_image_ids`
/// the request's `turnImageIds` (chat-mode.cjs checked it), `message` the user's text.
pub fn select(
    project: &JValue,
    message: &JValue,
    turn_image_ids: &JValue,
    multimodal: bool,
    retrieval: Option<&ImageRetrieval>,
    dirs: Dirs<'_>,
) -> Selection {
    let images = list(project.get("assets"));
    let retrieval = retrieval.filter(|_| multimodal);
    let explicit = matches!(turn_image_ids, JValue::Arr(_));
    let mut plan = match retrieval {
        Some(r) if !explicit => Some(asset_plan(project.get("assets"), r)),
        _ => None,
    };
    if let Some(plan) = plan.as_mut() {
        let q = if message.truthy() {
            to_lower(&units(&js_str(message)))
        } else {
            Vec::new()
        };
        for a in images.iter().filter(|a| !a.is_nullish()) {
            let name = a.get("name");
            if plan.indexed.has(a.get("id"))
                && name.truthy()
                && includes(&q, &to_lower(&units(&js_str(name))))
            {
                plan.retrieved.add(a.get("id").clone());
            }
        }
    }
    let named: Option<IdSet> = if explicit {
        Some(list(turn_image_ids).iter().cloned().collect())
    } else {
        plan.as_ref().map(|p| p.ride_along.clone())
    };
    let unattached_names = match &named {
        Some(named) => images
            .iter()
            .filter(|a| {
                !named.has(a.get("id"))
                    && !plan.as_ref().is_some_and(|p| p.retrieved.has(a.get("id")))
            })
            .map(|a| js_str(a.get("name")))
            .collect(),
        None => Vec::new(),
    };
    let ride_along = images
        .iter()
        .enumerate()
        .filter(|(_, a)| named.as_ref().is_none_or(|n| n.has(a.get("id"))))
        .map(|(i, _)| i)
        .collect();
    let mut retrieved = Vec::new();
    if let Some(r) = retrieval {
        retrieved = retrieved_images(project, r, plan.as_ref(), named.as_ref(), dirs);
    }
    Selection {
        plan,
        named,
        ride_along,
        unattached_names,
        retrieved,
    }
}

/// `retrievedImages`: the hits, then the images the question named that retrieval did not
/// return (their index row, its hash taken from the source now), resolved; an image this turn
/// already attaches is not repeated, nor one already listed.
fn retrieved_images(
    project: &JValue,
    retrieval: &ImageRetrieval,
    plan: Option<&AssetPlan>,
    named: Option<&IdSet>,
    dirs: Dirs<'_>,
) -> Vec<Resolved> {
    let by_asset: IdSet = retrieval
        .hits
        .iter()
        .map(asset_id_of)
        .filter(|id| id.truthy())
        .cloned()
        .collect();
    let mut hits: Vec<JValue> = retrieval.hits.clone();
    if let Some(plan) = plan {
        for id in plan.retrieved.iter() {
            if by_asset.has(id) {
                continue;
            }
            let Some(h) = retrieval
                .indexed
                .iter()
                .find(|x| strict_eq(asset_id_of(x), id))
            else {
                continue;
            };
            let attachment = list(project.get("files"))
                .iter()
                .find(|f| f.truthy() && strict_eq(f.get("name"), h.get("file")))
                .map_or(&UNDEFINED, |f| f.get("attachment").get("id"));
            hits.push(with_hash(h, format!("img:{}", js_str(attachment))));
        }
    }
    let mut seen = IdSet::default();
    let mut out = Vec::new();
    for h in &hits {
        let id = asset_id_of(h);
        if id.truthy() && named.is_some_and(|n| n.has(id)) {
            continue;
        }
        if let Some(r) = resolve_hit(project, h, dirs) {
            if !seen.has(&r.hash) {
                seen.add(r.hash.clone());
                out.push(r);
            }
        }
    }
    out
}

/// `{ ...h, hash }`.
fn with_hash(h: &JValue, hash: String) -> JValue {
    let mut items = match h {
        JValue::Obj(items) => items.clone(),
        _ => Vec::new(),
    };
    match items.iter_mut().find(|(k, _)| k == "hash") {
        Some((_, v)) => *v = JValue::Str(hash),
        None => items.push(("hash".to_owned(), JValue::Str(hash))),
    }
    JValue::Obj(items)
}

#[cfg(test)]
mod tests;
