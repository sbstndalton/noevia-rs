//! The rest of core chat-machines.cjs's request side (#1849, #1850, #1854) and core vision.cjs's image probe, for
//! a role served by another machine. Read-only and inert: nothing in the front calls this yet (p11-1 CONTINUE 5);
//! a turn on a machine still runs in Node.
//! - [`props`]: `propsSource(t).props(model)`, the template tools gate's read of a machine model's `/props`;
//! - [`fetch_json`] / [`classifier_machine`]: `fetchJson(url, init, timeoutMs)`, the router classifier's small JSON
//!   request to a machine, counted as a turn on that machine while it runs;
//! - [`VisionProbe`]: vision.cjs `createVisionProbe` (one 1×1 image to `/v1/chat/completions`, cached per address,
//!   headers and model: 5 minutes when supported, 30 s otherwise, 128 entries);
//! - [`vision_for`]: `visionFor(t, headers)`, the machine's `/props` modalities first, else the probe.
//!
//! Every request goes through [`Fetch`]; [`crate::machine_health::NetFetch`] is the on-this-network client.
use crate::machine_health::{BodyError, Capped, Clock, Fetch, FetchError};
use crate::machine_turn::{
    encode_uri_component, MachineTurns, MODELS_BODY_CAP, PROPS_BODY_CAP, PROPS_TIMEOUT,
};
use crate::machines::Target;
use js_json::{wtf16, JValue};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `fetchJson`'s default deadline.
pub const FETCH_JSON_TIMEOUT: Duration = Duration::from_millis(15_000);
/// vision.cjs: the probe's deadline.
pub const VISION_TIMEOUT: Duration = Duration::from_millis(20_000);
/// vision.cjs: how much of a refusal's body is read for the projector hint.
pub const VISION_BODY_CAP: usize = 64 * 1024;
pub const VISION_SUPPORTED_TTL_MS: u64 = 300_000;
pub const VISION_UNSUPPORTED_TTL_MS: u64 = 30_000;
/// vision.cjs: the cache drops its oldest entry once it holds more than this.
pub const VISION_MAX_ENTRIES: usize = 128;
/// vision.cjs's 1×1 PNG.
pub const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

pub const PROJECTOR_TEXT: &str =
    "The inference server needs a multimodal projector (mmproj) configured.";
pub const UNREACHED_TEXT: &str =
    "The image probe could not reach the inference server or timed out. Retry when the server is available.";
pub const NOT_ADMITTED_TEXT: &str =
    "The model cannot be loaded right now, so the image probe did not run.";
pub const MACHINE_VISION_TEXT: &str = "the machine reports image input";
pub const MACHINE_NO_VISION_TEXT: &str = "the machine reports no image input for this model";

/// `Buffer.concat(chunks).toString('utf8')`: ill-formed sequences read as U+FFFD (WHATWG decoding, as Rust's lossy
/// decoding does).
fn utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// ---- propsSource ----

/// `propsSource(t).props(model)`'s answer: `{ ok:false, status }` for a non-2xx answer, else `{ ok:true, status, body }`.
#[derive(Debug, Clone, PartialEq)]
pub enum Props {
    Ok { status: u16, body: JValue },
    NotOk { status: u16 },
}

impl Props {
    /// The object Node resolves with, keys in Node's order.
    pub fn to_json(&self) -> JValue {
        match self {
            Props::Ok { status, body } => JValue::obj([
                ("ok", JValue::Bool(true)),
                ("status", JValue::Num(f64::from(*status))),
                ("body", body.clone()),
            ]),
            Props::NotOk { status } => JValue::obj([
                ("ok", JValue::Bool(false)),
                ("status", JValue::Num(f64::from(*status))),
            ]),
        }
    }
}

/// Why `props(model)` throws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropsFailure {
    /// The request got no answer (the fetch threw).
    Fetch(FetchError),
    /// readCapped's `tooLarge` (more than [`PROPS_BODY_CAP`] bytes) or a broken read.
    Body(BodyError),
    /// `JSON.parse` refused the body (an empty body included).
    Unparsable,
}

