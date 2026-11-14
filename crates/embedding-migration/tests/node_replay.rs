//! Replays `fixtures/node-embedding-migration.v1.json` (written by `record.cjs` from noevia-core's
//! own modules): every answer, every host call in order with its arguments, and the saved file
//! after every step, over a stub host that answers from the same JSON world as the recorder's.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use embedding_migration::config::{write_atomic, NOT_JSON};
use embedding_migration::routes::{diary_url, DIARY_TIMEOUT_MS};
use embedding_migration::target::{normalize_target, seed_from_env, served_verdict};
use embedding_migration::{
    Config, Coverage, Embedding, Host, HttpReply, Migration, Pacing, ServeError, Target, User,
};
use js_json::{to_js_string, wtf16, JValue};
use std::path::PathBuf;

fn show(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "undefined".into())
}

fn arr(items: impl IntoIterator<Item = JValue>) -> JValue {
    JValue::Arr(items.into_iter().collect())
}

fn strict_eq(a: &JValue, b: &JValue) -> bool {
    matches!((a, b), (JValue::Null, JValue::Null)) || (a.as_str().is_some() && a == b)
}

fn text(v: &JValue) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// The recorder's stubs, answering from the same world.
struct Stub {
    world: JValue,
    calls: Vec<JValue>,
}

impl Stub {
    fn log(&mut self, name: &str, args: Vec<JValue>) {
        self.calls
            .push(arr(std::iter::once(name.into()).chain(args)));
    }
    fn w(&self, key: &str) -> &JValue {
        self.world.get(key)
    }
    fn svc(&self, key: &str) -> &JValue {
        self.world.get("service").get(key)
    }
    /// `list.shift()` of `world[a][b]` (or `world[a]`).
    fn shift(&mut self, path: &[&str]) -> Option<JValue> {
        let mut at = &mut self.world;
        for key in path {
            let JValue::Obj(items) = at else { return None };
            at = &mut items.iter_mut().find(|(k, _)| k == key)?.1;
        }
        match at {
            JValue::Arr(items) if !items.is_empty() => Some(items.remove(0)),
            _ => None,
        }
    }
    fn merge(&mut self, patch: &JValue) {
        let (JValue::Obj(items), JValue::Obj(patch)) = (&mut self.world, patch) else {
            panic!("world")
        };
        for (k, v) in patch {
            wtf16::set(items, k, v.clone());
        }
    }
}

