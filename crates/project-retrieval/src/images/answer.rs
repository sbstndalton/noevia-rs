//! What the chat model gets for the retrieved images of a turn: multimodal-rag.cjs `answerImages`
//! and `captionOnce`, and the chat.cjs steps around them (who may caption, the caption request and
//! its reply, the notes and parts put on the wire, the warning).
//!
//! - an off-box turn (#1735) gets their names only, never their bytes or text read from them;
//! - a local model that can see gets the images (at most [`MAX_IMAGES`], [`MAX_TOTAL_BYTES`]);
//! - one that cannot gets each image's own text, else a caption made once by the Vision role model
//!   and cached with the image, else its name, said plainly.
//!
//! The model calls, the vision probe, the caption cache and the framing are the caller's
//! ([`Ports`]); this module decides only. Named difference from Node: the names in the two notes
//! are quoted with [`crate::names::quote_name`] (quoted-name.cjs, #1588), where `answerImages`
//! uses a bare `JSON.stringify` that leaves U+2028/U+2029 and the bidirectional marks raw in the
//! system text (noevia#2802 item 2).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use js_json::JValue;

use super::{js_str, js_trim, known_mime, part, Resolved, MAX_BYTES, UNDEFINED};
use crate::names::{quote_name, quoted_names};

/// `answerImages`' `maxImages` default.
pub const MAX_IMAGES: usize = 3;
/// `answerImages`' `maxTotalBytes` default.
pub const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
/// An image's own text is cut to this many UTF-16 code units in its note.
pub const MAX_TEXT_UNITS: usize = 8000;
/// chat.cjs's caption request: `max_tokens`.
pub const CAPTION_MAX_TOKENS: u32 = 1024;
/// chat.cjs's caption reply cap (`readCappedJson(r, 4 MB)`).
pub const CAPTION_REPLY_MAX_BYTES: usize = 4 * 1024 * 1024;
/// The instruction of chat.cjs's caption request.
pub const DESCRIBE_PROMPT: &str = "Describe this image in detail, so someone who cannot see it can answer questions about it. Transcribe any text exactly, including numbers, labels and headings. If text is unclear, say so rather than guessing.";

/// A caption and the model that made it (`{ caption, model }`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caption {
    pub caption: String,
    pub model: Option<String>,
}

/// What the turn decides for its retrieved images (`answerImages`' answer).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Decided {
    /// `image_url` parts for the last user message.
    pub parts: Vec<JValue>,
    /// System-prompt blocks.
    pub notes: Vec<String>,
    /// What the user is told; empty when there is nothing to say.
    pub warning: String,
    /// The names of the images sent as parts.
    pub sent: Vec<String>,
}

/// What `answerImages` asks of its caller.
pub trait Ports {
    /// `canSee()`: whether the answering model reads images (the vision probe). Called once, only
    /// when the turn is on the box and has images; a failed probe is `false`.
    fn can_see(&self) -> impl Future<Output = bool>;
    /// `caption(image)`: a caption for an image with no text of its own (usually
    /// [`Captions::once`] over the cache and the Vision role model); a failure is `None`.
    fn caption(&self, image: &Resolved) -> impl Future<Output = Option<Caption>>;
    /// `image.bytes()`; `None` where Node's read throws.
    fn bytes(&self, image: &Resolved) -> Option<Vec<u8>> {
        image.bytes.read()
    }
    /// prompt-framing.cjs `frameUntrusted(kind, label, text)`.
    fn frame(&self, kind: &str, label: &str, text: &str) -> String;
}

/// The turn facts `answerImages` takes besides its ports.
#[derive(Clone, Copy, Debug)]
pub struct Turn<'a> {
    /// The answer goes off the box (`offBoxTurn`: an external or non-default provider, a cloud
    /// route), decided by core from the resolved route, never by the client.
    pub external: bool,
    /// `provider.label || 'an external provider'`.
    pub provider_label: &'a str,
    /// Whether a Vision role model is configured (only the wording changes).
    pub has_vision_role: bool,
    pub max_images: usize,
    pub max_total_bytes: usize,
}