/// core `propsSource(t).props(model)` (#1850): the machine's llama-server `/props` for `model`, shaped like the
/// model manager's `props()` answer, through the on-this-network fetch with a 2.5 s deadline and a 1 MiB cap.
pub async fn props<F: Fetch>(fetch: &F, t: &Target, model: &str) -> Result<Props, PropsFailure> {
    let url = format!("{}/props?model={}", t.base_url, encode_uri_component(model));
    let r = fetch
        .get(&url, &[], PROPS_TIMEOUT, Some(PROPS_BODY_CAP))
        .await
        .map_err(PropsFailure::Fetch)?;
    if !r.ok() {
        return Ok(Props::NotOk { status: r.status });
    }
    // readCapped of a response without a body reads '' (and JSON.parse('') throws).
    let text = match r.body {
        Some(Ok(text)) => text,
        Some(Err(e)) => return Err(PropsFailure::Body(e)),
        None => String::new(),
    };
    let body = wtf16::parse(&text).map_err(|_| PropsFailure::Unparsable)?;
    Ok(Props::Ok {
        status: r.status,
        body,
    })
}

// ---- fetchJson ----

/// The WHATWG origin of `url` (`new URL(url).origin`), `None` when it does not parse. An opaque origin reads
/// `"null"`, as in JS, so two of them compare equal.
pub fn origin_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()
        .map(|u| u.origin().ascii_serialization())
}

/// Which machine a `fetchJson(url)` request counts as a turn on (#1880): the first of `targets` (the chat roles'
/// machines in role order, [`crate::machine_turn::remote_roles`]) whose base URL has `url`'s origin. `Ok(None)`: no
/// such machine, or `url` does not parse (the request then fails by itself). `Err(())`: a target's base URL does not
/// parse, where Node's `new URL(t.baseUrl)` throws before any request.
pub fn classifier_machine<'a>(url: &str, targets: &'a [Target]) -> Result<Option<&'a Target>, ()> {
    let Some(host) = origin_of(url) else {
        return Ok(None);
    };
    for t in targets {
        let at = origin_of(&t.base_url).ok_or(())?;
        if at == host {
            return Ok(Some(t));
        }
    }
    Ok(None)
}

/// `init` of `fetchJson(url, init)`: the method (fetch's default `GET`), the headers and the body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Init {
    pub method: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

/// http.cjs fetchJson's shape as chat-machines.cjs answers it: `{ ok, status, body }` (the body parsed as JSON, else
/// its text), or `{ ok:false, status, error:'response too large' }` when the body could not be read within the cap.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonReply {
    Read { ok: bool, status: u16, body: JValue },
    TooLarge { status: u16 },
}

impl JsonReply {
    /// The object Node resolves with, keys in Node's order.
    pub fn to_json(&self) -> JValue {
        match self {
            JsonReply::Read { ok, status, body } => JValue::obj([
                ("ok", JValue::Bool(*ok)),
                ("status", JValue::Num(f64::from(*status))),
                ("body", body.clone()),
            ]),
            JsonReply::TooLarge { status } => JValue::obj([
                ("ok", JValue::Bool(false)),
                ("status", JValue::Num(f64::from(*status))),
                ("error", JValue::Str("response too large".into())),
            ]),
        }
    }
}

/// core `fetchJsonInner`: one request through the machine fetch; the body (any status) read within 256 KiB, then
/// `JSON.parse`d, its text kept when it is not JSON. `Err`: the fetch threw (no answer, a redirect, a refused
/// address), as Node's `fetchJson` rejects.
pub async fn fetch_json_once<F: Fetch>(
    fetch: &F,
    url: &str,
    init: &Init,
    timeout: Duration,
) -> Result<JsonReply, FetchError> {
    let method = init.method.as_deref().unwrap_or("GET");
    let r = fetch
        .send(
            method,
            url,
            &init.headers,
            init.body.clone(),
            timeout,
            MODELS_BODY_CAP,
        )
        .await?;
    let text = match r.body {
        // readCapped throws tooLarge past the cap; any read failure lands in the same catch.
        Ok(Capped { capped: true, .. }) | Err(_) => {
            return Ok(JsonReply::TooLarge { status: r.status })
        }
        Ok(Capped { bytes, .. }) => utf8(&bytes),
    };
    let body = wtf16::parse(&text).unwrap_or(JValue::Str(text));
    Ok(JsonReply::Read {
        ok: r.ok(),
        status: r.status,
        body,
    })
}

