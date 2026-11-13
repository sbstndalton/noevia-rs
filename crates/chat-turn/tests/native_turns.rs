//! The native continuation replayed against the recorded Node turns: each case's `prepared` frame
//! (recorded from core's hand-off) is run by `chat_turn::native::run` against a host that answers as
//! the recording's script did, and everything after the cut must be Node's, byte for byte: the
//! events, the engine requests (url, headers, body), the store calls and the workspace files.
//!
//! Every recorded prepared turn runs; a case not covered yet would be named in `NOT_YET`, with why.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod common;

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use chat_turn::after::{Keep, Saving};
use chat_turn::host::{Answer, EngineRequest, GateRefusal, Host, Read, Target, ToolRun};
use chat_turn::js;
use chat_turn::native::{self, Ended, Helpers, Process};
use chat_turn::prepared::Prepared;
use chat_turn::record::TurnRecord;
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};

const FIXED_NOW: f64 = 1_794_571_200_000.0;

/// The cases this step does not run yet, and why.
const NOT_YET: [(&str, &str); 0] = [];

fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// An engine answer from the script: chunks to read, or the body text.
#[derive(Debug, Default)]
struct Body {
    chunks: Vec<Chunk>,
    at: usize,
    text: String,
}

#[derive(Debug, Clone)]
enum Chunk {
    Bytes(Vec<u8>),
    Leave,
    Stall,
}

struct Script {
    case: JValue,
    rounds: Vec<JValue>,
    next: Mutex<usize>,
    left: Mutex<bool>,
    events: Mutex<Vec<String>>,
    requests: Mutex<Vec<EngineRequest>>,
    stores: Mutex<Vec<JValue>>,
    files: Mutex<BTreeMap<String, String>>,
    bridge: Mutex<Vec<JValue>>,
    approvals: Mutex<Vec<String>>,
    executions: Mutex<Vec<String>>,
    uuids: Mutex<u64>,
    leased: Mutex<bool>,
    loaded: Mutex<bool>,
    /// Every event the turn sent, the client's leaving aside, with whether it had left.
    observed: Mutex<Vec<(JValue, bool)>>,
}

/// The per-start id the recording's grant contexts carry (record_turns.cjs NOEVIA_START_ID).
const START_ID: &str = "0123456789abcdef0123456789abcdef";

/// The bytes the recorder's engine sent for one streamed round, exactly (`input.chunks`).
fn chunks_of(recorded: &JValue) -> Vec<Chunk> {
    let JValue::Arr(items) = recorded else {
        return Vec::new();
    };
    items
        .iter()
        .map(|c| match c {
            JValue::Str(m) if m == "leave" => Chunk::Leave,
            JValue::Str(m) if m == "stall" => Chunk::Stall,
            JValue::Str(m) if m == "wait" => Chunk::Bytes(Vec::new()),
            _ if c.get("text").as_str().is_some() => {
                Chunk::Bytes(c.get("text").as_str().unwrap().as_bytes().to_vec())
            }
            _ => Chunk::Bytes(base64(c.get("b64").as_str().unwrap())),
        })
        .collect()
}

