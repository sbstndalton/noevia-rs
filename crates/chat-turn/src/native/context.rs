//! S7 of a native turn: the model's lease, the runtime context limit and the context fit
//! (chat.cjs 1766-1825, chat-context.cjs `resolveRuntimeLimit`, `prepare`).

use context_fit::Saved;
use js_json::{wtf16, JValue};

use super::text::*;
use super::{Ended, Turn};
use crate::host::{EngineRequest, Host};
use crate::js;

/// What the fit gives the rounds.
pub(crate) struct Fit {
    pub messages: Vec<JValue>,
    pub max_tokens: u64,
    pub limit: u64,
    pub limit_source: String,
    pub history_count: usize,
    pub compacted_at: Option<f64>,
    pub covered: usize,
}

fn gate_error(text: &str, code: Option<&str>, retryable: Option<bool>) -> JValue {
    let mut pairs = vec![("text", js::s(text))];
    if let Some(c) = code {
        pairs.push(("code", js::s(c)));
    }
    if let Some(r) = retryable {
        pairs.push(("retryable", JValue::Bool(r)));
    }
    event("error", pairs)
}

/// One preparation's hold on its state file (chat-context.cjs `busy`).
struct Preparing<'a> {
    set: &'a std::sync::Mutex<std::collections::HashSet<String>>,
    key: String,
}

impl<'a> Preparing<'a> {
    fn take(
        set: &'a std::sync::Mutex<std::collections::HashSet<String>>,
        key: String,
    ) -> Option<Self> {
        let fresh = set
            .lock()
            .map(|mut b| b.insert(key.clone()))
            .unwrap_or(false);
        fresh.then_some(Preparing { set, key })
    }
}

impl Drop for Preparing<'_> {
    fn drop(&mut self) {
        if let Ok(mut b) = self.set.lock() {
            b.remove(&self.key);
        }
    }
}

/// The context id chat.cjs uses: `chatId || spaceId`, and `undefined` (its own file) without either.
pub(crate) fn context_id(chat: Option<&str>, space: Option<&str>) -> String {
    chat.filter(|c| !c.is_empty())
        .or(space.filter(|s| !s.is_empty()))
        .unwrap_or("undefined")
        .to_string()
}

