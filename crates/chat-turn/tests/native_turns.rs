//! The native continuation replayed against the recorded Node turns (`record_turns.cjs`): each case's
//! `prepared` frame (recorded from core's hand-off) is run by `chat_turn::native::run`, then the turn's
//! end ([`Saving::finish`]), against a host that answers only what the recording says Node's edges
//! answered: the engine's rounds and bytes, the model manager, the tool bridges (`bridge`), and the
//! other stubs after the cut (`stubs`: the tool policy, the chat-wide grant, the approval decision, the
//! tombstones, the template tools gate, the engine's busy signal), each asked in Node's order per stub. The
//! skills bridges (#272, #546) are in `bridge` too, in Node's order with the tool bridges; a read's ledger file
//! (core's record) lands in the workspace files as the bridge answers, and the files the preparation wrote
//! (`handoff.files`, a pinned skill's ledger) are there from the start. The clock moves on with the engine's
//! `{ tick }` frames, as the recorder's does.
//!
//! Everything after the cut must be Node's, byte for byte: the events, the engine requests (url,
//! headers, body, whether streamed, the deadline), the approval requests, every field of each tool run,
//! the tools run, the store calls and the workspace files; and every recorded answer must be asked for.
//!
//! The host gives the lease back the moment the client leaves, as Node's `close` handler does, so what
//! the turn still does after that (an audit, the bridge's count of a tool that was running) follows it.
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
use chat_turn::host::{
    Answer, EditTarget, EngineRequest, GateRefusal, Host, Ran, Read, Scrub, Skill, Target, ToolRun,
};
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
    /// The clock moves on this many milliseconds before the next chunk.
    Tick(f64),
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
    /// The other stubs' recorded answers after the cut, asked in order per stub.
    stubs: Mutex<Vec<JValue>>,
    approvals: Mutex<Vec<String>>,
    executions: Mutex<Vec<String>>,
    uuids: Mutex<u64>,
    /// The policy reads past the recording in a redeem-refusal case (D17: exactly one).
    extra_policy_reads: Mutex<u32>,
    leased: Mutex<bool>,
    /// The turn's lease: the recorder's gate counts the leases taken before the cut (a Vision role's description).
    lease: String,
    loaded: Mutex<bool>,
    /// Every event the turn sent, the client's leaving aside, with whether it had left.
    observed: Mutex<Vec<(JValue, bool)>>,
    /// How far the engine's `{ tick }` frames moved the clock on.
    clock: Mutex<f64>,
    /// The skills bridges that give no answer (core unreachable): the named differences' tests.
    down: Vec<&'static str>,
    /// With `skills-revoked` down: how many revocation checks are still answered first.
    revoked_answers: Mutex<usize>,
    /// The engine's busy signal yields once before it answers (two turns interleaved: noevia#2591).
    busy_yields: bool,
    /// The model the model manager reports loaded, when not the case's (an empty model: noevia#2591).
    serving: Option<String>,
}

/// One `Pending` before `Ready`: an await point a second turn can run in.
struct YieldOnce(bool);

impl Future for YieldOnce {
    type Output = ();
    fn poll(mut self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            Poll::Pending
        }
    }
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
            _ if c.get("tick").as_num().is_some() => Chunk::Tick(c.get("tick").as_num().unwrap()),
            _ if c.get("text").as_str().is_some() => {
                Chunk::Bytes(c.get("text").as_str().unwrap().as_bytes().to_vec())
            }
            _ => Chunk::Bytes(base64(c.get("b64").as_str().unwrap())),
        })
        .collect()
}