impl Host for Stub {
    type Index = String;
    fn lock(&mut self) -> Result<(), String> {
        self.log("lock", vec![]);
        match self.w("lockFail") {
            JValue::Str(reason) if !reason.is_empty() => Err(reason.clone()),
            _ => Ok(()),
        }
    }
    fn unlock(&mut self) {
        self.log("unlock", vec![]);
    }
    fn kick(&mut self) {
        self.log("kick", vec![]);
    }
    fn snapshot(&mut self) -> Option<JValue> {
        self.log("snapshot", vec![]);
        Some(self.w("snapshot").clone()).filter(|s| !s.is_nullish())
    }
    fn indexes(&mut self) -> Vec<String> {
        self.log("indexes", vec![]);
        match self.w("indexes") {
            JValue::Arr(items) => items.iter().map(text).collect(),
            _ => Vec::new(),
        }
    }
    fn coverage(&mut self, index: &String, t: &Target) -> Option<Coverage> {
        self.log("coverage", vec![index.as_str().into(), t.to_json()]);
        let c = self
            .w("coverage")
            .get(&format!("{index}|{}|{}", t.profile, t.model));
        c.is_object().then(|| Coverage {
            chunks: c.get("chunks").as_num().unwrap(),
            vectors: c.get("vectors").as_num().unwrap(),
        })
    }
    fn drop_slot(&mut self, index: &String, t: &Target) -> Result<(), String> {
        self.log("dropSlot", vec![index.as_str().into(), t.to_json()]);
        match self.w("dropFails") {
            JValue::Arr(items) if items.iter().any(|i| i.as_str() == Some(index)) => {
                Err("qa drop failed".into())
            }
            _ => Ok(()),
        }
    }
    fn served_models(&mut self, base_url: Option<&str>, model: &str) -> Option<Vec<String>> {
        self.log(
            "servedModels",
            vec![base_url.map(String::from).into(), model.into()],
        );
        match self.w("served").get(base_url.unwrap_or("")) {
            JValue::Arr(ids) => Some(ids.iter().map(text).collect()),
            _ => None,
        }
    }
    fn embed(&mut self, texts: &[&str], t: &Target) -> Result<Vec<Vec<f64>>, String> {
        let sent = arr(texts.iter().map(|s| JValue::from(*s)));
        self.log("embed", vec![sent, "document".into(), t.to_json()]);
        match self.w("vectors").get(t.base_url.as_deref().unwrap_or("")) {
            JValue::Str(e) => Err(e.clone()),
            JValue::Arr(rows) => Ok(rows
                .iter()
                .map(|row| match row {
                    JValue::Arr(xs) => xs.iter().map(|x| x.as_num().unwrap()).collect(),
                    _ => panic!("vector"),
                })
                .collect()),
            _ => Err("no vectors".into()),
        }
    }
    fn check_address(&mut self, url: &str) -> Result<(), String> {
        self.log("checkAddress", vec![url.into()]);
        match self.w("public").get(url) {
            JValue::Str(reason) if !reason.is_empty() => Err(reason.clone()),
            _ => Ok(()),
        }
    }
    fn diary(&mut self, url: &str, method: &str, body: Option<&str>) -> Result<HttpReply, String> {
        let timeout = JValue::Num(DIARY_TIMEOUT_MS as f64);
        let sent = body.map(String::from).into();
        self.log("diary", vec![url.into(), method.into(), sent, timeout]);
        let reply = match self.shift(&["diaryQueue"]) {
            Some(reply) => reply,
            None => self.w("diary").get(method).clone(),
        };
        if !reply.is_object() {
            return Err("no reply".into());
        }
        if let JValue::Str(e) = reply.get("throw") {
            return Err(e.clone());
        }
        Ok(HttpReply {
            ok: reply.get("ok").truthy(),
            status: reply.get("status").as_num().unwrap() as u16,
            body: reply.get("body").clone(),
        })
    }
    fn audit(&mut self, action: &str, user: &str, detail: &JValue) {
        let args = vec![action.into(), user.into(), JValue::Null, detail.clone()];
        self.log("audit", args);
    }
    fn service_refresh(&mut self) {
        self.log("serviceRefresh", vec![]);
    }
    fn service_managed(&mut self) -> bool {
        self.log("serviceManaged", vec![]);
        self.svc("managed").truthy()
    }
    fn service_enabled(&mut self) -> Option<bool> {
        self.log("serviceEnabled", vec![]);
        self.svc("enabled").as_bool()
    }
    fn service_status(&mut self) -> JValue {
        self.log("serviceStatus", vec![]);
        self.svc("status").clone()
    }
    fn service_base_url(&self) -> Option<String> {
        self.svc("baseUrl").as_str().map(String::from)
    }
    fn service_is_at(&mut self, address: &JValue) -> bool {
        self.log("serviceIsAt", vec![address.clone()]);
        match self.svc("at") {
            JValue::Arr(at) => at.iter().any(|a| strict_eq(a, address)),
            _ => false,
        }
    }
    fn service_file_of(&mut self, model: &JValue) -> Option<String> {
        self.log("serviceFileOf", vec![model.clone()]);
        let key = to_js_string(model).unwrap();
        self.svc("known").get(&key).as_str().map(String::from)
    }
    fn service_serve(&mut self, models: &[JValue]) -> Result<(), ServeError> {
        self.log("serviceServe", vec![arr(models.iter().cloned())]);
        match self.shift(&["service", "serveFail"]) {
            Some(f) if f.truthy() => Err(ServeError {
                status: f.get("status").as_num().map(|s| s as u16),
                message: text(f.get("message")),
                code: f.get("code").as_str().map(String::from),
            }),
            _ => Ok(()),
        }
    }
    fn service_wait_loaded(&mut self, models: &[String], timeout_ms: u64) -> bool {
        let names = arr(models.iter().map(|m| JValue::from(m.as_str())));
        self.log(
            "serviceWaitLoaded",
            vec![names, JValue::Num(timeout_ms as f64)],
        );
        self.shift(&["service", "loaded"])
            .is_none_or(|v| v.truthy())
    }
    fn resolve_file(&mut self, model: &str) -> Option<String> {
        self.log("resolveFile", vec![model.into()]);
        match self.svc("installed").get(model) {
            JValue::Str(f) if f != "!throw" => Some(f.clone()),
            _ => None,
        }
    }
}

fn env_of(env: &JValue) -> impl Fn(&str) -> Option<String> + '_ {
    move |key| env.get(key).as_str().map(String::from)
}