/// core `fetchJson(url, init, timeoutMs)` (#1849, #1880): [`fetch_json_once`], counted as a turn on the machine whose
/// origin it asks ([`classifier_machine`] over `targets`) while it runs. `begin(machine_id)` takes the count
/// (noevia-server machine_service `turnBegin`); its guard is dropped when the request ends, answer or not.
pub async fn fetch_json<F, G>(
    fetch: &F,
    targets: &[Target],
    url: &str,
    init: &Init,
    timeout: Duration,
    begin: impl FnOnce(&str) -> G,
) -> Result<JsonReply, FetchError>
where
    F: Fetch,
{
    let on = classifier_machine(url, targets).map_err(|()| FetchError::Network)?;
    let _turn = on.map(|t| begin(&t.machine_id));
    fetch_json_once(fetch, url, init, timeout).await
}

// ---- vision.cjs ----

/// What the image probe (and [`vision_for`]) answers: `{ supported, reason }`, `reason` null only when supported by
/// the probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vision {
    pub supported: bool,
    pub reason: Option<String>,
}

impl Vision {
    fn no(reason: &str) -> Self {
        Vision {
            supported: false,
            reason: Some(reason.to_owned()),
        }
    }

    pub fn to_json(&self) -> JValue {
        JValue::obj([
            ("supported", JValue::Bool(self.supported)),
            (
                "reason",
                self.reason
                    .as_ref()
                    .map_or(JValue::Null, |r| JValue::Str(r.clone())),
            ),
        ])
    }
}

/// vision.cjs `admit(baseUrl, model)` (#697): makes room for the model before the probe; `Err(publicMessage)` refuses
/// it (reported, never cached). Machines probe without one.
pub type Admit = Arc<
    dyn Fn(String, String) -> Pin<Box<dyn Future<Output = Result<(), Option<String>>> + Send>>
        + Send
        + Sync,
>;

/// `JSON.stringify([baseUrl, headers, model])`, the cache key: `headers` as the object a spread of them makes.
pub fn probe_key(base_url: &str, headers: &[(String, String)], model: &str) -> String {
    let mut items: Vec<(String, JValue)> = Vec::new();
    for (k, v) in headers {
        wtf16::set(&mut items, k, JValue::Str(v.clone()));
    }
    let key = JValue::Arr(vec![
        JValue::Str(base_url.to_owned()),
        JValue::Obj(items),
        JValue::Str(model.to_owned()),
    ]);
    wtf16::stringify(&key).unwrap_or_default()
}

/// `${String(baseUrl).replace(/\/+$/, '').replace(/\/v1$/, '')}/v1/chat/completions`.
pub fn probe_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    format!("{base}/v1/chat/completions")
}

/// The probe's request headers: `{ 'Content-Type': 'application/json', ...headers }` (a caller's own
/// `Content-Type` replaces it in place; property order kept).
pub fn probe_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut items: Vec<(String, JValue)> = vec![(
        "Content-Type".to_owned(),
        JValue::Str("application/json".into()),
    )];
    for (k, v) in headers {
        wtf16::set(&mut items, k, JValue::Str(v.clone()));
    }
    items
        .into_iter()
        .map(|(k, v)| (k, v.as_str().unwrap_or_default().to_owned()))
        .collect()
}

/// The probe's body, byte for byte as `JSON.stringify` writes it.
pub fn probe_body(model: &str) -> String {
    let body = JValue::obj([
        ("model", JValue::Str(model.to_owned())),
        ("max_tokens", JValue::Num(1.0)),
        (
            "messages",
            JValue::Arr(vec![JValue::obj([
                ("role", JValue::Str("user".into())),
                (
                    "content",
                    JValue::Arr(vec![
                        JValue::obj([
                            ("type", JValue::Str("text".into())),
                            ("text", JValue::Str("ok".into())),
                        ]),
                        JValue::obj([
                            ("type", JValue::Str("image_url".into())),
                            (
                                "image_url",
                                JValue::obj([(
                                    "url",
                                    JValue::Str(format!("data:image/png;base64,{PIXEL}")),
                                )]),
                            ),
                        ]),
                    ]),
                ),
            ])]),
        ),
    ]);
    wtf16::stringify(&body).unwrap_or_default()
}

