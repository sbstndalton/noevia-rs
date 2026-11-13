//! The native continuation of a prepared turn: chat.cjs from the hand-off to the title (lines
//! ~1766-2632), for the turns core hands off (rust-chat-native.cjs `refusal`).
//!
//! In order: the model's lease, the runtime context limit (loading the model when it is not loaded),
//! the context fit with the saved summary ([`context`]); the template's tools check and the route
//! notes; the rounds, each with its budget check and continuation compaction, its request with
//! reasoning effort and thinking cap, its stream with the idle limit and the forced answer after a cut,
//! and the empty stream's fallback ([`stream`]); after each round, Node's decisions in Node's order (the
//! announced-action retry, the final-answer round's calls not run, the tool calls through
//! [`crate::gate`], the decline pause, the final-answer round); the close (`telemetry` complete,
//! `done`) and the title. What a turn keeps when it ends is [`crate::after`].
//!
//! Named differences from Node, each documented where it is decided:
//! - waiting for the model's lease shows no queue position (the admission latch answers once granted);
//! - a summary or fallback reply that is not JSON reports a fixed sentence, not V8's parse message (both
//!   go through the provider-error classifier);
//! - frames `js_json::wtf16` cannot parse (numbers past f64, nesting past its limit) are skipped, where
//!   `JSON.parse` would read them;
//! - `CONTEXT_LOG` round logs are not written; the step-supervision decision is not asked (a turn with
//!   it on is not handed off);
//! - a non-streaming fallback reply whose tool call names a non-string `function.name` keeps it as its
//!   text; Node keeps the raw value, so its `tool` event carries it and the call is refused as unchecked
//!   (tool-exchange.cjs CHECK_FAULT).

mod context;
mod stream;
pub mod text;

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use js_json::JValue;

use crate::effort::Rejected;
use crate::gate::{Call, RecentWrites, Tools};
use crate::host::Host;
use crate::js;
use crate::prepared::Prepared;
use crate::round::{Exchange, Round, Slot};
use crate::sse::Utf8Stream;
use crate::title::{self, AskFacts, AttemptCounter};
use context::Fit;
use stream::StreamEnd;
pub use text::*;

/// State the executor keeps per process, as Node keeps it per module (see rust-chat-native.cjs).
#[derive(Debug)]
pub struct Process {
    pub rejected: Mutex<Rejected>,
    pub recent_writes: RecentWrites,
    loads: Mutex<HashMap<String, Vec<f64>>>,
    /// The chats preparing context now (chat-context.cjs `busy`).
    preparing: Mutex<HashSet<String>>,
    /// Title requests per chat (#1785).
    titles: Mutex<AttemptCounter>,
}

impl Default for Process {
    fn default() -> Self {
        Process {
            rejected: Mutex::default(),
            recent_writes: RecentWrites::default(),
            loads: Mutex::default(),
            preparing: Mutex::default(),
            titles: Mutex::new(AttemptCounter::new(title::MAX_ATTEMPTS, title::MAX_CHATS)),
        }
    }
}

impl Process {
    /// chat-context.cjs `expectedLoadSeconds`: the median of the last five loads, at least 1 s.
    fn expected_load_seconds(&self, model: &str) -> Option<f64> {
        let loads = self.loads.lock().ok()?;
        let mut list = loads.get(model)?.clone();
        if list.is_empty() {
            return None;
        }
        list.sort_by(f64::total_cmp);
        let mid = list.get(list.len() / 2).copied()?;
        Some((mid / 1000.0).round().max(1.0))
    }

    fn record_load(&self, model: &str, ms: f64) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        if let Ok(mut loads) = self.loads.lock() {
            let list = loads.entry(model.to_string()).or_default();
            list.push(ms);
            let excess = list.len().saturating_sub(5);
            list.drain(..excess);
        }
    }
}

/// The helpers of chat.cjs's rounds that live in their own modules.
pub struct Helpers<'a> {
    /// search-query.cjs `cleanSearchArgs`.
    pub clean_search_args: &'a (dyn Fn(&str, &str) -> String + Sync),
    /// announced-action.cjs `announcesToolAction` and `ANNOUNCED_NOTE`.
    pub announces_tool_action: &'a (dyn Fn(&str) -> bool + Sync),
    pub announced_note: &'a str,
    /// tool-result-reduce.cjs `reduceToolResult(result, { maxChars }).text`.
    pub reduce_tool_result: &'a (dyn Fn(&str, usize) -> String + Sync),
    /// final-answer.cjs `sourceLinks`, `fallbackText` and `FINAL_ANSWER_NOTE`.
    pub source_links: &'a (dyn Fn(&str) -> Vec<String> + Sync),
    pub fallback_text: &'a (dyn Fn(&[String]) -> String + Sync),
    pub final_answer_note: &'a str,
    /// The tool result cap for the model's copy (index.cjs TOOL_RESULT_CAP).
    pub tool_result_cap: usize,
}

