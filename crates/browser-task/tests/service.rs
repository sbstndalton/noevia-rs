//! Replays tests/fixtures/service-node.v1.json (core's own routes/browser.cjs over
//! browser-service.cjs, browser-executor.cjs, browser-policy.cjs and jobs.cjs, with the scripted fake
//! browser of tools/gen-service-fixtures.cjs) on this crate: every answer, every journal row, every
//! call the browser and the egress proxy received, every log line and the files left behind.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

mod fake;

use browser_task::routes::{self, Body, Caller};
use browser_task::service::{Egress, Options, Service};
use browser_task::Fail;
use fake::{Fake, Launcher, Log, World};
use js_json::JValue;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const NOW: f64 = 1_800_000_000_000.0;
const PROJECT: &str = "qa-project";

fn s(t: &str) -> JValue {
    JValue::Str(t.to_string())
}

fn id_of(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

struct FakeEgress {
    log: Log,
    grant_fails: Option<String>,
    revoke: JValue,
}

impl Egress for FakeEgress {
    fn endpoint(&self) -> Option<String> {
        Some("egress.example.test:8040".into())
    }
    fn grant(&self, task_id: &str, domains: &[String]) -> Result<JValue, Fail> {
        self.log.lock().unwrap().push(JValue::Arr(vec![
            s("grant"),
            s(task_id),
            JValue::Arr(domains.iter().map(|d| s(d)).collect()),
        ]));
        if let Some(m) = &self.grant_fails {
            return Err(Fail {
                status: None,
                message: "x".into(),
                public: Some(m.clone()),
                result: None,
            });
        }
        Ok(JValue::obj([("token", s(&format!("tok-{task_id}")))]))
    }
    fn revoke(&self, grant: &JValue) -> Result<JValue, Fail> {
        self.log
            .lock()
            .unwrap()
            .push(JValue::Arr(vec![s("revoke"), grant.get("token").clone()]));
        Ok(self.revoke.clone())
    }
    fn forget(&self, grant: &JValue, snapshot_id: &str) -> Option<Result<JValue, Fail>> {
        self.log.lock().unwrap().push(JValue::Arr(vec![
            s("forget"),
            grant.get("token").clone(),
            s(snapshot_id),
        ]));
        Some(Ok(JValue::obj([("ok", JValue::Bool(true))])))
    }
}

fn default_revoke() -> JValue {
    js_json::parse(r#"{"confirmed":true,"report":{"available":true,"final":true,"partial":false,"state":"final","snapshotId":"snap-1","hosts":["shop.example.test"],"allowed":1,"refused":0}}"#).unwrap()
}

/// Every string in `v` with `from` replaced by `to`.
fn replace_all(v: &JValue, from: &str, to: &str) -> JValue {
    match v {
        JValue::Str(t) => s(&t.replace(from, to)),
        JValue::Arr(items) => JValue::Arr(items.iter().map(|x| replace_all(x, from, to)).collect()),
        JValue::Obj(items) => JValue::Obj(
            items
                .iter()
                .map(|(k, x)| (k.clone(), replace_all(x, from, to)))
                .collect(),
        ),
        other => other.clone(),
    }
}

struct Ids {
    tasks: Vec<String>,
    approvals: Vec<String>,
}

impl Ids {
    fn sub(&self, v: &JValue) -> JValue {
        let mut out = v.clone();
        for (i, t) in self.tasks.iter().enumerate().rev() {
            out = replace_all(&out, &format!("<T{}>", i + 1), t);
        }
        for (i, a) in self.approvals.iter().enumerate().rev() {
            out = replace_all(&out, &format!("<A{}>", i + 1), a);
        }
        out
    }
}

fn files_under(base: &Path) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            if rel == "jobs" {
                continue;
            }
            if p.is_dir() {
                walk(&p, base, out);
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(base, base, &mut out);
    out.sort();
    out
}

fn journal(base: &Path, id: &str) -> JValue {
    let f = base.join("jobs").join(format!("{id}.jsonl"));
    match std::fs::read_to_string(f) {
        Ok(text) => JValue::Arr(
            text.lines()
                .filter(|l| !l.is_empty())
                .map(|l| js_json::parse(l).unwrap())
                .collect(),
        ),
        Err(_) => JValue::Null,
    }
}

fn ended(base: &Path, id: &str) -> bool {
    std::fs::read_to_string(base.join("jobs").join(format!("{id}.jsonl")))
        .map(|t| {
            [
                "job.completed",
                "job.failed",
                "job.cancelled",
                "job.interrupted",
            ]
            .iter()
            .any(|k| t.contains(&format!("\"type\":\"{k}\"")))
        })
        .unwrap_or(false)
}

struct Outcome {
    answers: Vec<JValue>,
    journals: Vec<(String, JValue)>,
    engine: JValue,
    egress: JValue,
    logs: JValue,
    files: Vec<String>,
}

fn opt_ms(options: &JValue, key: &str, default: u64) -> Duration {
    Duration::from_millis(options.get(key).as_num().map_or(default, |n| n as u64))
}

fn request(service: &Arc<Service>, op: &JValue, ids: &Ids, base: &Path, flag: bool) -> JValue {
    let req = ids.sub(op.get("req"));
    let (method, path) = (req.get_index(0), req.get_index(1));
    let who = op.get("as");
    let caller = match who {
        JValue::Null => None,
        JValue::Str(r) if r == "member" => Some(Caller { admin: false }),
        _ => Some(Caller { admin: true }),
    };
    let enabled = op.get("flag").as_bool().unwrap_or(flag);
    let raw = op.get("raw").as_str().map(str::to_string);
    let body = ids.sub(op.get("body"));
    let mut read = move || match &raw {
        Some(r) => js_json::parse(r).map(Body::Json).unwrap_or(Body::Invalid),
        None => Body::Json(body.clone()),
    };
    let found = |id: &str| (id == PROJECT).then(|| PROJECT.to_string());
    let req = routes::Request {
        method,
        path,
        caller: caller.as_ref(),
        enabled,
    };
    match routes::handle(service, &req, &found, base, &mut read) {
        None => JValue::obj([("unmatched", JValue::Bool(true))]),
        Some((status, body)) => {
            JValue::obj([("status", JValue::Num(f64::from(status))), ("body", body)])
        }
    }
}

fn run(sc: &JValue) -> Outcome {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let options = sc.get("options");
    let counter = Arc::new(Mutex::new(0u64));
    let new_id: Arc<dyn Fn() -> String + Send + Sync> = {
        let c = counter.clone();
        Arc::new(move || {
            let mut n = c.lock().unwrap();
            *n += 1;
            id_of(*n)
        })
    };
    let engine_log: Log = Arc::new(Mutex::new(Vec::new()));
    let egress_log: Log = Arc::new(Mutex::new(Vec::new()));
    let logs: Log = Arc::new(Mutex::new(Vec::new()));
    let world = Arc::new(Mutex::new(World {
        pages: sc.get("world").get("pages").clone(),
    }));
    let launch_fails = Some(options.get("launchFails").clone()).filter(JValue::is_object);
    let fake = Arc::new(Fake {
        log: engine_log.clone(),
        world: world.clone(),
        launch_fails,
    });
    let egress: Option<Arc<dyn Egress>> = if options.get("egress").as_bool() == Some(false) {
        None
    } else {
        let revoke = options.get("revoke");
        Some(Arc::new(FakeEgress {
            log: egress_log.clone(),
            grant_fails: options.get("grantFails").as_str().map(str::to_string),
            revoke: if revoke.is_object() {
                revoke.clone()
            } else {
                default_revoke()
            },
        }))
    };
    let log = {
        let l = logs.clone();
        Arc::new(move |e: JValue| l.lock().unwrap().push(e))
    };
    let mut opts = Options::new(Arc::new(Launcher(fake)), egress, log);
    opts.now = Arc::new(|| NOW);
    opts.new_id = new_id;
    opts.timeout = opt_ms(options, "timeoutMs", 60_000);
    opts.idle_timeout = opt_ms(options, "idleTimeoutMs", 600_000);
    opts.direct = options.get("direct").as_bool() == Some(true);
    let service = Service::new(opts);
    let flag = options.get("flag").as_bool() != Some(false);
    let mut ids = Ids {
        tasks: Vec::new(),
        approvals: Vec::new(),
    };
    let mut answers = Vec::new();
    let mut keyed: Vec<(String, std::thread::JoinHandle<JValue>)> = Vec::new();
    let JValue::Arr(ops) = sc.get("ops") else {
        panic!("ops")
    };
    for op in ops {
        if op.get("req").is_object() || matches!(op.get("req"), JValue::Arr(_)) {
            if let Some(key) = op.get("key").as_str() {
                let (service, op2, base2) = (service.clone(), op.clone(), base.clone());
                let ids_now = Ids {
                    tasks: ids.tasks.clone(),
                    approvals: ids.approvals.clone(),
                };
                keyed.push((
                    key.to_string(),
                    std::thread::spawn(move || request(&service, &op2, &ids_now, &base2, flag)),
                ));
            } else {
                let answer = request(&service, op, &ids, &base, flag);
                note(&answer, &mut ids);
                answers.push(answer);
            }
        } else if let Some(key) = op.get("settle").as_str() {
            let at = keyed.iter().position(|(k, _)| k == key).unwrap();
            let (_, handle) = keyed.remove(at);
            let answer = handle.join().unwrap();
            note(&answer, &mut ids);
            answers.push(answer);
        } else if let Some(task) = op.get("waitCard").as_str() {
            let task = ids.sub(&s(task)).as_str().unwrap().to_string();
            let mut card = JValue::Null;
            for _ in 0..500 {
                card = service
                    .get(&base, PROJECT, &task)
                    .map(|v| v.get("approval").clone())
                    .unwrap_or(JValue::Null);
                if !card.is_nullish() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if let Some(id) = card.get("id").as_str() {
                if !ids.approvals.iter().any(|a| a == id) {
                    ids.approvals.push(id.to_string());
                }
            }
            answers.push(JValue::obj([("card", card)]));
        } else if let Some(task) = op.get("waitEnd").as_str() {
            let task = ids.sub(&s(task)).as_str().unwrap().to_string();
            for _ in 0..500 {
                if ended(&base, &task) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(100));
            answers.push(JValue::obj([("ended", JValue::Bool(ended(&base, &task)))]));
        } else if let Some(task) = op.get("view").as_str() {
            let task = ids.sub(&s(task)).as_str().unwrap().to_string();
            let v = match service.get(&base, PROJECT, &task) {
                Ok(v) => v,
                Err(e) => JValue::obj([("error", s(&e.message))]),
            };
            if let Some(id) = v.get("approval").get("id").as_str() {
                if !ids.approvals.iter().any(|a| a == id) {
                    ids.approvals.push(id.to_string());
                }
            }
            answers.push(v);
        } else if op.get("mutate").is_object() {
            world.lock().unwrap().mutate(op.get("mutate"));
        } else if let Some(ms) = op.get("sleep").as_num() {
            std::thread::sleep(Duration::from_millis(ms as u64));
        }
    }
    std::thread::sleep(Duration::from_millis(100));
    let mut job_ids: Vec<String> = std::fs::read_dir(base.join("jobs"))
        .map(|d| {
            d.flatten()
                .filter_map(|e| {
                    e.file_name()
                        .to_str()
                        .and_then(|n| n.strip_suffix(".jsonl"))
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    job_ids.sort();
    let journals: Vec<(String, JValue)> = job_ids
        .iter()
        .map(|t| (t.clone(), journal(&base, t)))
        .collect();
    let b = base.to_string_lossy().into_owned();
    let norm = |v: JValue| replace_all(&v, &b, "<BASE>");
    let engine = JValue::Arr(engine_log.lock().unwrap().clone());
    let egress = JValue::Arr(egress_log.lock().unwrap().clone());
    let logged = JValue::Arr(logs.lock().unwrap().clone());
    Outcome {
        answers: answers.into_iter().map(norm).collect(),
        journals: journals
            .into_iter()
            .map(|(k, v): (String, JValue)| (k, norm(v)))
            .collect(),
        engine: norm(engine),
        egress: norm(egress),
        logs: norm(logged),
        files: files_under(&base),
    }
}

/// A 202 names a task; a view or card names an approval.
fn note(answer: &JValue, ids: &mut Ids) {
    if answer.get("status").as_num() == Some(202.0) {
        if let Some(t) = answer.get("body").get("taskId").as_str() {
            if !ids.tasks.iter().any(|x| x == t) {
                ids.tasks.push(t.to_string());
            }
        }
    }
}

trait Index {
    fn get_index(&self, i: usize) -> &str;
}

impl Index for JValue {
    fn get_index(&self, i: usize) -> &str {
        match self {
            JValue::Arr(items) => items.get(i).and_then(JValue::as_str).unwrap_or(""),
            _ => "",
        }
    }
}

/// The scenarios whose engine calls and log lines are compared as multisets (see the check below).
const UNORDERED: [&str; 1] = ["cancel-pending"];

fn sorted(v: &JValue) -> JValue {
    let JValue::Arr(items) = v else { return v.clone() };
    let mut keyed: Vec<(String, JValue)> = items.iter().map(|i| (js_json::stringify(i).unwrap_or_default(), i.clone())).collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    JValue::Arr(keyed.into_iter().map(|(_, i)| i).collect())
}

fn pretty(v: &JValue) -> String {
    js_json::stringify_pretty(v).unwrap_or_default()
}

#[test]
fn replays_core_browser_service() {
    let fixture = js_json::parse(include_str!("fixtures/service-node.v1.json")).unwrap();
    let JValue::Obj(scenarios) = fixture.get("scenarios") else {
        panic!("scenarios")
    };
    let only = std::env::var("BROWSER_SCENARIO").ok();
    let mut failures = Vec::new();
    for (name, sc) in scenarios {
        if only.as_deref().is_some_and(|o| o != name) {
            continue;
        }
        let got = run(sc);
        let mut check = |what: &str, want: &JValue, have: &JValue| {
            if want != have {
                failures.push(format!(
                    "{name} {what}\n--- node\n{}\n--- rust\n{}",
                    pretty(want),
                    pretty(have)
                ));
            }
        };
        check(
            "answers",
            sc.get("answers"),
            &JValue::Arr(got.answers.clone()),
        );
        let journals = JValue::Obj(got.journals.clone());
        check("journals", sc.get("journals"), &journals);
        // Named difference: a cancel landing while an action waits on its card closes the session on one
        // thread while the declined action finishes on another, so their call and log lines interleave in
        // no fixed order (core's order is its microtask order). The same lines, each exactly once.
        let unordered = UNORDERED.contains(&name.as_str());
        let order = |v: &JValue| if unordered { sorted(v) } else { v.clone() };
        check("engine", &order(sc.get("engine")), &order(&got.engine));
        check("egress", sc.get("egress"), &got.egress);
        check("logs", &order(sc.get("logs")), &order(&got.logs));
        check(
            "files",
            sc.get("files"),
            &JValue::Arr(got.files.iter().map(|f| s(f)).collect()),
        );
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