/// readCappedText's text: a character cut at the cap (a trailing U+FFFD) is dropped.
fn capped_text(c: &Capped) -> String {
    let text = utf8(&c.bytes);
    if c.capped {
        if let Some(cut) = text.strip_suffix('\u{FFFD}') {
            return cut.to_owned();
        }
    }
    text
}

/// `/mmproj|projector/i`.
fn names_projector(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("mmproj") || lower.contains("projector")
}

fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    })
}

struct Entry {
    result: Vision,
    until: u64,
}

/// vision.cjs `createVisionProbe({ fetchImpl, now, admit })`: one shared probe with its own cache. Like Node it does
/// not join concurrent probes of the same key; each runs and the last to finish is kept.
pub struct VisionProbe<F> {
    fetch: F,
    now: Clock,
    admit: Option<Admit>,
    // Map order: a key set again keeps its place; the oldest key is dropped first.
    cache: Mutex<Vec<(String, Entry)>>,
}

impl<F: Fetch> VisionProbe<F> {
    pub fn new(fetch: F) -> Self {
        Self::with(fetch, system_clock(), None)
    }

    pub fn with(fetch: F, now: Clock, admit: Option<Admit>) -> Self {
        VisionProbe {
            fetch,
            now,
            admit,
            cache: Mutex::new(Vec::new()),
        }
    }

