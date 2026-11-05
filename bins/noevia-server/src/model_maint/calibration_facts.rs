//! The native calibration facts (core `llamacpp-calibration.cjs` `stateFacts`, `listingFacts`,
//! `preparationFacts`, `compareAndLaunch`, `cancellationFacts`, `compareAndAbort`, and
//! `llamacpp-manager.cjs` `conservativeFor` / `tuneIdentity`). UNWIRED: the routes still read Node's.
//!
//! The same facts with the same tokens' meaning: a token is a salted sha256 over the facts it was
//! issued for (Node's per-process HMAC), so a launch or an abort applies only to the exact job,
//! controller, listing row and prepared profile the route checked.

use super::calibration_worker::{id_of, visible_job, Plan, Refusal, Worker};
use super::evidence as ev;
use js_json::{wtf16, JValue};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

/// `/(?:^|[-_.])((?:UD-)?(?:IQ\d[\w]*|Q\d(?:_[\dKSMLX]+)*|F16|BF16|F32|MXFP4))(?:[-_.]|\.gguf$)/i`
/// (ASCII classes, as JavaScript reads `\d` and `\w`).
const QUANT: &str = r"(?i)(?:^|[-_.])((?:UD-)?(?:IQ[0-9][0-9A-Za-z_]*|Q[0-9](?:_[0-9KSMLX]+)*|F16|BF16|F32|MXFP4))(?:[-_.]|\.gguf$)";

/// The facts could not be read (Node's operation threw; the bridge answered 503).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unavailable;

fn to_j(v: &Value) -> JValue {
    wtf16::parse(&v.to_string()).unwrap_or(JValue::Null)
}

fn obj(pairs: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

fn stale() -> JValue {
    obj(vec![("stale", JValue::Bool(true))])
}

/// `modelPathFromArgs(args)`: the value after `--model` / `-m`, or `''`.
fn model_path_from_args(args: Option<&Value>) -> String {
    let Some(Value::Array(a)) = args else {
        return String::new();
    };
    a.iter()
        .position(|v| matches!(v.as_str(), Some("--model" | "-m")))
        .and_then(|i| a.get(i + 1))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// `identityValid(value)` (the same check the route makes on the bridge's answer).
pub fn identity_valid(v: &JValue) -> bool {
    let JValue::Obj(fields) = v else {
        return false;
    };
    let mut keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
    keys.sort_unstable();
    keys == ["arch", "artifact", "build", "hardware", "profile", "quant"]
        && ["arch", "quant", "hardware"]
            .iter()
            .all(|k| matches!(v.get(k), JValue::Null | JValue::Str(_)))
        && v.get("artifact").as_str().is_some_and(|s| !s.is_empty())
        && v.get("profile").as_str().is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        && matches!(
            v.get("build"),
            JValue::Null | JValue::Str(_) | JValue::Obj(_)
        )
}

/// `Math.round(MemTotal kB / 1048576)`, or `None` (unreadable or 0).
fn mem_total_gib() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: f64 = text
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))?
        .trim_start()
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    let g = (kb / 1_048_576.0 + 0.5).floor();
    (g != 0.0).then_some(g)
}