fn base64(text: &str) -> Vec<u8> {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for b in text.bytes().filter(|b| *b != b'=') {
        acc = (acc << 6) | ABC.iter().position(|x| *x == b).unwrap() as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

impl Script {
    fn new(case: JValue) -> Script {
        let JValue::Arr(rounds) = case.get("input").get("rounds").clone() else {
            panic!()
        };
        let case_loaded = case.get("input").get("loaded") != &JValue::Bool(false);
        let recorded_bridge = match case.get("bridge") {
            JValue::Arr(b) => b.clone(),
            _ => Vec::new(),
        };
        Script {
            case,
            rounds,
            next: Mutex::new(0),
            left: Mutex::new(false),
            events: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            stores: Mutex::new(Vec::new()),
            files: Mutex::new(BTreeMap::new()),
            bridge: Mutex::new(recorded_bridge),
            approvals: Mutex::new(Vec::new()),
            executions: Mutex::new(Vec::new()),
            uuids: Mutex::new(0),
            leased: Mutex::new(false),
            loaded: Mutex::new(case_loaded),
            observed: Mutex::new(Vec::new()),
        }
    }

    fn store(&self, op: &str, mut pairs: Vec<(&str, JValue)>) {
        pairs.insert(0, ("op", js::s(op)));
        self.stores.lock().unwrap().push(js::obj(pairs));
    }

    fn file_name(id: &str) -> String {
        format!("context-{}.json", context_fit::fingerprint(&js::s(id)))
    }
}

impl Host for Script {
    type Body = Body;

    fn emit(&self, event: &JValue) {
        let left = *self.left.lock().unwrap();
        self.observed.lock().unwrap().push((event.clone(), left));
        if !left {
            self.events.lock().unwrap().push(js::stringify(event));
        }
    }

    fn left(&self) -> bool {
        *self.left.lock().unwrap()
    }

    fn now_ms(&self) -> f64 {
        FIXED_NOW
    }

    async fn lease(&self, model: &str) -> Result<Option<Vec<(String, String)>>, GateRefusal> {
        let refusal = self.case.get("input").get("leaseRefusal");
        if refusal.is_object() {
            return Err(GateRefusal {
                text: refusal.get("publicMessage").as_str().unwrap().to_string(),
                code: refusal.get("code").as_str().map(str::to_string),
                retryable: refusal.get("retryable").as_bool(),
            });
        }
        *self.leased.lock().unwrap() = true;
        self.store(
            "modelManager.acquireLease",
            vec![("model", js::s(model)), ("lease", js::s("qa-lease-1"))],
        );
        Ok(Some(vec![("X-Noevia-Lease".into(), "qa-lease-1".into())]))
    }

    async fn health(&self) -> Option<JValue> {
        let ctx = self.case.get("input").get("ctxSize").clone();
        let model = self
            .case
            .get("input")
            .get("project")
            .get("model")
            .as_str()
            .unwrap_or("qa-model")
            .to_string();
        let rows = if *self.loaded.lock().unwrap() {
            vec![js::obj([
                ("model_name", js::s(model)),
                ("loaded", JValue::Bool(true)),
                ("recipe_options", js::obj([("ctx_size", ctx)])),
            ])]
        } else {
            Vec::new()
        };
        Some(js::obj([("all_models_loaded", JValue::Arr(rows))]))
    }

    async fn load(&self, model: &str) -> bool {
        self.store("modelManager.load", vec![("model", js::s(model))]);
        *self.loaded.lock().unwrap() = true;
        true
    }

    async fn template_allows_tools(&self, _model: &str) -> bool {
        true
    }

    async fn request(&self, request: &EngineRequest) -> Result<Answer<Body>, String> {
        self.requests.lock().unwrap().push(request.clone());
        let mut next = self.next.lock().unwrap();
        let round = self
            .rounds
            .get(*next)
            .cloned()
            .unwrap_or_else(|| panic!("no engine round {}", *next + 1));
        *next += 1;
        if let Some(t) = round.get("throws").as_str() {
            return Err(t.to_string());
        }
        if let Some(status) = round.get("status").as_num().filter(|s| *s != 200.0) {
            let text = round.get("body").as_str().unwrap_or("").to_string();
            return Ok(Answer {
                status: status as u16,
                headers: Vec::new(),
                body: Body {
                    text,
                    ..Body::default()
                },
            });
        }
        if round.get("json").is_object() {
            return Ok(Answer {
                status: 200,
                headers: Vec::new(),
                body: Body {
                    text: js::stringify(round.get("json")),
                    ..Body::default()
                },
            });
        }
        let recorded = self.case.get("input").get("chunks").clone();
        let JValue::Arr(per_round) = recorded else {
            panic!()
        };
        Ok(Answer {
            status: 200,
            headers: Vec::new(),
            body: Body {
                chunks: chunks_of(&per_round[*next - 1]),
                ..Body::default()
            },
        })
    }

    async fn read(&self, body: &mut Body, idle_ms: Option<u64>) -> Read {
        if *self.left.lock().unwrap() {
            return Read::Aborted;
        }
        let Some(chunk) = body.chunks.get(body.at).cloned() else {
            return Read::End;
        };
        body.at += 1;
        match chunk {
            Chunk::Bytes(b) => Read::Chunk(b),
            Chunk::Leave => {
                *self.left.lock().unwrap() = true;
                Read::Aborted
            }
            Chunk::Stall => match idle_ms {
                Some(_) => Read::Idle,
                None => panic!("a stall before the first output would wait forever"),
            },
        }
    }

    async fn text(&self, body: Body, cap: usize) -> (String, bool) {
        let capped = body.text.len() > cap;
        (body.text, capped)
    }

    fn read_state(&self, id: &str) -> JValue {
        match self.files.lock().unwrap().get(&Script::file_name(id)) {
            Some(text) => wtf16::parse(text).unwrap(),
            None => js::obj(Vec::<(String, JValue)>::new()),
        }
    }

    fn save_state(&self, id: &str, state: &JValue) -> Result<(), String> {
        self.files
            .lock()
            .unwrap()
            .insert(Script::file_name(id), js::stringify(state));
        Ok(())
    }

    fn release_lease(&self) {
        if std::mem::take(&mut *self.leased.lock().unwrap()) {
            self.store("lease.release", vec![("lease", js::s("qa-lease-1"))]);
        }
    }

    fn mark_tools_unsupported(&self, model: &str) {
        self.store(
            "templateToolsGate.markUnsupported",
            vec![("model", js::s(model))],
        );
    }

    fn engine_busy(&self) -> bool {
        false
    }

    fn title_auto_named(&self, list: Option<&str>, chat: &str, first: &str) -> bool {
        self.store(
            "chatTitles.autoNamed",
            vec![
                ("listId", list.map_or(JValue::Null, js::s)),
                ("chatId", js::s(chat)),
                ("first", js::s(first)),
            ],
        );
        self.case.get("input").get("titles").get("autoNamed") != &JValue::Bool(false)
    }

    fn title_save(&self, list: Option<&str>, chat: &str, title: &str, first: &str) {
        self.store(
            "chatTitles.save",
            vec![
                ("listId", list.map_or(JValue::Null, js::s)),
                ("chatId", js::s(chat)),
                ("title", js::s(title)),
                ("first", js::s(first)),
            ],
        );
    }

    fn record_usage(&self, model: &str, reported: &JValue) {
        self.store(
            "recordUsage",
            vec![("model", js::s(model)), ("reported", reported.clone())],
        );
    }

    fn user_id(&self) -> String {
        "qa-user".into()
    }

    fn uuid(&self) -> String {
        let mut n = self.uuids.lock().unwrap();
        *n += 1;
        format!("00000000-0000-4000-8000-{:012}", *n)
    }

    fn tool_policy(&self, _name: &str, write: bool) -> String {
        // record_turns.cjs toolPolicy: writes ask, reads run.
        if write { "ask" } else { "allow" }.into()
    }

    async fn write_target(&self, name: &str, args: &str, _chat_key: Option<&str>) -> Target {
        let next = self.bridge.lock().unwrap().remove(0);
        assert_eq!(
            (
                next.get("op").as_str(),
                next.get("name").as_str(),
                next.get("args").as_str()
            ),
            (Some("target"), Some(name), Some(args))
        );
        let found = next.get("found");
        if let Some(e) = found.get("error").as_str() {
            Target::Refused(e.to_string())
        } else if let Some(t) = found.get("target").as_str() {
            Target::Found {
                target: t.to_string(),
                kind: found.get("kind").as_str().map(str::to_string),
            }
        } else {
            Target::None
        }
    }

    async fn permission_binding(
        &self,
        chat_id: Option<&str>,
        scope: &str,
        _holder: &JValue,
    ) -> Result<JValue, ()> {
        let Some(chat) = chat_id else {
            return Ok(JValue::Null);
        };
        let context = js::obj([
            ("canonicalChatId", js::s(chat)),
            ("holder", js::obj([("kind", js::s("absent"))])),
            ("generation", js::s("0")),
            ("start", js::s(START_ID)),
            ("tombstoned", JValue::Bool(false)),
        ]);
        Ok(js::obj([
            ("user", js::s("qa-user")),
            ("canonicalChatId", js::s(chat)),
            ("scope", js::s(scope)),
            ("context", context),
        ]))
    }

    fn chat_wide_approved(&self, _chat_id: Option<&str>, _scope: &str, _binding: &JValue) -> bool {
        false
    }

    async fn await_approval(&self, request: &JValue) -> String {
        self.approvals.lock().unwrap().push(js::stringify(request));
        self.case
            .get("input")
            .get("decision")
            .as_str()
            .unwrap_or("approve")
            .to_string()
    }

    fn revoke_grant(&self, _chat_id: Option<&str>) {}

    fn audit(&self, event: &str, details: &JValue) {
        self.store(
            "audit",
            vec![
                ("event", js::s(event)),
                ("actor", js::s("qa-user")),
                ("subject", js::s("qa-user")),
                ("details", details.clone()),
            ],
        );
    }

    async fn execute_tool(&self, run: &ToolRun) -> Result<(String, bool), String> {
        let next = self.bridge.lock().unwrap().remove(0);
        assert_eq!(
            (
                next.get("op").as_str(),
                next.get("name").as_str(),
                next.get("args").as_str()
            ),
            (
                Some("execute"),
                Some(run.name.as_str()),
                Some(run.args.as_str())
            )
        );
        self.executions.lock().unwrap().push(run.name.clone());
        // The bridge counts the call as chat.cjs does.
        self.store("recordToolUse", vec![("name", js::s(run.name.clone()))]);
        Ok((
            next.get("text").as_str().unwrap().to_string(),
            next.get("failed") == &JValue::Bool(true),
        ))
    }

    fn prefill_sample(&self, _model: &str, _prompt: f64, _ms: f64) {}

    fn mtp_evidence(&self, _model: &str, _drafted: f64, _accepted: f64) {}
}

impl Keep for Script {
    fn append_if_last(&self, chat: &str, message: &str, entry: &JValue) -> Result<String, String> {
        self.store(
            "transcripts.appendIfLast",
            vec![
                ("chatId", js::s(chat)),
                ("message", js::s(message)),
                ("entry", entry.clone()),
                ("options", js::obj([("origin", js::s("turn"))])),
            ],
        );
        Ok(self
            .case
            .get("input")
            .get("appendOutcome")
            .as_str()
            .unwrap_or("saved")
            .to_string())
    }

    fn keep_partial(&self, chat: &str, file: &JValue) {
        self.files
            .lock()
            .unwrap()
            .insert(partial_name(chat), js::stringify(file));
    }

    fn end_partial(&self, chat: &str) {
        self.files.lock().unwrap().remove(&partial_name(chat));
    }

    fn tombstoned(&self, _chat: &str) -> bool {
        false
    }

    fn delete_unlisted(&self, chat: &str) {
        self.store("transcripts.deleteChat", vec![("chatId", js::s(chat))]);
    }
}

/// chat-partial.cjs fileFor: the sha256 of the chat id itself.
fn partial_name(chat: &str) -> String {
    let digest = Sha256::digest(chat.as_bytes());
    format!(
        "chat-partials/{}.json",
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

fn fixture() -> JValue {
    wtf16::parse(common::FIXTURE).unwrap()
}

fn headers_json(h: &[(String, String)]) -> String {
    js::stringify(&JValue::Obj(
        h.iter()
            .map(|(k, v)| (k.clone(), js::s(v.clone())))
            .collect(),
    ))
}

fn replay(f: &JValue, case: &JValue) -> (Script, Ended) {
    let prepared =
        Prepared::read(case.get("handoff").get("prepared")).unwrap_or_else(|e| panic!("{e:?}"));
    let script = Script::new(case.clone());
    let process = Process::default();
    let helpers = Helpers {
        clean_search_args: &|n, a| chat_turn::helpers::clean_search_args(n, a).into_owned(),
        announces_tool_action: &chat_turn::helpers::announces_tool_action,
        announced_note: chat_turn::helpers::ANNOUNCED_NOTE,
        reduce_tool_result: &|text, cap| chat_turn::reduce::reduce_tool_result(text, cap).text,
        source_links: &chat_turn::helpers::source_links,
        fallback_text: &|s| chat_turn::helpers::fallback_text(s),
        final_answer_note: chat_turn::helpers::FINAL_ANSWER_NOTE,
        tool_result_cap: 8000,
    };
    let ended = block_on(native::run(&script, &process, &helpers, &prepared));
    // The executor's turn guard: the record of everything sent (the relayed events first), then the
    // interrupted reply and the partial.
    let mut saving = Saving::new(TurnRecord::at(FIXED_NOW as i64));
    let JValue::Arr(events) = case.get("events") else {
        panic!()
    };
    let before = case.get("handoff").get("before").as_num().unwrap() as usize;
    for e in &events[..before] {
        saving.observe(e, false);
    }
    for (e, left) in script.observed.lock().unwrap().iter() {
        saving.observe(e, *left);
    }
    let left = matches!(ended, Ended::Left);
    let chat = prepared.record.then(|| {
        case.get("input")
            .get("body")
            .get("chatId")
            .as_str()
            .unwrap()
            .to_string()
    });
    // The lease goes back as the response ends or the client leaves, before the store is asked.
    saving.finish(
        &script,
        chat.as_deref(),
        &prepared.message,
        left,
        FIXED_NOW as i64,
    );
    let _ = f;
    (script, ended)
}

#[test]
fn every_prepared_turn_reads() {
    let f = fixture();
    let JValue::Arr(cases) = f.get("cases") else {
        panic!()
    };
    let mut read = 0;
    for c in cases.iter().filter(|c| c.get("handoff").is_object()) {
        let p = Prepared::read(c.get("handoff").get("prepared"))
            .unwrap_or_else(|e| panic!("{}: {e:?}", c.get("name").as_str().unwrap()));
        assert!(p.model.starts_with("qa"));
        assert_eq!(c.get("handoff").get("before").as_num(), Some(5.0));
        read += 1;
    }
    assert!(read >= 30);
}

#[test]
fn the_native_continuation_is_node_after_the_cut() {
    let f = fixture();
    let JValue::Arr(cases) = f.get("cases") else {
        panic!()
    };
    let mut ran = Vec::new();
    for c in cases
        .iter()
        .filter(|c| c.get("handoff").get("prepared").is_object())
    {
        let name = c.get("name").as_str().unwrap().to_string();
        if NOT_YET.iter().any(|(n, _)| *n == name) {
            continue;
        }
        let (script, _ended) = replay(&f, c);
        let JValue::Arr(events) = c.get("events") else {
            panic!()
        };
        let before = c.get("handoff").get("before").as_num().unwrap() as usize;
        let want: Vec<String> = events[before..].iter().map(js::stringify).collect();
        let got = script.events.lock().unwrap().clone();
        let kinds = |list: &[String]| {
            list.iter()
                .map(|e| {
                    wtf16::parse(e)
                        .unwrap()
                        .get("type")
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(
            got,
            want,
            "{name}: events after the cut\n native: {}\n node:   {}",
            kinds(&got),
            kinds(&want)
        );
        let JValue::Arr(requests) = c.get("requests") else {
            panic!()
        };
        let sent = script.requests.lock().unwrap().clone();
        assert_eq!(sent.len(), requests.len(), "{name}: engine requests");
        for (i, (s, r)) in sent.iter().zip(requests).enumerate() {
            assert_eq!(s.url, r.get("url").as_str().unwrap(), "{name} request {i}");
            assert_eq!(
                headers_json(&s.headers),
                js::stringify(r.get("headers")),
                "{name} request {i} headers"
            );
            let mut body = r.get("body").clone();
            if let Some(key) = body.get("tools").get("$tools").as_str() {
                let tools = f.get("tools").get(key).clone();
                js::set(&mut body, "tools", tools);
            }
            assert_eq!(s.body, js::stringify(&body), "{name} request {i} body");
        }
        let JValue::Arr(approvals) = c.get("approvals") else {
            panic!()
        };
        let want: Vec<String> = approvals.iter().map(js::stringify).collect();
        assert_eq!(
            *script.approvals.lock().unwrap(),
            want,
            "{name}: approval requests"
        );
        let JValue::Arr(executions) = c.get("executions") else {
            panic!()
        };
        let want: Vec<String> = executions
            .iter()
            .map(|e| e.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            *script.executions.lock().unwrap(),
            want,
            "{name}: tools run"
        );
        assert!(
            script.bridge.lock().unwrap().is_empty(),
            "{name}: every bridge answer was asked for"
        );

        {
            let JValue::Arr(stores) = c.get("stores") else {
                panic!()
            };
            let want: Vec<String> = stores.iter().map(js::stringify).collect();
            let got: Vec<String> = script
                .stores
                .lock()
                .unwrap()
                .iter()
                .map(js::stringify)
                .collect();
            assert_eq!(got, want, "{name}: store calls");
            let JValue::Obj(files) = c.get("files") else {
                panic!()
            };
            let mine = script.files.lock().unwrap().clone();
            let want: BTreeMap<String, String> = files
                .iter()
                .map(|(k, v)| (k.clone(), js::stringify(v)))
                .collect();
            assert_eq!(mine, want, "{name}: workspace files");
        }
        ran.push(name);
    }
    eprintln!("native replay ran: {ran:?}");
    assert_eq!(ran.len(), 39, "{ran:?}");
}

/// The front spawns the turn on its runtime: `native::run` must be `Send` for a `Send + Sync` host.
#[test]
fn the_native_turn_can_be_spawned() {
    fn spawnable<T: Send>(_: &T) {}
    let f = fixture();
    let JValue::Arr(cases) = f.get("cases") else {
        panic!()
    };
    let prepared = Prepared::read(cases[0].get("handoff").get("prepared")).unwrap();
    let script = Script::new(cases[0].clone());
    let process = Process::default();
    let helpers = Helpers {
        clean_search_args: &|n, a| chat_turn::helpers::clean_search_args(n, a).into_owned(),
        announces_tool_action: &chat_turn::helpers::announces_tool_action,
        announced_note: chat_turn::helpers::ANNOUNCED_NOTE,
        reduce_tool_result: &|text, cap| chat_turn::reduce::reduce_tool_result(text, cap).text,
        source_links: &chat_turn::helpers::source_links,
        fallback_text: &|s| chat_turn::helpers::fallback_text(s),
        final_answer_note: chat_turn::helpers::FINAL_ANSWER_NOTE,
        tool_result_cap: 8000,
    };
    let turn = native::run(&script, &process, &helpers, &prepared);
    spawnable(&turn);
}

/// What core wrote for each hand-off, split by the front's frame reader at several chunk sizes: the
/// frames before `prepared` are forwarded as written, and the `prepared` frame is the recorded one.
#[test]
fn the_frame_reader_finds_every_recorded_hand_off() {
    use chat_turn::handoff::{Frames, Piece};
    let f = fixture();
    let JValue::Arr(cases) = f.get("cases") else {
        panic!()
    };
    for c in cases
        .iter()
        .filter(|c| c.get("handoff").get("prepared").is_object())
    {
        let JValue::Arr(raw) = c.get("handoff").get("raw") else {
            panic!()
        };
        let lines: Vec<String> = raw
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect();
        let bytes = lines.concat().into_bytes();
        let before: Vec<u8> = lines[..lines.len() - 1].concat().into_bytes();
        for size in [1, 7, 64, 4096, bytes.len()] {
            let mut frames = Frames::new();
            let mut forwarded = Vec::new();
            let mut prepared = None;
            for chunk in bytes.chunks(size) {
                for piece in frames.feed(chunk) {
                    match piece {
                        Piece::Forward(b) => forwarded.extend(b),
                        Piece::Prepared(p) => prepared = Some(p),
                        Piece::Unreadable => {
                            panic!("{}: unreadable", c.get("name").as_str().unwrap())
                        }
                    }
                }
            }
            assert_eq!(
                forwarded,
                before,
                "{} at {size}",
                c.get("name").as_str().unwrap()
            );
            assert_eq!(
                js::stringify(&prepared.unwrap()),
                js::stringify(c.get("handoff").get("prepared"))
            );
        }
    }
}