impl<H: Host> Turn<'_, H> {
    fn fail(&self, text: &str) -> Ended {
        self.host.emit(&event("error", vec![("text", js::s(text))]));
        if self.host.left() {
            Ended::Left
        } else {
            Ended::Error
        }
    }

    /// The lease, the limit and the fit, ending with the `context` event.
    pub(crate) async fn prepare(&mut self) -> Result<Fit, Ended> {
        let host = self.host;
        let p = self.p;
        if p.model_lease {
            match host.lease(&p.model).await {
                Ok(headers) => self.lease = headers.unwrap_or_default(),
                Err(g) => {
                    host.emit(&gate_error(&g.text, g.code.as_deref(), g.retryable));
                    return Err(if host.left() {
                        Ended::Left
                    } else {
                        Ended::Error
                    });
                }
            }
        }
        if host.left() {
            return Err(Ended::Left);
        }
        let (limit, limit_source) = if p.manager_enabled {
            match self.runtime_limit().await {
                Ok(found) => found,
                Err(text) => return Err(self.fail(&text)),
            }
        } else if let Some(hosted) = &p.runtime_limit {
            // A provider row's turn: chat-context.cjs `hostedLimit`, as core settled it.
            hosted.clone()
        } else {
            (
                8192,
                "Conservative fallback; backend limit unavailable".to_string(),
            )
        };
        if host.left() {
            return Err(Ended::Left);
        }
        let id = context_id(p.context_id.as_deref(), None);
        // chat-context.cjs `prepare`: one preparation per state file (an account's chat) at a time in this
        // process; given back however the fit ends, a dropped turn included.
        let Some(_preparing) = Preparing::take(
            &self.process.preparing,
            format!("{}\u{0}{id}", host.user_id()),
        ) else {
            return Err(self
                .fail("This chat is already preparing context. Wait for that request to finish."));
        };
        self.fit(&id, limit, &limit_source).await
    }

    async fn fit(&mut self, id: &str, limit: u64, limit_source: &str) -> Result<Fit, Ended> {
        let host = self.host;
        let p = self.p;
        let mut state = host.read_state(id);
        if !state.is_object() {
            state = JValue::Obj(Vec::new());
        }
        let saved = saved_of(&state);
        let tools = JValue::Arr(p.tools.clone());
        let started = host.now_ms();
        #[allow(clippy::cast_possible_truncation)]
        let now_i = started as i64;
        let summarize = |summary: String, older: Vec<JValue>, max_tokens: u64| {
            self.summarize(summary, older, max_tokens)
        };
        let fitted = context_fit::prepare(
            &p.messages,
            &tools,
            limit,
            // chat.cjs `force: body.compactOnly === true`.
            p.compact_only,
            &saved,
            now_i,
            summarize,
            &mut |text: &str, id: &str| {
                host.emit(&status(text, id, None));
            },
        )
        .await;
        // Node takes these times after the summaries, as the fit returns.
        let now = host.now_ms();
        let others = |m: &JValue| m.get("role").as_str() != Some("system");
        let fitted = match fitted {
            Ok(f) => f,
            Err(refused) => {
                // chat-context.cjs saves the meter before it refuses a turn that is still too full; that refusal
                // only comes when no compaction ran, so the state is the saved one (its summary dropped if unused).
                if refused.0.starts_with("Context is still too full") {
                    let original: Vec<JValue> =
                        p.messages.iter().filter(|m| others(m)).cloned().collect();
                    let (applied, covered) = context_fit::apply_summary(&original, &saved);
                    let wire: Vec<JValue> = p
                        .messages
                        .iter()
                        .filter(|m| !others(m))
                        .cloned()
                        .chain(applied)
                        .collect();
                    let meter = context_fit::measure(&wire, &tools, limit);
                    if covered == 0 {
                        for k in ["summary", "covered", "prefix", "compactedAt"] {
                            js::remove(&mut state, k);
                        }
                    }
                    let mut m = meter_json(&meter, &p.model, limit_source, now);
                    js::set(&mut m, "historyCount", js::count(original.len()));
                    js::set(
                        &mut m,
                        "compactedAt",
                        state.get("compactedAt").clone().or_null(),
                    );
                    js::set(
                        &mut m,
                        "covered",
                        js::count(if covered == 0 { 0 } else { saved.covered }),
                    );
                    js::set(&mut state, "meter", m);
                    if let Err(e) = host.save_state(id, &state) {
                        return Err(self.fail(&e));
                    }
                }
                return Err(self.fail(&refused.0));
            }
        };
        let compacted_now =
            fitted.state.covered > 0 && fitted.state.summary.is_some() && fitted.state != saved;
        let compacted_at = if compacted_now {
            Some(now)
        } else {
            #[allow(clippy::cast_precision_loss)]
            fitted
                .state
                .compacted_at
                .filter(|_| fitted.state.covered > 0)
                .map(|t| t as f64)
        };
        match &fitted.state {
            Saved {
                summary: Some(summary),
                covered,
                prefix,
                ..
            } if *covered > 0 => {
                js::set(&mut state, "summary", js::s(summary.clone()));
                js::set(&mut state, "covered", js::count(*covered));
                js::set(
                    &mut state,
                    "prefix",
                    prefix.clone().map_or(JValue::Undefined, js::s),
                );
                js::set(
                    &mut state,
                    "compactedAt",
                    compacted_at.map_or(JValue::Undefined, js::n),
                );
            }
            _ => {
                for k in ["summary", "covered", "prefix", "compactedAt"] {
                    js::remove(&mut state, k);
                }
            }
        }
        let mut meter = meter_json(&fitted.meter, &p.model, limit_source, now);
        js::set(&mut meter, "historyCount", js::count(fitted.history_count));
        js::set(
            &mut meter,
            "compactedAt",
            compacted_at.map_or(JValue::Null, js::n),
        );
        js::set(&mut meter, "covered", js::count(fitted.state.covered));
        js::set(&mut state, "meter", meter.clone());
        if let Err(e) = host.save_state(id, &state) {
            return Err(self.fail(&e));
        }
        let mut context = vec![("type".to_string(), js::s("context"))];
        if let JValue::Obj(items) = meter {
            context.extend(items);
        }
        host.emit(&JValue::Obj(context));
        Ok(Fit {
            messages: fitted.messages,
            max_tokens: fitted.max_tokens,
            limit,
            limit_source: limit_source.to_string(),
            history_count: fitted.history_count,
            compacted_at,
            covered: fitted.state.covered,
        })
    }

    /// The compaction's model call (chat.cjs `summarizeContext`): effort low, no meta, 180 s.
    pub(crate) async fn summarize(
        &self,
        summary: String,
        older: Vec<JValue>,
        max_tokens: u64,
    ) -> Result<JValue, context_fit::Refused> {
        let refuse = |t: &str| context_fit::Refused(t.to_string());
        let user = js::stringify(&js::obj([
            ("previousSummary", js::s(summary)),
            ("messages", JValue::Arr(older)),
        ]));
        let body = js::obj([
            ("model", js::s(self.p.model.clone())),
            ("stream", JValue::Bool(false)),
            ("max_tokens", num(max_tokens)),
            (
                "messages",
                JValue::Arr(vec![
                    js::obj([
                        ("role", js::s("system")),
                        ("content", js::s(SUMMARY_PROMPT)),
                    ]),
                    js::obj([("role", js::s("user")), ("content", js::s(user))]),
                ]),
            ),
        ]);
        let answer = self
            .request(&body, "low", None, false, Some(180_000))
            .await
            .map_err(|e| refuse(&e))?;
        let Some(reply) = answer.body.filter(|_| (200..300).contains(&answer.status)) else {
            return Err(refuse(
                "Compaction failed at the model provider. Your transcript is unchanged.",
            ));
        };
        let (text, capped) = self.host.text(reply, SUMMARY_REPLY_CAP).await;
        if capped {
            return Err(refuse("Compaction reply from the model provider was too large. Your transcript is unchanged."));
        }
        // A reply that is not JSON: V8's parse message there, a fixed sentence here (named difference).
        let parsed = wtf16::parse(&text).map_err(|_| refuse("Compaction reply from the model provider was not JSON. Your transcript is unchanged."))?;
        let choice = match parsed.get("choices") {
            JValue::Arr(c) => c.first().cloned().unwrap_or(JValue::Undefined),
            _ => JValue::Undefined,
        };
        if choice.get("finish_reason").as_str() == Some("length") {
            return Err(refuse("Compaction summary was cut off; previous context is retained. Try Low thinking or another model."));
        }
        Ok(choice.get("message").get("content").clone())
    }

    /// chat-context.cjs `resolveManagedLimit`, with its observation file.
    async fn runtime_limit(&self) -> Result<(u64, String), String> {
        let host = self.host;
        let p = self.p;
        let model = p.model.as_str();
        let alive = |m: &JValue| {
            m.get("model_name").as_str() == Some(model)
                && m.get("loaded").truthy()
                && m.get("backend_alive") != &JValue::Bool(false)
        };
        let loaded = |h: &JValue| matches!(h.get("all_models_loaded"), JValue::Arr(rows) if rows.iter().any(alive));
        let mut health = host.health().await.ok_or(
            "Could not read the model backend context. Try again when the backend is available.",
        )?;
        if !loaded(&health) {
            let seconds = self.process.expected_load_seconds(model);
            let who = match &p.role {
                Some(role) => format!("{model} for the {role} role"),
                None => model.to_string(),
            };
            let text = format!(
                "Loading the selected model {who} and checking its context allocation…{}",
                seconds.map_or(String::new(), |s| format!(
                    " Expected wait about {} s.",
                    js_json::number_to_string(s)
                ))
            );
            let mut detail = vec![("model", js::s(model))];
            if let Some(role) = &p.role {
                detail.push(("role", js::s(role.clone())));
            }
            if let Some(s) = seconds {
                detail.push(("expectedSeconds", js::n(s)));
            }
            host.emit(&status(&text, "loadingModel", Some(js::obj(detail))));
            if host.left() {
                return Err("This operation was aborted".into());
            }
            let started = host.now_ms();
            if !host.load(model).await {
                return Err(
                    "The selected model could not load. Its context allocation was not changed."
                        .into(),
                );
            }
            self.process.record_load(model, host.now_ms() - started);
            health = host.health().await.ok_or(
                "The model loaded, but its context allocation could not be checked. Try again.",
            )?;
            if !loaded(&health) {
                return Err("The selected model is no longer loaded. Another request may have switched models; try again.".into());
            }
        }
        let rows = match health.get("all_models_loaded") {
            JValue::Arr(rows) => rows.clone(),
            _ => Vec::new(),
        };
        let configured = rows.iter().find(|m| alive(m)).map_or(f64::NAN, |e| {
            js::to_number(e.get("recipe_options").get("ctx_size"))
        });
        let (limit, source) = if configured.is_finite() && configured >= 2048.0 {
            // Floored, at least 2048: a context size.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let l = configured.floor() as u64;
            (l, "Configured backend context")
        } else {
            (8192, "Conservative fallback; backend limit unavailable")
        };
        let entry = rows
            .iter()
            .find(|m| m.get("model_name").as_str() == Some(model) && m.get("loaded").truthy());
        if let Some(entry) =
            entry.filter(|e| js::to_number(e.get("recipe_options").get("ctx_size")) >= 2048.0)
        {
            let id = format!(
                "runtime-model:{}",
                context_fit::fingerprint(&JValue::Arr(vec![
                    js::s(p.limit_scope.clone()),
                    js::s(model)
                ]))
            );
            let previous = host.read_state(&id);
            let configuration = context_fit::fingerprint(&JValue::Arr(vec![
                health.get("manager").clone(),
                health.get("version").clone(),
                entry.get("engineVersion").clone(),
                entry.get("checkpoint").clone(),
                entry.get("recipe_options").clone(),
                entry.get("slots").clone(),
            ]));
            let observation = js::obj([
                ("model", js::s(model)),
                ("limit", num(limit)),
                ("source", js::s(source)),
                ("configuration", js::s(configuration.clone())),
                ("managerVersion", health.get("version").clone().or_null()),
                (
                    "engineVersion",
                    entry.get("engineVersion").clone().or_null(),
                ),
                ("qualification", js::s("allocation-observation-only")),
                ("observedAt", js::n(host.now_ms())),
            ]);
            let mut history = match previous.get("history") {
                JValue::Arr(h) => h.clone(),
                _ => Vec::new(),
            };
            let current = previous.get("current");
            if current.truthy()
                && current.get("configuration").as_str() != Some(configuration.as_str())
            {
                history.push(current.clone());
            }
            let keep = history.len().saturating_sub(20);
            history.drain(..keep);
            host.save_state(
                &id,
                &js::obj([("current", observation), ("history", JValue::Arr(history))]),
            )?;
        }
        Ok((limit, source.to_string()))
    }
}

/// An engine request with this turn's headers.
pub(crate) fn engine_request(
    url: &str,
    headers: Vec<(String, String)>,
    body: &JValue,
    stream: bool,
    timeout_ms: Option<u64>,
) -> EngineRequest {
    EngineRequest {
        url: url.to_string(),
        headers,
        body: js::stringify(body),
        stream,
        timeout_ms,
    }
}
