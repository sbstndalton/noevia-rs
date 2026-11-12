#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::expect_used
)]
use chat_mirror_sched::journal::{Change, ChangeSink, JournalRead, JournalSource, ReadError};
use chat_mirror_sched::{BoxFut, BrainExecutor, Log, MirrorExecutor, MirrorOutcome};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

pub const U1: &str = "11111111-2222-4333-8444-555555555555";
pub const U2: &str = "21111111-2222-4333-8444-555555555555";
pub const START: &str = "0123456789abcdef0123456789abcdef";

#[derive(Default)]
pub struct Lines(pub Mutex<Vec<String>>);
impl Log for Lines {
    fn line(&self, text: &str) {
        self.0.lock().unwrap().push(text.to_string());
    }
}
impl Lines {
    pub fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// A scripted journal per user.
#[derive(Default)]
pub struct Fake {
    pub journals: Mutex<HashMap<String, Journal>>,
    pub reads: Mutex<Vec<(String, u64)>>,
    pub fail: Mutex<HashMap<String, VecDeque<ReadError>>>,
    pub stamps: Mutex<HashMap<String, String>>,
}

#[derive(Clone)]
pub struct Journal {
    pub entries: Vec<Value>,
    pub start: String,
}

impl Fake {
    pub fn new() -> Arc<Fake> {
        Arc::new(Fake::default())
    }
    pub fn set(&self, user: &str, entries: Vec<Value>) {
        self.journals.lock().unwrap().insert(
            user.to_string(),
            Journal {
                entries,
                start: START.to_string(),
            },
        );
        let n = self.stamps.lock().unwrap().len();
        self.stamps.lock().unwrap().insert(
            user.to_string(),
            format!("s{n}-{}", self.reads.lock().unwrap().len()),
        );
    }
    pub fn push(&self, user: &str, e: Value) {
        let mut g = self.journals.lock().unwrap();
        let j = g.entry(user.to_string()).or_insert(Journal {
            entries: vec![],
            start: START.to_string(),
        });
        j.entries.push(e);
        drop(g);
        let mut st = self.stamps.lock().unwrap();
        let next = st
            .get(user)
            .map(|s| format!("{s}+"))
            .unwrap_or_else(|| "s+".into());
        st.insert(user.to_string(), next);
    }
    pub fn read_count(&self, user: &str) -> usize {
        self.reads
            .lock()
            .unwrap()
            .iter()
            .filter(|(u, _)| u == user)
            .count()
    }
}

impl JournalSource for Fake {
    fn read(&self, user: &str, after: u64) -> BoxFut<Result<JournalRead, ReadError>> {
        self.reads.lock().unwrap().push((user.to_string(), after));
        if let Some(e) = self
            .fail
            .lock()
            .unwrap()
            .get_mut(user)
            .and_then(VecDeque::pop_front)
        {
            return Box::pin(async move { Err(e) });
        }
        let j = self.journals.lock().unwrap().get(user).cloned();
        Box::pin(async move {
            let Some(j) = j else {
                return Err(ReadError::Gone);
            };
            let all: Vec<Value> = j.entries.clone();
            let oldest = all.first().and_then(|e| e["seq"].as_u64());
            let latest = all.last().and_then(|e| e["seq"].as_u64());
            let newer: Vec<Value> = all
                .into_iter()
                .filter(|e| e["seq"].as_u64().unwrap_or(0) > after)
                .collect();
            let more = newer.len() > 1000;
            Ok(JournalRead {
                entries: newer.into_iter().take(1000).collect(),
                oldest_seq: oldest,
                latest_seq: latest,
                more,
                start: j.start,
            })
        })
    }
    fn stamp(&self, user: &str) -> Option<String> {
        self.stamps.lock().unwrap().get(user).cloned()
    }
}

pub fn entry(seq: u64, chat: &str, rev: &str, op: &str, origin: &str) -> Value {
    json!({"seq": seq, "chatId": chat, "revision": rev, "op": op, "origin": origin, "start": START, "at": 1})
}
pub fn save(seq: u64, chat: &str, rev: &str) -> Value {
    entry(seq, chat, rev, "write", "history")
}

#[derive(Default)]
pub struct Sink {
    pub changed: Mutex<Vec<(String, String, Change)>>,
    pub rescans: Mutex<Vec<String>>,
}
impl ChangeSink for Sink {
    fn changed(&self, user: &str, chat: &str, change: &Change) {
        self.changed
            .lock()
            .unwrap()
            .push((user.into(), chat.into(), change.clone()));
    }
    fn rescanned(&self, user: &str) {
        self.rescans.lock().unwrap().push(user.into());
    }
}
impl Sink {
    pub fn chats(&self) -> Vec<(String, bool)> {
        self.changed
            .lock()
            .unwrap()
            .iter()
            .map(|(_, c, ch)| (c.clone(), ch.brain))
            .collect()
    }
    pub fn take(&self) -> Vec<(String, bool)> {
        let v = self.chats();
        self.changed.lock().unwrap().clear();
        v
    }
}

/// A mirror executor that records its passes and answers from a script (default: Done).
#[derive(Default)]
pub struct MirrorFake {
    pub calls: Mutex<Vec<(String, Vec<String>, tokio::time::Instant)>>,
    pub script: Mutex<VecDeque<Step>>,
}
pub enum Step {
    Out(MirrorOutcome),
    Panic,
    /// Hold the pass until released.
    Hold(Arc<tokio::sync::Notify>),
}
impl MirrorFake {
    pub fn new() -> Arc<MirrorFake> {
        Arc::new(MirrorFake::default())
    }
    pub fn then(&self, s: Step) {
        self.script.lock().unwrap().push_back(s);
    }
    pub fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
    pub fn passes(&self, user: &str) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(u, _, _)| u == user)
            .map(|(_, d, _)| {
                let mut d = d.clone();
                d.sort();
                d
            })
            .collect()
    }
}
impl MirrorExecutor for MirrorFake {
    fn sync(&self, user: &str, dirty: Vec<String>) -> BoxFut<MirrorOutcome> {
        self.calls
            .lock()
            .unwrap()
            .push((user.to_string(), dirty, tokio::time::Instant::now()));
        let step = self.script.lock().unwrap().pop_front();
        Box::pin(async move {
            match step {
                None => MirrorOutcome::Done,
                Some(Step::Out(o)) => o,
                Some(Step::Panic) => panic!("executor panicked"),
                Some(Step::Hold(n)) => {
                    n.notified().await;
                    MirrorOutcome::Done
                }
            }
        })
    }
}