impl<'a> Turn<'a> {
    /// The defaults chat.cjs uses.
    pub fn new(external: bool, provider_label: &'a str, has_vision_role: bool) -> Self {
        Turn {
            external,
            provider_label,
            has_vision_role,
            max_images: MAX_IMAGES,
            max_total_bytes: MAX_TOTAL_BYTES,
        }
    }
}

/// chat.cjs: `provider.label || 'an external provider'`.
pub fn provider_label(label: &JValue) -> String {
    if label.truthy() {
        js_str(label)
    } else {
        "an external provider".to_owned()
    }
}

fn quote_list(names: &[String]) -> String {
    names
        .iter()
        .map(|n| quote_name(n))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `answerImages({ images, external, providerLabel, canSee, caption, hasVisionRole, frame,
/// maxImages, maxTotalBytes })`.
pub async fn answer_images<P: Ports>(images: &[Resolved], turn: Turn<'_>, ports: &P) -> Decided {
    let mut out = Decided::default();
    if images.is_empty() {
        return out;
    }
    let label = turn.provider_label;
    if turn.external {
        let names: Vec<String> = images.iter().map(|i| i.name.clone()).collect();
        out.notes.push(format!(
            "Images in this project look relevant to this message ({}) but were not sent: images, and text read from them, are never sent to {label} automatically. Say so if asked about them; do not guess at their contents.",
            quote_list(&names)
        ));
        out.warning = format!(
            "Relevant project images were not sent: images are never sent to {label} automatically."
        );
        return out;
    }
    let seeing = ports.can_see().await;
    let mut as_text = Vec::new();
    let mut budget = turn.max_total_bytes;
    for image in images {
        if seeing && out.parts.len() < turn.max_images {
            if let Some(bytes) = ports.bytes(image) {
                if bytes.len() <= MAX_BYTES && bytes.len() <= budget && known_mime(&image.mime) {
                    budget -= bytes.len();
                    out.parts
                        .push(part(image.mime.as_str().unwrap_or_default(), &bytes));
                    out.sent.push(image.name.clone());
                    continue;
                }
            }
        }
        as_text.push(image);
    }
    let mut name_only = Vec::new();
    for image in as_text {
        if !image.text.is_empty() {
            let text = js_json::js_slice(&image.text, MAX_TEXT_UNITS);
            out.notes
                .push(ports.frame("image text", &image.name, &text));
            continue;
        }
        match ports.caption(image).await {
            Some(made) if !made.caption.is_empty() => {
                let label = match made.model.as_deref() {
                    Some(model) if !model.is_empty() => {
                        format!("{} (caption by {model})", image.name)
                    }
                    _ => image.name.clone(),
                };
                out.notes
                    .push(ports.frame("image caption", &label, &made.caption));
            }
            _ => name_only.push(image.name.clone()),
        }
    }
    if !name_only.is_empty() {
        let why = if turn.has_vision_role {
            "the Vision model could not caption them"
        } else {
            "no Vision model is configured to caption them"
        };
        out.notes.push(format!(
            "Images in this project look relevant to this message ({}), but only their file names are known: the answering model cannot see images and {why}. Say so if asked about them; do not guess at their contents.",
            quote_list(&name_only)
        ));
        out.warning = format!(
            "Only the file names of {} were used: {why}.",
            if name_only.len() == 1 {
                "a relevant image"
            } else {
                "relevant images"
            }
        );
    }
    out
}

/// chat.cjs `captionImage`'s first test: whether a caption may be made at all. `vision_model` =
/// a Vision role model is configured; `off_box` = `offBoxTurn(...)`; `provider` = the caption
/// provider's id and whether egress counts it external (`None` = no caption provider);
/// `remote` = the Vision role is on another machine (`chatMachines.target('vision')`, #1840).
pub fn may_caption(
    vision_model: bool,
    off_box: bool,
    provider: Option<(&str, bool)>,
    remote: bool,
    default_provider_id: &str,
) -> bool {
    let Some((id, external)) = provider else {
        return false;
    };
    vision_model && !off_box && (id == default_provider_id || remote) && !external
}

/// chat.cjs: the caption's in-flight key, `JSON.stringify([uid, project.id, image.hash])`.
pub fn caption_key(user_id: &JValue, project_id: &JValue, hash: &JValue) -> String {
    js_json::stringify(&JValue::Arr(vec![
        user_id.clone(),
        project_id.clone(),
        hash.clone(),
    ]))
    .unwrap_or_default()
}

/// chat.cjs: the caption request's body to `<provider>/v1/chat/completions` (key order as Node's).
pub fn caption_request(model: &str, mime: &str, bytes: &[u8]) -> JValue {
    JValue::obj([
        ("model", JValue::from(model)),
        ("max_tokens", JValue::Num(f64::from(CAPTION_MAX_TOKENS))),
        (
            "chat_template_kwargs",
            JValue::obj([("enable_thinking", JValue::Bool(false))]),
        ),
        (
            "messages",
            JValue::Arr(vec![JValue::obj([
                ("role", JValue::from("user")),
                (
                    "content",
                    JValue::Arr(vec![
                        JValue::obj([
                            ("type", JValue::from("text")),
                            ("text", JValue::from(DESCRIBE_PROMPT)),
                        ]),
                        super::part(mime, bytes),
                    ]),
                ),
            ])]),
        ),
    ])
}

/// chat.cjs: the caption in a reply (`String(out?.choices?.[0]?.message?.content || '').trim()`),
/// `None` when empty or cut by the token limit (`finish_reason === 'length'`).
pub fn caption_reply(out: &JValue) -> Option<String> {
    let first = match out.get("choices") {
        JValue::Arr(items) => items.first().unwrap_or(&UNDEFINED),
        _ => &UNDEFINED,
    };
    let content = first.get("message").get("content");
    let text = if content.truthy() {
        js_trim(&js_str(content))
    } else {
        String::new()
    };
    if text.is_empty() || first.get("finish_reason").as_str() == Some("length") {
        return None;
    }
    Some(text)
}

/// chat.cjs after `answerImages`: the notes join every system message (or open the wire as one),
/// the parts follow the last user message under a "(Retrieved images: ...)" label, and the warning
/// joins the turn's vision warning.
pub fn apply(wire: &mut Vec<JValue>, decided: &Decided, vision_warning: &mut String) {
    if !decided.notes.is_empty() {
        let note = decided.notes.join("\n\n");
        let is_system = |m: &JValue| m.get("role").as_str() == Some("system");
        if wire.iter().any(is_system) {
            for m in wire.iter_mut().filter(|m| is_system(m)) {
                let content = format!("{}\n\n{note}", js_str(m.get("content")));
                set(m, "content", JValue::Str(content));
            }
        } else {
            wire.insert(
                0,
                JValue::obj([
                    ("role", JValue::from("system")),
                    ("content", JValue::Str(note)),
                ]),
            );
        }
    }
    if !decided.parts.is_empty() {
        if let Some(last) = wire
            .iter_mut()
            .rev()
            .find(|m| m.get("role").as_str() == Some("user"))
        {
            let names: Vec<&str> = decided.sent.iter().map(String::as_str).collect();
            let label = format!("(Retrieved images: {})", quoted_names(&names));
            let content = match last.get("content") {
                JValue::Arr(items) => {
                    let mut items = items.clone();
                    items.push(text_part(label));
                    items.extend(decided.parts.iter().cloned());
                    items
                }
                other => {
                    let text = other.as_str().unwrap_or_default();
                    let mut items = vec![text_part(format!("{text}\n\n{label}"))];
                    items.extend(decided.parts.iter().cloned());
                    items
                }
            };
            set(last, "content", JValue::Arr(content));
        }
    }
    join_warning(vision_warning, &decided.warning);
}

/// `visionWarning += `${visionWarning ? ' ' : ''}${warning}`` when there is a warning.
pub fn join_warning(acc: &mut String, warning: &str) {
    if warning.is_empty() {
        return;
    }
    if !acc.is_empty() {
        acc.push(' ');
    }
    acc.push_str(warning);
}

fn text_part(text: String) -> JValue {
    JValue::obj([("type", JValue::from("text")), ("text", JValue::Str(text))])
}

/// `{ ...m, key: value }` / `m.key = value`: an existing key keeps its place.
fn set(m: &mut JValue, key: &str, value: JValue) {
    if let JValue::Obj(items) = m {
        match items.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => *v = value,
            None => items.push((key.to_owned(), value)),
        }
    }
}

