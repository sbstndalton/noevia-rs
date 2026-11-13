//! The native continuation replayed against the recorded Node turns: each case's `prepared` frame
//! (recorded from core's hand-off) is run by `chat_turn::native::run` against a host that answers as
//! the recording's script did, and everything after the cut must be Node's, byte for byte: the
//! events, the engine requests (url, headers, body), the store calls and the workspace files.
//!
//! This step covers turns whose model calls no tool; the tool cases, the title and the
//! interrupted-turn saves are the next steps. Each case not covered yet is named, with why.
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

use chat_turn::host::{EngineRequest, GateRefusal, Host, Read};
use chat_turn::js;
use chat_turn::native::{self, Ended, Helpers, Process};
use chat_turn::prepared::Prepared;
use js_json::{wtf16, JValue};

const FIXED_NOW: f64 = 1_794_571_200_000.0;

/// The cases this step does not run yet, and why.
const NOT_YET: [(&str, &str); 14] = [
    ("tool_read", "tools"),
    ("tool_streamed_args", "tools"),
    ("tool_approved", "tools"),
    ("tool_declined", "tools"),
    ("tool_approval_timeout", "tools"),
    ("tool_write_target_refused", "tools"),
    ("tool_unknown", "tools"),
    ("final_pass", "tools"),
    ("final_pass_no_answer", "tools"),
    ("fallback_fails_then_final_pass", "tools"),
    ("title", "the title (B3)"),
    ("title_still_derived", "the title (B3)"),
    ("title_failed", "the title (B3)"),
    ("title_renamed", "the title's store check (B3)"),
];

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
}

fn chunks_of(round: &JValue) -> Vec<Chunk> {
    let mut out = Vec::new();
    let JValue::Arr(frames) = round.get("frames") else {
        return out;
    };
    for f in frames {
        if f.get("leave") == &JValue::Bool(true) {
            out.push(Chunk::Leave);
        } else if f.get("stall") == &JValue::Bool(true) {
            out.push(Chunk::Stall);
        } else if f.get("done") == &JValue::Bool(true) {
            out.push(Chunk::Bytes(b"data: [DONE]\n\n".to_vec()));
        } else if let JValue::Str(raw) = f {
            out.push(Chunk::Bytes(raw.as_bytes().to_vec()));
        } else if f.get("split").is_object() {
            // record_turns.cjs: the line cut at half its length in UTF-16 units.
            let line = format!("data: {}\n\n", js::stringify(f.get("split")));
            let half = js::len(&line) / 2;
            let head = js::slice(&line, half);
            out.push(Chunk::Bytes(head.as_bytes().to_vec()));
            out.push(Chunk::Bytes(line.as_bytes()[head.len()..].to_vec()));
        } else if f.get("splitBytes").is_object() {
            let bytes = format!("data: {}\n\n", js::stringify(f.get("splitBytes"))).into_bytes();
            let at = bytes.iter().position(|b| *b >= 0xf0).unwrap() + 2;
            out.push(Chunk::Bytes(bytes[..at].to_vec()));
            out.push(Chunk::Bytes(bytes[at..].to_vec()));
        } else {
            out.push(Chunk::Bytes(
                format!("data: {}\n\n", js::stringify(f)).into_bytes(),
            ));
        }
    }
    out
}

impl Script {
    fn new(case: JValue) -> Script {
        let JValue::Arr(rounds) = case.get("input").get("rounds").clone() else {
            panic!()
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
        if !*self.left.lock().unwrap() {
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
        self.store(
            "modelManager.acquireLease",
            vec![("model", js::s(model)), ("lease", js::s("qa-lease-1"))],
        );
        Ok(Some(vec![("X-Noevia-Lease".into(), "qa-lease-1".into())]))
    }

    async fn health(&self) -> Option<JValue> {
        let ctx = self.case.get("input").get("ctxSize").clone();
        Some(js::obj([(
            "all_models_loaded",
            JValue::Arr(vec![js::obj([
                ("model_name", js::s("qa-model")),
                ("loaded", JValue::Bool(true)),
                ("recipe_options", js::obj([("ctx_size", ctx)])),
            ])]),
        )]))
    }

    async fn load(&self, model: &str) -> bool {
        self.store("modelManager.load", vec![("model", js::s(model))]);
        true
    }

    async fn template_allows_tools(&self, _model: &str) -> bool {
        true
    }

    async fn request(&self, request: &EngineRequest) -> Result<(u16, Body), String> {
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
            return Ok((
                status as u16,
                Body {
                    text,
                    ..Body::default()
                },
            ));
        }
        if round.get("json").is_object() {
            return Ok((
                200,
                Body {
                    text: js::stringify(round.get("json")),
                    ..Body::default()
                },
            ));
        }
        Ok((
            200,
            Body {
                chunks: chunks_of(&round),
                ..Body::default()
            },
        ))
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

    fn save_state(&self, id: &str, state: &JValue) {
        self.files
            .lock()
            .unwrap()
            .insert(Script::file_name(id), js::stringify(state));
    }

    fn record_usage(&self, model: &str, reported: &JValue) {
        self.store(
            "recordUsage",
            vec![("model", js::s(model)), ("reported", reported.clone())],
        );
    }

    fn prefill_sample(&self, _model: &str, _prompt: f64, _ms: f64) {}

    fn mtp_evidence(&self, _model: &str, _drafted: f64, _accepted: f64) {}
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
        clean_search_args: &|_, a| a.to_string(),
        announces_tool_action: &|_| false,
        announced_note: "",
    };
    let ended = block_on(native::run(&script, &process, &helpers, &prepared));
    // The executor's turn guard gives the lease back when the turn ends (Node: on finish/close).
    script.store("lease.release", vec![("lease", js::s("qa-lease-1"))]);
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
        assert_eq!(p.model, "qa-model");
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
        let (script, ended) = replay(&f, c);
        let JValue::Arr(events) = c.get("events") else {
            panic!()
        };
        let before = c.get("handoff").get("before").as_num().unwrap() as usize;
        let want: Vec<String> = events[before..].iter().map(js::stringify).collect();
        let got = script.events.lock().unwrap().clone();
        assert_eq!(got, want, "{name}: events after the cut");
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
        let left = matches!(ended, Ended::Left);
        if !left {
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
    assert!(ran.len() >= 15, "{ran:?}");
}
