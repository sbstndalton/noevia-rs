use super::*;

fn s(v: &str) -> JValue {
    JValue::Str(v.to_owned())
}

#[test]
fn plaintext_is_a_non_empty_string_that_is_not_an_envelope() {
    assert!(is_plaintext_key(&s("synthetic-plain-key")));
    assert!(is_plaintext_key(&s("enc:")));
    assert!(is_plaintext_key(&s("enc:v")));
    assert!(is_plaintext_key(&s("enc:v:abc")), "no digit");
    assert!(is_plaintext_key(&s("enc:vx1:abc")));
    assert!(is_plaintext_key(&s("enc:v1")), "no colon after the digits");
    assert!(is_plaintext_key(&s(" enc:v1:abc")), "anchored at the start");
    assert!(is_plaintext_key(&s("ENC:v1:abc")), "case-sensitive");
    assert!(
        is_plaintext_key(&s("enc:v١:abc")),
        "\\d is ASCII only in JS"
    );
    assert!(!is_plaintext_key(&s("enc:v1:")));
    assert!(
        !is_plaintext_key(&s("enc:v2:AAAA")),
        "a bound envelope is left alone (L5)"
    );
    assert!(!is_plaintext_key(&s("enc:v10:x")));
    assert!(!is_plaintext_key(&s("")));
    assert!(!is_plaintext_key(&JValue::Num(12345.0)));
    assert!(!is_plaintext_key(&JValue::Null));
    assert!(!is_plaintext_key(&JValue::Bool(true)));
    assert!(!is_plaintext_key(&JValue::Undefined));
    assert!(!is_plaintext_key(&JValue::Arr(vec![s("x")])));
}

