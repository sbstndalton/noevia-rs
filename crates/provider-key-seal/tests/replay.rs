//! Replays core's recorded seal-on-load migration (tests/fixtures/node-seal.v1.json, written by
//! tests/record-node-seal.cjs from core's own createWorkspaceStore) through this crate: the load's
//! needsKeySeal(), every text written (byte for byte), the store reads, the file at the end, the
//! warning kinds, and a second migration started at once (quiet after a failure or conflict).
//!
//! Without the fixture the test passes with a note, unless PROVIDER_KEY_SEAL_ORACLE_REQUIRED=1
//! (CI, once the recording is committed).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use js_json::{wtf16, JValue};
use provider_key_seal::{
    needs_key_seal, seal_legacy, Admit, Failure, Gate, Outcome, Plan, Read, Sealer, Store,
};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/node-seal.v1.json"
);
const THROW: &str = "synthetic-throw";

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// The recorder's stub sealer: `enc:v1:stub-` + hex of the UTF-8 bytes; `throws` refuses
/// `synthetic-throw`.
struct Stub {
    throws: bool,
}

impl Sealer for Stub {
    fn seal(&self, plaintext: &[u8]) -> Result<String, Failure> {
        if self.throws && plaintext == THROW.as_bytes() {
            return Err(Failure::Seal);
        }
        let mut out = String::from("enc:v1:stub-");
        for b in plaintext {
            let _ = write!(out, "{b:02x}");
        }
        Ok(out)
    }
}

/// core's project-store stub: the version is the text's digest, so equal text is the same
/// version; here the text itself stands for it ("" for no file, as the stub's version is).
struct Recorded {
    text: RefCell<Option<String>>,
    races: RefCell<VecDeque<Option<String>>>,
    fail_read: bool,
    fail_write: bool,
    reads: RefCell<usize>,
    writes: RefCell<Vec<String>>,
}

fn version_of(text: &Option<String>) -> String {
    match text {
        Some(t) => format!("v:{t}"),
        None => String::new(),
    }
}

impl Store for Recorded {
    async fn read(&self, _: &str, _: &str) -> Result<Read, Failure> {
        *self.reads.borrow_mut() += 1;
        if self.fail_read {
            return Err(Failure::StoreRead);
        }
        let text = self.text.borrow().clone();
        Ok(Read {
            version: version_of(&text),
            text,
        })
    }

    async fn write(&self, _: &str, _: &str, version: &str, text: &str) -> Result<bool, Failure> {
        if self.fail_write {
            return Err(Failure::StoreWrite);
        }
        if let Some(Some(front)) = self.races.borrow_mut().pop_front() {
            *self.text.borrow_mut() = Some(front);
        }
        if version != version_of(&self.text.borrow()) {
            return Ok(false);
        }
        *self.text.borrow_mut() = Some(text.to_owned());
        self.writes.borrow_mut().push(text.to_owned());
        Ok(true)
    }
}

fn opt_str(v: &JValue) -> Option<String> {
    match v {
        JValue::Str(s) => Some(s.clone()),
        JValue::Null | JValue::Undefined => None,
        other => panic!("expected a string or null, got {other:?}"),
    }
}

fn strings(v: &JValue) -> Vec<Option<String>> {
    match v {
        JValue::Arr(items) => items.iter().map(opt_str).collect(),
        JValue::Null | JValue::Undefined => Vec::new(),
        other => panic!("expected a list, got {other:?}"),
    }
}

fn count(v: &JValue) -> usize {
    v.as_num().expect("a count") as usize
}

/// One migration through the gate at `now`, as core's sealLegacyProviders() call does.
fn migrate(
    gate: &mut Gate,
    store: &Recorded,
    sealer: Option<&dyn Sealer>,
    now: u64,
) -> Option<Outcome> {
    match gate.begin(now) {
        Admit::Run => {
            let outcome = block_on(seal_legacy(store, sealer, "u"));
            gate.end(&outcome, now);
            Some(outcome)
        }
        Admit::Quiet => None,
        Admit::Join => panic!("nothing runs concurrently in a replay"),
    }
}