/// How the turn ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Ended {
    /// `done` was sent (and the title, when one came).
    Done,
    /// The stream ended on an `error` with nothing after it.
    Error,
    /// The client went away; nothing more is written.
    Left,
}

/// One native turn while it runs.
pub(crate) struct Turn<'a, H: Host> {
    host: &'a H,
    process: &'a Process,
    helpers: &'a Helpers<'a>,
    p: &'a Prepared,
    lease: Vec<(String, String)>,
    exchange: Exchange,
    decoder: Utf8Stream,
    /// chat.cjs `exchangeKey`, drawn before the lease as there.
    exchange_key: String,
}

/// What index.cjs writes when a turn's handler throws after the stream began (a state file that cannot
/// be saved mid-turn): the reply ends here, with no `done`.
pub const FAILED_TEXT: &str = "The request could not be completed. Please retry.";

/// Runs the rest of a prepared turn.
pub async fn run<H: Host>(
    host: &H,
    process: &Process,
    helpers: &Helpers<'_>,
    p: &Prepared,
) -> Ended {
    let mut t = Turn {
        host,
        process,
        helpers,
        p,
        lease: Vec::new(),
        exchange: Exchange::new(p.model.clone(), p.started_at, true),
        decoder: Utf8Stream::new(),
        exchange_key: host.uuid(),
    };
    let ended = match t.prepare().await {
        Ok(fit) => t.rounds(fit).await,
        Err(ended) => ended,
    };
    host.release_lease();
    ended
}

/// `Math.max(...keys) + 1` of a round's calls.
fn next_offset(calls: &[(f64, Slot)]) -> f64 {
    calls
        .iter()
        .map(|(k, _)| *k)
        .fold(f64::NEG_INFINITY, |a, b| {
            if a.is_nan() || b.is_nan() {
                f64::NAN
            } else {
                a.max(b)
            }
        })
        + 1.0
}

/// The rounds' own state, across rounds.
struct Rounds {
    messages: Vec<JValue>,
    /// `roundMessages !== prepared.messages`: the round-0 `context` event is sent when they differ.
    changed: bool,
    tools_off: bool,
    final_pass: bool,
    announced_retried: bool,
    paused: bool,
    applied: Vec<(String, Option<String>)>,
    gathered: Vec<String>,
    continuation: Option<(f64, usize)>,
    /// chat.cjs `roundHasContent`, `roundContent`, `roundReasoning`: set by the last round that read a stream.
    last: Round,
}