    pub fn len(&self) -> usize {
        self.cache.lock().map_or(0, |c| c.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn cached(&self, key: &str) -> Option<Vision> {
        let now = (self.now)();
        let cache = self.cache.lock().ok()?;
        cache
            .iter()
            .find(|(k, _)| k == key)
            .filter(|(_, e)| e.until > now)
            .map(|(_, e)| e.result.clone())
    }

    fn keep(&self, key: String, result: &Vision) {
        let ttl = if result.supported {
            VISION_SUPPORTED_TTL_MS
        } else {
            VISION_UNSUPPORTED_TTL_MS
        };
        let until = (self.now)().saturating_add(ttl);
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        let entry = Entry {
            result: result.clone(),
            until,
        };
        match cache.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = entry,
            None => cache.push((key, entry)),
        }
        if cache.len() > VISION_MAX_ENTRIES {
            cache.remove(0);
        }
    }

    /// The probe `(baseUrl, headers, model)`.
    pub async fn probe(&self, base_url: &str, headers: &[(String, String)], model: &str) -> Vision {
        let key = probe_key(base_url, headers, model);
        if let Some(hit) = self.cached(&key) {
            return hit;
        }
        if let Some(admit) = &self.admit {
            if let Err(message) = admit(base_url.to_owned(), model.to_owned()).await {
                return Vision::no(message.as_deref().unwrap_or(NOT_ADMITTED_TEXT));
            }
        }
        let sent = self
            .fetch
            .send(
                "POST",
                &probe_url(base_url),
                &probe_headers(headers),
                Some(probe_body(model)),
                VISION_TIMEOUT,
                VISION_BODY_CAP,
            )
            .await;
        let result = match sent {
            Ok(r) if r.ok() => Vision {
                supported: true,
                reason: None,
            },
            Ok(r) => match &r.body {
                Ok(c) if names_projector(&capped_text(c)) => Vision::no(PROJECTOR_TEXT),
                Ok(_) => Vision::no(&format!(
                    "The inference server rejected the image probe (HTTP {}). Check model configuration and server availability.",
                    r.status
                )),
                // The read threw: the probe's catch.
                Err(_) => Vision::no(UNREACHED_TEXT),
            },
            Err(_) => Vision::no(UNREACHED_TEXT),
        };
        self.keep(key, &result);
        result
    }
}

/// core `visionFor(t, headers)` (#1854): whether the role's model on its machine reads images. The machine's `/props`
/// modalities (cached with its health, [`MachineTurns::facts`]) answer when they say; a server that does not say gets
/// the image probe at `<base>/v1`. Nothing is recorded in this machine's model evidence.
pub async fn vision_for<F: Fetch, P: Fetch>(
    turns: &MachineTurns<F>,
    probe: &VisionProbe<P>,
    t: &Target,
    headers: &[(String, String)],
) -> Vision {
    match turns.facts(t).await.vision {
        Some(true) => Vision {
            supported: true,
            reason: Some(MACHINE_VISION_TEXT.to_owned()),
        },
        Some(false) => Vision::no(MACHINE_NO_VISION_TEXT),
        None => {
            probe
                .probe(&format!("{}/v1", t.base_url), headers, &t.model)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_url_drops_trailing_slashes_then_one_v1() {
        assert_eq!(
            probe_url("http://pi.lan:8080/v1"),
            "http://pi.lan:8080/v1/chat/completions"
        );
        assert_eq!(
            probe_url("http://pi.lan:8080/v1//"),
            "http://pi.lan:8080/v1/chat/completions"
        );
        assert_eq!(
            probe_url("http://pi.lan:8080/"),
            "http://pi.lan:8080/v1/chat/completions"
        );
        // Only one /v1 at the end goes.
        assert_eq!(
            probe_url("http://h/v1/v1"),
            "http://h/v1/v1/chat/completions"
        );
        assert_eq!(probe_url("http://h/xv1"), "http://h/x/v1/chat/completions");
    }

    #[test]
    fn the_key_is_json_of_base_headers_and_model() {
        let h = vec![("Authorization".to_owned(), "Bearer qa".to_owned())];
        assert_eq!(
            probe_key("http://h/v1", &h, "qa-model"),
            r#"["http://h/v1",{"Authorization":"Bearer qa"},"qa-model"]"#
        );
        assert_eq!(probe_key("b", &[], "m\"\n"), r#"["b",{},"m\"\n"]"#);
        // An index-like header name goes first, as a JS object orders it.
        let h = vec![
            ("x".to_owned(), "1".to_owned()),
            ("7".to_owned(), "2".to_owned()),
        ];
        assert_eq!(probe_key("b", &h, "m"), r#"["b",{"7":"2","x":"1"},"m"]"#);
    }

    #[test]
    fn the_probe_headers_spread_over_the_content_type() {
        assert_eq!(
            probe_headers(&[]),
            vec![("Content-Type".to_owned(), "application/json".to_owned())]
        );
        let h = probe_headers(&[
            ("X-QA".to_owned(), "1".to_owned()),
            ("Content-Type".to_owned(), "text/plain".to_owned()),
        ]);
        assert_eq!(
            h,
            vec![
                ("Content-Type".to_owned(), "text/plain".to_owned()),
                ("X-QA".to_owned(), "1".to_owned())
            ]
        );
    }

    #[test]
    fn the_probe_body_is_node_bytes() {
        assert_eq!(
            probe_body("qa-vision"),
            format!(
                r#"{{"model":"qa-vision","max_tokens":1,"messages":[{{"role":"user","content":[{{"type":"text","text":"ok"}},{{"type":"image_url","image_url":{{"url":"data:image/png;base64,{PIXEL}"}}}}]}}]}}"#
            )
        );
    }

    #[test]
    fn a_cut_character_is_dropped_only_when_capped() {
        let cut = Capped {
            bytes: vec![b'a', 0xE2, 0x82],
            capped: true,
        };
        assert_eq!(capped_text(&cut), "a");
        let whole = Capped {
            bytes: vec![b'a', 0xE2, 0x82],
            capped: false,
        };
        assert_eq!(capped_text(&whole), "a\u{FFFD}");
    }

    #[test]
    fn the_projector_hint_ignores_case() {
        assert!(names_projector("no MMPROJ loaded"));
        assert!(names_projector("Projector missing"));
        assert!(!names_projector("image input not supported"));
    }

    #[test]
    fn origins_compare_as_whatwg_writes_them() {
        assert_eq!(
            origin_of("http://PI.lan:80/x").as_deref(),
            Some("http://pi.lan")
        );
        assert_eq!(
            origin_of("http://pi.lan:8080/v1/chat").as_deref(),
            Some("http://pi.lan:8080")
        );
        assert_eq!(
            origin_of("https://[::1]:443/").as_deref(),
            Some("https://[::1]")
        );
        assert_eq!(origin_of("data:text/plain,x").as_deref(), Some("null"));
        assert_eq!(origin_of("not a url"), None);
    }
}