/// `captionOnce`'s in-flight table: one caption per key at a time; a second request for the same
/// key while the first runs waits for it and gets its answer. Shared by every turn of the process.
#[derive(Default)]
pub struct Captions {
    flights: Mutex<HashMap<String, Arc<Flight>>>,
}

#[derive(Default)]
struct Flight {
    state: Mutex<FlightState>,
}

#[derive(Default)]
struct FlightState {
    done: Option<Option<Caption>>,
    wakers: Vec<Waker>,
}

impl Flight {
    fn finish(&self, answer: Option<Caption>) {
        let wakers = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.done.is_some() {
                return;
            }
            state.done = Some(answer);
            std::mem::take(&mut state.wakers)
        };
        for w in wakers {
            w.wake();
        }
    }
}

/// Waits for a flight another caller leads.
struct Follow(Arc<Flight>);

impl Future for Follow {
    type Output = Option<Caption>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        match &state.done {
            Some(answer) => Poll::Ready(answer.clone()),
            None => {
                if !state.wakers.iter().any(|w| w.will_wake(cx.waker())) {
                    state.wakers.push(cx.waker().clone());
                }
                Poll::Pending
            }
        }
    }
}

/// The leader's end of a flight: it always finishes it and leaves the table, also when the
/// leading future is dropped mid-way (its followers then get `None`, as Node's waiters get the
/// rejection that `captionImage`'s caller turns into a name-only image).
struct Lead<'a> {
    table: &'a Captions,
    key: String,
    flight: Arc<Flight>,
}

