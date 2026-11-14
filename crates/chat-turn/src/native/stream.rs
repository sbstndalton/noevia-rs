//! One model request of a native turn and the reading of its answer: reasoning-effort.cjs
//! `requestWithEffort` (the 4xx retry, the `meta` report), the stream as chat.cjs reads it (lines
//! 2066-2165) with thinking-budget.cjs's cap around it, and the empty stream's one non-streaming retry
//! (2174-2209).

use js_json::{wtf16, JValue};

use super::context::engine_request;
use super::text::*;
use super::Turn;
use crate::cap::{Cap, Cut, Step, CUT_MARK};
use crate::effort::{self, Built};
use crate::host::{Host, Read};
use crate::js;
use crate::round::{Effect, Flow, Round, Slot, MAX_TOOL_ARGS_UNITS};
use crate::sse::Lines;

/// What one request brought back.
pub(crate) struct Answer<B> {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Option<B>,
    /// The first 64 KiB of a failed answer, read once (Node reads clones of it).
    pub detail: Option<String>,
    pub built: Built,
}

/// How a round's stream ended.
pub(crate) enum StreamEnd {
    /// Read to its end.
    Done,
    /// An error frame: the reply ends at once.
    EndTurn,
    /// The client went away.
    Left,
    /// It broke; this error text is the round's last event.
    Failed(String),
    /// A loaded skill was revoked while it streamed (#272): the stream is cancelled.
    Revoked,
}

