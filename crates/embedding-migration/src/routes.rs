//! `routes/embedding.cjs`: `GET`/`POST /api/embedding` for administrators. Request reading is
//! `embedding-admin`'s; this is what follows it: the address and served-model checks, the
//! projects half ([`Migration`]), the Diary half (its sidecar's `/api/internal/embedding`, service
//! token only, no tenant), the audit record, the embedding service's sync after a change, and
//! the `service-switch` / `service-off` actions (#1668) with `servedSet()`.

use crate::migration::Migration;
use crate::target::{check_served, COERCE_ERROR};
use crate::Host;
use chat_turn::js;
use embedding_admin::{aggregate_status, read_request, served_probes, Action};
use js_json::{to_js_string, wtf16, CoerceError, JValue};

/// `fetchJson`'s deadline for the Diary.
pub const DIARY_TIMEOUT_MS: u64 = 15_000;
/// `waitTimeoutMs`: how long `service-switch` waits for the model to load.
pub const WAIT_TIMEOUT_MS: u64 = 180_000;

/// The body text of index.cjs's answer to an error the route throws (`errorResponse` of an error
/// without a status: 500 `{ error: 'Internal error' }`).
pub const INTERNAL_ERROR: &str = "Internal error";

/// What the request answers when Node's route throws a TypeError (an unexpected value it cannot
/// read): nothing after the throw runs, and index.cjs's catch sends 500 `{ error: 'Internal error' }`.
pub fn internal_error() -> Answer {
    answer(500, [("error", INTERNAL_ERROR.into())])
}

/// An answer: the status and the JSON body (sent with `Cache-Control: no-store`).
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub status: u16,
    pub body: JValue,
}

/// `fetchJson`'s `{ ok, status, body }`.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpReply {
    pub ok: bool,
    pub status: u16,
    pub body: JValue,
}

/// What `service.serve` threw.
#[derive(Debug, Clone, PartialEq)]
pub struct ServeError {
    pub status: Option<u16>,
    pub message: String,
    pub code: Option<String>,
}

/// The signed-in user (`authn.user`).
#[derive(Debug, Clone, Copy)]
pub struct User<'a> {
    pub id: &'a str,
    pub admin: bool,
}

/// `${DIARY_BASE without trailing slashes}/api/internal/embedding`, `None` when no Diary is set.
pub fn diary_url(base: &str) -> Option<String> {
    (!base.is_empty()).then(|| format!("{}/api/internal/embedding", base.trim_end_matches('/')))
}

fn answer<'a>(status: u16, pairs: impl IntoIterator<Item = (&'a str, JValue)>) -> Answer {
    Answer {
        status,
        body: JValue::obj(pairs),
    }
}

fn refuse(status: u16, code: &str, message: impl Into<String>) -> Answer {
    answer(
        status,
        [
            ("error", JValue::Str(message.into())),
            ("code", code.into()),
        ],
    )
}

/// `String(v)`; an object with an own `toString` (Node throws) reads as `[object Object]`.
fn string_of(v: &JValue) -> String {
    to_js_string(v).unwrap_or_else(|_| "[object Object]".into())
}

/// `===` between JSON values (objects are never the same one here).
fn strict_eq(a: &JValue, b: &JValue) -> bool {
    match (a, b) {
        (JValue::Str(x), JValue::Str(y)) => x == y,
        (JValue::Num(x), JValue::Num(y)) => x == y,
        (JValue::Bool(x), JValue::Bool(y)) => x == y,
        (JValue::Null, JValue::Null) | (JValue::Undefined, JValue::Undefined) => true,
        _ => false,
    }
}

fn is_object(v: &JValue) -> bool {
    matches!(v, JValue::Obj(_) | JValue::Arr(_))
}

