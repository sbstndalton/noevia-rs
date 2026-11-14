//! A host whose steps can fail where core's cannot (the front's scan clear and download tracker
//! bridges): its answer is the request's, and nothing after the failed step runs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use js_json::JValue;
use model_pull::artifact::{self, Artifacts, FetchError, Fetched, Fetcher};
use model_pull::manager::{Config, Kind, Reply, Request, Thrown};
use model_pull::{handle, Answered, Call, Host, Outcome};
use std::sync::Mutex;

#[derive(Default)]
struct Failing {
    clear_fails: bool,
    requested_fails: bool,
    log: Vec<&'static str>,
}

impl Fetcher for Failing {
    fn fetch(&mut self, _: &artifact::Fetch) -> Result<Fetched, FetchError> {
        panic!("no fetch for a pull")
    }
    fn now_ms(&self) -> i64 {
        0
    }
}

impl Host for Failing {
    fn invalidate_model_reads(&mut self) {
        self.log.push("invalidate");
    }
    fn clear_model_scan(&mut self) -> Result<(), Answered> {
        self.log.push("clear");
        if self.clear_fails {
            return Err(Answered {
                status: 502,
                message: "qa: the clear failed".into(),
            });
        }
        Ok(())
    }
    fn send(&mut self, _: &Request) -> Result<Reply, Thrown> {
        self.log.push("send");
        Ok(Reply {
            ok: true,
            status: 200,
            body: JValue::Obj(Vec::new()),
        })
    }
    fn enter(&mut self) -> Result<(), Thrown> {
        self.log.push("enter");
        Ok(())
    }
    fn leave(&mut self) {
        self.log.push("leave");
    }
    fn downloads_requested(&mut self, _: &JValue) -> Result<(), Answered> {
        self.log.push("requested");
        if self.requested_fails {
            return Err(Answered {
                status: 503,
                message: "The download could not be recorded.".into(),
            });
        }
        Ok(())
    }
    fn downloads_rejected(&mut self, _: &JValue) {
        self.log.push("rejected");
    }
}

fn pull(host: &mut Failing) -> Outcome {
    let config = Config {
        kind: Kind::Llamacpp,
        base_url: "http://qa-router.invalid".into(),
        api_key: None,
    };
    let call = Call {
        method: "POST",
        path: "/api/models/pull",
        query: "",
        admin: true,
        body: Some(br#"{"checkpoint":"qa-org/qa-model:Q8_0"}"#),
    };
    handle(&config, &Mutex::new(Artifacts::new()), call, host)
}

fn error(status: u16, message: &str) -> Outcome {
    Outcome::Answer {
        status,
        body: JValue::obj([("error", JValue::from(message))]),
    }
}

#[test]
fn a_failed_scan_clear_is_the_answer_and_nothing_follows() {
    let mut host = Failing {
        clear_fails: true,
        ..Failing::default()
    };
    assert_eq!(pull(&mut host), error(502, "qa: the clear failed"));
    assert_eq!(host.log, ["invalidate", "clear", "invalidate"]);
}

#[test]
fn a_failed_download_record_leaves_the_gate_and_asks_no_router() {
    let mut host = Failing {
        requested_fails: true,
        ..Failing::default()
    };
    assert_eq!(
        pull(&mut host),
        error(503, "The download could not be recorded.")
    );
    assert_eq!(
        host.log,
        [
            "invalidate",
            "clear",
            "enter",
            "requested",
            "leave",
            "invalidate"
        ]
    );
    let mut fine = Failing::default();
    assert!(matches!(
        pull(&mut fine),
        Outcome::Answer { status: 200, .. }
    ));
    assert_eq!(
        fine.log,
        [
            "invalidate",
            "clear",
            "enter",
            "requested",
            "send",
            "leave",
            "invalidate"
        ]
    );
}