impl<H: Host> Turn<'_, H> {
    pub(crate) fn headers(&self) -> Vec<(String, String)> {
        let mut h = vec![("Content-Type".to_string(), "application/json".to_string())];
        h.extend(self.lease.iter().cloned());
        h
    }

    async fn send(
        &self,
        body: &JValue,
        stream: bool,
        timeout_ms: Option<u64>,
    ) -> Result<crate::host::Answer<H::Body>, String> {
        self.host
            .request(&engine_request(
                &self.p.upstream_url,
                self.headers(),
                body,
                stream,
                timeout_ms,
            ))
            .await
    }

    /// `requestWithEffort`: the request, one retry when a 4xx refuses the effort or the budget, and the
    /// `meta` it reports (when `report`), sent once the answer is in, failed or not.
    pub(crate) async fn request(
        &self,
        body: &JValue,
        effort: &str,
        cap: Option<u64>,
        report: bool,
        timeout_ms: Option<u64>,
    ) -> Result<Answer<H::Body>, String> {
        let model = self.p.model.as_str();
        let provider = &self.p.provider;
        let mut built = {
            let rejected = self
                .process
                .rejected
                .lock()
                .map(|r| r.clone())
                .unwrap_or_default();
            effort::build(body, provider, model, effort, cap, &rejected)
        };
        let stream = built.request.get("stream") == &JValue::Bool(true);
        let mut answer = self.send(&built.request, stream, timeout_ms).await?;
        if (400..500).contains(&answer.status) {
            let (text, _) = self.host.text(answer.body, PROVIDER_ERROR_BODY_CAP).await;
            let warning = match self.process.rejected.lock() {
                Ok(mut rejected) => effort::retry_after_refusal(
                    &mut built,
                    &text,
                    provider,
                    model,
                    effort,
                    &mut rejected,
                ),
                Err(_) => None,
            };
            let Some(w) = warning else {
                if report {
                    self.host.emit(&effort::meta_event(&built, effort));
                }
                return Ok(Answer {
                    status: answer.status,
                    headers: answer.headers,
                    body: None,
                    detail: Some(text),
                    built,
                });
            };
            if report {
                self.host.emit(&event("warning", vec![("text", js::s(w))]));
            }
            answer = self.send(&built.request, stream, timeout_ms).await?;
        }
        if report {
            self.host.emit(&effort::meta_event(&built, effort));
        }
        if !(200..300).contains(&answer.status) {
            let (text, _) = self.host.text(answer.body, PROVIDER_ERROR_BODY_CAP).await;
            return Ok(Answer {
                status: answer.status,
                headers: answer.headers,
                body: None,
                detail: Some(text),
                built,
            });
        }
        Ok(Answer {
            status: answer.status,
            headers: answer.headers,
            body: Some(answer.body),
            detail: None,
            built,
        })
    }

    fn effects(&self, out: Vec<Effect>) {
        for e in out {
            match e {
                Effect::Event(v) => self.host.emit(&v),
                Effect::Usage(reported) => self.host.record_usage(&self.p.model, &reported),
                Effect::PrefillSample {
                    prompt,
                    first_token_ms,
                } => self
                    .host
                    .prefill_sample(&self.p.model, prompt, first_token_ms),
                Effect::Mtp { drafted, accepted } => {
                    self.host.mtp_evidence(&self.p.model, drafted, accepted)
                }
            }
        }
    }

    /// Feeds bytes to the round's line reader; `false` when an error frame ended the turn.
    fn feed(&mut self, lines: &mut Lines, round: &mut Round, bytes: &[u8]) -> bool {
        for payload in lines.feed(&mut self.decoder, bytes) {
            let mut out = Vec::new();
            let flow = self.exchange.frame(
                round,
                &payload,
                self.host.now_ms(),
                self.helpers.clean_search_args,
                &|error, shown| provider_error(error, None, shown),
                &mut out,
            );
            self.effects(out);
            if flow == Flow::EndTurn {
                return false;
            }
        }
        true
    }

    /// chat.cjs's check before each chunk the round's stream yields: at most once a second, a loaded skill
    /// revoked meanwhile cuts the stream.
    async fn skill_cut(&self) -> bool {
        self.skills.due(self.host.now_ms()) && self.skills.revoked(self.host).await
    }

    /// Reads a round's stream, the thinking cap's cut and forced answer included.
    pub(crate) async fn read_stream(
        &mut self,
        body: H::Body,
        built: &Built,
        round: &mut Round,
    ) -> StreamEnd {
        let idle = (self.p.stream_idle_ms > 0.0).then_some(self.p.stream_idle_ms);
        let mut cap = built
            .thinking_cap
            .filter(|_| !built.answer_directly)
            .map(Cap::new);
        let mut lines = Lines::new();
        let mut started = false;
        let mut current = Some(body);
        loop {
            let Some(b) = current.as_mut() else {
                return StreamEnd::Done;
            };
            // usize from f64: a duration in milliseconds.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let read = self
                .host
                .read(
                    b,
                    if started {
                        idle.map(|i| i as u64)
                    } else {
                        None
                    },
                )
                .await;
            let (chunk, step) = match read {
                Read::Aborted => return StreamEnd::Left,
                Read::Idle => {
                    return StreamEnd::Failed(format!(
                        "The model sent no output for {} s after it started, so the reply was ended. No tool ran for this step. Try again, or choose another model.",
                        seconds_one_decimal(self.p.stream_idle_ms)
                    ))
                }
                Read::Failed(message) => {
                    if self.host.left() {
                        return StreamEnd::Left;
                    }
                    return StreamEnd::Failed(provider_error(&js::s(message), None, self.exchange.shown_text));
                }
                Read::End => match cap.as_mut().map(|c| c.step(None)) {
                    Some(Step::Cut { cut, before }) => (None, Some((cut, before))),
                    _ => return StreamEnd::Done,
                },
                Read::Chunk(c) => match cap.as_mut().map(|cp| cp.step(Some(&c))) {
                    Some(Step::Cut { cut, before }) => (None, Some((cut, before))),
                    _ => (Some(c), None),
                },
            };
            if let Some(c) = chunk {
                if self.skill_cut().await {
                    return StreamEnd::Revoked;
                }
                if !self.feed(&mut lines, round, &c) {
                    return StreamEnd::EndTurn;
                }
                started = true;
                continue;
            }
            let Some((cut, before)) = step else { continue };
            // thinking-budget.cjs: the bytes before a held-back [DONE] reach the reader first, the stream
            // is cancelled, the warning is sent, the reader gets the cut mark (closing an open line), and
            // only then is the answer asked for.
            if let Some(b) = before {
                if self.skill_cut().await {
                    return StreamEnd::Revoked;
                }
                if !self.feed(&mut lines, round, &b) {
                    return StreamEnd::EndTurn;
                }
            }
            // The old stream is cancelled before the forced answer is asked for (one engine slot).
            drop(current.take());
            self.host
                .emit(&event("warning", vec![("text", js::s(cut_warning(cut)))]));
            if self.skill_cut().await {
                return StreamEnd::Revoked;
            }
            if !self.feed(&mut lines, round, CUT_MARK) {
                return StreamEnd::EndTurn;
            }
            started = false;
            let forced = effort::forced_answer_body(&built.request, &self.p.provider);
            match self.send(&forced, true, None).await {
                Ok(a) if (200..300).contains(&a.status) => current = Some(a.body),
                Ok(a) => {
                    return StreamEnd::Failed(provider_error(
                        &js::s(format!(
                            "The forced answer request failed (HTTP {}). Try again.",
                            a.status
                        )),
                        None,
                        self.exchange.shown_text,
                    ))
                }
                Err(message) => {
                    if self.host.left() {
                        return StreamEnd::Left;
                    }
                    return StreamEnd::Failed(provider_error(
                        &js::s(message),
                        None,
                        self.exchange.shown_text,
                    ));
                }
            }
        }
    }

    /// The empty stream's one non-streaming retry (300 s): its events, and its calls into the round.
    pub(crate) async fn fallback(
        &mut self,
        body: &JValue,
        cap: Option<u64>,
        tools: Option<&[JValue]>,
        round: &mut Round,
    ) {
        let p = self.p;
        round.started_ms = self.host.now_ms();
        round.first_token_ms = None;
        let mut request = js::obj([
            ("model", body.get("model").clone()),
            ("max_tokens", body.get("max_tokens").clone()),
            ("messages", body.get("messages").clone()),
            ("stream", JValue::Bool(false)),
        ]);
        if let JValue::Obj(params) = &p.sampling {
            for (k, v) in params {
                js::set(&mut request, k, v.clone());
            }
        }
        if p.thinking_off {
            js::set(
                &mut request,
                "chat_template_kwargs",
                js::obj([("enable_thinking", JValue::Bool(false))]),
            );
        }
        // chat.cjs offers the tools here even on the final-answer round (requestTools() only).
        if let Some(tools) = tools {
            js::set(&mut request, "tools", JValue::Arr(tools.to_vec()));
        }
        let shown = self.exchange.shown_text;
        let error = |text: String| event("error", vec![("text", js::s(text))]);
        let answer = match self
            .request(&request, &p.effort, cap, true, Some(300_000))
            .await
        {
            Ok(a) => a,
            Err(e) => {
                self.host
                    .emit(&error(provider_error(&js::s(e), None, shown)));
                return;
            }
        };
        let Some(reply) = answer.body else {
            let detail = answer.detail.unwrap_or_default();
            self.host.emit(&error(provider_failure_text(
                answer.status,
                &answer.headers,
                &detail,
                shown,
            )));
            return;
        };
        let (text, capped) = self.host.text(reply, FALLBACK_REPLY_CAP).await;
        let full = if capped {
            Err("reply exceeded the 16384 KB limit".to_string())
        } else {
            // A reply that is not JSON: V8's SyntaxError message there, this one here (named difference).
            wtf16::parse(&text).map_err(|_| "The fallback reply is not JSON".to_string())
        };
        let full = match full {
            Ok(f) => f,
            Err(e) => {
                self.host
                    .emit(&error(provider_error(&js::s(e), None, shown)));
                return;
            }
        };
        let msg = match full.get("choices") {
            JValue::Arr(c) => c
                .first()
                .map(|c| c.get("message").clone())
                .unwrap_or(JValue::Undefined),
            JValue::Obj(_) => full.get("choices").get("0").get("message").clone(),
            _ => JValue::Undefined,
        };
        let calls = match msg.get("tool_calls") {
            JValue::Arr(t) => t.clone(),
            _ => Vec::new(),
        };
        let mut out = Vec::new();
        if msg.get("reasoning_content").truthy() || msg.get("content").truthy() || !calls.is_empty()
        {
            self.exchange
                .mark_first_output(round, self.host.now_ms(), &mut out);
        }
        if msg.get("reasoning_content").truthy() {
            js::concat(
                &mut round.reasoning,
                &js_json::to_js_string(msg.get("reasoning_content")).unwrap_or_default(),
            );
            out.push(Effect::Event(event(
                "reasoning",
                vec![("text", msg.get("reasoning_content").clone())],
            )));
        }
        if msg.get("content").truthy() {
            round.has_content = true;
            self.exchange.shown_text = true;
            js::concat(
                &mut round.content,
                &js_json::to_js_string(msg.get("content")).unwrap_or_default(),
            );
            out.push(Effect::Event(event(
                "delta",
                vec![("text", msg.get("content").clone())],
            )));
        }
        for tc in &calls {
            #[allow(clippy::cast_precision_loss)]
            let index = round.tool_calls.len() as f64;
            let field = |v: &JValue| {
                if v.truthy() {
                    js_json::to_js_string(v).unwrap_or_default()
                } else {
                    String::new()
                }
            };
            let id = tc.get("id");
            let (name, args) = (
                field(tc.get("function").get("name")),
                field(tc.get("function").get("arguments")),
            );
            let too_large = [field(id), name.clone(), args.clone()]
                .iter()
                .any(|v| js::len(v) > MAX_TOOL_ARGS_UNITS);
            let shown_name = if too_large {
                js::slice(&name, 64)
            } else {
                name.clone()
            };
            let made = too_large || !id.truthy();
            round.tool_calls.push((
                index,
                Slot {
                    id: if made {
                        js::s(format!("call-{}", crate::round::number_text(index)))
                    } else {
                        id.clone()
                    },
                    id_made: made,
                    name: shown_name.clone(),
                    args: if too_large {
                        String::new()
                    } else {
                        args.clone()
                    },
                    too_large,
                    name_cut: false,
                },
            ));
            out.push(Effect::Event(event(
                "tool",
                vec![
                    ("index", js::n(self.exchange.tool_offset + index)),
                    ("name", js::s(shown_name)),
                    (
                        "args",
                        js::s(if too_large {
                            String::new()
                        } else {
                            (self.helpers.clean_search_args)(&name, &args)
                        }),
                    ),
                ],
            )));
        }
        let usage = full.get("usage").clone();
        let timings = full.get("timings").clone();
        self.exchange
            .report_usage(round, &usage, &timings, &mut out);
        self.effects(out);
        round.saw_anything = true;
    }
}

fn cut_warning(cut: Cut) -> &'static str {
    cut.warning()
}