/// A JS value as the `serde_json::Value` `embedding-admin` reads (undefined fields left out).
pub fn to_serde(v: &JValue) -> serde_json::Value {
    use serde_json::Value;
    match v {
        JValue::Undefined | JValue::Null => Value::Null,
        JValue::Bool(b) => Value::Bool(*b),
        JValue::Num(n) if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 => {
            Value::from(*n as i64)
        }
        JValue::Num(n) => serde_json::Number::from_f64(*n).map_or(Value::Null, Value::Number),
        JValue::Str(s) => Value::String(s.clone()),
        JValue::Arr(items) => Value::Array(items.iter().map(to_serde).collect()),
        JValue::Obj(items) => Value::Object(
            items
                .iter()
                .filter(|(_, x)| !matches!(x, JValue::Undefined))
                .map(|(k, x)| (k.clone(), to_serde(x)))
                .collect(),
        ),
    }
}

/// A half that ran and answered something (`[..].filter(Boolean)`).
fn ran(half: &Option<JValue>) -> Option<&JValue> {
    half.as_ref().filter(|h| h.truthy())
}

#[derive(Debug, Clone)]
pub struct Embedding {
    pub migration: Migration,
    /// [`diary_url`] of `DIARY_BASE`.
    pub diary_url: Option<String>,
    pub wait_timeout_ms: u64,
}

impl Embedding {
    pub fn new(migration: Migration, diary_url: Option<String>) -> Embedding {
        Embedding {
            migration,
            diary_url,
            wait_timeout_ms: WAIT_TIMEOUT_MS,
        }
    }

    /// `handle()` for `/api/embedding`: `body` is the parsed request (`Err`: not JSON).
    pub fn handle<H: Host>(
        &mut self,
        host: &mut H,
        method: &str,
        user: Option<User<'_>>,
        body: Result<&JValue, ()>,
    ) -> Answer {
        let error = |status, text: &str| answer(status, [("error", text.into())]);
        let Some(user) = user else {
            return error(401, "Sign in required");
        };
        if !user.admin {
            return error(403, "Administrator required");
        }
        if method == "GET" {
            host.service_refresh();
            let projects = self.migration.status(host);
            let diary = self.diary(host, "GET", None);
            let mut out = vec![("projects", projects), ("diary", diary)];
            if host.service_managed() {
                out.push(("service", host.service_status()));
            }
            return answer(200, out);
        }
        if method != "POST" {
            return error(405, "Method not allowed");
        }
        match body {
            Ok(body) => self.apply(host, user.id, body),
            Err(()) => error(400, "invalid JSON"),
        }
    }

    /// `diary(method, body)`: the Diary's answer, or an `{ error, status?, code? }` half.
    fn diary<H: Host>(&mut self, host: &mut H, method: &str, body: Option<&JValue>) -> JValue {
        let Some(url) = &self.diary_url else {
            return JValue::obj([
                ("error", "The Diary sidecar is not configured.".into()),
                ("code", "notConfigured".into()),
            ]);
        };
        let unreachable = |message: &str| {
            let message = if message.is_empty() { "Error" } else { message };
            let text = format!(
                "The Diary could not be reached ({}).",
                wtf16::slice(message, 120)
            );
            JValue::obj([("error", text.into())])
        };
        let text = body.and_then(wtf16::stringify);
        let reply = match host.diary(url, method, text.as_deref()) {
            Ok(reply) if reply.ok => return reply.body,
            Ok(reply) => reply,
            Err(message) => return unreachable(&message),
        };
        let b = &reply.body;
        let said = [b.get("error"), b.get("detail")]
            .into_iter()
            .find(|v| v.truthy());
        let message = match said.map(to_js_string) {
            Some(Ok(text)) => text,
            Some(Err(_)) => return unreachable(COERCE_ERROR),
            None => format!("The Diary answered HTTP {}.", reply.status),
        };
        let mut out = vec![
            ("error", JValue::Str(wtf16::slice(&message, 300))),
            ("status", JValue::Num(f64::from(reply.status))),
        ];
        if b.get("code").truthy() {
            out.push(("code", b.get("code").clone()));
        }
        JValue::obj(out)
    }