/// The recorded text with V8's `JSON.parse` message replaced by the port's.
fn expected(v: &JValue, json_error: &Option<String>) -> String {
    let shown = show(v);
    match json_error {
        Some(message) => {
            let quoted = show(&JValue::Str(format!("({message})")));
            let inner = &quoted[1..quoted.len() - 1];
            shown.replace(inner, &format!("({NOT_JSON})"))
        }
        None => shown,
    }
}

fn boot(sc: &JValue, world: &JValue, rec: &JValue, json_error: &mut Option<String>) -> Embedding {
    let env = env_of(sc.get("env"));
    let (config, warnings) = Config::load(PathBuf::from(text(sc.get("stateFile"))), &env);
    *json_error = rec.get("jsonError").as_str().map(String::from);
    let got = arr(warnings.into_iter().map(JValue::from));
    assert_eq!(
        show(&got),
        expected(rec.get("warnings"), json_error),
        "{} boot",
        text(sc.get("name"))
    );
    let base = text(world.get("diaryBase"));
    Embedding::new(
        Migration::new(config, Pacing::from_env(&env)),
        diary_url(&base),
    )
}

fn replay(sc: &JValue) -> usize {
    let name = text(sc.get("name"));
    let file = PathBuf::from(text(sc.get("stateFile")));
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    if let JValue::Str(initial) = sc.get("file") {
        std::fs::write(&file, initial).unwrap();
    }
    let mut host = Stub {
        world: sc.get("world").clone(),
        calls: Vec::new(),
    };
    let mut json_error = None;
    let mut api = boot(sc, &host.world, sc.get("boot"), &mut json_error);
    let JValue::Arr(steps) = sc.get("steps") else {
        panic!("steps")
    };
    for (n, rec) in steps.iter().enumerate() {
        let at = format!("{name} step {n}: {}", show(rec.get("step")));
        let step = rec.get("step");
        host.calls.clear();
        if step.get("world").is_object() {
            host.merge(step.get("world"));
        } else if step.get("restart").truthy() {
            api = boot(sc, &host.world, rec.get("boot"), &mut json_error);
        } else if !matches!(step.get("writeFile"), JValue::Undefined) {
            match step.get("writeFile") {
                JValue::Str(t) => std::fs::write(&file, t).unwrap(),
                _ => {
                    let _ = std::fs::remove_file(&file);
                }
            }
        } else if step.get("refuse").is_object() {
            let reason = |k: &str| step.get("refuse").get(k).as_str().map(String::from);
            let (active, next) = (reason("active"), reason("next"));
            api.migration
                .config
                .refuse_seeded(active.as_deref(), next.as_deref());
        } else {
            let method = match step.get("method").as_str() {
                Some(m) => m,
                None if step.get("get").truthy() => "GET",
                None => "POST",
            };
            let user = match step.get("as").as_str().unwrap_or("admin") {
                "admin" => Some(User {
                    id: "qa-admin-1",
                    admin: true,
                }),
                "member" => Some(User {
                    id: "qa-member-1",
                    admin: false,
                }),
                _ => None,
            };
            let body = wtf16::parse(step.get("post").as_str().unwrap_or("")).map_err(|_| ());
            let answer = api.handle(&mut host, method, user, body.as_ref().map_err(|_| ()));
            assert_eq!(
                f64::from(answer.status),
                rec.get("status").as_num().unwrap(),
                "{at} status"
            );
            assert_eq!(
                show(&answer.body),
                expected(rec.get("body"), &json_error),
                "{at} body"
            );
        }
        let calls = arr(host.calls.drain(..));
        assert_eq!(show(&calls), show(rec.get("calls")), "{at} calls");
        let saved: JValue = std::fs::read_to_string(&file).ok().into();
        assert_eq!(show(&saved), show(rec.get("file")), "{at} file");
    }
    steps.len()
}

fn fixture() -> JValue {
    wtf16::parse(include_str!("fixtures/node-embedding-migration.v1.json")).unwrap()
}

fn list<'a>(f: &'a JValue, key: &str) -> &'a [JValue] {
    match f.get(key) {
        JValue::Arr(items) if !items.is_empty() => items,
        _ => panic!("no {key} cases"),
    }
}

