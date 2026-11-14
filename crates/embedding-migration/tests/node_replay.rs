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
    Config, Coverage, Embedding, Host, HttpReply, Migration, Pacing, Seeded, ServeError, Target,
    User,
};
use js_json::{wtf16, JValue};
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
        // Not in the recording: a host that fails inside the config lock's hold.
        if self.w("panicKick").truthy() {
            panic!("qa host panicked inside the hold");
        }
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
        if !self.svc("baseUrl").truthy() {
            return false; // `!!baseUrl && ...`
        }
        match self.svc("at") {
            JValue::Arr(at) => at.iter().any(|a| strict_eq(a, address)),
            _ => false,
        }
    }
    fn service_file_of(&mut self, model: &JValue) -> Option<String> {
        self.log("serviceFileOf", vec![model.clone()]);
        // `===`: only a string names an entry.
        let key = model.as_str()?;
        self.svc("known").get(key).as_str().map(String::from)
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

/// The recorder's `stateOf(config)`: `config.get()` without its change counter, and `damaged`.
fn state_of(c: &Config) -> JValue {
    JValue::obj([
        ("active", c.active().to_json()),
        ("next", c.next().map_or(JValue::Null, Target::to_json)),
        (
            "previous",
            c.previous().map_or(JValue::Null, Target::to_json),
        ),
        ("source", c.source().as_str().into()),
        ("damaged", c.damaged().map_or(JValue::Null, JValue::from)),
    ])
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
            let config = &mut api.migration.config;
            if step.get("transient").truthy() {
                config.refuse_seeded_as(active.as_deref(), next.as_deref(), true);
            } else {
                config.refuse_seeded(active.as_deref(), next.as_deref());
            }
            assert_eq!(
                show(&state_of(config)),
                show(rec.get("state")),
                "{at} state"
            );
        } else if let JValue::Str(which) = step.get("release") {
            let config = &mut api.migration.config;
            match which.as_str() {
                "active" => config.release_seeded(Seeded::Active),
                "next" => config.release_seeded(Seeded::Next),
                _ => {} // `held[which]` is never set for another name
            }
            assert_eq!(
                show(&state_of(config)),
                show(rec.get("state")),
                "{at} state"
            );
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

/// `nextChanged` needs a change between the checks and the hold, which one request at a time never
/// gives the recording: the transition refuses a switch to anything but the checked next.
#[test]
fn a_switch_to_another_next_is_refused() {
    let t = |model: &str| Target {
        base_url: None,
        model: model.into(),
        profile: "nomic-v1".into(),
    };
    let state = embedding_migration::State {
        active: t("qa-embed-a"),
        next: Some(t("qa-embed-b")),
        previous: None,
    };
    let mut config = Config::new(state, None, false);
    let refused = config.switch_to_next(Some(&t("qa-embed-c"))).unwrap_err();
    assert_eq!(
        (refused.status, refused.code),
        (Some(409), Some("nextChanged"))
    );
    assert_eq!(config.next().map(|n| n.model.as_str()), Some("qa-embed-b"));
    config.switch_to_next(Some(&t("qa-embed-b"))).unwrap();
    assert_eq!(config.active().model, "qa-embed-b");
    assert_eq!(
        config.previous().map(|p| p.model.as_str()),
        Some("qa-embed-a")
    );
}

fn qa_target(model: &str, base_url: Option<&str>) -> Target {
    Target {
        base_url: base_url.map(String::from),
        model: model.into(),
        profile: "nomic-v1".into(),
    }
}

const QA_SVC: &str = "http://qa-embed.invalid:8080";
const QA_ADMIN: Option<User<'static>> = Some(User {
    id: "qa-admin-1",
    admin: true,
});

