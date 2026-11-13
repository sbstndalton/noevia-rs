//! Differential test: `exchange-fixtures.json` (recorded from the frozen Python `_run_exchange` by
//! `generate_exchange_fixtures.py`) replayed through `Exchange::run` with the same fake store and
//! stub retriever, and two stub engines on 127.0.0.1 (companion and aux) answering the same
//! scripted responses: result or failure, events, every request to each engine, every store and
//! search call, and the session afterwards must match.
#![allow(clippy::indexing_slicing, clippy::panic)] // serde_json `Value[...]` on fixture data

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Response, StatusCode};
use chrono::NaiveDate;
use diary_answer::exchange::{Exchange, ExchangeError};
use diary_answer::model::{Client, Config};
use diary_core::assembler::{
    Assembler, Hit, Search, SearchFailed, SearchOutcome, Store, StoreError,
};
use diary_core::pipeline::{Failed, Journal, Pipeline};
use diary_core::retrieval_trace::RetrievalTrace;
use serde_json::{json, Value};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[derive(Default)]
struct Engine {
    responses: Vec<Value>,
    sent: Vec<Value>,
}

async fn serve(engine: Arc<Mutex<Engine>>) -> Result<String, Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = axum::Router::new().fallback(move |req: axum::http::Request<Body>| {
        let engine = engine.clone();
        async move {
            let bytes = axum::body::to_bytes(req.into_body(), 1 << 24)
                .await
                .unwrap_or_default();
            let mut e = engine.lock().unwrap_or_else(|p| p.into_inner());
            e.sent
                .push(serde_json::from_slice(&bytes).unwrap_or(Value::Null));
            let r = if e.responses.is_empty() {
                json!([500, "text/plain", "no more synthetic answers"])
            } else {
                e.responses.remove(0)
            };
            Response::builder()
                .status(
                    StatusCode::from_u16(r[0].as_u64().unwrap_or(500) as u16)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                )
                .header("content-type", r[1].as_str().unwrap_or(""))
                .body(Body::from(r[2].as_str().unwrap_or("").to_owned()))
                .unwrap_or_else(|_| Response::new(Body::empty()))
        }
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(format!("http://{addr}/v1"))
}

struct FakeStore {
    spec: Value,
    calls: Mutex<Vec<Value>>,
}

impl FakeStore {
    fn record(&self, v: Value) {
        self.calls.lock().unwrap_or_else(|p| p.into_inner()).push(v);
    }
    fn cut(text: &str, max: i64) -> String {
        diary_core::pyconv::slice(text, None, Some(max))
    }
}

impl Store for FakeStore {
    fn day_text(&self, day: NaiveDate, max_chars: i64) -> Result<String, StoreError> {
        let iso = day.format("%Y-%m-%d").to_string();
        self.record(json!(["day", iso, max_chars]));
        Ok(Self::cut(
            self.spec["days"][&iso].as_str().unwrap_or(""),
            max_chars,
        ))
    }
    fn standing_sections_text(&self, max_chars: i64) -> Result<String, StoreError> {
        self.record(json!(["standing", max_chars]));
        Ok(Self::cut(
            self.spec["standing"].as_str().unwrap_or(""),
            max_chars,
        ))
    }
    fn memory_text(&self) -> String {
        self.spec["memory"].as_str().unwrap_or("").to_owned()
    }
}

struct JournalView<'a>(&'a FakeStore);

impl Journal for JournalView<'_> {
    fn log_exchange(
        &mut self,
        day: &str,
        sub_header: &str,
        me: &str,
        claude: &str,
    ) -> Result<String, (bool, String)> {
        self.0
            .record(json!(["log_exchange", day, sub_header, me, claude]));
        if self.0.spec["log"] == json!("fail") {
            return Err((false, "synthetic write failure".into()));
        }
        Ok("qa-xid-0042".into())
    }
    fn index_enabled(&self) -> bool {
        self.0.spec["index_enabled"].as_bool().unwrap_or(false)
    }
    fn standing_sections_text(&mut self, max_chars: i64) -> Result<String, Failed> {
        Store::standing_sections_text(self.0, max_chars).map_err(|_| Failed)
    }
    fn update_standing_sections(
        &mut self,
        open: &Value,
        timeline: &Value,
        today: &str,
    ) -> Result<Option<String>, Failed> {
        self.0.record(json!(["update", open, timeline, today]));
        Ok(Some("qa-journal-7".into()))
    }
}

struct StubSearch {
    rows: Vec<Value>,
    calls: Mutex<Vec<Value>>,
}

impl Search for StubSearch {
    fn search(
        &self,
        query: &str,
        top_k: i64,
        min_score: f64,
        with_ids: bool,
    ) -> Result<SearchOutcome, SearchFailed> {
        self.calls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(json!([query, top_k, min_score, with_ids]));
        let hits: Vec<Hit> = self
            .rows
            .iter()
            .filter(|r| r["score"].as_f64().is_some_and(|s| s >= min_score))
            .take(usize::try_from(top_k.max(0)).unwrap_or(0))
            .map(|r| Hit {
                id: if with_ids { r["id"].as_i64() } else { None },
                day: r["day"].as_str().unwrap_or("").to_owned(),
                header: r["header"].as_str().unwrap_or("").to_owned(),
                text: r["text"].as_str().unwrap_or("").to_owned(),
                score: r["score"].as_f64().unwrap_or(0.0),
            })
            .collect();
        Ok(SearchOutcome { hits, status: None })
    }
}