/// How many engine requests and store calls the recorded turn made before the cut (a Vision role's
/// description: its request and its lease); the native turn starts after them.
fn before_cut(case: &JValue) -> (usize, usize) {
    let count = |k: &str| case.get("handoff").get(k).as_num().unwrap_or(0.0) as usize;
    (count("requests"), count("stores"))
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
        let recorded = |key: &str| match case.get(key) {
            JValue::Arr(b) => b.clone(),
            _ => Vec::new(),
        };
        let (recorded_bridge, recorded_stubs) = (recorded("bridge"), recorded("stubs"));
        let (skip, before) = before_cut(&case);
        let earlier = recorded("stores")
            .iter()
            .take(before)
            .filter(|s| s.get("op").as_str() == Some("modelManager.acquireLease"))
            .count();
        let seeded: BTreeMap<String, String> = match case.get("handoff").get("files") {
            JValue::Obj(files) => files
                .iter()
                .map(|(k, v)| (k.clone(), js::stringify(v)))
                .collect(),
            _ => BTreeMap::new(),
        };
        Script {
            case,
            rounds,
            next: Mutex::new(skip),
            left: Mutex::new(false),
            events: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            stores: Mutex::new(Vec::new()),
            files: Mutex::new(seeded),
            bridge: Mutex::new(recorded_bridge),
            stubs: Mutex::new(recorded_stubs),
            approvals: Mutex::new(Vec::new()),
            executions: Mutex::new(Vec::new()),
            uuids: Mutex::new(0),
            extra_policy_reads: Mutex::new(0),
            leased: Mutex::new(false),
            lease: format!("qa-lease-{}", earlier + 1),
            loaded: Mutex::new(case_loaded),
            observed: Mutex::new(Vec::new()),
            clock: Mutex::new(0.0),
            down: Vec::new(),
            revoked_answers: Mutex::new(0),
            busy_yields: false,
            serving: None,
        }
    }

    /// The next recorded bridge call, which must be `op`.
    fn bridged(&self, op: &str) -> JValue {
        let next = self.bridge.lock().unwrap().remove(0);
        assert_eq!(next.get("op").as_str(), Some(op), "the next bridge call");
        next
    }

    fn skill_list(v: &JValue) -> Vec<Skill> {
        let JValue::Arr(items) = v else {
            panic!("not a skill list: {v:?}")
        };
        items
            .iter()
            .map(|k| Skill {
                file: k.get("file").as_str().unwrap().to_string(),
                hash: k.get("hash").as_str().unwrap().to_string(),
                name: k.get("name").as_str().unwrap().to_string(),
            })
            .collect()
    }

    fn chat_asked(&self, recorded: &JValue, chat_id: Option<&str>) {
        assert_eq!(
            chat_id,
            self.case.get("input").get("body").get("chatId").as_str(),
            "the chat a skills bridge names"
        );
        if !matches!(recorded, JValue::Undefined) {
            assert_eq!(recorded.as_str(), chat_id, "the chat the scrub ran for");
        }
    }

    fn store(&self, op: &str, mut pairs: Vec<(&str, JValue)>) {
        pairs.insert(0, ("op", js::s(op)));
        self.stores.lock().unwrap().push(js::obj(pairs));
    }

    fn file_name(id: &str) -> String {
        format!("context-{}.json", context_fit::fingerprint(&js::s(id)))
    }

    /// The next recorded answer of stub `op`, whose recorded question must be `asked`.
    fn answer(&self, op: &str, asked: &[(&str, JValue)]) -> JValue {
        let mut stubs = self.stubs.lock().unwrap();
        let at = stubs
            .iter()
            .position(|s| s.get("op").as_str() == Some(op))
            .unwrap_or_else(|| panic!("Node's {op} was not asked this often (asked {asked:?})"));
        let entry = stubs.remove(at);
        for (k, v) in asked {
            assert_eq!(
                js::stringify(entry.get(k)),
                js::stringify(v),
                "{op}: the question's {k}"
            );
        }
        entry.get("answer").clone()
    }

    /// The client goes away: the front gives the lease back at once, as Node's `close` handler does.
    fn leave(&self) {
        *self.left.lock().unwrap() = true;
        self.release_lease();
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
        FIXED_NOW + *self.clock.lock().unwrap()
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
            vec![
                ("model", js::s(model)),
                ("lease", js::s(self.lease.clone())),
            ],
        );
        Ok(Some(vec![("X-Noevia-Lease".into(), self.lease.clone())]))
    }

    async fn health(&self) -> Option<JValue> {
        let ctx = self.case.get("input").get("ctxSize").clone();
        let model = self.serving.clone().unwrap_or_else(|| {
            self.case
                .get("input")
                .get("project")
                .get("model")
                .as_str()
                .unwrap_or("qa-model")
                .to_string()
        });
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

    async fn template_allows_tools(&self, model: &str) -> bool {
        self.answer("allowsTools", &[("model", js::s(model))]) == JValue::Bool(true)
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
        while let Some(Chunk::Tick(ms)) = body.chunks.get(body.at) {
            *self.clock.lock().unwrap() += *ms;
            body.at += 1;
        }
        let Some(chunk) = body.chunks.get(body.at).cloned() else {
            return Read::End;
        };
        body.at += 1;
        match chunk {
            Chunk::Bytes(b) => Read::Chunk(b),
            Chunk::Leave => {
                self.leave();
                Read::Aborted
            }
            Chunk::Stall => match idle_ms {
                Some(_) => Read::Idle,
                None => panic!("a stall before the first output would wait forever"),
            },
            Chunk::Tick(_) => unreachable!(),
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
            self.store("lease.release", vec![("lease", js::s(self.lease.clone()))]);
        }
    }

    fn mark_tools_unsupported(&self, model: &str) {
        self.store(
            "templateToolsGate.markUnsupported",
            vec![("model", js::s(model))],
        );
    }

    fn gate_record(&self, event: &str, tool: &str) {
        self.store(
            "toolGate.record",
            vec![("event", js::s(event)), ("tool", js::s(tool))],
        );
    }

    async fn engine_busy(&self) -> bool {
        if self.busy_yields {
            YieldOnce(false).await;
        }
        self.answer("inferenceBusy", &[("n", js::n(1.0))]) == JValue::Bool(true)
    }

    async fn supervise(&self, round: i32, outputs: &[JValue], goal: &str, wrote: bool) -> String {
        // The recorder answers core's supervisor (step-supervision.cjs decide) with the case's verdicts.
        self.answer(
            "supervise",
            &[
                ("round", js::n(f64::from(round))),
                ("goal", js::s(goal)),
                ("wrote", JValue::Bool(wrote)),
                ("outputs", JValue::Arr(outputs.to_vec())),
            ],
        )
        .as_str()
        .unwrap_or("continue")
        .to_string()
    }

    async fn title_auto_named(&self, list: Option<&str>, chat: &str, first: &str) -> bool {
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

    fn tool_policy(&self, name: &str, write: bool) -> String {
        // Named difference (D17): the native gate reads the policy for dispatch before core's execute bridge
        // redeems the grant; chat.cjs redeems first and stops there when the redeem is refused or unchecked, so
        // its recording has one policy read fewer. Those cases' writes ask (the default).
        let redeem_refused = self
            .case
            .get("input")
            .get("approvalFront")
            .get("redeem")
            .as_str()
            .is_some();
        let left = self
            .stubs
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.get("op").as_str() == Some("policy"));
        if redeem_refused && !left {
            // Exactly one read more than Node's.
            let mut extra = self.extra_policy_reads.lock().unwrap();
            *extra += 1;
            assert_eq!(*extra, 1, "more than the one extra policy read");
            return "ask".into();
        }
        self.answer(
            "policy",
            &[("name", js::s(name)), ("write", JValue::Bool(write))],
        )
        .as_str()
        .unwrap()
        .to_string()
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

    async fn edit_target(&self, project_id: Option<&str>, name: &str, args: &str) -> EditTarget {
        let next = self.bridge.lock().unwrap().remove(0);
        assert_eq!(
            (next.get("op").as_str(), next.get("args").as_str()),
            (Some("edit-target"), Some(args)),
            "{name}: the edit target asked"
        );
        // chat.cjs resolves against the turn's own project (`project ? getProject(project.id) : null`).
        assert_eq!(
            project_id,
            self.case.get("input").get("project").get("id").as_str()
        );
        let found = next.get("found");
        match (found.get("error").as_str(), found.get("path").as_str()) {
            (Some(e), _) => EditTarget::Refused(e.to_string()),
            (None, Some(path)) => EditTarget::Found {
                path: path.to_string(),
                account: found.get("account").as_str().map(str::to_string),
                digest: found.get("digest").as_str().unwrap().to_string(),
            },
            _ => EditTarget::Failed,
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

    async fn chat_wide_approved(
        &self,
        chat_id: Option<&str>,
        scope: &str,
        _binding: &JValue,
        _exchange_key: &str,
    ) -> Option<String> {
        // `leaveInGrant`: the client goes away while the grant check is out; its answer still comes.
        if self
            .case
            .get("input")
            .get("approvalFront")
            .get("leaveInGrant")
            == &JValue::Bool(true)
        {
            self.leave();
        }
        (self.answer(
            "chatWideApproved",
            &[
                ("chatId", chat_id.map_or(JValue::Null, js::s)),
                ("scope", js::s(scope)),
            ],
        ) == JValue::Bool(true))
        .then(|| "0123456789abcdef0123456789abcdef".to_string())
    }

    async fn open_approval(&self, _request: &JValue) -> bool {
        self.answer("openApproval", &[]) == JValue::Bool(true)
    }

    async fn await_approval(&self, request: &JValue) -> String {
        self.approvals.lock().unwrap().push(js::stringify(request));
        let left = self
            .stubs
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.get("op").as_str() == Some("awaitApproval"))
            .is_some_and(|s| s.get("left") == &JValue::Bool(true));
        let decision = self.answer("awaitApproval", &[]);
        if left {
            // The card was open when the client left: the gate's abortSignal answers 'aborted'.
            self.leave();
        }
        decision.as_str().unwrap().to_string()
    }

    async fn revoke_grant(&self, chat_id: Option<&str>) {
        self.store(
            "revokeChatGrant",
            vec![
                ("userId", js::s("qa-user")),
                ("chatId", chat_id.map_or(JValue::Null, js::s)),
            ],
        );
    }

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

    async fn execute_tool(&self, run: &ToolRun) -> Ran {
        // A grant is redeemed by the execute bridge itself: Node's redeem answer comes first (D17).
        if let Some(grant) = &run.grant {
            assert_eq!(grant.chat, "qa-chat-1", "the grant's chat");
            let redeemed = self.answer("redeemChatGrant", &[("scope", js::s(grant.scope.clone()))]);
            match redeemed.as_str() {
                Some("refused") => return Ran::Refused,
                Some("unavailable") => return Ran::Unavailable,
                _ => {}
            }
        }
        let next = self.bridge.lock().unwrap().remove(0);
        assert_eq!(
            next.get("fenced") == &JValue::Bool(true),
            run.grant.is_some(),
            "the tool run under a redeemed grant"
        );
        // Every field the bridge request carries, as Node passed it to executeToolCall.
        let opt = |v: &Option<String>| v.clone().map_or(JValue::Null, js::s);
        let asked = js::obj([
            ("op", js::s("execute")),
            ("projectId", opt(&run.project_id)),
            ("name", js::s(run.name.clone())),
            ("args", js::s(run.args.clone())),
            (
                "allowed",
                JValue::Arr(run.allowed.iter().cloned().map(js::s).collect()),
            ),
            (
                "routes",
                JValue::Arr(
                    run.routes
                        .iter()
                        .map(|(n, s)| JValue::Arr(vec![js::s(n.clone()), js::s(s.clone())]))
                        .collect(),
                ),
            ),
            ("chatKey", opt(&run.chat_key)),
            ("exchangeKey", js::s(run.exchange_key.clone())),
        ]);
        let JValue::Obj(fields) = &asked else {
            panic!()
        };
        assert_eq!(
            next.get("editTarget").as_str(),
            run.edit_target.as_deref(),
            "the tool run's editTarget"
        );
        for (k, v) in fields {
            assert_eq!(
                js::stringify(next.get(k)),
                js::stringify(v),
                "the tool run's {k}"
            );
        }
        // The recorder lists what the Drive and MCP tool engines ran; core's built-in tools run in its toolboxes.
        if !["read_project_file", "get_current_time", "space_page_save"]
            .contains(&run.name.as_str())
        {
            self.executions.lock().unwrap().push(run.name.clone());
        }
        if let Some(message) = next.get("threw").as_str() {
            return Ran::Threw(message.to_string());
        }
        if next.get("left") == &JValue::Bool(true) {
            self.leave();
        }
        let not_run = next.get("notRun") == &JValue::Bool(true);
        // The bridge counts the call as chat.cjs does (not one its fence stopped).
        if !not_run {
            self.store("recordToolUse", vec![("name", js::s(run.name.clone()))]);
        }
        Ran::Done {
            text: next.get("text").as_str().unwrap().to_string(),
            failed: next.get("failed") == &JValue::Bool(true),
            not_run,
        }
    }

    async fn skills_revoked(
        &self,
        project_id: &str,
        loaded: &[Skill],
    ) -> Option<Vec<(String, String)>> {
        if self.down.contains(&"skills-revoked") {
            let mut left = self.revoked_answers.lock().unwrap();
            if *left == 0 {
                return None;
            }
            *left -= 1;
        }
        let next = self.bridged("skills-revoked");
        assert_eq!(
            Some(project_id),
            self.case.get("input").get("project").get("id").as_str()
        );
        assert_eq!(
            loaded,
            Script::skill_list(next.get("loaded")),
            "the loaded skills checked"
        );
        let JValue::Arr(gone) = next.get("gone") else {
            panic!()
        };
        Some(
            gone.iter()
                .map(|g| {
                    (
                        g.get("file").as_str().unwrap().to_string(),
                        g.get("name").as_str().unwrap().to_string(),
                    )
                })
                .collect(),
        )
    }

    async fn skills_read(
        &self,
        project_id: &str,
        chat_id: Option<&str>,
        candidates: &[Skill],
        args: &str,
        result: &str,
    ) -> Option<Vec<Skill>> {
        if self.down.contains(&"skills-read") {
            return None;
        }
        let next = self.bridged("skills-read");
        assert_eq!(
            Some(project_id),
            self.case.get("input").get("project").get("id").as_str()
        );
        self.chat_asked(&JValue::Undefined, chat_id);
        assert_eq!(
            candidates,
            Script::skill_list(next.get("candidates")),
            "the read's candidates"
        );
        assert_eq!(Some(args), next.get("args").as_str(), "the read's args");
        assert_eq!(
            Some(result),
            next.get("result").as_str(),
            "the read's result"
        );
        // Core records each loaded version in the account's skill ledger.
        if next.get("ledger").is_object() {
            self.files.lock().unwrap().insert(
                "skill-history.json".into(),
                js::stringify(next.get("ledger")),
            );
        }
        Some(Script::skill_list(next.get("loaded")))
    }

    async fn skills_scrub(
        &self,
        project_id: &str,
        chat_id: Option<&str>,
        messages: &[JValue],
    ) -> Option<Scrub> {
        if self.down.contains(&"skills-scrub") {
            return None;
        }
        let next = self.bridged("skills-scrub");
        assert_eq!(
            Some(project_id),
            self.case.get("input").get("project").get("id").as_str()
        );
        self.chat_asked(next.get("chatId"), chat_id);
        assert_eq!(
            js::stringify(&JValue::Arr(messages.to_vec())),
            js::stringify(next.get("messages")),
            "the messages the scrub reads"
        );
        let (JValue::Arr(changed), JValue::Arr(removed)) =
            (next.get("changed"), next.get("removed"))
        else {
            panic!()
        };
        Some(Scrub {
            changed: changed
                .iter()
                .map(|c| {
                    let JValue::Arr(pair) = c else { panic!() };
                    (
                        pair[0].as_num().unwrap() as usize,
                        pair[1].as_str().unwrap().to_string(),
                    )
                })
                .collect(),
            removed: removed
                .iter()
                .map(|n| n.as_str().unwrap().to_string())
                .collect(),
        })
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

    fn tombstoned(&self, chat: &str) -> bool {
        let JValue::Arr(ids) = self.answer("tombstones", &[]) else {
            panic!("tombstones: not a list")
        };
        ids.iter().any(|id| id.as_str() == Some(chat))
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
    replay_with(f, case, &[])
}

fn replay_with(f: &JValue, case: &JValue, down: &[&'static str]) -> (Script, Ended) {
    let prepared =
        Prepared::read(case.get("handoff").get("prepared")).unwrap_or_else(|e| panic!("{e:?}"));
    let mut script = Script::new(case.clone());
    script.down = down.to_vec();
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
    let chat = case
        .get("input")
        .get("body")
        .get("chatId")
        .as_str()
        .map(str::to_string);
    // The lease goes back as the response ends or the client leaves, before the store is asked.
    saving.finish(
        &script,
        chat.as_deref(),
        prepared.record,
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
    let mut refused = Vec::new();
    for c in cases.iter().filter(|c| c.get("handoff").is_object()) {
        let name = c.get("name").as_str().unwrap();
        if let Some(why) = c.get("handoff").get("refused").as_str() {
            // A turn core keeps: no frame, Node runs it.
            assert_eq!(c.get("handoff").get("prepared"), &JValue::Null, "{name}");
            refused.push(format!("{name}: {why}"));
            continue;
        }
        let p = Prepared::read(c.get("handoff").get("prepared"))
            .unwrap_or_else(|e| panic!("{}: {e:?}", c.get("name").as_str().unwrap()));
        assert!(p.model.starts_with("qa"));
        // In the recordings the cut comes right after the preparation's `tools_scope` (and a pinned skill's
        // `skills_scope`); a pending route pre-load's `loadingModel` status could follow it (no case has one).
        let JValue::Arr(events) = c.get("events") else {
            panic!()
        };
        let mut scope = events
            .iter()
            .position(|e| e.get("type").as_str() == Some("tools_scope"))
            .unwrap_or_else(|| panic!("{}: no tools_scope", c.get("name").as_str().unwrap()));
        if events.get(scope + 1).map(|e| e.get("type").as_str()) == Some(Some("skills_scope")) {
            scope += 1;
        }
        assert_eq!(
            c.get("handoff").get("before").as_num(),
            Some((scope + 1) as f64),
            "{}",
            c.get("name").as_str().unwrap()
        );
        // The project edit tools are named for the gate (chat.cjs projectEditTool) exactly when offered.
        let offered = |n: &str| {
            p.tools
                .iter()
                .any(|t| t.get("function").get("name").as_str() == Some(n))
        };
        let edits: Vec<&str> = ["project_append_file", "project_replace_text"]
            .into_iter()
            .filter(|n| offered(n))
            .collect();
        assert_eq!(p.edit_tools, edits, "{name}");
        read += 1;
    }
    assert!(read >= 30);
    // The coworker page save (#2446) is handed off too now (the `once` capability): no recorded turn stays in Node.
    // Every recorded turn is handed off now (the tool gate's prefetch too).
    assert_eq!(refused, Vec::<String>::new());
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
        let (skip, earlier) = before_cut(c);
        let JValue::Arr(requests) = c.get("requests") else {
            panic!()
        };
        let requests = &requests[skip..];
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
            assert_eq!(
                JValue::Bool(s.stream),
                *r.get("stream"),
                "{name} request {i} streamed"
            );
            assert_eq!(
                s.timeout_ms.map_or(JValue::Null, |ms| js::n(ms as f64)),
                *r.get("timeoutMs"),
                "{name} request {i} deadline"
            );
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
        assert_eq!(
            *script.stubs.lock().unwrap(),
            Vec::<JValue>::new(),
            "{name}: every stub answer was asked for"
        );

        {
            let JValue::Arr(stores) = c.get("stores") else {
                panic!()
            };
            let want: Vec<String> = stores[earlier..].iter().map(js::stringify).collect();
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
    assert_eq!(ran.len(), 149, "{ran:?}");
}

/// A tool the account blocks is not offered: no request lists it, and the model's call of it anyway is
/// the exchange's refusal (it is not in the allowed set), before any policy read.
#[test]
fn a_blocked_tool_is_not_offered_and_its_call_is_refused() {
    let f = fixture();
    let JValue::Arr(cases) = f.get("cases") else {
        panic!()
    };
    let c = cases
        .iter()
        .find(|c| c.get("name").as_str() == Some("tool_policy_block"))
        .unwrap();
    assert_eq!(
        js::stringify(c.get("input").get("policy")),
        r#"{"drive_get_metadata":"block"}"#
    );
    let JValue::Arr(requests) = c.get("requests") else {
        panic!()
    };
    assert!(!requests.is_empty());
    for r in requests {
        let key = r.get("body").get("tools").get("$tools").as_str().unwrap();
        let JValue::Arr(tools) = f.get("tools").get(key) else {
            panic!()
        };
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("function").get("name").as_str())
            .collect();
        assert!(names.contains(&"drive_read_file"), "{names:?}");
        assert!(!names.contains(&"drive_get_metadata"), "{names:?}");
    }
    let JValue::Arr(events) = c.get("events") else {
        panic!()
    };
    let result = events
        .iter()
        .find(|e| e.get("type").as_str() == Some("tool_result"))
        .unwrap();
    assert_eq!(
        result.get("text").as_str(),
        Some(r#"ERROR: tool "drive_get_metadata" is not enabled for this project"#)
    );
    assert!(!js::stringify(c.get("stubs")).contains(r#""op":"policy""#));
}

/// Named difference (gate.rs): an edit whose file the bridge gives no answer for, at the check after the
/// card or the one before the run, is refused with the lookup sentence and audited `edit-target`; the
/// chat's grant is not revoked and nothing runs. Node has no bridge, so this is the executor's own.
#[test]
fn an_edit_the_bridge_cannot_resolve_after_the_card_is_refused_without_revoking() {
    let f = fixture();
    let JValue::Arr(cases) = f.get("cases") else {
        panic!()
    };
    let base = cases
        .iter()
        .find(|c| c.get("name").as_str() == Some("edit_approved"))
        .unwrap();
    for at in [1usize, 2] {
        let mut case = base.clone();
        let JValue::Arr(mut bridge) = case.get("bridge").clone() else {
            panic!()
        };
        let edits: Vec<usize> = bridge
            .iter()
            .enumerate()
            .filter(|(_, b)| b.get("op").as_str() == Some("edit-target"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            edits.len(),
            3,
            "edit_approved resolves its file three times"
        );
        js::set(
            &mut bridge[edits[at]],
            "found",
            js::obj(Vec::<(String, JValue)>::new()),
        );
        js::set(&mut case, "bridge", JValue::Arr(bridge));
        let (script, _) = replay(&f, &case);
        let events = script.events.lock().unwrap().clone();
        let result = events
            .iter()
            .map(|e| wtf16::parse(e).unwrap())
            .find(|e| e.get("type").as_str() == Some("tool_result"))
            .unwrap_or_else(|| panic!("{at}: no tool_result in {events:?}"));
        assert_eq!(
            result.get("text").as_str(),
            Some("ERROR: The file this would change could not be looked up. project_append_file was not run and nothing was changed."),
            "{at}"
        );
        let stores: Vec<String> = script
            .stores
            .lock()
            .unwrap()
            .iter()
            .map(js::stringify)
            .collect();
        assert!(
            !stores.iter().any(|s| s.contains("revokeChatGrant")),
            "{at}: {stores:?}"
        );
        assert!(
            stores.iter().any(|s| s.contains(r#""event":"tool.denied""#)
                && s.contains(r#""reason":"edit-target""#)),
            "{at}: {stores:?}"
        );
        assert!(script.executions.lock().unwrap().is_empty(), "{at}");
    }
}

/// Named differences (skills.rs): a skills bridge that gives no answer fails closed, ending the turn with the
/// failure text (not skill_revoked: nothing was revoked) before any further round or tool. Node asks no bridge,
/// so no recording shows these.
#[test]
fn a_skills_bridge_without_an_answer_ends_the_turn_with_the_failure_text() {
    let f = fixture();
    let JValue::Arr(cases) = f.get("cases") else {
        panic!()
    };
    let case = |name: &str| {
        cases
            .iter()
            .find(|c| c.get("name").as_str() == Some(name))
            .unwrap()
    };
    let events = |script: &Script| -> Vec<JValue> {
        script
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|e| wtf16::parse(e).unwrap())
            .collect()
    };
    let failed = format!(r#"{{"type":"error","text":"{}"}}"#, native::FAILED_TEXT);
    let last = |script: &Script| js::stringify(events(script).last().unwrap());
    // The revocation check at the pinned turn's first round: nothing is asked of the engine.
    let (pinned, ended) = replay_with(&f, case("skills_pinned"), &["skills-revoked"]);
    assert_eq!(ended, Ended::Error);
    assert_eq!(last(&pinned), failed);
    assert!(pinned.requests.lock().unwrap().is_empty());
    assert!(!events(&pinned)
        .iter()
        .any(|e| e.get("code").as_str() == Some("skill_revoked")));
    // The read after the first round's call: its result is shown, then the turn ends; no second round, and
    // the ledger has no record core did not write.
    let (read, ended) = replay_with(&f, case("skills_read_noted"), &["skills-read"]);
    assert_eq!(ended, Ended::Error);
    assert_eq!(last(&read), failed);
    assert_eq!(
        read.requests.lock().unwrap().len(),
        1,
        "no round after the read"
    );
    let types: Vec<String> = events(&read)
        .iter()
        .map(|e| e.get("type").as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        types[types.len() - 2..],
        ["tool_result".to_string(), "error".to_string()]
    );
    assert!(!read
        .files
        .lock()
        .unwrap()
        .contains_key("skill-history.json"));
    // The revocation check in the gate (the pinned turn's first call): refused unchecked, unaudited, not run.
    let gated = Script::new(case("skills_pinned").clone());
    *gated.revoked_answers.lock().unwrap() = 1;
    let mut gated = gated;
    gated.down = vec!["skills-revoked"];
    let prepared = Prepared::read(case("skills_pinned").get("handoff").get("prepared")).unwrap();
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
    let ended = block_on(native::run(
        &gated,
        &Process::default(),
        &helpers,
        &prepared,
    ));
    assert_eq!(ended, Ended::Error);
    let got = events(&gated);
    let result = got
        .iter()
        .find(|e| e.get("type").as_str() == Some("tool_result"))
        .unwrap();
    assert_eq!(
        result.get("text").as_str(),
        Some(chat_turn::gate::CHECK_FAULT)
    );
    assert_eq!(js::stringify(got.last().unwrap()), failed);
    assert_eq!(
        gated.requests.lock().unwrap().len(),
        1,
        "no round after the call"
    );
    assert!(gated.executions.lock().unwrap().is_empty());
    assert!(!gated
        .stores
        .lock()
        .unwrap()
        .iter()
        .any(|s| s.get("op").as_str() == Some("audit")));
    // The round-0 scrub: before any request.
    let (plain, ended) = replay_with(&f, case("skills_plain"), &["skills-scrub"]);
    assert_eq!(ended, Ended::Error);
    assert_eq!(last(&plain), failed);
    assert!(plain.requests.lock().unwrap().is_empty());
}

/// noevia#2591: chat-title.cjs `generateTitle` asks nothing with an empty model (the attempt still counts, as
/// in Node), and two turns of one chat that reach the title at its last allowed attempt make one title request,
/// not two: Node checks and records the attempt with no await between them, the native turn awaits the engine's
/// busy signal in between, so it re-checks under the counter's lock.
#[test]
fn a_title_needs_a_model_and_its_attempts_hold_across_turns_of_one_chat() {
    let f = fixture();
    let JValue::Arr(cases) = f.get("cases") else {
        panic!()
    };
    let case = cases
        .iter()
        .find(|c| c.get("name").as_str() == Some("title"))
        .unwrap();
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
    let prepared = Prepared::read(case.get("handoff").get("prepared")).unwrap();
    let titles = |script: &Script| {
        script
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| !r.stream)
            .count()
    };
    let named = |script: &Script| {
        script
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|e| wtf16::parse(e).unwrap().get("type").as_str() == Some("title"))
    };

    // An empty model: the turn ends with `done` and no title request.
    let mut unnamed = prepared.clone();
    unnamed.model = String::new();
    let unnamed_turn = || {
        let mut script = Script::new(case.clone());
        script.serving = Some(String::new());
        script
    };
    let script = unnamed_turn();
    let ended = block_on(native::run(
        &script,
        &Process::default(),
        &helpers,
        &unnamed,
    ));
    assert_eq!(ended, Ended::Done, "{:?}", script.events.lock().unwrap());
    assert_eq!(titles(&script), 0, "no title request without a model");
    assert!(!named(&script));

    // Two attempts made, one by an unnamed turn (it still counts), then two turns of the chat at once.
    let process = Process::default();
    let script = unnamed_turn();
    block_on(native::run(&script, &process, &helpers, &unnamed));
    let script = Script::new(case.clone());
    block_on(native::run(&script, &process, &helpers, &prepared));
    assert_eq!(titles(&script), 1);
    let mut a = Script::new(case.clone());
    a.busy_yields = true;
    let mut b = Script::new(case.clone());
    b.busy_yields = true;
    let (mut ta, mut tb) = (
        pin!(native::run(&a, &process, &helpers, &prepared)),
        pin!(native::run(&b, &process, &helpers, &prepared)),
    );
    let mut cx = Context::from_waker(Waker::noop());
    let (mut ea, mut eb) = (None, None);
    while ea.is_none() || eb.is_none() {
        if ea.is_none() {
            if let Poll::Ready(e) = ta.as_mut().poll(&mut cx) {
                ea = Some(e);
            }
        }
        if eb.is_none() {
            if let Poll::Ready(e) = tb.as_mut().poll(&mut cx) {
                eb = Some(e);
            }
        }
    }
    assert_eq!((ea, eb), (Some(Ended::Done), Some(Ended::Done)));
    assert_eq!(
        titles(&a) + titles(&b),
        1,
        "the third attempt is the last (title::MAX_ATTEMPTS)"
    );
    assert!(named(&a) != named(&b));
    // And none after it.
    let script = Script::new(case.clone());
    block_on(native::run(&script, &process, &helpers, &prepared));
    assert_eq!(titles(&script), 0);
}

/// Core keeps a turn whose prepared frame is longer than its cap in Node (rust-chat-native.cjs
/// `frameTooLarge`); the front's reader must take every frame up to that cap, or such a turn would fail
/// after core let it go.
#[test]
fn the_reader_takes_every_frame_core_hands_off() {
    let f = fixture();
    let cap = f
        .get("preparedCap")
        .as_num()
        .expect("the recording names core's cap") as usize;
    assert!(
        cap <= chat_turn::handoff::PREPARED_CAP,
        "core {cap} > front {}",
        chat_turn::handoff::PREPARED_CAP
    );
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

/// What core wrote for each hand-off, on the front's NDJSON wire (chat port S7: rust-chat-wire.cjs writes
/// `JSON.stringify(event) + "\n"` where the recording's event stream has `data: <same>\n\n`), split by
/// the front's reader at several chunk sizes: the lines before `prepared` pass on as written, and the
/// `prepared` line is the recorded frame.
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
            .map(|l| {
                let sse = l.as_str().unwrap();
                let json = sse
                    .strip_prefix("data: ")
                    .and_then(|t| t.strip_suffix("\n\n"))
                    .unwrap_or_else(|| panic!("not one event frame: {sse:?}"));
                format!("{json}\n")
            })
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

/// #769 (R15): a frame with the provenance policy on says `provenance: true`; any other present value refuses the
/// frame (it is never read as "off"), and a frame without the key is a turn without the policy.
#[test]
fn the_provenance_flag_is_true_or_absent() {
    let fixture = wtf16::parse(common::FIXTURE).unwrap();
    let JValue::Arr(cases) = fixture.get("cases") else {
        panic!()
    };
    let case = |name: &str| {
        cases
            .iter()
            .find(|c| c.get("name").as_str() == Some(name))
            .unwrap()
            .clone()
    };
    let frame = case("provenance_answer")
        .get("handoff")
        .get("prepared")
        .clone();
    assert!(Prepared::read(&frame).unwrap().provenance);
    for bad in [js::s("true"), js::n(1.0), JValue::Null, JValue::Bool(false)] {
        let mut f = frame.clone();
        js::set(&mut f, "provenance", bad.clone());
        assert!(Prepared::read(&f).is_err(), "{bad:?}");
    }
    let plain = case("plain");
    assert!(
        !Prepared::read(plain.get("handoff").get("prepared"))
            .unwrap()
            .provenance
    );
}

/// A frame whose gate names a write (chat.cjs never forces or prefetches one) is refused, not run without its card
/// (R15 review): the recorded `gate_require_called` frame with its tool swapped for one of the frame's writes.
#[test]
fn a_gate_naming_a_write_refuses_the_frame() {
    let fixture = wtf16::parse(common::FIXTURE).unwrap();
    let JValue::Arr(cases) = fixture.get("cases") else {
        panic!()
    };
    let frame = cases
        .iter()
        .find(|c| c.get("name").as_str() == Some("gate_require_called"))
        .unwrap()
        .get("handoff")
        .get("prepared")
        .clone();
    assert_eq!(
        Prepared::read(&frame).unwrap().gate_tool.as_deref(),
        Some("drive_read_file")
    );
    let JValue::Arr(writes) = frame.get("writes") else {
        panic!()
    };
    let write = writes.first().unwrap().clone();
    let mut f = frame.clone();
    js::set(&mut f, "gate", js::obj([("tool", write)]));
    assert!(Prepared::read(&f).is_err());
}