#[test]
fn every_recorded_case_replays_as_node_answered() {
    let f = fixture();
    assert_eq!(f.get("version").as_num(), Some(1.0));
    assert_eq!(
        text(f.get("sources").get("server/routes/embedding.cjs")),
        "ae34b6be30f2b8880a5c6c0c141ed9d1e709e87ef92202bb3cc0e79465db9854"
    );
    let failure = |e: &embedding_migration::Failure| {
        let status = e.status.map_or(JValue::Null, |s| JValue::Num(f64::from(s)));
        JValue::obj([
            ("message", e.message.as_str().into()),
            ("status", status),
            ("code", e.code.into()),
        ])
    };
    for case in list(&f, "normalize") {
        let got = match normalize_target(case.get("input")) {
            Ok(t) => JValue::obj([("ok", t.to_json())]),
            Err(e) => JValue::obj([("error", failure(&e))]),
        };
        let want = JValue::obj([
            ("ok", case.get("ok").clone()),
            ("error", case.get("error").clone()),
        ]);
        assert_eq!(
            show(&got),
            show(&want),
            "normalize {}",
            show(case.get("input"))
        );
    }
    for case in list(&f, "served") {
        let ids: Option<Vec<String>> = match case.get("ids") {
            JValue::Arr(ids) => Some(ids.iter().map(text).collect()),
            _ => None,
        };
        let got = match served_verdict(&text(case.get("model")), ids.as_deref()) {
            Ok(()) => JValue::obj([("ok", JValue::Bool(true))]),
            Err(e) => JValue::obj([
                ("ok", JValue::Bool(false)),
                ("code", e.code.into()),
                (
                    "served",
                    e.served().map_or(JValue::Undefined, |s| {
                        arr(s.iter().map(|x| JValue::from(x.as_str())))
                    }),
                ),
                ("error", e.message.into()),
            ]),
        };
        assert_eq!(
            show(&got),
            show(case.get("verdict")),
            "served {}",
            show(case)
        );
    }
    for case in list(&f, "seeds") {
        let (state, warnings) = seed_from_env(&env_of(case.get("env")));
        let got = JValue::obj([
            ("active", state.active.to_json()),
            (
                "next",
                state.next.as_ref().map_or(JValue::Null, Target::to_json),
            ),
            ("previous", JValue::Null),
        ]);
        assert_eq!(
            show(&got),
            show(case.get("state")),
            "seed {}",
            show(case.get("env"))
        );
        let warnings = arr(warnings.into_iter().map(JValue::from));
        assert_eq!(show(&warnings), show(case.get("warnings")));
    }
    // Relative state files, as recorded, so messages naming them match.
    let work = std::env::temp_dir().join(format!("qa-embed-replay-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    std::env::set_current_dir(&work).unwrap();
    let scenarios = list(&f, "scenarios");
    let steps: usize = scenarios.iter().map(replay).sum();
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    std::fs::remove_dir_all(&work).unwrap();
    assert!(
        scenarios.len() >= 150 && steps >= 1100,
        "{} scenarios, {steps} steps",
        scenarios.len()
    );
}

/// Not recordable from Node with the same message: a failed write leaves the state and the file
/// as they were, answers 500 and releases the lock.
#[test]
fn a_failed_write_changes_nothing_and_releases_the_lock() {
    let dir = std::env::temp_dir().join(format!("qa-embed-write-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let blocker = dir.join("qa-blocker");
    std::fs::write(&blocker, "qa").unwrap();
    let env = |k: &str| (k == "EMBEDDING_NEXT_MODEL").then(|| "qa-embed-b".to_string());
    let (config, _) = Config::load(blocker.join("embedding-config.json"), &env);
    let mut api = Embedding::new(Migration::new(config, Pacing::from_env(&env)), None);
    let world = r#"{"indexes":[],"snapshot":null,"service":{"managed":false}}"#;
    let mut host = Stub {
        world: wtf16::parse(world).unwrap(),
        calls: Vec::new(),
    };
    let body = wtf16::parse(r#"{"action":"clear-next","scope":"projects"}"#).unwrap();
    let admin = Some(User {
        id: "qa-admin-1",
        admin: true,
    });
    let answer = api.handle(&mut host, "POST", admin, Ok(&body));
    assert_eq!(answer.status, 500);
    assert_eq!(
        answer.body.get("projects").get("status").as_num(),
        Some(500.0)
    );
    assert_eq!(
        api.migration.config.next().map(|t| t.model.as_str()),
        Some("qa-embed-b")
    );
    assert_eq!(show(&host.calls[0]), r#"["lock"]"#);
    assert_eq!(show(&host.calls[1]), r#"["unlock"]"#);
    // The atomic writer itself: mode 0600, no temporary file left.
    let file = dir.join("sub").join("embedding-config.json");
    write_atomic(&file, b"{}").unwrap();
    write_atomic(&file, b"{\"qa\":1}").unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "{\"qa\":1}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let left = std::fs::read_dir(dir.join("sub")).unwrap().count();
    assert_eq!(left, 1);
    std::fs::remove_dir_all(&dir).unwrap();
}