#[test]
fn the_load_reports_plaintext_only_where_cores_load_does_not_throw() {
    let yes = |t: &str| assert!(needs_key_seal(Some(t)), "{t}");
    let no = |t: &str| assert!(!needs_key_seal(Some(t)), "{t}");
    yes(r#"{"providers":[{"id":"p","apiKey":"synthetic-plain-key"}]}"#);
    yes(r#"{"providers":[{"id":"a","apiKey":""},{"id":"b","apiKey":"synthetic-plain-key"}]}"#);
    yes(r#"{"providers":[[],{"apiKey":"k"}]}"#);
    yes(r#"{"providers":[{"apiKey":"enc:v1:x"},{"apiKey":"k"}],"other":1}"#);
    // A duplicate key: JSON.parse keeps the last value.
    yes(r#"{"providers":[{"apiKey":"enc:v1:x","apiKey":"k"}]}"#);
    no(r#"{"providers":[{"apiKey":"k","apiKey":"enc:v1:x"}]}"#);
    no(
        r#"{"providers":[{"apiKey":"enc:v1:x"},{"apiKey":"enc:v2:y"},{"apiKey":12345},{"apiKey":""}]}"#,
    );
    no(r#"{"providers":[]}"#);
    no(r#"{}"#);
    no(r#"{"providers":null}"#);
    no(r#"{"providers":0}"#);
    no(r#"{"providers":""}"#);
    // Not iterable, or a row core's openProviderKey cannot assign on: the load throws, fail closed.
    no(r#"{"providers":{"0":{"apiKey":"k"}}}"#);
    no(r#"{"providers":"k"}"#);
    no(r#"{"providers":true}"#);
    no(r#"{"providers":7}"#);
    no(r#"{"providers":[null,{"apiKey":"k"}]}"#);
    no(r#"{"providers":[{"apiKey":"k"},null]}"#);
    no(r#"{"providers":[{"apiKey":"k"},5]}"#);
    no(r#"{"providers":[{"apiKey":"k"},"row"]}"#);
    no(r#"{"providers":[{"apiKey":"k"},false]}"#);
    no("null");
    no("[]");
    no(r#"[{"apiKey":"k"}]"#);
    no(r#""text""#);
    no("5");
    no("not json");
    no("");
    no("\u{feff}{\"providers\":[{\"apiKey\":\"k\"}]}");
    assert!(!needs_key_seal(None));
}

struct Stub;
impl Sealer for Stub {
    fn seal(&self, plaintext: &[u8]) -> Result<String, Failure> {
        // The recorder's stub: `enc:v1:` then the plaintext's hex (never real ciphertext).
        let mut out = String::from("enc:v1:stub-");
        for b in plaintext {
            out.push_str(&format!("{b:02x}"));
        }
        Ok(out)
    }
}

struct Refusing;
impl Sealer for Refusing {
    fn seal(&self, _: &[u8]) -> Result<String, Failure> {
        Err(Failure::Seal)
    }
}

fn written(p: Result<Plan, Failure>) -> (String, usize) {
    match p {
        Ok(Plan::Write { text, sealed }) => (text, sealed),
        other => panic!("expected a write, got {other:?}"),
    }
}

#[test]
fn a_plan_seals_every_plaintext_row_and_writes_the_doc_as_json_stringify_pretty() {
    let text = r#"{"providers":[{"id":"plain","apiKey":"ab"},{"id":"sealed","apiKey":"enc:v1:x"},{"id":"none","apiKey":""},{"id":"n","apiKey":5},null,"row",[]],"extra":{"1":true,"b":1.50,"0":null}}"#;
    let (out, sealed) = written(plan(Some(text), Some(&Stub)));
    assert_eq!(sealed, 1);
    let want = "{\n  \"providers\": [\n    {\n      \"id\": \"plain\",\n      \"apiKey\": \"enc:v1:stub-6162\"\n    },\n    {\n      \"id\": \"sealed\",\n      \"apiKey\": \"enc:v1:x\"\n    },\n    {\n      \"id\": \"none\",\n      \"apiKey\": \"\"\n    },\n    {\n      \"id\": \"n\",\n      \"apiKey\": 5\n    },\n    null,\n    \"row\",\n    []\n  ],\n  \"extra\": {\n    \"0\": null,\n    \"1\": true,\n    \"b\": 1.5\n  }\n}";
    assert_eq!(out, want);
}

#[test]
fn a_plan_writes_nothing_where_core_returns() {
    assert_eq!(plan(None, Some(&Stub)), Ok(Plan::NoFile));
    assert_eq!(
        plan(Some("{"), Some(&Stub)),
        Ok(Plan::NotJson(Unparsed::Invalid))
    );
    assert_eq!(
        plan(Some(""), Some(&Stub)),
        Ok(Plan::NotJson(Unparsed::Invalid))
    );
    for t in [
        "null",
        "0",
        "false",
        "\"\"",
        "\"s\"",
        "[]",
        r#"[{"apiKey":"k"}]"#,
        "{}",
        r#"{"providers":null}"#,
        r#"{"providers":{"0":{"apiKey":"k"}}}"#,
        r#"{"providers":"k"}"#,
    ] {
        assert_eq!(plan(Some(t), Some(&Stub)), Ok(Plan::NoRows), "{t}");
    }
    for t in [
        r#"{"providers":[]}"#,
        r#"{"providers":[{"apiKey":"enc:v2:y"},{"apiKey":7},{"apiKey":""},{"id":"x"},null,3,"k",[]]}"#,
    ] {
        assert_eq!(plan(Some(t), Some(&Stub)), Ok(Plan::NothingToSeal), "{t}");
    }
}

#[test]
fn a_row_that_is_null_or_a_primitive_does_not_stop_the_seal_of_the_others() {
    // Core's migration skips them (`row &&`, `(5).apiKey` is undefined), unlike its load.
    let (out, sealed) = written(plan(
        Some(r#"{"providers":[null,5,"x",{"apiKey":"k"}]}"#),
        Some(&Stub),
    ));
    assert_eq!(sealed, 1);
    assert!(out.contains("\"apiKey\": \"enc:v1:stub-6b\""), "{out}");
}

#[test]
fn without_a_sealer_nothing_is_written() {
    assert_eq!(
        plan(Some(r#"{"providers":[{"apiKey":"k"}]}"#), None),
        Ok(Plan::NoSealer)
    );
    assert_eq!(
        plan(Some(r#"{"providers":[{"apiKey":""}]}"#), None),
        Ok(Plan::NothingToSeal)
    );
}

#[test]
fn a_sealer_error_ends_the_plan_with_a_code_only() {
    let e = plan(
        Some(r#"{"providers":[{"apiKey":"synthetic-plain-key"}]}"#),
        Some(&Refusing),
    );
    assert_eq!(e, Err(Failure::Seal));
    let warning = Outcome::Failed(Failure::Seal).warning().unwrap_or_default();
    assert_eq!(
        warning,
        "[providers] could not seal legacy provider keys through the project store: seal_failed"
    );
    assert!(!warning.contains("synthetic"));
}

#[test]
fn lone_surrogates_survive_the_rewrite_and_reach_the_sealer_as_node_utf8() {
    // A lone surrogate in another field is written back as JSON.stringify writes it.
    let (out, _) = written(plan(
        Some(r#"{"providers":[{"label":"a\ud83d","apiKey":"k\udc00"}]}"#),
        Some(&Stub),
    ));
    assert!(out.contains(r#""label": "a\ud83d""#), "{out}");
    // `k` then U+FFFD (ef bf bd), as Buffer.from(value, 'utf8') encodes the lone surrogate.
    assert!(out.contains("enc:v1:stub-6befbfbd"), "{out}");
    assert_eq!(node_utf8("é😀"), "é😀".as_bytes());
}

#[test]
fn numbers_and_key_order_are_rewritten_as_node_does() {
    let (out, _) = written(plan(
        Some(r#"{"z":1e2,"providers":[{"apiKey":"k","10":0,"2":-0}],"a":0.1}"#),
        Some(&Stub),
    ));
    let want = "{\n  \"z\": 100,\n  \"providers\": [\n    {\n      \"2\": 0,\n      \"10\": 0,\n      \"apiKey\": \"enc:v1:stub-6b\"\n    }\n  ],\n  \"a\": 0.1\n}";
    assert_eq!(out, want);
}

#[test]
fn the_gate_is_single_flight_and_quiet_for_thirty_seconds_after_a_failure() {
    let mut g = Gate::new();
    assert_eq!(g.begin(1_000), Admit::Run, "sealFailedAt starts at 0");
    assert_eq!(g.begin(1_001), Admit::Join);
    g.end(&Outcome::Failed(Failure::StoreRead), 2_000);
    assert!(!g.running());
    assert_eq!(g.begin(2_000), Admit::Quiet);
    assert_eq!(g.begin(31_999), Admit::Quiet);
    assert_eq!(g.begin(1_500), Admit::Quiet, "a clock that went back");
    assert_eq!(g.begin(32_000), Admit::Run);
    g.end(&Outcome::Conflicted, 40_000);
    assert_eq!(
        g.begin(69_999),
        Admit::Quiet,
        "a CAS that never landed quiets too"
    );
    assert_eq!(g.begin(70_000), Admit::Run);
    g.end(
        &Outcome::Sealed {
            sealed: 1,
            attempts: 1,
        },
        70_001,
    );
    assert_eq!(g.begin(70_002), Admit::Run, "success does not quiet");
    g.end(
        &Outcome::Nothing {
            plan: Plan::NothingToSeal,
            attempts: 1,
        },
        70_003,
    );
    assert_eq!(g.begin(70_004), Admit::Run);
}

#[test]
fn the_gate_joins_a_running_seal_even_inside_the_quiet_window() {
    let mut g = Gate::new();
    assert_eq!(g.begin(0), Admit::Run);
    g.end(&Outcome::Failed(Failure::Seal), 10);
    assert_eq!(g.begin(40_000), Admit::Run);
    // Still running; a failure recorded earlier never turns a join into Quiet.
    assert_eq!(g.begin(40_001), Admit::Join);
}

mod driver {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    /// The futures here are ready at once; a scripted store never waits.
    pub(crate) fn block_on<F: std::future::Future>(f: F) -> F::Output {
        let mut f = pin!(f);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }

    /// A front store over one file: a version counter, a scripted read failure, and
    /// "the front writes meanwhile" before chosen writes.
    pub(crate) struct Scripted {
        pub(crate) text: RefCell<Option<String>>,
        pub(crate) version: RefCell<u64>,
        pub(crate) fail_read: bool,
        pub(crate) fail_write: bool,
        /// For each write, in order: text the front writes first (a new version), if any.
        pub(crate) races: RefCell<VecDeque<Option<String>>>,
        pub(crate) reads: RefCell<usize>,
        pub(crate) writes: RefCell<Vec<String>>,
    }

    impl Scripted {
        pub(crate) fn new(text: Option<&str>) -> Self {
            Scripted {
                text: RefCell::new(text.map(str::to_owned)),
                version: RefCell::new(1),
                fail_read: false,
                fail_write: false,
                races: RefCell::new(VecDeque::new()),
                reads: RefCell::new(0),
                writes: RefCell::new(Vec::new()),
            }
        }
    }

    impl Store for Scripted {
        async fn read(&self, user: &str, file: &str) -> Result<Read, Failure> {
            assert_eq!((user, file), ("u", FILE));
            *self.reads.borrow_mut() += 1;
            if self.fail_read {
                return Err(Failure::StoreRead);
            }
            Ok(Read {
                version: self.version.borrow().to_string(),
                text: self.text.borrow().clone(),
            })
        }
        async fn write(
            &self,
            _: &str,
            _: &str,
            version: &str,
            text: &str,
        ) -> Result<bool, Failure> {
            if self.fail_write {
                return Err(Failure::StoreWrite);
            }
            if let Some(Some(front)) = self.races.borrow_mut().pop_front() {
                *self.text.borrow_mut() = Some(front);
                *self.version.borrow_mut() += 1;
            }
            if version != self.version.borrow().to_string() {
                return Ok(false);
            }
            self.writes.borrow_mut().push(text.to_owned());
            *self.text.borrow_mut() = Some(text.to_owned());
            *self.version.borrow_mut() += 1;
            Ok(true)
        }
    }

    const PLAIN: &str = r#"{"providers":[{"id":"plain","apiKey":"k"}]}"#;

    #[test]
    fn one_write_then_nothing_left() {
        let store = Scripted::new(Some(PLAIN));
        let out = block_on(seal_legacy(&store, Some(&Stub), "u"));
        assert_eq!(
            out,
            Outcome::Sealed {
                sealed: 1,
                attempts: 1
            }
        );
        assert_eq!(store.writes.borrow().len(), 1);
        let now = store.text.borrow().clone();
        assert!(
            !needs_key_seal(now.as_deref()),
            "the load sees nothing left"
        );
        let again = block_on(seal_legacy(&store, Some(&Stub), "u"));
        assert_eq!(
            again,
            Outcome::Nothing {
                plan: Plan::NothingToSeal,
                attempts: 1
            }
        );
        assert_eq!(
            store.writes.borrow().len(),
            1,
            "a second pass writes nothing"
        );
    }

    #[test]
    fn a_front_write_meanwhile_is_merged_again_and_survives() {
        let store = Scripted::new(Some(PLAIN));
        store.races.borrow_mut().push_back(Some(
            r#"{"providers":[{"id":"plain","apiKey":"k"},{"id":"added-by-front","apiKey":""}]}"#
                .to_owned(),
        ));
        let out = block_on(seal_legacy(&store, Some(&Stub), "u"));
        assert_eq!(
            out,
            Outcome::Sealed {
                sealed: 1,
                attempts: 2
            }
        );
        let text = store.text.borrow().clone().unwrap_or_default();
        assert!(text.contains("added-by-front"), "{text}");
        assert!(text.contains("enc:v1:stub-6b"), "{text}");
        assert_eq!(*store.reads.borrow(), 2);
    }

    #[test]
    fn four_conflicts_end_conflicted() {
        let store = Scripted::new(Some(PLAIN));
        for _ in 0..MAX_ATTEMPTS {
            store.races.borrow_mut().push_back(Some(PLAIN.to_owned()));
        }
        let out = block_on(seal_legacy(&store, Some(&Stub), "u"));
        assert_eq!(out, Outcome::Conflicted);
        assert_eq!(*store.reads.borrow(), MAX_ATTEMPTS);
        assert!(store.writes.borrow().is_empty());
        assert_eq!(out.warning().as_deref(), Some(WARN_CONFLICT));
        assert!(out.quiets());
    }

    #[test]
    fn three_conflicts_then_a_landing_write() {
        let store = Scripted::new(Some(PLAIN));
        for _ in 0..MAX_ATTEMPTS - 1 {
            store.races.borrow_mut().push_back(Some(PLAIN.to_owned()));
        }
        let out = block_on(seal_legacy(&store, Some(&Stub), "u"));
        assert_eq!(
            out,
            Outcome::Sealed {
                sealed: 1,
                attempts: MAX_ATTEMPTS
            }
        );
    }

    #[test]
    fn the_front_removing_the_key_meanwhile_ends_with_nothing() {
        let store = Scripted::new(Some(PLAIN));
        store.races.borrow_mut().push_back(Some(
            r#"{"providers":[{"id":"plain","apiKey":"enc:v1:front"}]}"#.to_owned(),
        ));
        let out = block_on(seal_legacy(&store, Some(&Stub), "u"));
        assert_eq!(
            out,
            Outcome::Nothing {
                plan: Plan::NothingToSeal,
                attempts: 2
            }
        );
        assert!(store.writes.borrow().is_empty());
    }

    #[test]
    fn store_and_sealer_failures_are_one_attempt_and_quiet() {
        let mut store = Scripted::new(Some(PLAIN));
        store.fail_read = true;
        let out = block_on(seal_legacy(&store, Some(&Stub), "u"));
        assert_eq!(out, Outcome::Failed(Failure::StoreRead));
        assert_eq!(*store.reads.borrow(), 1);
        assert!(out.quiets());

        let mut store = Scripted::new(Some(PLAIN));
        store.fail_write = true;
        let out = block_on(seal_legacy(&store, Some(&Stub), "u"));
        assert_eq!(out, Outcome::Failed(Failure::StoreWrite));
        assert_eq!(*store.reads.borrow(), 1);

        let store = Scripted::new(Some(PLAIN));
        let out = block_on(seal_legacy(&store, Some(&Refusing), "u"));
        assert_eq!(out, Outcome::Failed(Failure::Seal));
        assert!(store.writes.borrow().is_empty());
    }

    #[test]
    fn no_file_and_unparseable_files_end_with_nothing() {
        let store = Scripted::new(None);
        assert_eq!(
            block_on(seal_legacy(&store, Some(&Stub), "u")),
            Outcome::Nothing {
                plan: Plan::NoFile,
                attempts: 1
            }
        );
        let store = Scripted::new(Some("{oops"));
        assert_eq!(
            block_on(seal_legacy(&store, Some(&Stub), "u")),
            Outcome::Nothing {
                plan: Plan::NotJson(Unparsed::Invalid),
                attempts: 1
            }
        );
    }
}