#[derive(Default)]
pub struct BrainFake {
    pub calls: Mutex<Vec<(String, String)>>,
    pub script: Mutex<VecDeque<String>>,
    pub running: Arc<Mutex<usize>>,
    pub peak: Arc<Mutex<usize>>,
    pub work: Mutex<std::time::Duration>,
}
impl BrainFake {
    pub fn new() -> Arc<BrainFake> {
        Arc::new(BrainFake::default())
    }
    pub fn then(&self, reason: &str) {
        self.script.lock().unwrap().push_back(reason.to_string());
    }
    pub fn peak(&self) -> usize {
        *self.peak.lock().unwrap()
    }
    pub fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}
impl BrainExecutor for BrainFake {
    fn run(&self, user: &str, chat: &str) -> BoxFut<String> {
        self.calls.lock().unwrap().push((user.into(), chat.into()));
        let reason = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "current".into());
        let work = *self.work.lock().unwrap();
        let (r, p) = (Arc::clone(&self.running), Arc::clone(&self.peak));
        Box::pin(async move {
            {
                let mut n = r.lock().unwrap();
                *n += 1;
                let mut peak = p.lock().unwrap();
                *peak = (*peak).max(*n);
            }
            if work > std::time::Duration::ZERO {
                tokio::time::sleep(work).await;
            }
            *r.lock().unwrap() -= 1;
            if reason == "panic" {
                panic!("executor panicked");
            }
            reason
        })
    }
}

/// Lets spawned tasks run to their next await without moving the clock.
pub async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

/// Moves the paused clock by `d` and lets everything that became due run.
pub async fn advance(d: std::time::Duration) {
    settle().await;
    tokio::time::advance(d).await;
    settle().await;
}