impl Drop for Lead<'_> {
    fn drop(&mut self) {
        self.flight.finish(None);
        let mut flights = self
            .table
            .flights
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if flights
            .get(&self.key)
            .is_some_and(|f| Arc::ptr_eq(f, &self.flight))
        {
            flights.remove(&self.key);
        }
    }
}

impl Captions {
    /// `captionOnce(key, { read, generate, write })`: the cached caption (`read`), or, when none,
    /// the answer of the caption already being made for `key`, or a new one from `generate`,
    /// trimmed and stored with `write` (a failed store is the writer's to swallow: the caption is
    /// still used for this answer).
    pub async fn once<R, G, F, W>(
        &self,
        key: &str,
        read: R,
        generate: G,
        write: W,
    ) -> Option<Caption>
    where
        R: FnOnce() -> Option<Caption>,
        G: FnOnce() -> F,
        F: Future<Output = Option<Caption>>,
        W: FnOnce(&str, Option<&str>),
    {
        if let Some(cached) = read().filter(|c| !c.caption.is_empty()) {
            return Some(cached);
        }
        let lead = {
            let mut flights = self.flights.lock().unwrap_or_else(PoisonError::into_inner);
            match flights.get(key) {
                Some(flight) => Err(Follow(Arc::clone(flight))),
                None => {
                    let flight = Arc::new(Flight::default());
                    flights.insert(key.to_owned(), Arc::clone(&flight));
                    Ok(Lead {
                        table: self,
                        key: key.to_owned(),
                        flight,
                    })
                }
            }
        };
        let lead = match lead {
            Ok(lead) => lead,
            Err(follow) => return follow.await,
        };
        let answer = match generate().await {
            Some(made) => {
                let caption = js_trim(&made.caption);
                if caption.is_empty() {
                    None
                } else {
                    let model = made.model.filter(|m| !m.is_empty());
                    write(&caption, model.as_deref());
                    Some(Caption { caption, model })
                }
            }
            None => None,
        };
        lead.flight.finish(answer.clone());
        drop(lead);
        answer
    }

    /// How many captions are being made now (for tests and the turn's diagnostics).
    pub fn in_flight(&self) -> usize {
        self.flights
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[cfg(test)]
mod tests;
