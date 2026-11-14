//! The artifact check over shared state through `handle`: the lock is held only to start and to
//! finish, so a request that arrives while a check runs (here: from inside that check's fetch)
//! joins it, a third distinct check is refused while two run, and a run that panics does not stay
//! counted as running.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::JValue;
use model_pull::artifact::{self, Artifacts, FetchError, Fetched, Fetcher};
use model_pull::manager::{Config, Kind, Reply, Request, Thrown};
use model_pull::{handle, Call, Host, Outcome};
use std::sync::Mutex;

fn config() -> Config {
    Config {
        kind: Kind::Lemonade,
        base_url: "http://qa-manager.invalid".into(),
        api_key: None,
    }
}

fn call(repo: &str) -> String {
    format!("repo=qa-org/{repo}&variant=Q4_K_M")
}

/// Answers every variants call with one variant, and every revision request with a 404 (a
/// quick `unknown`); inside a fetch it makes the nested requests of its plan.
struct Nested<'a> {
    artifacts: &'a Mutex<Artifacts>,
    nested: Vec<(String, Vec<String>)>,
    seen: Vec<(String, Outcome)>,
    panic_in_fetch: bool,
}

impl Fetcher for Nested<'_> {
    fn fetch(&mut self, _: &artifact::Fetch) -> Result<Fetched, FetchError> {
        assert!(
            self.artifacts.try_lock().is_ok(),
            "the lock is not held while a check fetches"
        );
        if self.panic_in_fetch {
            panic!("qa: the fetch panicked");
        }
        for (repo, inner) in std::mem::take(&mut self.nested) {
            let mut host = Nested {
                artifacts: self.artifacts,
                nested: vec![],
                seen: vec![],
                panic_in_fetch: false,
            };
            if let Some(next) = inner.first() {
                host.nested.push((next.clone(), inner[1..].to_vec()));
            }
            let query = call(&repo);
            let out = handle(&config(), self.artifacts, get(&query), &mut host);
            self.seen.push((repo, out));
            self.seen.extend(host.seen);
        }
        Ok(Fetched {
            status: 404,
            content_range: None,
            body: Ok(Vec::new()),
        })
    }

    fn now_ms(&self) -> i64 {
        1_794_744_000_000
    }
}

impl Host for Nested<'_> {
    fn invalidate_model_reads(&mut self) {}
    fn clear_model_scan(&mut self) {}
    fn send(&mut self, _: &Request) -> Result<Reply, Thrown> {
        Ok(Reply {
            ok: true,
            status: 200,
            body: JValue::obj([(
                "variants",
                JValue::Arr(vec![JValue::obj([
                    ("name", JValue::from("Q4_K_M")),
                    ("files", JValue::Arr(vec![JValue::from("qa.gguf")])),
                ])]),
            )]),
        })
    }
    fn enter(&mut self) -> Result<(), Thrown> {
        Ok(())
    }
    fn leave(&mut self) {}
    fn downloads_requested(&mut self, _: &JValue) {}
    fn downloads_rejected(&mut self, _: &JValue) {}
}

fn get(query: &str) -> Call<'_> {
    Call {
        method: "GET",
        path: "/api/models/mtp-artifact",
        query,
        admin: false,
        body: None,
    }
}

fn reason(out: &Outcome) -> &str {
    match out {
        Outcome::Answer { status: 200, body } => body.get("reason").as_str().unwrap(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_running_check_is_joined_and_a_third_is_refused() {
    let artifacts = Mutex::new(Artifacts::new());
    let mut host = Nested {
        artifacts: &artifacts,
        // While qa-one runs: qa-one again (joins), then qa-two (runs), and while that runs, qa-three.
        nested: vec![
            ("qa-one".into(), vec![]),
            ("qa-two".into(), vec!["qa-three".into()]),
        ],
        seen: vec![],
        panic_in_fetch: false,
    };
    let query = call("qa-one");
    let out = handle(&config(), &artifacts, get(&query), &mut host);
    assert_eq!(reason(&out), artifact::REVISION_UNAVAILABLE);
    let seen: Vec<&str> = host.seen.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(seen, ["qa-one", "qa-two", "qa-three"]);
    match &host.seen[0].1 {
        Outcome::Join(key) => assert_eq!(key.repo, "qa-org/qa-one"),
        other => panic!("{other:?}"),
    }
    assert_eq!(reason(&host.seen[1].1), artifact::REVISION_UNAVAILABLE);
    assert_eq!(reason(&host.seen[2].1), artifact::BUSY);
    assert!(artifacts.lock().unwrap().running().is_empty());
}

#[test]
fn a_run_that_panics_is_abandoned() {
    let artifacts = Mutex::new(Artifacts::new());
    let query = call("qa-panics");
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut host = Nested {
            artifacts: &artifacts,
            nested: vec![],
            seen: vec![],
            panic_in_fetch: true,
        };
        handle(&config(), &artifacts, get(&query), &mut host)
    }));
    assert!(panicked.is_err());
    assert!(artifacts.lock().unwrap().running().is_empty());
    let mut host = Nested {
        artifacts: &artifacts,
        nested: vec![],
        seen: vec![],
        panic_in_fetch: false,
    };
    let out = handle(&config(), &artifacts, get(&query), &mut host);
    assert_eq!(reason(&out), artifact::REVISION_UNAVAILABLE);
}
