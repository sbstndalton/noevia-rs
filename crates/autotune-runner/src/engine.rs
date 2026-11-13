//! The runner's calls to the router (core llamacpp-full-autotune.cjs `engineBoundary`,
//! `requestsProcessing`, `heldForeign`, `yieldToForeign`, `unloadAll`, `chat`, `load`, `unloadOwn`,
//! the deferred re-read after a restore, `flushReload`, `routerPairs` and the untuned scan).

use crate::deps::{Abort, Call, Deps, Fail, Spawn};
use crate::runner::Runner;
use crate::{js, quality};
use js_json::{wtf16, JValue};
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

/// Router states in which a row holds (or is about to hold) memory (`LIVE`).
const LIVE: [&str; 3] = ["loaded", "loading", "sleeping"];

fn status_of(row: &JValue) -> Option<&str> {
    row.get("status").get("value").as_str()
}

fn live(row: &JValue) -> bool {
    status_of(row).is_some_and(|s| LIVE.contains(&s))
}

/// `String(v)`.
pub(crate) fn string_of(v: &JValue) -> String {
    js_json::to_js_string(v).unwrap_or_default()
}

/// `Array.isArray(r.body?.data)` and `r.ok`.
fn listing_rows(r: &JValue) -> Option<&[JValue]> {
    if !r.get("ok").truthy() {
        return None;
    }
    match r.get("body").get("data") {
        JValue::Arr(rows) => Some(rows),
        _ => None,
    }
}