impl<H: Host> Turn<'_, H> {
    fn context_event(meter: JValue) -> JValue {
        let mut context = vec![("type".to_string(), js::s("context"))];
        if let JValue::Obj(items) = meter {
            context.extend(items);
        }
        JValue::Obj(context)
    }

    /// The round's budget, its continuation compaction, and the meter saved (and sent).
    /// `Err(true)`: the turn ends at once (no close); `Err(false)`: the round loop ends (the close follows).
    async fn budget(&self, s: &mut Rounds, fit: &Fit, round: i32) -> Result<(), bool> {
        let host = self.host;
        let p = self.p;
        let tools = JValue::Arr(p.tools.clone());
        let mut budget = context_fit::measure(&s.messages, &tools, fit.limit);
        let mut compacted = false;
        if over(&budget) {
            let summarize = |summary: String, older: Vec<JValue>, max_tokens: u64| {
                self.summarize(summary, older, max_tokens)
            };
            let fitted = context_fit::compact_continuation(
                &s.messages,
                &tools,
                fit.limit,
                summarize,
                &mut |text: &str, id: &str| {
                    host.emit(&status(text, id, None));
                },
            )
            .await;
            let c = match fitted {
                Ok(c) => c,
                Err(refused) => {
                    host.emit(&event("error", vec![("text", js::s(refused.0))]));
                    return Err(false);
                }
            };
            s.messages = c.messages;
            s.changed = true;
            budget = c.meter;
            compacted = c.compacted;
            if c.truncated > 0 {
                let what = if c.truncated == 1 {
                    "A tool result was".to_string()
                } else {
                    format!("{} tool results were", c.truncated)
                };
                host.emit(&event("warning", vec![("text", js::s(format!("{what} shortened to fit this model's context, so the answer may rest on part of the output.")))]));
            }
            if compacted {
                // Node: the meter (measured inside), then the compaction's time.
                let mut m = meter_json(&budget, &p.model, &fit.limit_source, host.now_ms());
                s.continuation = Some((host.now_ms(), c.covered));
                js::set(&mut m, "historyCount", js::count(fit.history_count));
                js::set(
                    &mut m,
                    "compactedAt",
                    s.continuation.map_or(JValue::Null, |(t, _)| js::n(t)),
                );
                js::set(&mut m, "covered", js::count(c.covered));
                host.emit(&Self::context_event(m));
            }
        }
        let id = context::context_id(p.context_id.as_deref(), None);
        let mut snapshot = host.read_state(&id);
        if !snapshot.is_object() {
            snapshot = JValue::Obj(Vec::new());
        }
        let mut meter = meter_json(&budget, &p.model, &fit.limit_source, host.now_ms());
        js::set(&mut meter, "historyCount", js::count(fit.history_count));
        let compacted_at = match s.continuation {
            Some((t, _)) => js::n(t),
            None => fit.compacted_at.map_or(JValue::Null, js::n),
        };
        js::set(&mut meter, "compactedAt", compacted_at);
        js::set(
            &mut meter,
            "covered",
            js::count(s.continuation.map_or(fit.covered, |(_, c)| c)),
        );
        js::set(&mut snapshot, "meter", meter.clone());
        if host.save_state(&id, &snapshot).is_err() {
            host.emit(&event("error", vec![("text", js::s(FAILED_TEXT))]));
            return Err(true);
        }
        if !compacted && (round > 0 || s.changed) {
            host.emit(&Self::context_event(meter));
        }
        Ok(())
    }

    /// The round's request body (chat.cjs 2025).
    fn body(&self, s: &Rounds, fit: &Fit) -> JValue {
        let p = self.p;
        let mut body = js::obj([
            ("model", js::s(p.model.clone())),
            ("max_tokens", num(fit.max_tokens)),
            ("messages", JValue::Arr(s.messages.clone())),
            ("stream", JValue::Bool(true)),
            (
                "stream_options",
                js::obj([("include_usage", JValue::Bool(true))]),
            ),
        ]);
        if let JValue::Obj(params) = &p.sampling {
            for (k, v) in params {
                js::set(&mut body, k, v.clone());
            }
        }
        if p.thinking_off {
            js::set(
                &mut body,
                "chat_template_kwargs",
                js::obj([("enable_thinking", JValue::Bool(false))]),
            );
        }
        if !s.final_pass && self.offers_tools(s) {
            js::set(&mut body, "tools", JValue::Arr(p.tools.clone()));
        }
        body
    }

    /// chat.cjs `requestTools().tools`.
    fn offers_tools(&self, s: &Rounds) -> bool {
        !s.tools_off && !self.p.tools.is_empty()
    }

    async fn rounds(&mut self, fit: Fit) -> Ended {
        let host = self.host;
        let p = self.p;
        let cap = (!p.thinking_off)
            .then(|| context_fit::thinking::thinking_cap(&p.effort, fit.max_tokens));
        let mut s = Rounds {
            messages: fit.messages.clone(),
            changed: false,
            tools_off: false,
            final_pass: false,
            announced_retried: false,
            paused: false,
            applied: Vec::new(),
            gathered: Vec::new(),
            continuation: None,
            last: Round::new(host.now_ms()),
        };
        if p.template_gate && !p.tools.is_empty() && !host.template_allows_tools(&p.model).await {
            s.tools_off = true;
            host.emit(&event("warning", vec![("text", js::s(TOOLS_OFF_NOTICE))]));
            s.messages = with_system_note(&s.messages, TOOLS_OFF_MODEL_NOTE);
            s.changed = true;
        }
        if !s.tools_off && !p.route_note.is_empty() {
            s.messages = with_system_note(&s.messages, &p.route_note);
            s.changed = true;
        }
        let names = |list: &[String]| list.iter().cloned().collect::<HashSet<String>>();
        let mut gate = Tools::new(
            p.tools
                .iter()
                .filter_map(|t| t.get("function").get("name").as_str().map(str::to_string))
                .collect(),
            names(&p.writes),
            names(&p.public),
            names(&p.automatic_only_writes),
            p.routes.clone(),
            (
                p.chat_id.clone(),
                p.chat_key.clone(),
                p.grant_scope.clone(),
                p.grant_holder.clone(),
                names(&p.recent_writes),
            ),
            p.project_id.clone(),
            self.exchange_key.clone(),
            p.decline_ends_reply,
            &self.process.recent_writes,
            self.helpers.reduce_tool_result,
            self.helpers.source_links,
            self.helpers.tool_result_cap,
        );
        let mut round: i32 = 0;
        loop {
            if round >= if s.final_pass { 4 } else { 3 } || host.left() {
                break;
            }
            match self.budget(&mut s, &fit, round).await {
                Ok(()) => {}
                Err(true) => return Ended::Error,
                Err(false) => break,
            }
            let mut body = self.body(&s, &fit);
            // chat.cjs takes the round's start before its request (the prefill sample's clock).
            let started = host.now_ms();
            let offered = !s.final_pass && self.offers_tools(&s);
            let unreachable = |e: &str| {
                event(
                    "error",
                    vec![(
                        "text",
                        js::s(format!("{} unreachable: {e}", p.provider_label)),
                    )],
                )
            };
            let mut answer = match self.request(&body, &p.effort, cap, true, None).await {
                Ok(a) => a,
                Err(e) => {
                    if !host.left() {
                        host.emit(&unreachable(&e));
                    }
                    break;
                }
            };
            // #1002: the engine refused the tools for the model's template: once more without them.
            if offered
                && p.template_gate
                && answer.status >= 400
                && answer
                    .detail
                    .as_deref()
                    .is_some_and(|d| tools_refused(answer.status, d))
            {
                s.tools_off = true;
                host.emit(&event("warning", vec![("text", js::s(TOOLS_RETRY_NOTICE))]));
                s.messages = with_system_note(
                    &without_system_note(&s.messages, &p.route_note),
                    TOOLS_OFF_MODEL_NOTE,
                );
                s.changed = true;
                body = self.body(&s, &fit);
                answer = match self.request(&body, &p.effort, cap, true, None).await {
                    Ok(a) => a,
                    Err(e) => {
                        if !host.left() {
                            host.emit(&unreachable(&e));
                        }
                        break;
                    }
                };
                // #1015: remembered only when dropping the tools is what fixed it.
                if (200..300).contains(&answer.status) {
                    host.mark_tools_unsupported(&p.model);
                }
            }
            let Some(stream_body) = answer.body.take() else {
                let detail = answer.detail.clone().unwrap_or_default();
                host.emit(&event(
                    "error",
                    vec![(
                        "text",
                        js::s(provider_failure_text(
                            answer.status,
                            &answer.headers,
                            &detail,
                            false,
                        )),
                    )],
                ));
                break;
            };
            let mut current = Round::new(started);
            match self
                .read_stream(stream_body, &answer.built, &mut current)
                .await
            {
                StreamEnd::Done => {}
                StreamEnd::EndTurn => return Ended::Error,
                StreamEnd::Left => return Ended::Left,
                StreamEnd::Failed(text) => {
                    host.emit(&event("error", vec![("text", js::s(text))]));
                    s.last = current;
                    break;
                }
            }
            if !current.saw_anything {
                self.fallback(&body, cap, self.offers_tools(&s), &mut current)
                    .await;
            }
            // #1552: tools on offer, no call, and the reply ends on announcing one: one more try.
            if current.tool_calls.is_empty()
                && current.saw_anything
                && !s.announced_retried
                && round < 2
                && self.offers_tools(&s)
                && !host.left()
                && (self.helpers.announces_tool_action)(&current.content)
            {
                s.announced_retried = true;
                host.emit(&event(
                    "preamble",
                    vec![("text", js::s(current.content.clone()))],
                ));
                s.messages.push(js::obj([
                    ("role", js::s("assistant")),
                    ("content", js::s(current.content.clone())),
                ]));
                s.messages.push(js::obj([
                    ("role", js::s("user")),
                    ("content", js::s(self.helpers.announced_note)),
                ]));
                s.changed = true;
                s.last = Round::new(host.now_ms());
                continue;
            }
            // #1693: the final-answer round offers no tools; a call it writes anyway is not run.
            if s.final_pass && !current.tool_calls.is_empty() {
                for (i, slot) in &current.tool_calls {
                    host.emit(&event(
                        "tool_result",
                        vec![
                            ("index", js::n(self.exchange.tool_offset + i)),
                            ("name", js::s(slot.name.clone())),
                            (
                                "text",
                                js::s("Not run: this step only writes the final answer."),
                            ),
                            ("notRun", JValue::Bool(true)),
                        ],
                    ));
                }
                self.exchange.tool_offset += next_offset(&current.tool_calls);
                current.tool_calls.clear();
            }
            let calls = current.tool_calls.len();
            if calls > 0 {
                self.run_calls(&mut gate, &mut current, &mut s).await;
                self.exchange.tool_offset += next_offset(&current.tool_calls);
            }
            if p.decline_ends_reply && !gate.declined.is_empty() {
                s.paused = true;
                self.declined(&gate.declined, s.applied.len());
                s.last = current;
                break;
            }
            let needs_final = if calls > 0 {
                round >= 2
            } else {
                round > 0 && !current.has_content
            };
            if !s.final_pass && !host.left() && needs_final {
                s.final_pass = true;
                s.messages.push(js::obj([
                    ("role", js::s("user")),
                    ("content", js::s(self.helpers.final_answer_note)),
                ]));
                current.has_content = false;
                current.content.clear();
                s.last = current;
                round += 1;
                continue;
            }
            s.last = current;
            if s.final_pass || round == 2 || calls == 0 {
                break;
            }
            round += 1;
        }
        if host.left() {
            return Ended::Left;
        }
        self.close(&s).await
    }

    /// #666: the person declined a write; the reply ends with a fixed note (the client words it).
    fn declined(&self, declined: &[String], applied: usize) {
        let mut unique: Vec<String> = Vec::new();
        for n in declined {
            if !unique.contains(n) {
                unique.push(n.clone());
            }
        }
        let names = unique.join(", ");
        let text = if applied > 0 {
            let saved = if applied == 1 {
                "1 change was".to_string()
            } else {
                format!("{applied} changes were")
            };
            format!("{saved} saved. You declined {names}, so nothing else was changed.")
        } else {
            format!("No change was made: you declined {names}.")
        };
        self.host.emit(&event(
            "paused",
            vec![
                ("reason", js::s("declined")),
                ("applied", js::count(applied)),
                (
                    "declined",
                    JValue::Arr(unique.into_iter().map(js::s).collect()),
                ),
                ("text", js::s(text)),
            ],
        ));
    }

    /// A round's calls: their arguments and ids settled, the narration kept, then each through the gate.
    async fn run_calls(&mut self, gate: &mut Tools<'_>, round: &mut Round, s: &mut Rounds) {
        let host = self.host;
        for (_, slot) in round.tool_calls.iter_mut() {
            slot.args = if slot.too_large {
                "{}".to_string()
            } else {
                (self.helpers.clean_search_args)(&slot.name, &slot.args)
            };
        }
        // Every call needs a unique id: the first real one stays, missing, oversized or repeated ones are made.
        let mut used: HashSet<String> = round
            .tool_calls
            .iter()
            .filter(|(_, c)| !c.id_made)
            .map(|(_, c)| js::stringify(&c.id))
            .collect();
        let mut seen = HashSet::new();
        for (i, c) in round.tool_calls.iter_mut() {
            let key = js::stringify(&c.id);
            if !c.id_made && seen.insert(key) {
                continue;
            }
            let base = format!("call-{}", crate::round::number_text(*i));
            let mut id = base.clone();
            let mut k = 2;
            while used.contains(&js::stringify(&js::s(id.clone()))) {
                id = format!("{base}-{k}");
                k += 1;
            }
            used.insert(js::stringify(&js::s(id.clone())));
            c.id = js::s(id);
        }
        if js::has_text(&round.content) {
            host.emit(&event(
                "preamble",
                vec![("text", js::s(round.content.clone()))],
            ));
            round.has_content = false;
        }
        let calls: Vec<JValue> = round
            .tool_calls
            .iter()
            .map(|(_, c)| {
                js::obj([
                    ("id", c.id.clone()),
                    ("type", js::s("function")),
                    (
                        "function",
                        js::obj([
                            ("name", js::s(c.name.clone())),
                            ("arguments", js::s(c.args.clone())),
                        ]),
                    ),
                ])
            })
            .collect();
        s.messages.push(js::obj([
            ("role", js::s("assistant")),
            ("content", JValue::Null),
            ("tool_calls", JValue::Arr(calls)),
        ]));
        s.changed = true;
        for (i, c) in &round.tool_calls {
            if host.left() {
                break;
            }
            let call = Call {
                index: *i,
                id: c.id.clone(),
                name: c.name.clone(),
                args: c.args.clone(),
                too_large: c.too_large,
            };
            let outcome = gate.call(host, &call, self.exchange.tool_offset).await;
            host.emit(&outcome.event);
            s.messages.push(outcome.message);
            if let Some(a) = outcome.applied {
                s.applied.push(a);
            }
            for link in outcome.links {
                if !s.gathered.contains(&link) {
                    s.gathered.push(link);
                }
            }
        }
    }

    /// The close: the notes for a reply with no answer, `telemetry` complete, `done`, then the title.
    async fn close(&mut self, s: &Rounds) -> Ended {
        let host = self.host;
        let p = self.p;
        if !s.paused && !s.last.has_content && s.final_pass {
            host.emit(&event(
                "delta",
                vec![("text", js::s((self.helpers.fallback_text)(&s.gathered)))],
            ));
        } else if !s.paused && !s.last.has_content && js::has_text(&s.last.reasoning) {
            host.emit(&event("delta", vec![("text", js::s(REASONING_ONLY_NOTE))]));
        }
        host.emit(&event(
            "telemetry",
            vec![
                ("phase", js::s("complete")),
                ("model", js::s(p.model.clone())),
                ("timeToFirstToken", self.exchange.time_to_first_token()),
            ],
        ));
        host.emit(&event("done", vec![("model", js::s(p.model.clone()))]));
        self.title(s).await;
        Ended::Done
    }

    /// #1785: after a chat's first completed reply, a short title from the model that just answered.
    async fn title(&self, s: &Rounds) {
        let host = self.host;
        let p = self.p;
        let t = &p.title;
        let user = host.user_id();
        let key = title::attempt_key(Some(&user), t.list_id.as_deref(), &t.chat_id);
        let facts = AskFacts {
            store_wired: t.enabled,
            paused: s.paused,
            round_has_content: s.last.has_content,
            reply: &s.last.content,
            chat_id: &t.chat_id,
            compact_only: false,
            diary: false,
            local: true,
            machine: false,
        };
        // chat.cjs's conditions in its order, the store and the engine asked only when the ones before hold.
        if !title::ask_for_title(&facts, || true, || true, || false) {
            return;
        }
        if !(t.first_reply
            || host
                .title_auto_named(t.list_id.as_deref(), &t.chat_id, &t.first_message)
                .await)
        {
            return;
        }
        if !self.process.titles.lock().is_ok_and(|a| a.allowed(&key)) {
            return;
        }
        if host.engine_busy().await {
            return;
        }
        if let Ok(mut a) = self.process.titles.lock() {
            a.record(&key);
        }
        // chat-title.cjs generateTitle: nothing to name without a message and a reply.
        if !js::has_text(&p.message) || !js::has_text(&s.last.content) {
            return;
        }
        let body = title::title_request_body(&p.model, &p.message, &s.last.content);
        let request = context::engine_request(
            &p.upstream_url,
            self.headers(),
            &body,
            false,
            Some(title::TIMEOUT_MS),
        );
        let Ok(answer) = host.request(&request).await else {
            return;
        };
        if host.left() || !(200..300).contains(&answer.status) {
            return;
        }
        let (text, capped) = host.text(answer.body, title::REPLY_BYTES_CAP).await;
        if host.left() || capped {
            return;
        }
        if let Some(name) = title::title_from_reply_text(&text) {
            host.emit(&event(
                "title",
                vec![
                    ("chatId", js::s(t.chat_id.clone())),
                    ("title", js::s(name.clone())),
                ],
            ));
            host.title_save(t.list_id.as_deref(), &t.chat_id, &name, &t.first_message);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn the_next_offset_follows_math_max() {
        let slot = |i: f64| {
            (
                i,
                Slot {
                    id: JValue::Null,
                    id_made: true,
                    name: String::new(),
                    args: String::new(),
                    too_large: false,
                    name_cut: false,
                },
            )
        };
        assert_eq!(next_offset(&[slot(0.0), slot(2.0)]), 3.0);
        assert!(next_offset(&[slot(f64::NAN), slot(1.0)]).is_nan());
    }
}