/// An in-memory configuration (no lock) with `active` and `next`, and a stub over `world`.
fn in_memory(active: Target, next: Option<Target>, diary: bool, world: &str) -> (Embedding, Stub) {
    let state = embedding_migration::State {
        active,
        next,
        previous: None,
    };
    let config = Config::new(state, None, false);
    let diary = diary.then(|| "http://qa-diary.invalid:8090/api/internal/embedding".to_string());
    let api = Embedding::new(Migration::new(config, Pacing::from_env(&|_| None)), diary);
    let host = Stub {
        world: wtf16::parse(world).unwrap(),
        calls: Vec::new(),
    };
    (api, host)
}

/// The names of the host calls made, in order.
fn call_names(host: &Stub) -> Vec<String> {
    host.calls
        .iter()
        .map(|c| match c {
            JValue::Arr(items) => text(&items[0]),
            _ => panic!("call"),
        })
        .collect()
}

fn called(host: &Stub, name: &str) -> usize {
    call_names(host).iter().filter(|n| *n == name).count()
}

/// Finding: a Host panic inside the config lock's hold must still end the hold and unlock once.
#[test]
fn a_host_panic_inside_the_hold_unlocks_once_and_ends_the_hold() {
    let dir = std::env::temp_dir().join(format!("qa-embed-panic-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let env = |k: &str| match k {
        "EMBEDDING_MODEL" => Some("qa-embed-a".to_string()),
        "EMBEDDING_NEXT_MODEL" => Some("qa-embed-b".to_string()),
        _ => None,
    };
    let (config, _) = Config::load(dir.join("embedding-config.json"), &env);
    assert!(config.holds_lock());
    let mut migration = Migration::new(config, Pacing::from_env(&env));
    let world = r#"{"indexes":[],"snapshot":null,"dropFails":[],"panicKick":true}"#;
    let mut host = Stub {
        world: wtf16::parse(world).unwrap(),
        calls: Vec::new(),
    };
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        migration.clear_next(&mut host)
    }));
    assert!(caught.is_err(), "the host's panic goes on to the caller");
    assert_eq!(call_names(&host), ["lock", "kick", "unlock"]);
    // The hold ended: a change outside a lock is refused again.
    let outside = migration.config.clear_next().unwrap_err();
    assert_eq!(
        outside.message,
        "Embedding configuration requires the publication lock"
    );
    // The change written before the panic stays, and the next request locks afresh.
    assert!(migration.config.next().is_none());
    host.merge(&wtf16::parse(r#"{"panicKick":false}"#).unwrap());
    host.calls.clear();
    migration.clear_next(&mut host).unwrap();
    let names = call_names(&host);
    assert_eq!(
        (names.first(), names.last()),
        (Some(&"lock".into()), Some(&"unlock".into()))
    );
    assert_eq!(called(&host, "unlock"), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Named difference: Node's parity check passes vacuously when an endpoint gives fewer vectors
/// than sample texts (`sims.every(Number.isFinite)` of an empty list, `Math.min()` = Infinity);
/// the port fails it closed.
#[test]
fn parity_fails_closed_without_one_vector_per_sample_text() {
    for (vectors, shown) in [
        ("[]", JValue::Null),
        ("[[1,0,0],[0,1,0]]", JValue::Num(1.0)),
    ] {
        let world = format!(
            r#"{{"indexes":[],"snapshot":null,"served":{{"{QA_SVC}":["qa-embed-a"]}},
                "vectors":{{"":{vectors},"{QA_SVC}":{vectors}}},"service":{{"managed":false}}}}"#
        );
        let (mut api, mut host) = in_memory(
            qa_target("qa-embed-a", None),
            Some(qa_target("qa-embed-a", Some(QA_SVC))),
            false,
            &world,
        );
        let refused = api.migration.switch_to_next(&mut host, false).unwrap_err();
        assert_eq!(refused.code, Some("parityFailed"), "{vectors}");
        let parity = JValue::obj([("ok", JValue::Bool(false)), ("minCosine", shown)]);
        assert_eq!(
            refused.detail.as_deref(),
            Some(&embedding_migration::Detail::Parity(parity)),
            "{vectors}"
        );
        // A forced switch still goes through (as Node's does on a failed check).
        api.migration.switch_to_next(&mut host, true).unwrap();
        assert_eq!(
            api.migration.config.active().base_url.as_deref(),
            Some(QA_SVC)
        );
    }
}

/// Finding: a truthy `status.models` that is not an array makes Node's service-switch throw
/// before it serves anything; the port answers 500 and never serves (no rollback to `[]`).
#[test]
fn service_switch_with_unreadable_models_serves_nothing() {
    for models in ["{}", r#""x""#, "7", "true"] {
        let world = format!(
            r#"{{"indexes":[],"snapshot":null,"service":{{"managed":true,"enabled":true,
                "baseUrl":"{QA_SVC}","at":["{QA_SVC}"],"known":{{"qa-embed-a":"qa/a.gguf"}},
                "installed":{{"qa-embed-b":"qa/b.gguf"}},"status":{{"models":{models}}},
                "serveFail":[],"loaded":[false]}}}}"#
        );
        let (mut api, mut host) =
            in_memory(qa_target("qa-embed-a", Some(QA_SVC)), None, false, &world);
        let body = wtf16::parse(r#"{"action":"service-switch","model":"qa-embed-b"}"#).unwrap();
        let answer = api.handle(&mut host, "POST", QA_ADMIN, Ok(&body));
        assert_eq!(answer.status, 500, "{models}");
        assert_eq!(show(&answer.body), r#"{"error":"Internal error"}"#);
        assert_eq!(called(&host, "serviceServe"), 0, "{models}");
        assert_eq!(called(&host, "serviceWaitLoaded"), 0, "{models}");
        assert_eq!(called(&host, "audit"), 0, "{models}");
    }
}