/// `encodeURIComponent(s)` (`None` where JS throws: a lone surrogate).
pub(crate) fn encode_uri_component(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if wtf16::lone_unit(c).is_some() {
            return None;
        }
        if c.is_ascii_alphanumeric() || "-_.!~*'()".contains(c) {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    Some(out)
}

/// `idList(ids)`: at most four ids (each cut to 120 UTF-16 units) and how many more.
pub(crate) fn id_list(ids: &[JValue]) -> String {
    let mut out = ids
        .iter()
        .take(4)
        .map(|id| wtf16::slice(&string_of(id), 120))
        .collect::<Vec<_>>()
        .join(", ");
    if ids.len() > 4 {
        out.push_str(&format!(" and {} more", ids.len() - 4));
    }
    out
}

/// llamacpp-metrics.cjs `parseMetrics(text).requests_processing` (the last valid line wins).
pub(crate) fn requests_processing_of(text: &str) -> Option<f64> {
    let mut value = None;
    for line in text.split('\n') {
        let Some(rest) = line.strip_prefix("llamacpp:") else {
            continue;
        };
        let name_len = rest
            .bytes()
            .take_while(|b| b.is_ascii_lowercase() || *b == b'_')
            .count();
        if name_len == 0 {
            continue;
        }
        let (name, after) = rest.split_at(name_len);
        let trimmed = after.trim_start_matches(js::is_js_space);
        if trimmed.len() == after.len() || trimmed.is_empty() || trimmed.contains(js::is_js_space) {
            continue;
        }
        let n = js::number_of_str(trimmed);
        if name == "requests_processing" && n.is_finite() && n >= 0.0 {
            value = Some(n);
        }
    }
    value
}

/// `Object.assign(Error('Another client loaded a model during tuning.'), { fatal: true })`.
fn another_client() -> Fail {
    Fail::fatal("Another client loaded a model during tuning.")
}

const MEMORY_FLOOR: &str = "Available memory fell below the safety floor.";

impl<D: Deps> Runner<D> {
    /// `engineBoundary`: the engine's own words never reach the job (#1049).
    fn boundary(&self, r: Result<JValue, Fail>) -> Result<JValue, Fail> {
        r.map_err(|e| {
            if e.cancelled || e.abort || e.fatal || e.public_message.is_some() {
                return e;
            }
            self.deps()
                .log(&format!("[autotune] engine request failed: {}", e.message));
            let lower = e.message.to_lowercase();
            Fail {
                engine_failure: true,
                ..Fail::new(
                    if lower.contains("timed out") || lower.contains("timeout") {
                        "The model server request timed out."
                    } else {
                        "The model server request failed."
                    },
                )
            }
        })
    }

    pub(crate) async fn raw_models(&self) -> Result<JValue, Fail> {
        let r = self.deps().raw_models().await;
        self.boundary(r)
    }

    pub(crate) async fn request(&self, call: Call, abort: Option<Abort>) -> Result<JValue, Fail> {
        let r = self.deps().request(call, abort).await;
        self.boundary(r)
    }

    /// `requestsProcessing(id)`: requests in flight on a resident model, `None` when unreadable.
    pub(crate) async fn requests_processing(&self, id: &JValue) -> Option<f64> {
        let path = format!(
            "/metrics?model={}&autoload=false",
            encode_uri_component(&string_of(id))?
        );
        let r = self.request(Call::get(path, 6000), None).await.ok()?;
        if !r.get("ok").truthy() {
            return None;
        }
        let text = r.get("body").as_str()?;
        requests_processing_of(text)
            .filter(|n| n.fract() == 0.0 && *n >= 0.0 && *n <= f64::from(u32::MAX))
    }

    /// `heldForeign(rows, tuning)` (#1067): live rows of another client that only the contention
    /// decision may stop.
    pub(crate) async fn held_foreign(&self, rows: &[JValue], tuning: &JValue) -> Vec<JValue> {
        let done: Vec<JValue> = js::items(self.job().get("models"))
            .iter()
            .filter(|m| m.get("status").as_str() == Some("passed"))
            .map(|m| m.get("model").clone())
            .collect();
        let mut held = Vec::new();
        for row in rows {
            if row.get("id") == tuning || !live(row) {
                continue;
            }
            if status_of(row) == Some("loading") {
                held.push(row.get("id").clone());
                continue;
            }
            let busy = self.requests_processing(row.get("id")).await;
            if busy.is_some_and(|n| n > 0.0) || (busy.is_none() && !done.contains(row.get("id"))) {
                held.push(row.get("id").clone());
            }
        }
        held
    }

    /// `yieldToForeign(j, legacy, { restoring })` (#1062): waits until the contention decision
    /// says the router is clear (or unloads the other client's idle models), at most
    /// [`crate::runner::FOREIGN_YIELDS`] waits per model.
    pub(crate) fn yield_to_foreign<'a>(
        &'a self,
        legacy: Fail,
        restoring: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), Fail>> + Send + 'a>> {
        Box::pin(async move {
            let tuning = self.job().get("model").clone();
            let Some(tuning_name) = tuning
                .as_str()
                .filter(|t| !t.is_empty())
                .map(str::to_string)
            else {
                return Err(legacy);
            };
            let mi = self.current_item();
            let started_at = self.deps().now();
            let mut prev = JValue::Null;
            let mut announced = false;
            let config = self.config().clone();
            loop {
                if !restoring {
                    self.check()?;
                }
                let listing = self.raw_models().await?;
                let Some(data) = listing_rows(&listing) else {
                    return Err(Fail::new("The model server stopped responding."));
                };
                let mut rows = Vec::new();
                for row in data {
                    let status = row
                        .get("status")
                        .get("value")
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    let resident = row.get("id").as_str() != Some(tuning_name.as_str())
                        && (status == "loaded" || status == "sleeping");
                    let busy = if resident {
                        self.requests_processing(row.get("id"))
                            .await
                            .map_or(JValue::Null, JValue::Num)
                    } else {
                        JValue::Null
                    };
                    rows.push(JValue::obj([
                        ("id", row.get("id").clone()),
                        ("status", JValue::from(status)),
                        ("busy", busy),
                    ]));
                }
                let request = JValue::obj([
                    ("tuning", tuning.clone()),
                    ("rows", JValue::Arr(rows)),
                    ("prev", prev.clone()),
                    ("startedAt", JValue::Num(started_at)),
                    ("now", JValue::Num(self.deps().now())),
                    ("maxWaitMs", JValue::Num(config.foreign_wait_ms as f64)),
                    ("quietMs", JValue::Num(config.foreign_quiet_ms as f64)),
                ]);
                let Ok(d) = self.deps().contention(&request) else {
                    return Err(legacy);
                };
                let action = d.get("action").as_str().unwrap_or("");
                if action == "proceed" {
                    if announced {
                        self.note("The other client is done; tuning continues.")?;
                    }
                    return Ok(());
                }
                let foreign = js::items(d.get("foreign")).to_vec();
                if !announced {
                    announced = true;
                    let yields = match mi {
                        Some(mi) => self
                            .with_item(mi, |item| {
                                let n = item
                                    .get("_foreignYields")
                                    .as_num()
                                    .filter(|n| *n != 0.0 && !n.is_nan())
                                    .unwrap_or(0.0)
                                    + 1.0;
                                js::set(item, "_foreignYields", JValue::Num(n));
                                n
                            })
                            .unwrap_or(1.0),
                        None => 1.0,
                    };
                    if yields > f64::from(crate::runner::FOREIGN_YIELDS) {
                        return Err(Fail {
                            foreign_timeout: true,
                            ..Fail::fatal(format!(
                                "Another client kept loading {} during tuning. Stop it, then resume auto-tune.",
                                id_list(&foreign)
                            ))
                        });
                    }
                    self.note(&format!(
                        "Another client is using {}. Tuning is paused until it is idle (at most {} min).",
                        id_list(&foreign),
                        js::num(js::round(config.foreign_wait_ms as f64 / 60000.0))
                    ))?;
                }
                if action == "give_up" {
                    let waited = d.get("waitedMs").as_num().unwrap_or(f64::NAN);
                    return Err(Fail {
                        foreign_timeout: true,
                        ..Fail::fatal(format!(
                            "Another client kept using {} for {} min. Resume auto-tune when it is done.",
                            id_list(&foreign),
                            js::num(js::round(waited / 60000.0))
                        ))
                    });
                }
                if action == "unload" {
                    let mut raced = false;
                    let unload = js::items(d.get("unload")).to_vec();
                    for id in &unload {
                        if !restoring {
                            self.check()?;
                        }
                        // #1069: a request that just arrived wins.
                        let now2 = self.requests_processing(id).await;
                        if now2.is_some_and(|n| n > 0.0)
                            || (now2.is_none() && d.get("reason").as_str() == Some("idle"))
                        {
                            raced = true;
                            break;
                        }
                        let u = self
                            .request(
                                Call::post(
                                    "/models/unload",
                                    JValue::obj([("model", id.clone())]),
                                    60000,
                                ),
                                None,
                            )
                            .await?;
                        if !u.get("ok").truthy() {
                            return Err(Fail::new(format!(
                                "Could not unload {} before testing.",
                                string_of(id)
                            )));
                        }
                    }
                    if !raced {
                        self.note(&format!(
                            "Unloaded {}, idle after another client used it; tuning continues.",
                            id_list(&unload)
                        ))?;
                        return Ok(());
                    }
                    prev = JValue::Null;
                    self.deps().sleep(config.foreign_poll_ms).await;
                    continue;
                }
                prev = JValue::obj([
                    ("fingerprint", d.get("fingerprint").clone()),
                    ("since", d.get("since").clone()),
                ]);
                self.deps().sleep(config.foreign_poll_ms).await;
            }
        })
    }

    /// `unloadAll({ restoring })`: every live model unloaded and confirmed, waiting for another
    /// client when one is using the router.
    pub(crate) async fn unload_all(&self, restoring: bool) -> Result<(), Fail> {
        loop {
            match self.unload_all_once(restoring).await? {
                None => return Ok(()),
                Some(foreign) => self.yield_to_foreign(Fail::new(foreign), restoring).await?,
            }
        }
    }

    async fn unload_all_once(&self, restoring: bool) -> Result<Option<String>, Fail> {
        if !restoring {
            self.check()?;
        }
        let tuning = self.job().get("model").clone();
        let r = self.raw_models().await?;
        let Some(data) = listing_rows(&r).map(<[JValue]>::to_vec) else {
            return Err(Fail::new("The model server is not responding."));
        };
        let mut asked: Vec<JValue> = Vec::new();
        let mut gone: Vec<JValue> = Vec::new();
        if tuning.truthy() {
            let held = self.held_foreign(&data, &tuning).await;
            if !held.is_empty() {
                return Ok(Some(format!(
                    "Another client has {} loaded.",
                    id_list(&held)
                )));
            }
        }
        for row in &data {
            if !live(row) {
                continue;
            }
            asked.push(row.get("id").clone());
            if !restoring {
                self.check()?;
            }
            let u = self
                .request(
                    Call::post(
                        "/models/unload",
                        JValue::obj([("model", row.get("id").clone())]),
                        60000,
                    ),
                    None,
                )
                .await?;
            if !u.get("ok").truthy() {
                return Err(Fail::new(format!(
                    "Could not unload {} before testing.",
                    string_of(row.get("id"))
                )));
            }
        }
        let deadline = self.deps().now() + 60000.0;
        for poll in 0..120 {
            if !restoring {
                self.check()?;
            }
            let listing = self.raw_models().await?;
            let Some(rows) = listing_rows(&listing) else {
                return Err(Fail::new(
                    "The model server stopped responding while unloading.",
                ));
            };
            for row in rows {
                if status_of(row) == Some("unloaded") {
                    gone.push(row.get("id").clone());
                }
            }
            let pending: Vec<&JValue> = rows
                .iter()
                .filter(|r| status_of(r) != Some("unloaded"))
                .collect();
            if pending.is_empty() {
                return Ok(None);
            }
            let message = format!(
                "Timed out waiting for router unload: {}.",
                pending
                    .iter()
                    .map(|row| {
                        let status = row.get("status").get("value");
                        let shown = if status.truthy() {
                            string_of(status)
                        } else {
                            "unknown".to_string()
                        };
                        format!("{} ({shown})", string_of(row.get("id")))
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let foreign: Vec<&&JValue> = pending
                .iter()
                .filter(|row| row.get("id") != &tuning && live(row))
                .collect();
            if foreign.iter().any(|row| {
                status_of(row) == Some("loading")
                    || gone.contains(row.get("id"))
                    || !asked.contains(row.get("id"))
            }) {
                return Ok(Some(message));
            }
            if poll == 119 || self.deps().now() >= deadline {
                if !foreign.is_empty() {
                    return Ok(Some(message));
                }
                return Err(Fail::new(message));
            }
            self.deps().sleep(500).await;
        }
        Ok(None)
    }

    /// `load(j)`: loads the profile under test and waits until the router lists it loaded.
    pub(crate) fn load(&self) -> Pin<Box<dyn Future<Output = Result<(), Fail>> + Send + '_>> {
        Box::pin(async move {
            self.check()?;
            let model = self.job().get("model").clone();
            let ask = || async {
                let response = self
                    .request(
                        Call::post(
                            "/models/load",
                            JValue::obj([("model", model.clone())]),
                            600_000,
                        ),
                        None,
                    )
                    .await?;
                if response.get("ok").truthy() {
                    Ok(())
                } else {
                    Err(Fail::new("The test profile could not be loaded."))
                }
            };
            ask().await?;
            let floor = self.config().memory_floor_gib;
            let mut polls = 0;
            while polls < 1200 {
                self.check()?;
                if self.deps().read_memory().is_some_and(|free| free < floor) {
                    return Err(Fail::new(MEMORY_FLOOR));
                }
                let r = self.raw_models().await?;
                if !r.get("ok").truthy() {
                    return Err(Fail::new("The model server stopped responding."));
                }
                let data = js::items(r.get("body").get("data"));
                if data.iter().any(|m| {
                    m.get("id") != &model && matches!(status_of(m), Some("loaded" | "loading"))
                }) {
                    // #1062: the router evicted the profile under test for another client.
                    self.yield_to_foreign(another_client(), false).await?;
                    ask().await?;
                    polls = 1;
                    continue;
                }
                let row = data.iter().find(|m| m.get("id") == &model);
                if row.and_then(status_of) == Some("loaded") {
                    return Ok(());
                }
                if row.is_none_or(|row| {
                    row.get("status").get("failed").truthy() || status_of(row) == Some("unloaded")
                }) {
                    return Err(Fail::new("The test profile failed to load."));
                }
                self.deps().sleep(500).await;
                polls += 1;
            }
            Err(Fail::new("Loading the test profile timed out."))
        })
    }

    /// `othersLiveNow(model)`: another client has a model live (`None`: unreadable).
    pub(crate) async fn others_live_now(&self, model: &JValue) -> Option<bool> {
        let r = self.raw_models().await.ok()?;
        let data = listing_rows(&r)?;
        Some(data.iter().any(|row| row.get("id") != model && live(row)))
    }

    /// `evictedBy(model)`.
    async fn evicted_by(&self, model: &str) -> bool {
        self.job().get("model").as_str() == Some(model)
            && self.others_live_now(&JValue::from(model)).await == Some(true)
    }

    /// `chat(model, prompt, max, extra)`: one non-streamed completion, with the memory guard and
    /// the wait for another client (#1062, #1069). `extra` (the serving check) answers
    /// `{ status, bodyText }`; otherwise `{ text, gen, prompt, drafted, accepted, finishReason }` or
    /// `{ failure }`.
    pub(crate) fn chat<'a>(
        &'a self,
        model: &'a str,
        prompt: &'a str,
        max: u32,
        extra: Option<&'a JValue>,
    ) -> Pin<Box<dyn Future<Output = Result<JValue, Fail>> + Send + 'a>> {
        Box::pin(async move {
            self.check()?;
            let rows = self.raw_models().await?;
            let Some(data) = listing_rows(&rows) else {
                return Err(Fail::new("The model server stopped responding."));
            };
            if data.iter().any(|r| {
                r.get("id").as_str() != Some(model)
                    && matches!(status_of(r), Some("loaded" | "loading"))
            }) {
                if self.job().get("model").as_str() != Some(model) {
                    return Err(another_client());
                }
                self.yield_to_foreign(another_client(), false).await?;
                self.load().await?;
                return self.chat(model, prompt, max, extra).await;
            }
            let abort = Abort::default();
            self.st().set_inflight(Some(abort.clone()));
            let floor = self.config().memory_floor_gib;
            let low = std::sync::atomic::AtomicBool::new(false);
            let guard = || {
                if self.deps().read_memory().is_some_and(|free| free < floor) {
                    low.store(true, std::sync::atomic::Ordering::SeqCst);
                    abort.abort();
                }
            };
            guard();
            let mut body = JValue::obj([
                ("model", JValue::from(model)),
                ("stream", JValue::Bool(false)),
                ("temperature", JValue::Num(0.0)),
                ("max_tokens", JValue::Num(f64::from(max))),
                ("cache_prompt", JValue::Bool(false)),
            ]);
            if let Some(effort) = self
                .deps()
                .quirks(model)
                .reasoning_effort
                .filter(|e| !e.is_empty())
            {
                js::set(&mut body, "reasoning_effort", effort);
            }
            js::set(
                &mut body,
                "chat_template_kwargs",
                JValue::obj([("enable_thinking", JValue::Bool(false))]),
            );
            js::set(
                &mut body,
                "messages",
                JValue::Arr(vec![JValue::obj([
                    ("role", JValue::from("user")),
                    ("content", JValue::from(prompt)),
                ])]),
            );
            if let Some(extra) = extra.and_then(js::entries) {
                for (k, v) in extra {
                    js::set(&mut body, k, v.clone());
                }
            }
            let request = self.request(
                Call::post("/v1/chat/completions", body, 180_000),
                Some(abort.clone()),
            );
            let result = with_guard(request, async {
                loop {
                    self.deps().tick(500).await;
                    guard();
                }
            })
            .await;
            self.st().set_inflight(None);
            let low_memory = || low.load(std::sync::atomic::Ordering::SeqCst);
            let r = match result {
                Ok(r) => r,
                Err(e) => {
                    if low_memory() {
                        return Err(Fail::new(MEMORY_FLOOR));
                    }
                    if !self.cancelled() && !e.cancelled && self.evicted_by(model).await {
                        return self.retry_after_foreign(model, prompt, max, extra).await;
                    }
                    return Err(e);
                }
            };
            self.check()?;
            if low_memory() {
                return Err(Fail::new(MEMORY_FLOOR));
            }
            let ok = r.get("ok").truthy();
            let status = js::number(r.get("status"));
            let choices = js::items(r.get("body").get("choices"));
            let failed = if extra.is_some() {
                !(200.0..300.0).contains(&status) && !ok
            } else {
                !ok || choices.is_empty()
            };
            if failed && self.evicted_by(model).await {
                return self.retry_after_foreign(model, prompt, max, extra).await;
            }
            if extra.is_some() {
                let shown = match r.get("status") {
                    JValue::Num(n) if n.fract() == 0.0 && n.is_finite() => *n,
                    _ if ok => 200.0,
                    _ => 0.0,
                };
                let text = match r.get("body") {
                    JValue::Str(s) => s.clone(),
                    JValue::Undefined | JValue::Null => "null".to_string(),
                    other => wtf16::stringify(other).unwrap_or_default(),
                };
                return Ok(JValue::obj([
                    ("status", JValue::Num(shown)),
                    ("bodyText", JValue::from(text)),
                ]));
            }
            if !ok {
                let s = r.get("status");
                let shown = if s.truthy() {
                    string_of(s)
                } else {
                    "error".to_string()
                };
                return Ok(JValue::obj([(
                    "failure",
                    JValue::from(format!("HTTP {shown}")),
                )]));
            }
            let Some(choice) = choices.first() else {
                return Ok(JValue::obj([("failure", JValue::from("no response"))]));
            };
            let t = r.get("body").get("timings");
            let content = choice.get("message").get("content");
            let or_zero = |v: f64| if v.is_nan() || v == 0.0 { 0.0 } else { v };
            Ok(JValue::obj([
                (
                    "text",
                    if content.truthy() {
                        content.clone()
                    } else {
                        JValue::from("")
                    },
                ),
                (
                    "gen",
                    JValue::Num(js::number(t.get("predicted_per_second"))),
                ),
                (
                    "prompt",
                    JValue::Num(js::number(t.get("prompt_per_second"))),
                ),
                (
                    "drafted",
                    JValue::Num(or_zero(js::number(t.get("draft_n")))),
                ),
                (
                    "accepted",
                    JValue::Num(or_zero(js::number(t.get("draft_n_accepted")))),
                ),
                ("finishReason", choice.get("finish_reason").clone()),
            ]))
        })
    }

    async fn retry_after_foreign(
        &self,
        model: &str,
        prompt: &str,
        max: u32,
        extra: Option<&JValue>,
    ) -> Result<JValue, Fail> {
        self.yield_to_foreign(another_client(), false).await?;
        self.load().await?;
        self.chat(model, prompt, max, extra).await
    }

    /// `qualityCheck(model, chat, gate(model))` over this runner's chat.
    pub(crate) async fn quality(
        &self,
        model: &str,
        baseline: Option<&JValue>,
    ) -> Result<JValue, Fail> {
        let budget = self
            .deps()
            .quirks(model)
            .quality_budget
            .filter(|b| *b != 0)
            .unwrap_or(64);
        // quality::quality_check's loop, written out: an async closure here would not be `Send`.
        let mut checks = Vec::new();
        for q in quality::probes_for(baseline) {
            let r = self.chat(model, q.prompt, budget, None).await?;
            checks.push(quality::judge(q, &r));
        }
        Ok(quality::summary(checks, baseline))
    }

    /// `unloadOwn(j)` (#1068): only the compact model stopped, never another client's.
    pub(crate) async fn unload_own(&self) -> Result<(), Fail> {
        let model = self.job().get("model").clone();
        let first = self.raw_models().await?;
        let Some(data) = listing_rows(&first) else {
            return Err(Fail::new(
                "The model server stopped responding while unloading.",
            ));
        };
        if data
            .iter()
            .find(|m| m.get("id") == &model)
            .is_some_and(live)
        {
            // A refused unload is not a failure if the row goes (or is) unloaded anyway.
            let _ = self
                .request(
                    Call::post(
                        "/models/unload",
                        JValue::obj([("model", model.clone())]),
                        60000,
                    ),
                    None,
                )
                .await;
        }
        let deadline = self.deps().now() + 60000.0;
        for _ in 0..120 {
            let listing = self.raw_models().await?;
            let Some(rows) = listing_rows(&listing) else {
                return Err(Fail::new(
                    "The model server stopped responding while unloading.",
                ));
            };
            let row = rows.iter().find(|m| m.get("id") == &model);
            if row.is_none_or(|r| status_of(r) == Some("unloaded")) {
                return Ok(());
            }
            if self.deps().now() >= deadline {
                break;
            }
            self.deps().sleep(500).await;
        }
        Err(Fail::new(format!(
            "Timed out waiting for router unload: {}.",
            string_of(&model)
        )))
    }

    /// `request('/models?reload=1', {}, 120000)`.
    pub(crate) async fn reread(&self) -> Result<JValue, Fail> {
        self.request(Call::get("/models?reload=1", 120_000), None)
            .await
    }

    /// The internal `reloadRouter(j)` (#1068): the router re-reads the restored file now, or once
    /// no other client has a model live.
    pub(crate) async fn reread_after_restore(&self) -> Result<(), Fail> {
        let model = self.job().get("model").clone();
        if self.others_live_now(&model).await == Some(false) {
            if self.deps().invalidate_baseline("autotune").await {
                let reload = self.reread().await?;
                if !reload.get("ok").truthy() {
                    return Err(Fail::new("The router did not confirm restored settings."));
                }
                self.with_job(|j| js::set(j, "_reloadPending", false));
                return self.save();
            }
            self.with_job(|j| js::set(j, "_reloadPending", true));
            self.note("The model server has not re-read the restored models.ini; it does so before this model next loads.")?;
            self.schedule_reload(0);
            return Ok(());
        }
        self.with_job(|j| js::set(j, "_reloadPending", true));
        self.note(
            "models.ini is restored; the model server re-reads it once the other client is done.",
        )?;
        self.schedule_reload(0);
        Ok(())
    }

    /// `scheduleReload(j, tries)`: a background re-read every `foreignPollMs * 15` until it
    /// succeeds (at most 120 tries).
    pub(crate) fn schedule_reload(&self, tries: u32) {
        let generation = {
            let mut st = self.st();
            st.reload_timer += 1;
            st.reload_timer
        };
        if !self.job().get("_reloadPending").truthy() {
            return;
        }
        if tries > 120 {
            // The durable job may be unavailable during shutdown; the re-read remains owed.
            let _ = self.note("The model server has not re-read the restored models.ini; it does so before this model next loads.");
            return;
        }
        let me = self.clone();
        let delay = (self.config().foreign_poll_ms * 15).max(1);
        self.deps().spawn(Spawn::Timer, async move {
            me.deps().tick(delay).await;
            if me.st().reload_timer != generation || !me.job().get("_reloadPending").truthy() {
                return;
            }
            let model = me.job().get("model").clone();
            if me.others_live_now(&model).await == Some(false) {
                // #1072: a flush may have re-read the file while this check was in flight.
                if !me.job().get("_reloadPending").truthy() {
                    return;
                }
                if me.deps().invalidate_baseline("autotune").await {
                    if let Ok(r) = me.reread().await {
                        if r.get("ok").truthy() {
                            me.with_job(|j| js::set(j, "_reloadPending", false));
                            let _ = me.note("The model server re-read the restored models.ini.");
                            return;
                        }
                    }
                }
            }
            me.schedule_reload(tries + 1);
        });
    }

    /// `flushReload(model, guard)` (#1068): before the compact model loads again, the router
    /// re-reads the restored file even with another client live. A no-op unless one is owed.
    pub(crate) async fn flush_reload(&self, model: Option<&str>) -> Result<(), Fail> {
        self.flush_reload_guarded(model, &|| Ok(())).await
    }

    pub(crate) async fn flush_reload_guarded(
        &self,
        model: Option<&str>,
        guard: &(dyn Fn() -> Result<(), Fail> + Sync),
    ) -> Result<(), Fail> {
        let j = self.job();
        if !j.get("_reloadPending").truthy()
            || model.is_some_and(|m| !m.is_empty() && j.get("model").as_str() != Some(m))
        {
            return Ok(());
        }
        self.st().reload_timer += 1;
        let r = if self.deps().invalidate_baseline("autotune").await {
            self.reread().await.ok()
        } else {
            None
        };
        guard()?;
        if !r.is_some_and(|r| r.get("ok").truthy()) {
            self.schedule_reload(0);
            return Err(Fail::public(
                "The model server has not re-read the restored settings yet. Try again shortly.",
            ));
        }
        self.with_job(|j| js::set(j, "_reloadPending", false));
        self.note("The model server re-read the restored models.ini.")
    }

    /// `routerPairs()` (#1079): the router's rows and which of them pair as a model and its long
    /// profile.
    pub(crate) async fn router_pairs(&self) -> Result<(JValue, JValue), Fail> {
        let r = self.raw_models().await?;
        let Some(rows) = listing_rows(&r) else {
            return Err(Fail::public("The model server is not responding."));
        };
        let rows = JValue::Arr(rows.to_vec());
        let pairs = self.deps().long_pairs(&rows);
        Ok((rows, pairs))
    }

    /// `scanProfile(model)`.
    pub(crate) fn scan_profile(&self, model: &str) -> JValue {
        let p = self.deps().presets_get(model);
        JValue::obj([
            ("exists", JValue::Bool(p.get("exists").truthy())),
            ("defaults", js::spread(&[p.get("defaults")])),
            ("options", js::spread(&[p.get("options")])),
        ])
    }

    /// `scanFacts()`: the untuned scan's per-row facts (the classification, signature and
    /// selection are the scan policy's).
    pub(crate) async fn scan_facts(&self) -> Result<JValue, Fail> {
        let r = self.raw_models().await?;
        let Some(data) = listing_rows(&r).map(<[JValue]>::to_vec) else {
            return Err(Fail::public("The model server is not responding."));
        };
        if data.len() > 512 {
            return Err(Fail::new("scan row cap"));
        }
        let rows_value = JValue::Arr(data.clone());
        let longs: Vec<JValue> = js::items(&self.deps().long_pairs(&rows_value))
            .iter()
            .map(|p| p.get("long").clone())
            .collect();
        let mut rows = Vec::new();
        for raw in &data {
            let id = raw.get("id").clone();
            let id_text = string_of(&id);
            let profile = self.scan_profile(&id_text);
            let args = raw.get("status").get("args");
            let args = if args.truthy() {
                args.clone()
            } else {
                JValue::Arr(Vec::new())
            };
            let path = crate::model_path_from_args(&args);
            let system = self.deps().is_system_model(&id_text, path.as_deref())
                || self.deps().is_sidecar_model(&id_text);
            let mut row = JValue::obj([
                ("id", id.clone()),
                ("long", JValue::Bool(longs.contains(&id))),
                ("system", JValue::Bool(system)),
                ("missing", JValue::Bool(self.deps().file_missing(raw))),
                ("configured", profile.get("exists").clone()),
                ("profile", profile),
                ("args", args),
                ("latest", JValue::Null),
            ]);
            let classified = self.deps().scan(JValue::obj([
                ("version", JValue::Num(1.0)),
                ("op", JValue::from("classify")),
                ("rows", JValue::Arr(vec![row.clone()])),
            ]))?;
            if !js::items(classified.get("eligible")).is_empty() {
                js::set(&mut row, "allowQ5Kv", self.allow_q5(&id_text));
                let identity = self.deps().identity_for(&id_text).await?;
                let Some(identity) = identity.filter(JValue::truthy) else {
                    return Err(Fail::public("Model identity could not be read."));
                };
                js::set(&mut row, "identity", identity);
                js::set(&mut row, "profile", self.scan_profile(&id_text));
                let latest = js::items(self.st().state.get("history").get(&id_text))
                    .first()
                    .cloned();
                js::set(
                    &mut row,
                    "latest",
                    match latest {
                        None | Some(JValue::Null) | Some(JValue::Undefined) => JValue::Null,
                        Some(l) => JValue::obj([
                            ("version", l.get("version").clone()),
                            ("signature", l.get("signature").clone()),
                        ]),
                    },
                );
            }
            rows.push(row);
        }
        Ok(JValue::obj([
            ("version", JValue::Num(1.0)),
            ("rows", JValue::Arr(rows)),
        ]))
    }

    /// `candidates()`: the scan policy over the scan facts.
    pub(crate) async fn candidates(&self) -> Result<JValue, Fail> {
        let facts = self.scan_facts().await?;
        let mut request =
            JValue::obj([("version", JValue::Num(1.0)), ("op", JValue::from("scan"))]);
        if let Some(items) = js::entries(&facts) {
            for (k, v) in items {
                js::set(&mut request, k, v.clone());
            }
        }
        self.deps().scan(request)
    }
}