impl Worker {
    fn sign(&self, kind: &str, facts: &JValue) -> String {
        let text = wtf16::stringify(facts).unwrap_or_default();
        Sha256::digest(format!("{}:{kind}:{text}", self.salt).as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn object_id(n: u64) -> JValue {
        if n == 0 {
            JValue::Null
        } else {
            JValue::Num(n as f64)
        }
    }

    /// `launchToken()`: the job, its object, the cancel flag and the live controller.
    fn launch_token(&self) -> String {
        let facts = self.with(|i| {
            obj(vec![
                ("job", i.state.job().clone()),
                (
                    "jobObject",
                    Self::object_id(if i.state.job().is_nullish() {
                        0
                    } else {
                        i.job_object
                    }),
                ),
                ("cancelRequested", JValue::Bool(i.cancel_requested)),
                ("controller", Self::object_id(i.controller)),
            ])
        });
        self.sign("launch", &facts)
    }

    fn cancel_token(&self) -> String {
        let facts = self.with(|i| {
            obj(vec![
                ("job", i.state.job().clone()),
                (
                    "jobObject",
                    Self::object_id(if i.state.job().is_nullish() {
                        0
                    } else {
                        i.job_object
                    }),
                ),
                ("controller", Self::object_id(i.controller)),
                ("cancelRequested", JValue::Bool(i.cancel_requested)),
            ])
        });
        self.sign("cancel", &facts)
    }

    fn cancel_generation(&self) -> String {
        let facts = self.with(|i| {
            obj(vec![(
                "jobObject",
                Self::object_id(if i.state.job().is_nullish() {
                    0
                } else {
                    i.job_object
                }),
            )])
        });
        self.sign("cancel", &facts)
    }

    /// `stateFacts()`: `{ token, starting, job, history }`.
    pub fn state_facts(&self) -> JValue {
        let token = self.launch_token();
        self.with(|i| {
            obj(vec![
                ("token", JValue::Str(token)),
                ("starting", JValue::Bool(i.launching)),
                ("job", visible_job(i)),
                (
                    "history",
                    match i.state.get("history") {
                        JValue::Undefined => JValue::Null,
                        h => h.clone(),
                    },
                ),
            ])
        })
    }

    /// `listingFacts(model, token)`: `{ stale }`, `{ backend: false }`, or the row, whether it is a
    /// system model, and the row token.
    pub async fn listing_facts(&self, model: &str, token: &str) -> JValue {
        if token != self.launch_token() {
            return stale();
        }
        let Some(router) = self.maint.router.as_ref() else {
            return obj(vec![("backend", JValue::Bool(false))]);
        };
        let rows = match router.models(None).await {
            Ok(r) if r.ok => super::router::rows(&r.body).cloned(),
            _ => None,
        };
        let Some(rows) = rows else {
            return obj(vec![("backend", JValue::Bool(false))]);
        };
        if token != self.launch_token() {
            return stale();
        }
        let row = rows.iter().find(|m| id_of(m) == Some(model));
        let system = row.is_some_and(|r| {
            let path = model_path_from_args(r.get("status").and_then(|s| s.get("args")));
            let roles = crate::model_kind::roles_beside(self.maint.sizing.presets_path());
            crate::model_kind::is_system_model_at(
                &roles,
                model,
                (!path.is_empty()).then_some(path.as_str()),
            )
        });
        let row = row.map_or(JValue::Null, to_j);
        let row_token = self.sign(
            "row",
            &obj(vec![
                ("token", JValue::Str(token.to_owned())),
                ("model", JValue::Str(model.to_owned())),
                ("row", row.clone()),
                ("system", JValue::Bool(system)),
            ]),
        );
        obj(vec![
            ("backend", JValue::Bool(true)),
            ("row", row),
            ("system", JValue::Bool(system)),
            ("rowToken", JValue::Str(row_token)),
        ])
    }

    /// `presets.get(model)` and the file's revision; `Err` where Node's store throws.
    async fn profile(&self, model: &str) -> Result<(JValue, String), Unavailable> {
        let ini = self.maint.ini.clone().ok_or(Unavailable)?;
        let model = model.to_owned();
        tokio::task::spawn_blocking(move || {
            let p = ini.read().map_err(|_| Unavailable)?;
            let opts = |o: &[(&'static str, String)]| {
                JValue::Obj(
                    o.iter()
                        .map(|(k, v)| ((*k).to_owned(), JValue::Str(v.clone())))
                        .collect(),
                )
            };
            let section = p.span(&model);
            let profile = obj(vec![
                ("model", JValue::Str(model.clone())),
                ("revision", JValue::Str(p.revision.clone())),
                ("exists", JValue::Bool(section.is_some())),
                (
                    "options",
                    section.map_or(JValue::Obj(Vec::new()), |(_, _, o)| opts(o)),
                ),
                (
                    "defaults",
                    p.span("*")
                        .map_or(JValue::Obj(Vec::new()), |(_, _, o)| opts(o)),
                ),
                (
                    "fields",
                    JValue::Arr(
                        crate::model_sizing::Presets::field_names()
                            .into_iter()
                            .map(|f| JValue::Str(f.into()))
                            .collect(),
                    ),
                ),
            ]);
            Ok((profile, p.revision.clone()))
        })
        .await
        .map_err(|_| Unavailable)?
    }

    async fn revision_now(&self) -> Option<String> {
        let ini = self.maint.ini.clone()?;
        tokio::task::spawn_blocking(move || ini.snapshot().ok().map(|s| s.revision))
            .await
            .ok()
            .flatten()
    }

    /// llamacpp-manager.cjs `tuneIdentity(model)`: `None` where it rejects.
    pub async fn identity_for(&self, model: &str) -> Option<JValue> {
        let sizing = Arc::clone(&self.maint.sizing);
        let listing = sizing.listing().await;
        let m = model.to_owned();
        let s = Arc::clone(&sizing);
        let (arch, file) = tokio::task::spawn_blocking(move || s.identity_read(&m, &listing))
            .await
            .ok()?
            .ok()?;
        let base = file
            .as_ref()
            .and_then(|f| f.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let quant = regex::Regex::new(QUANT)
            .ok()
            .and_then(|r| {
                r.captures(&base)
                    .and_then(|c| c.get(1))
                    .map(|q| q.as_str().to_uppercase())
            })
            .map_or(JValue::Null, JValue::Str);
        let artifact = match file.clone() {
            Some(f) => tokio::task::spawn_blocking(move || ev::file_fingerprint(&f))
                .await
                .ok()
                .flatten()
                .map_or(JValue::Null, JValue::Str),
            None => JValue::Null,
        };
        let build = match self.maint.router.as_ref() {
            Some(r) => match r.get("/props", None, Duration::from_secs(8)).await {
                Ok(reply) => reply
                    .body
                    .as_ref()
                    .and_then(|b| b.get("build_info"))
                    .map(to_j)
                    .filter(JValue::truthy)
                    .unwrap_or(JValue::Null),
                Err(_) => JValue::Null,
            },
            None => JValue::Null,
        };
        let mem = mem_total_gib();
        let ini = self.maint.ini.clone()?;
        let m = model.to_owned();
        let sections = tokio::task::spawn_blocking(move || {
            let p = ini.read().ok()?;
            let text = |name: &str| {
                p.span(name)
                    .map(|(start, end, _)| p.lines().get(start..end).unwrap_or(&[]).join("\n"))
                    .unwrap_or_default()
            };
            Some((text("*"), text(&m)))
        })
        .await
        .ok()
        .flatten()?;
        let profile = ev::identity_hash(&JValue::Arr(vec![
            JValue::Str(sections.0),
            JValue::Str(sections.1),
        ]));
        let parts: Vec<String> = [
            mem.map(|g| format!("{}GiB", super::calibration::plain(g))),
            wtf16::stringify(&build),
        ]
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        .collect();
        let hardware = if parts.is_empty() {
            JValue::Null
        } else {
            JValue::Str(parts.join(" "))
        };
        Some(obj(vec![
            ("arch", arch),
            ("quant", quant),
            ("artifact", artifact),
            ("build", build),
            ("profile", JValue::Str(profile)),
            ("hardware", hardware),
        ]))
    }

    /// `preparationFacts(model, token, rowToken)` with Node's inference budget (0: none). `Err`
    /// where Node's throws (the profile could not be read).
    pub async fn preparation_facts(
        &self,
        model: &str,
        token: &str,
        row_token: &str,
        inference: f64,
    ) -> Result<JValue, Unavailable> {
        let facts = self.listing_facts(model, token).await;
        if facts.get("stale").as_bool() == Some(true)
            || facts.get("backend").as_bool() != Some(true)
        {
            return Ok(facts);
        }
        if facts.get("rowToken").as_str() != Some(row_token) {
            return Ok(stale());
        }
        let (profile, revision) = self.profile(model).await?;
        let sizing = Arc::clone(&self.maint.sizing);
        let listing = sizing.listing().await;
        let m = model.to_owned();
        let conservative =
            tokio::task::spawn_blocking(move || sizing.conservative_for(&m, inference, &listing))
                .await
                .ok()
                .flatten()
                .unwrap_or(JValue::Null);
        let identity = self.identity_for(model).await.unwrap_or(JValue::Null);
        if !identity_valid(&identity) {
            return Ok(obj(vec![("identityReady", JValue::Bool(false))]));
        }
        if token != self.launch_token()
            || self.revision_now().await.as_deref() != Some(revision.as_str())
        {
            return Ok(stale());
        }
        let prepared = vec![
            ("row", facts.get("row").clone()),
            ("system", facts.get("system").clone()),
            ("profile", profile),
            ("conservative", conservative),
            ("identity", identity),
            ("identityReady", JValue::Bool(true)),
            ("revision", JValue::Str(revision)),
            ("ladderReady", JValue::Bool(true)),
        ];
        let mut signed = vec![
            ("token", JValue::Str(token.to_owned())),
            ("model", JValue::Str(model.to_owned())),
            ("rowToken", JValue::Str(row_token.to_owned())),
        ];
        signed.extend(prepared.iter().cloned());
        let prepared_token = self.sign("prepared", &obj(signed));
        let mut out = prepared;
        out.push(("preparedToken", JValue::Str(prepared_token)));
        Ok(obj(out))
    }

    /// `compareAndLaunch({ model, token, rowToken, preparedToken, plan })`: `{ applied: true, job }`
    /// or `{ applied: false, reason[, hold] }`; `Err` for an invalid plan or an unreadable profile.
    pub async fn compare_and_launch(
        self: &Arc<Self>,
        model: &str,
        token: &str,
        row_token: &str,
        prepared_token: &str,
        plan: &JValue,
        inference: f64,
    ) -> Result<JValue, Unavailable> {
        let refused = |reason: &str| {
            obj(vec![
                ("applied", JValue::Bool(false)),
                ("reason", JValue::Str(reason.into())),
            ])
        };
        let busy = self.with(|i| {
            i.launching || i.active || i.state.job().get("status").as_str() == Some("running")
        });
        if busy || token != self.launch_token() {
            return Ok(refused("stale"));
        }
        // Defensive command schema, not public selection policy.
        let budget = plan.get("promptBudgetSeconds").as_num();
        let native = plan.get("native").as_num();
        let (Some(budget), Some(native), JValue::Obj(base)) = (budget, native, plan.get("base"))
        else {
            return Err(Unavailable);
        };
        if budget.fract() != 0.0 || !(15.0..=1800.0).contains(&budget) || !native.is_finite() {
            return Err(Unavailable);
        }
        let plan = Plan {
            prompt_budget_seconds: budget,
            native,
            base: base.clone(),
        };
        self.with(|i| i.launching = true);
        let result = self
            .launch_checked(
                model,
                token,
                row_token,
                prepared_token,
                plan,
                inference,
                &refused,
            )
            .await;
        self.with(|i| i.launching = false);
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn launch_checked(
        self: &Arc<Self>,
        model: &str,
        token: &str,
        row_token: &str,
        prepared_token: &str,
        plan: Plan,
        inference: f64,
        refused: &impl Fn(&str) -> JValue,
    ) -> Result<JValue, Unavailable> {
        let check = |facts: &JValue| -> Option<&'static str> {
            if facts.get("identityReady").as_bool() == Some(false)
                || facts.get("ladderReady").as_bool() == Some(false)
            {
                Some("unavailable")
            } else if facts.get("preparedToken").as_str() != Some(prepared_token) {
                Some("stale")
            } else {
                None
            }
        };
        let before = self
            .preparation_facts(model, token, row_token, inference)
            .await?;
        if let Some(r) = check(&before) {
            return Ok(refused(r));
        }
        let hold = match self.take_hold(model, &plan).await {
            Ok(h) => h,
            Err(Refusal::Limit) => return Ok(refused("limit")),
            Err(Refusal::Hold(h)) => {
                let mut out = refused("hold");
                if let JValue::Obj(f) = &mut out {
                    f.push(("hold".into(), h));
                }
                return Ok(out);
            }
            Err(_) => return Err(Unavailable),
        };
        let after = match self
            .preparation_facts(model, token, row_token, inference)
            .await
        {
            Ok(a) => a,
            Err(Unavailable) => {
                hold.release().await;
                return Err(Unavailable);
            }
        };
        if let Some(r) = check(&after) {
            hold.release().await;
            return Ok(refused(r));
        }
        match self.start(hold, model, plan).await {
            Ok(job) => Ok(obj(vec![("applied", JValue::Bool(true)), ("job", job)])),
            Err(_) => Err(Unavailable),
        }
    }

    /// `cancellationFacts()`: `{ token, generation, job }`.
    pub fn cancellation_facts(&self) -> JValue {
        obj(vec![
            ("token", JValue::Str(self.cancel_token())),
            ("generation", JValue::Str(self.cancel_generation())),
            ("job", self.job()),
        ])
    }

    /// `compareAndAbort(token)`: the exact job/controller snapshot the token was issued for is
    /// cancelled; `{ applied: false }` otherwise.
    pub fn compare_and_abort(&self, token: &str) -> JValue {
        // During the boot recovery the job is the file's, with no run to stop: not applied (the route
        // retries, then answers unavailable), never a false success.
        if token != self.cancel_token() || self.with(|i| i.recovering || i.state.job().is_nullish())
        {
            return obj(vec![("applied", JValue::Bool(false))]);
        }
        let generation = self.cancel_generation();
        let job = self.with(|i| {
            let job = visible_job(i);
            i.cancel_requested = true;
            if let Some(a) = &i.inflight {
                a.abort();
            }
            job
        });
        obj(vec![
            ("applied", JValue::Bool(true)),
            ("job", job),
            ("generation", JValue::Str(generation)),
        ])
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn quant_follows_the_node_pattern() {
        let re = regex::Regex::new(QUANT).unwrap();
        let q = |n: &str| {
            re.captures(n)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_uppercase())
        };
        assert_eq!(q("Qwen3-8B-Q4_K_M.gguf").as_deref(), Some("Q4_K_M"));
        assert_eq!(q("model-UD-IQ2_XXS.gguf").as_deref(), Some("UD-IQ2_XXS"));
        assert_eq!(q("gemma.bf16.gguf").as_deref(), Some("BF16"));
        assert_eq!(q("plain.gguf"), None);
        assert_eq!(
            model_path_from_args(Some(&serde_json::json!(["-m", "/models/a.gguf"]))),
            "/models/a.gguf"
        );
        assert_eq!(
            model_path_from_args(Some(&serde_json::json!(["--model"]))),
            ""
        );
    }

    /// The GGUF header the oracle wrote (`gguf(HEADER)` in the fixture's script).
    fn gguf(header: &serde_json::Map<String, Value>) -> Vec<u8> {
        let mut out = b"GGUF".to_vec();
        out.extend(3u32.to_le_bytes());
        out.extend(0u64.to_le_bytes());
        out.extend((header.len() as u64).to_le_bytes());
        // The script's insertion order (serde_json sorts object keys).
        const ORDER: [&str; 7] = [
            "general.architecture",
            "llama.context_length",
            "llama.embedding_length",
            "llama.block_count",
            "llama.attention.head_count",
            "llama.attention.head_count_kv",
            "tokenizer.chat_template",
        ];
        assert_eq!(header.len(), ORDER.len());
        let s = |out: &mut Vec<u8>, t: &str| {
            out.extend((t.len() as u64).to_le_bytes());
            out.extend(t.as_bytes());
        };
        for k in ORDER {
            let v = &header[k];
            s(&mut out, k);
            let t = v[0].as_u64().unwrap() as u32;
            out.extend(t.to_le_bytes());
            if t == 4 {
                out.extend((v[1].as_u64().unwrap() as u32).to_le_bytes());
            } else {
                s(&mut out, v[1].as_str().unwrap());
            }
        }
        out
    }

    async fn stub(rows: Value, props: Value) -> u16 {
        use axum::body::Body;
        use axum::http::{Request, Response};
        let app = axum::Router::new().fallback(move |req: Request<Body>| {
            let (rows, props) = (rows.clone(), props.clone());
            async move {
                let (code, body) = match req.uri().path() {
                    "/models" if !rows.is_null() => (200, serde_json::json!({ "data": rows })),
                    "/props" if !props.is_null() => (200, props),
                    "/models" | "/props" => (502, serde_json::json!({})),
                    _ => (404, serde_json::json!({})),
                };
                Response::builder()
                    .status(code)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await });
        port
    }

    /// Order-insensitive JSON, with this host's MemTotal taken out of `hardware`.
    fn normal(v: &JValue) -> Value {
        let mut v: Value = serde_json::from_str(&wtf16::stringify(v).unwrap()).unwrap();
        if let Some(h) = v.pointer_mut("/identity/hardware") {
            if let Some(text) = h.as_str() {
                let t = match text.split_once("GiB") {
                    Some((n, rest)) if n.bytes().all(|b| b.is_ascii_digit()) => {
                        format!("<mem>GiB{rest}")
                    }
                    _ => text.to_owned(),
                };
                *h = Value::String(t);
            }
        }
        v
    }

    /// tests/fixtures/calibration-prepare-node.json: old Core's listing and preparation facts
    /// (conservativeFor, tuneIdentity) over synthetic GGUF headers; native gives the same facts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preparation_facts_match_the_actual_node_manager() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/calibration-prepare-node.json"
        ))
        .unwrap();
        let header = gguf(fixture["header"].as_object().unwrap());
        let cases = fixture["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 7);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let d = tempfile::tempdir().unwrap();
            let mtime =
                std::time::UNIX_EPOCH + Duration::from_secs(case["mtime"].as_u64().unwrap());
            for f in case["files"].as_array().unwrap() {
                let p = d.path().join(f.as_str().unwrap());
                std::fs::write(&p, &header).unwrap();
                std::fs::File::options()
                    .write(true)
                    .open(&p)
                    .unwrap()
                    .set_modified(mtime)
                    .unwrap();
            }
            std::fs::write(d.path().join("models.ini"), case["ini"].as_str().unwrap()).unwrap();
            let port = stub(case["rows"].clone(), case["props"].clone()).await;
            let env = [
                ("MODEL_MANAGER_KIND".to_owned(), "llamacpp".to_owned()),
                (
                    "MODEL_MANAGER_BASE_URL".to_owned(),
                    format!("http://127.0.0.1:{port}"),
                ),
                (
                    "LLAMACPP_PRESET_PATH".to_owned(),
                    d.path().join("models.ini").display().to_string(),
                ),
                (
                    "LLAMACPP_MODELS_PATH".to_owned(),
                    d.path().display().to_string(),
                ),
                ("UI_DATA_DIR".to_owned(), d.path().display().to_string()),
            ];
            let get = |k: &str| env.iter().find(|(e, _)| e == k).map(|(_, v)| v.clone());
            let engine = Arc::new(server_stats::Engine::from_lookup(&get).unwrap());
            let sizing = Arc::new(crate::model_sizing::Sizing::from_lookup(
                engine.kind(),
                Arc::clone(&engine),
                &get,
            ));
            let maint = Arc::new(
                super::super::Maint::from_lookup(engine.kind(), &engine, sizing, &get).unwrap(),
            );
            let w = Worker::new(
                maint,
                Arc::new(|| Some(64.0)),
                2.0,
                super::super::calibration_worker::Limits::default(),
            );
            let model = case["model"].as_str().unwrap();
            let token = w.state_facts().get("token").as_str().unwrap().to_owned();
            let listing = w.listing_facts(model, &token).await;
            let row_token = listing.get("rowToken").as_str().map(str::to_owned);
            let mut listed = listing.clone();
            if let JValue::Obj(f) = &mut listed {
                f.retain(|(k, _)| k != "rowToken");
            }
            assert_eq!(normal(&listed), case["listing"], "{name}: listing");
            let Some(row_token) = row_token else {
                assert!(case["prepared"].is_null(), "{name}");
                continue;
            };
            let prepared = w
                .preparation_facts(model, &token, &row_token, 16.0)
                .await
                .unwrap();
            let mut p = prepared.clone();
            if let JValue::Obj(f) = &mut p {
                f.retain(|(k, _)| k != "preparedToken");
            }
            let mut want = case["prepared"].clone();
            if let Some(h) = want.pointer_mut("/identity/hardware") {
                let t = h.as_str().unwrap().to_owned();
                let (_, rest) = t.split_once("GiB").unwrap();
                *h = Value::String(format!("<mem>GiB{rest}"));
            }
            assert_eq!(normal(&p), want, "{name}: prepared");
            if prepared.get("identityReady").as_bool() == Some(true) {
                assert!(prepared
                    .get("preparedToken")
                    .as_str()
                    .is_some_and(|t| t.len() == 64));
                // A token from another listing is stale.
                assert_eq!(
                    w.preparation_facts(model, &token, &"0".repeat(64), 16.0)
                        .await
                        .unwrap()
                        .get("stale"),
                    &JValue::Bool(true)
                );
            }
        }
    }
}