fn client(base: &str, model: &str) -> Result<Client, Box<dyn std::error::Error>> {
    Ok(Client::new(Config {
        base_url: base.to_owned(),
        chat_model: model.into(),
        max_retries: 1,
        retry_backoff_s: 0.0,
        ..Config::default()
    })?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exchanges_match_python() -> TestResult {
    let fx: Value = serde_json::from_str(include_str!("exchange-fixtures.json"))?;
    let pipeline = Pipeline {
        templates: fx["templates"].as_object().cloned().ok_or("templates")?,
    };
    let prompt = fx["system_prompt"].as_str().ok_or("prompt")?.to_owned();
    let main_engine = Arc::new(Mutex::new(Engine::default()));
    let aux_engine = Arc::new(Mutex::new(Engine::default()));
    let main_base = serve(main_engine.clone()).await?;
    let aux_base = serve(aux_engine.clone()).await?;
    let cfg = json!({"retrieval": {}, "context": {}});
    let day = NaiveDate::from_ymd_opt(2031, 3, 20).ok_or("day")?;
    let cases = fx["cases"].as_array().ok_or("cases")?;
    assert!(cases.len() >= 80);
    for case in cases {
        let name = case["name"].as_str().unwrap_or("?");
        for (engine, key) in [(&main_engine, "main"), (&aux_engine, "aux")] {
            let mut e = engine.lock().unwrap_or_else(|p| p.into_inner());
            e.responses = case[key].as_array().cloned().unwrap_or_default();
            e.sent.clear();
        }
        let store = FakeStore {
            spec: case["store"].clone(),
            calls: Mutex::new(Vec::new()),
        };
        let search = StubSearch {
            rows: case["rows"].as_array().cloned().unwrap_or_default(),
            calls: Mutex::new(Vec::new()),
        };
        let assembler = Assembler {
            store: &store,
            search: Some(&search),
            cfg: &cfg,
            system_prompt: prompt.clone(),
            local_reference: String::new(),
        };
        let main = client(&main_base, "qa-chat")?;
        let aux = client(&aux_base, "qa-aux")?;
        let exchange = Exchange {
            assembler: &assembler,
            main: &main,
            aux: &aux,
            pipeline: &pipeline,
            max_answer_tokens: case["max_answer_tokens"].as_i64().ok_or("cap")?,
        };
        let mut session: Vec<Value> = case["turns"].as_array().cloned().unwrap_or_default();
        let mut events: Vec<Value> = Vec::new();
        let mut trace = case["trace"]
            .as_bool()
            .unwrap_or(false)
            .then(RetrievalTrace::new);
        let streamed = case["streamed"].as_bool().unwrap_or(false);
        let mut sink = |e: Value| events.push(e);
        let mut journal = JournalView(&store);
        let out = exchange
            .run(
                &mut journal,
                case["message"].as_str().ok_or("message")?,
                &mut session,
                day,
                case["extra_context"].as_str().unwrap_or(""),
                if streamed { Some(&mut sink) } else { None },
                trace.as_mut(),
            )
            .await;
        let expected = &case["expected"];
        match out {
            Ok(result) => {
                assert!(
                    expected["error"].is_null(),
                    "{name}: Python raised {}",
                    expected["error"]
                );
                assert_eq!(Value::Object(result), expected["result"], "{name}: result");
            }
            Err(e) => {
                let class = match &e {
                    ExchangeError::Model(m) => m.python_class(),
                    ExchangeError::Timeout { .. } => "ExchangeTimeout",
                    ExchangeError::NativeWriteUnavailable => "NativeWriteUnavailable",
                    ExchangeError::Context(_) => "ContextError",
                };
                assert_eq!(json!(class), expected["error"], "{name}: {e:?}");
            }
        }
        assert_eq!(json!(events), expected["events"], "{name}: events");
        let main_sent = main_engine
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sent
            .clone();
        let aux_sent = aux_engine
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sent
            .clone();
        assert_eq!(
            json!(main_sent),
            expected["main_sent"],
            "{name}: companion requests"
        );
        assert_eq!(
            json!(aux_sent),
            expected["aux_sent"],
            "{name}: aux requests"
        );
        assert_eq!(
            json!(*store.calls.lock().unwrap_or_else(|p| p.into_inner())),
            expected["store_calls"],
            "{name}: store calls"
        );
        assert_eq!(
            json!(*search.calls.lock().unwrap_or_else(|p| p.into_inner())),
            expected["search_calls"],
            "{name}: search calls"
        );
        assert_eq!(json!(session), expected["session"], "{name}: session");
    }
    Ok(())
}