/// Polls `main` to its end while `side` (a loop that never ends) runs beside it.
async fn with_guard<T>(main: impl Future<Output = T>, side: impl Future<Output = ()>) -> T {
    let mut main = std::pin::pin!(main);
    let mut side = std::pin::pin!(side);
    std::future::poll_fn(|cx| {
        if let Poll::Ready(v) = main.as_mut().poll(cx) {
            return Poll::Ready(v);
        }
        let _ = side.as_mut().poll(cx);
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_component() {
        assert_eq!(
            encode_uri_component("qa model/é").as_deref(),
            Some("qa%20model%2F%C3%A9")
        );
        assert_eq!(
            encode_uri_component("a-b_c.d!~*'()").as_deref(),
            Some("a-b_c.d!~*'()")
        );
    }

    #[test]
    fn metrics_line() {
        let text = "# HELP\nllamacpp:requests_processing 2\nllamacpp:requests_processing{x} 5\n";
        assert_eq!(requests_processing_of(text), Some(2.0));
        assert_eq!(
            requests_processing_of("llamacpp:requests_processing 1\r"),
            None
        );
        assert_eq!(
            requests_processing_of("llamacpp:requests_processing -1"),
            None
        );
        assert_eq!(
            requests_processing_of("llamacpp:requests_processing 0x10"),
            Some(16.0)
        );
    }

    #[test]
    fn ids() {
        let ids: Vec<JValue> = (0..6).map(|i| JValue::from(format!("m{i}"))).collect();
        assert_eq!(id_list(&ids), "m0, m1, m2, m3 and 2 more");
    }
}