    /// `apply(body, authn)`: a non-service action on the halves `scope` names, or a service action.
    pub fn apply<H: Host>(&mut self, host: &mut H, user: &str, body: &JValue) -> Answer {
        let request = match read_request(&to_serde(body)) {
            Ok(request) => request,
            Err(reply) => return answer(reply.status, [("error", reply.error.into())]),
        };
        if request.action.is_service() {
            return self.service_action(host, user, request.action, body);
        }
        let target = body.get("target");
        if let Some(chosen) = &request.target {
            for address in [target.get("baseUrl"), target.get("diaryBaseUrl")] {
                let text = if address.is_nullish() {
                    String::new()
                } else {
                    string_of(address)
                };
                let address = js::trim(&text);
                if address.is_empty() {
                    continue; // the inference engine
                }
                if let Err(reason) = host.check_address(address) {
                    return refuse(400, "publicEndpoint", format!("The embeddings server must be on this network: it receives the text of every project and the Diary ({reason})."));
                }
            }
            let probes = match served_probes(&request) {
                Ok(probes) => probes,
                Err(reply) => return refuse(reply.status, "diaryEndpointRequired", reply.error),
            };
            for probe in probes {
                if let Err(f) = check_served(host, probe.as_deref(), &chosen.model) {
                    let mut out = vec![
                        ("error", JValue::Str(f.message.clone())),
                        ("code", f.code.unwrap_or_default().into()),
                    ];
                    if let Some(served) = crate::target::served_list(&f) {
                        out.push(("served", served));
                    }
                    return answer(409, out);
                }
            }
        }
        let action = request.action;
        let projects = request.scope.includes_projects().then(|| {
            let m = &mut self.migration;
            let result = match action {
                Action::SetNext => m.set_next(host, target),
                Action::ClearNext => m.clear_next(host),
                Action::Switch => m.switch_to_next(host, request.force),
                Action::Rollback => m.rollback(host),
                _ => m.finish(host),
            };
            result.unwrap_or_else(|f| f.to_json())
        });
        let diary = request.scope.includes_diary().then(|| {
            let mut sent = vec![("action", JValue::from(action.as_str()))];
            if target.truthy() && is_object(target) {
                let base = [target.get("diaryBaseUrl"), target.get("baseUrl")]
                    .into_iter()
                    .find(|v| !v.is_nullish())
                    .map_or_else(|| JValue::from(""), Clone::clone);
                sent.push((
                    "target",
                    JValue::obj([
                        ("model", target.get("model").clone()),
                        ("profile", target.get("profile").clone()),
                        ("base_url", base),
                    ]),
                ));
            }
            if request.force {
                sent.push(("force", JValue::Bool(true)));
            }
            self.diary(host, "POST", Some(&JValue::obj(sent)))
        });
        let word = |half: &Option<JValue>| match ran(half) {
            Some(h) if h.get("error").truthy() => "refused",
            Some(_) => "ok",
            None => "skipped",
        };
        let or_null = |v: &JValue| if v.truthy() { v.clone() } else { JValue::Null };
        let detail = JValue::obj([
            ("scope", request.scope.as_str().into()),
            ("model", or_null(target.get("model"))),
            ("profile", or_null(target.get("profile"))),
            ("force", JValue::Bool(request.force)),
            ("projects", word(&projects).into()),
            ("diary", word(&diary).into()),
        ]);
        host.audit(&format!("embedding.{}", action.as_str()), user, &detail);
        let changed = [&projects, &diary]
            .into_iter()
            .any(|h| ran(h).is_some_and(|h| !h.get("error").truthy()));
        if changed
            && matches!(
                action,
                Action::Finish | Action::ClearNext | Action::Rollback | Action::Switch
            )
            && self.sync_service(host).is_err()
        {
            // `servedSet` threw: `apply` rejects after the change and the audit record.
            return internal_error();
        }
        let status = aggregate_status(
            ran(&projects).map(to_serde).as_ref(),
            ran(&diary).map(to_serde).as_ref(),
        );
        let mut out = Vec::new();
        out.extend(projects.map(|p| ("projects", p)));
        out.extend(diary.map(|d| ("diary", d)));
        answer(status, out)
    }