/// Finding: a Diary address `String()` cannot convert makes Node's `servedSet` throw: the sync
/// after a change serves nothing (the change and its audit record stay) and a service-switch
/// fails before serving; both answer 500.
#[test]
fn an_unreadable_diary_address_stops_the_served_set() {
    let world = format!(
        r#"{{"indexes":[],"snapshot":null,"dropFails":[],"diaryQueue":[],
            "diary":{{"GET":{{"ok":true,"status":200,"body":{{"active":{{"model":"qa-embed-b",
            "profile":"nomic-v1","base_url":{{"toString":1}}}},"next":null,"previous":null}}}}}},
            "service":{{"managed":true,"enabled":true,"baseUrl":"{QA_SVC}","at":["{QA_SVC}"],
            "known":{{"qa-embed-a":"qa/a.gguf","qa-embed-b":"qa/b.gguf","qa-embed-c":"qa/c.gguf"}},
            "installed":{{"qa-embed-c":"qa/c.gguf"}},"status":{{"models":[]}},"serveFail":[],"loaded":[]}}}}"#
    );
    let (mut api, mut host) = in_memory(
        qa_target("qa-embed-a", Some(QA_SVC)),
        Some(qa_target("qa-embed-c", Some(QA_SVC))),
        true,
        &world,
    );
    let clear = wtf16::parse(r#"{"action":"clear-next","scope":"projects"}"#).unwrap();
    let answer = api.handle(&mut host, "POST", QA_ADMIN, Ok(&clear));
    assert_eq!(
        (answer.status, show(&answer.body)),
        (500, r#"{"error":"Internal error"}"#.into())
    );
    assert!(
        api.migration.config.next().is_none(),
        "the change itself stays"
    );
    assert_eq!(called(&host, "audit"), 1);
    assert_eq!(called(&host, "serviceServe"), 0, "the sync serves nothing");
    host.calls.clear();
    let switch = wtf16::parse(r#"{"action":"service-switch","model":"qa-embed-c"}"#).unwrap();
    let answer = api.handle(&mut host, "POST", QA_ADMIN, Ok(&switch));
    assert_eq!(
        (answer.status, show(&answer.body)),
        (500, r#"{"error":"Internal error"}"#.into())
    );
    assert_eq!(
        called(&host, "serviceServe"),
        0,
        "nothing served before the throw"
    );
    // The plain `served_set` keeps its signature: a throw is "do not shrink" (None).
    let c = api.migration.status(&mut host);
    let d = host.w("diary").get("GET").get("body").clone();
    assert_eq!(Embedding::served_set(&mut host, &c, &d, true), None);
    assert!(Embedding::try_served_set(&mut host, &c, &d, true).is_err());
    // #2665: with the service at an address, `isAt` converts the Diary address too, so the set
    // with no Diary defaults throws as well (without an address it does not: the test below).
    assert!(Embedding::try_served_set(&mut host, &c, &d, false).is_err());
}

/// #2665: production's `service.isAt` converts a Diary address only once the service has an
/// address (`!!baseUrl && sameAddress(...)`). Without one, the set with no Diary defaults reads an
/// unconvertible address without throwing; the full set still throws in `diaryDefault`.
#[test]
fn is_at_converts_a_diary_address_only_when_the_service_has_an_address() {
    let world = |base: &str| {
        format!(
            r#"{{"indexes":[],"snapshot":null,"service":{{"managed":true,"enabled":true,
                "baseUrl":{base},"at":["{QA_SVC}"],"known":{{"qa-embed-a":"qa/a.gguf"}}}}}}"#
        )
    };
    let d = wtf16::parse(
        r#"{"active":{"model":"qa-embed-b","profile":"nomic-v1","base_url":{"toString":1}},
            "next":null,"previous":null}"#,
    )
    .unwrap();
    let service = format!(r#""{QA_SVC}""#);
    for (base, known_ok) in [("null", true), (r#""""#, true), (service.as_str(), false)] {
        let (api, mut host) = in_memory(
            qa_target("qa-embed-a", Some(QA_SVC)),
            None,
            false,
            &world(base),
        );
        let c = api.migration.status(&mut host);
        host.calls.clear();
        let known = Embedding::try_served_set(&mut host, &c, &d, false);
        assert_eq!(known.is_ok(), known_ok, "baseUrl {base}");
        assert_eq!(
            called(&host, "serviceIsAt"),
            2,
            "baseUrl {base}: the call is made"
        );
        assert!(
            Embedding::try_served_set(&mut host, &c, &d, true).is_err(),
            "{base}"
        );
    }
}

/// Finding: `releaseSeeded` takes back only a transient start-up refusal (also replayed from the
/// recording); a next an administrator set since is never replaced.
#[test]
fn release_seeded_takes_back_only_a_transient_refusal() {
    let active = qa_target("qa-embed-a", Some(QA_SVC));
    let next = qa_target("qa-embed-b", Some("http://qa-other.invalid:9090"));
    let state = embedding_migration::State {
        active: active.clone(),
        next: Some(next.clone()),
        previous: None,
    };
    let mut config = Config::new(state.clone(), None, false);
    config.refuse_seeded_as(Some("qa: not yet"), Some("qa: next not yet"), true);
    assert_eq!(config.active().profile, "unavailable");
    assert!(config.next().is_none());
    assert_eq!(config.damaged(), Some("qa: not yet"));
    config.release_seeded(Seeded::Next);
    assert_eq!(config.next(), Some(&next));
    config.release_seeded(Seeded::Active);
    assert_eq!((config.active(), config.damaged()), (&active, None));
    config.clear_next().unwrap(); // unlocked again
    let mut fixed = Config::new(state.clone(), None, false);
    fixed.refuse_seeded(Some("qa: refused"), Some("qa: next refused"));
    fixed.release_seeded(Seeded::Active);
    fixed.release_seeded(Seeded::Next);
    assert_eq!(
        (fixed.active().profile.as_str(), fixed.next()),
        ("unavailable", None)
    );
    let mut changed = Config::new(state, None, false);
    changed.refuse_seeded_as(None, Some("qa: next not yet"), true);
    let own = qa_target("qa-embed-c", None);
    changed.set_next(own.clone()).unwrap();
    changed.release_seeded(Seeded::Next);
    assert_eq!(changed.next(), Some(&own));
}