fn kind(outcome: &Option<Outcome>) -> Option<&'static str> {
    match outcome {
        Some(Outcome::Conflicted) => Some("conflict"),
        Some(Outcome::Failed(_)) => Some("failed"),
        _ => None,
    }
}

#[test]
fn replays_cores_recorded_migration() {
    let required = std::env::var("PROVIDER_KEY_SEAL_ORACLE_REQUIRED").as_deref() == Ok("1");
    let Ok(text) = std::fs::read_to_string(FIXTURE) else {
        assert!(
            !required,
            "{FIXTURE} is missing; record it with tests/record-node-seal.cjs"
        );
        eprintln!("provider-key-seal: no recording at {FIXTURE}; replay skipped");
        return;
    };
    let doc = wtf16::parse(&text).expect("the recording parses");
    assert_eq!(doc.get("version").as_num(), Some(1.0));
    let JValue::Arr(cases) = doc.get("cases") else {
        panic!("no cases")
    };
    assert!(cases.len() >= 30, "{} cases", cases.len());
    let mut known = 0;
    for c in cases {
        let name = c.get("name").as_str().unwrap_or_default().to_owned();
        let file = opt_str(c.get("file"));
        let sealer_kind = c.get("sealer").as_str().unwrap_or_default();
        let stub = Stub {
            throws: sealer_kind == "throws",
        };
        let sealer: Option<&dyn Sealer> = if sealer_kind == "none" {
            None
        } else {
            Some(&stub)
        };
        let store = Recorded {
            text: RefCell::new(file.clone()),
            races: RefCell::new(strings(c.get("races")).into()),
            fail_read: c.get("failRead").truthy(),
            fail_write: c.get("failWrite").truthy(),
            reads: RefCell::new(0),
            writes: RefCell::new(Vec::new()),
        };
        let node_writes: Vec<String> = strings(c.get("writes")).into_iter().flatten().collect();

        // The known difference: without secrets core rewrites the file on every migration with
        // no key changed (noevia issue filed); the crate writes nothing.
        if sealer.is_none() {
            let mut gate = Gate::new();
            let first = migrate(&mut gate, &store, None, 0);
            assert!(
                matches!(
                    first,
                    Some(Outcome::Nothing {
                        plan: Plan::NoSealer | Plan::NothingToSeal,
                        ..
                    })
                ),
                "{name}: {first:?}"
            );
            assert!(store.writes.borrow().is_empty(), "{name}");
            for w in &node_writes {
                // Core's rewrite never changes a key: the doc reads the same.
                assert_eq!(
                    wtf16::parse(w).ok(),
                    file.as_deref().and_then(|f| wtf16::parse(f).ok()),
                    "{name}: core's no-secrets write changed the doc"
                );
            }
            known += 1;
            continue;
        }

        assert_eq!(
            needs_key_seal(file.as_deref()),
            c.get("needsKeySeal").truthy(),
            "{name}: needsKeySeal"
        );
        let mut gate = Gate::new();
        let first = migrate(&mut gate, &store, sealer, 0);
        let reads_first = *store.reads.borrow();
        let second = migrate(&mut gate, &store, sealer, 1);
        let reads_second = *store.reads.borrow() - reads_first;

        assert_eq!(*store.writes.borrow(), node_writes, "{name}: writes");
        assert_eq!(reads_first, count(c.get("reads")), "{name}: reads");
        assert_eq!(
            reads_second,
            count(c.get("readsSecond")),
            "{name}: reads of the second migration"
        );
        assert_eq!(
            *store.text.borrow(),
            opt_str(c.get("final")),
            "{name}: the file at the end"
        );
        let warnings: Vec<&str> = [kind(&first), kind(&second)]
            .into_iter()
            .flatten()
            .collect();
        let node_warnings: Vec<String> = strings(c.get("warning")).into_iter().flatten().collect();
        assert_eq!(warnings, node_warnings, "{name}: warnings");
    }
    assert!(known >= 2, "the no-secrets cases are recorded");
}