    /// `diaryTargets()`: the Diary's targets, `None` when they cannot be known.
    fn diary_targets<H: Host>(&mut self, host: &mut H) -> Option<JValue> {
        if self.diary_url.is_none() {
            return Some(JValue::obj([
                ("active", JValue::Null),
                ("next", JValue::Null),
                ("previous", JValue::Null),
            ]));
        }
        let d = self.diary(host, "GET", None);
        let active = d.get("active");
        (d.truthy() && !d.get("error").truthy() && active.truthy() && is_object(active))
            .then_some(d)
    }

    /// `servedSet(c, d, { diaryDefaults })` (#1668, #1683): the `{ model, file }` the embedding
    /// service must serve for the projects' status `c` and the Diary's `d`; `None` when the models
    /// in use do not fit in two, and also when Node's `servedSet` throws (see
    /// [`Embedding::try_served_set`]): either way the caller must not shrink what is served.
    pub fn served_set<H: Host>(
        host: &mut H,
        c: &JValue,
        d: &JValue,
        diary_defaults: bool,
    ) -> Option<Vec<JValue>> {
        Embedding::try_served_set(host, c, d, diary_defaults).unwrap_or(None)
    }

    /// [`Embedding::served_set`], with `Err` where Node's throws: a Diary target's address that
    /// `String()` cannot convert (an object with its own `toString`), read when that target is not
    /// at the service and might use the Diary's default server. The host calls up to the throw are
    /// made.
    pub fn try_served_set<H: Host>(
        host: &mut H,
        c: &JValue,
        d: &JValue,
        diary_defaults: bool,
    ) -> Result<Option<Vec<JValue>>, CoerceError> {
        fn add<H: Host>(
            host: &mut H,
            out: &mut Vec<(JValue, String)>,
            t: &JValue,
            on_default: bool,
        ) -> Result<bool, CoerceError> {
            let model = t.get("model");
            if !t.truthy() || !model.truthy() {
                return Ok(true);
            }
            let address = match t.get("baseUrl") {
                JValue::Undefined => t.get("base_url"),
                given => given,
            };
            // #1683: a Diary target with no address uses the Diary's own default server.
            // `String(address ?? '')` throws for an object with its own `toString`.
            let diary_default = || -> Result<bool, CoerceError> {
                Ok(address.is_nullish() || js::trim(&to_js_string(address)?).is_empty())
            };
            let on_service = host.service_is_at(address) || (on_default && diary_default()?);
            if !on_service || out.iter().any(|(m, _)| strict_eq(m, model)) {
                return Ok(true);
            }
            let Some(file) = host.service_file_of(model).filter(|f| !f.is_empty()) else {
                return Ok(true);
            };
            out.push((model.clone(), file));
            Ok(out.len() <= 2)
        }
        let mut out: Vec<(JValue, String)> = Vec::new();
        for (key, from_diary) in [
            ("active", false),
            ("active", true),
            ("next", false),
            ("next", true),
        ] {
            let t = if from_diary { d.get(key) } else { c.get(key) };
            if !add(host, &mut out, t, diary_defaults && from_diary)? {
                return Ok(None);
            }
        }
        for (t, from_diary) in [(c.get("previous"), false), (d.get("previous"), true)] {
            if out.len() < 2 {
                add(host, &mut out, t, diary_defaults && from_diary)?;
            }
        }
        Ok(Some(
            out.into_iter()
                .map(|(model, file)| JValue::obj([("model", model), ("file", file.into())]))
                .collect(),
        ))
    }

    /// `syncService()`: after a change, the service serves only what is still configured. `Err`
    /// when `servedSet` threw: nothing is served then.
    fn sync_service<H: Host>(&mut self, host: &mut H) -> Result<(), CoerceError> {
        if !host.service_managed() || host.service_enabled() != Some(true) {
            return Ok(());
        }
        let Some(d) = self.diary_targets(host) else {
            return Ok(()); // the Diary may still use a model: keep serving what is served
        };
        let c = self.migration.status(host);
        if let Some(want) = Embedding::try_served_set(host, &c, &d, true)? {
            if !want.is_empty() {
                let _ = host.service_serve(&want);
            }
        }
        Ok(())
    }

    /// `serviceActionHeld(action, body, authn)`.
    fn service_action<H: Host>(
        &mut self,
        host: &mut H,
        user: &str,
        action: Action,
        body: &JValue,
    ) -> Answer {
        let failed = |e: ServeError| {
            answer(
                e.status.unwrap_or(502),
                [
                    ("error", JValue::Str(e.message)),
                    ("code", e.code.map_or(JValue::Undefined, JValue::Str)),
                ],
            )
        };
        if !host.service_managed() {
            return refuse(409, "notManaged", "The embedding service is not managed by noevia on this server (see docs/deployment.md, #1668).");
        }
        let c = self.migration.status(host);
        let unknown = "The Diary's embedding settings could not be read, so";
        if action == Action::ServiceOff {
            if c.get("next").truthy() {
                return refuse(
                    409,
                    "reindexUnderway",
                    "A re-index is under way. Cancel it before turning embedding off.",
                );
            }
            if c.get("previous").truthy() {
                return refuse(409, "finishFirst", "Finish the last embedding switch (or roll it back) before turning embedding off.");
            }
            let Some(d) = self.diary_targets(host) else {
                return refuse(409, "diaryUnknown", format!("{unknown} turning embedding off could stop a switch it has under way. Try again when the Diary answers."));
            };
            if d.get("next").truthy() || d.get("previous").truthy() {
                return refuse(409, "finishFirst", "The Diary has not finished its embedding switch. Finish it (or roll it back) before turning embedding off.");
            }
            if let Err(e) = host.service_serve(&[]) {
                return failed(e);
            }
            host.audit("embedding.service-off", user, &JValue::Obj(Vec::new()));
            return answer(200, [("service", host.service_status())]);
        }
        let named = body.get("model");
        let model = js::trim(&if named.truthy() {
            string_of(named)
        } else {
            String::new()
        })
        .to_string();
        if model.is_empty() {
            return answer(400, [("error", "service-switch needs a model".into())]);
        }
        if c.get("next").truthy() {
            return refuse(
                409,
                "reindexUnderway",
                "A re-index is already under way. Finish or cancel it first.",
            );
        }
        if c.get("previous").truthy() {
            return refuse(
                409,
                "finishFirst",
                "Finish the last embedding switch (or roll it back) before switching again.",
            );
        }
        let Some(file) = host.resolve_file(&model).filter(|f| !f.is_empty()) else {
            return refuse(
                404,
                "modelNotInstalled",
                format!("{model} is not an installed model file."),
            );
        };
        let active = c.get("active");
        let active_url = active.get("baseUrl");
        // Keep the address spelled as the active target has it: the same server is not a move.
        let address = if active.truthy() && host.service_is_at(active_url) {
            active_url.clone()
        } else {
            host.service_base_url().into()
        };
        let from_off = host.service_enabled() != Some(true);
        let active_on_service = active.truthy() && host.service_is_at(active_url) && !from_off;
        let wanted = JValue::from(model.as_str());
        let has = |set: &[JValue]| set.iter().any(|e| strict_eq(e.get("model"), &wanted));
        let mut keep = Vec::new();
        if active_on_service {
            let Some(d) = self.diary_targets(host) else {
                return refuse(409, "diaryUnknown", format!("{unknown} the switch could drop a model it still uses. Try again when the Diary answers."));
            };
            // A throw here (an unreadable Diary address) ends the request before anything is served.
            let Ok(all) = Embedding::try_served_set(host, &c, &d, true) else {
                return internal_error();
            };
            let Ok(known) = Embedding::try_served_set(host, &c, &d, false) else {
                return internal_error();
            };
            let overflows = |set: &Option<Vec<JValue>>| {
                set.as_ref()
                    .is_none_or(|s| s.len() + usize::from(!has(s)) > 2)
            };
            if overflows(&all) && !overflows(&known) {
                return refuse(409, "diaryAddressUnknown", "The Diary uses its default embedding server, so noevia cannot tell whether it shares the embedding service. Give the Diary an explicit embeddings address in Settings, then switch again.");
            }
            let Some(all) = all else {
                return refuse(409, "tooManyModels", "The embedding service already serves two models in use. Finish or cancel the switch under way first.");
            };
            keep = all;
        }
        if has(&keep) {
            return refuse(
                409,
                "alreadyActive",
                format!("{model} is already the embedding model."),
            );
        }
        if keep.len() + 1 > 2 {
            let names: Vec<String> = keep
                .iter()
                .map(|e| match e.get("model") {
                    m if m.is_nullish() => String::new(),
                    m => string_of(m),
                })
                .collect();
            return refuse(409, "tooManyModels", format!("Projects and the Diary use {}; the embedding service serves at most two models. Finish or roll back the other switch first.", names.join(" and ")));
        }
        let before: Vec<JValue> = if from_off {
            Vec::new()
        } else {
            match host.service_status().get("models") {
                JValue::Arr(models) => models
                    .iter()
                    .filter(|e| e.truthy() && e.get("model").truthy() && e.get("file").truthy())
                    .map(|e| {
                        JValue::obj([
                            ("model", e.get("model").clone()),
                            ("file", e.get("file").clone()),
                        ])
                    })
                    .collect(),
                // `(status?.models || []).filter`: absent or falsy is nothing served.
                models if !models.truthy() => Vec::new(),
                // Anything else has no `filter`: Node throws a TypeError before serving, so the
                // rollback below could never serve a made-up empty list over models in use.
                _ => return internal_error(),
            }
        };
        let mut serve = keep;
        serve.push(JValue::obj([
            ("model", wanted.clone()),
            ("file", file.into()),
        ]));
        if let Err(e) = host.service_serve(&serve) {
            return failed(e);
        }
        if !host.service_wait_loaded(std::slice::from_ref(&model), self.wait_timeout_ms) {
            // Back to what was served, so a model nothing uses is not left loaded.
            let _ = host.service_serve(&before);
            return answer(504, [
                ("error", format!("The embedding service did not load {model} in time. Nothing was re-indexed; try again or pick another model.").into()),
                ("code", "serviceNotLoaded".into()),
                ("service", host.service_status()),
            ]);
        }
        let profile = body.get("profile");
        // Back on with the model the stored vectors already belong to: nothing to re-index.
        if from_off && active.truthy() {
            let chosen = if profile.truthy() {
                profile
            } else {
                active.get("profile")
            };
            let address_of = |v: &JValue| if v.truthy() { v.clone() } else { JValue::Null };
            // sameTarget: `${profile}\0${model}` and the address (empty is the engine).
            let identity = |profile: &JValue, model: &JValue| {
                format!("{}\0{}", string_of(profile), string_of(model))
            };
            let same = identity(active.get("profile"), active.get("model"))
                == identity(chosen, &wanted)
                && strict_eq(&address_of(active_url), &address_of(&address));
            if same {
                let service = host.service_status();
                return answer(
                    200,
                    [
                        ("service", service),
                        ("projects", self.migration.status(host)),
                    ],
                );
            }
        }
        let target = JValue::obj([
            ("baseUrl", address.clone()),
            ("model", wanted),
            ("profile", profile.clone()),
            ("diaryBaseUrl", address),
        ]);
        let request = JValue::obj([
            ("action", "set-next".into()),
            ("scope", "all".into()),
            ("target", target),
        ]);
        let mut r = self.apply(host, user, &request);
        if let JValue::Obj(items) = &mut r.body {
            wtf16::set(items, "service", host.service_status());
        }
        r
    }
}
