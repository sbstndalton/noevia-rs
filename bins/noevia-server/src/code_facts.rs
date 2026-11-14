//! Track R11: the front answers for itself what core says on `POST /internal/code/engine` for one
//! Code start (crates/engine-facts, recorded from core), behind `NOEVIA_RUST_ENGINE_FACTS`, which is
//! off by default:
//!
//! - unset (or any other value): core's answer, exactly as before;
//! - `shadow`: core's answer is used; the front also builds its own from the same start and logs
//!   one `[code]` line saying whether they agree and, if not, which fields differ (field names
//!   only: an engine key is never logged);
//! - `rust`: the front's answer is used. The tune state (`paused`, core's
//!   `modelManager.tuningActive()`: the maintenance hold, calibration and auto-tune) is still core's,
//!   asked over the same bridge only when the Code role is on this machine, until the auto-tune
//!   runner's track publishes it in the front; core unreachable is the same 503 as today.
//!
//! What the front reads for its answer: the deployment's default provider row from the environment
//! (it always leads the provider view, so no provider file is read), the account's
//! `auto-roles.json`, the no-model-selected default the front keeps (model_maint `last_loaded`, which
//! the front adopts on its own installed reads while core also adopts on its own: a known source
//! of shadow differences when no Code or Smart role is set), the inference machines registry
//! (`chatTarget` for Code, then Smart), the features codePipeline and plannerReview, and the
//! project's stored record (its chats for the shared context).
use crate::{auto_roles_routes, inference_machines as registry, machine_service, App};
use engine_facts::code::{self, Answer, CodeEnv, Deployment, Inputs};
use engine_facts::provider;
use js_json::{wtf16, JValue};
use std::sync::Arc;

/// The switch.
pub const SWITCH: &str = "NOEVIA_RUST_ENGINE_FACTS";

/// Where one start's engine facts come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// core's bridge, as before.
    #[default]
    Core,
    /// core's bridge, compared with the front's own answer.
    Shadow,
    /// the front's own answer (the tune state still core's).
    Rust,
}

/// The switch and what the front's answer reads of the environment (read once, as core reads it
/// at its start).
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub mode: Mode,
    pub deployment: Option<Deployment>,
    pub env: CodeEnv,
}

impl Settings {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Settings {
        let mode = match get(SWITCH).as_deref() {
            Some("shadow") => Mode::Shadow,
            Some("rust") => Mode::Rust,
            _ => Mode::Core,
        };
        Settings {
            mode,
            deployment: Some(Deployment::from_lookup(get)),
            env: CodeEnv::from_lookup(get),
        }
    }
}

/// core's native machines client: a target is `null` or an object whose machine id, address and
/// model are text and whose port is an integer; anything else (and a registry that cannot be read)
/// is core's `machinesUnavailable`.
fn checked(target: serde_json::Value) -> Result<JValue, ()> {
    if target.is_null() {
        return Ok(JValue::Null);
    }
    let text = |at: &str| target.pointer(at).is_some_and(serde_json::Value::is_string);
    let ok = text("/machine/id")
        && text("/baseUrl")
        && text("/model")
        && target.get("port").is_some_and(|p| {
            p.is_i64() || p.is_u64() || p.as_f64().is_some_and(|p| p.fract() == 0.0)
        });
    if !ok {
        return Err(());
    }
    wtf16::parse(&target.to_string()).map_err(|_| ())
}

/// The account's roles file, `null` when it does not read (core's `readJson(file, null)`).
async fn roles(app: &Arc<App>, user: &str) -> JValue {
    let Some(data_dir) = app.config.data_dir.clone() else {
        return JValue::Null;
    };
    let user = user.to_string();
    tokio::task::spawn_blocking(move || {
        server_projects::store::user_dir(&data_dir, &user)
            .map(|dir| auto_roles_routes::read_roles(&dir))
            .unwrap_or(JValue::Null)
    })
    .await
    .unwrap_or(JValue::Null)
}

/// What the front's answer was built from, besides the project and the tune state.
struct Read {
    machines: Option<serde_json::Value>,
    roles: JValue,
    last_loaded: JValue,
    role_wanted: bool,
}

async fn read(app: &Arc<App>, user: &str) -> Read {
    use server_features::Feature;
    Read {
        machines: machine_service::read(app).await.ok(),
        roles: roles(app, user).await,
        last_loaded: app
            .config
            .maint
            .last_loaded()
            .map_or(JValue::Null, JValue::Str),
        role_wanted: crate::code_routes::feature(app, Feature::CodePipeline)
            || crate::code_routes::feature(app, Feature::PlannerReview),
    }
}

impl Read {
    fn target(&self, role: &str) -> Result<JValue, ()> {
        checked(registry::target(self.machines.as_ref().ok_or(())?, role))
    }

    /// Whether the Code role is on another machine (`Err`: the registry cannot say).
    fn remote(&self) -> Result<bool, ()> {
        code::code_target(|r| self.target(r), || Some(self.roles.clone())).map(|t| t.truthy())
    }

    fn answer(&self, settings: &Settings, record: &JValue, paused: JValue) -> Answer {
        let Some(deployment) = settings.deployment.as_ref() else {
            return Answer::Unavailable("unconfigured");
        };
        let row = deployment.provider();
        let inputs = Inputs {
            env: &settings.env,
            provider: &row,
            external: provider::is_external(Some(&row)),
            roles: &self.roles,
            last_loaded: &self.last_loaded,
            role_wanted: self.role_wanted,
        };
        code::answer(Some(record), &inputs, |r| self.target(r), || paused)
    }
}

fn log(entry: &JValue) {
    println!("[code] {}", wtf16::stringify(entry).unwrap_or_default());
}

/// The names of the fields where two answers differ (`engine.model`, `context`, ...), never their
/// values.
pub fn differing(core: &JValue, front: &JValue) -> Vec<String> {
    let keys = |v: &JValue| -> Vec<String> {
        match v {
            JValue::Obj(items) => items.iter().map(|(k, _)| k.clone()).collect(),
            _ => Vec::new(),
        }
    };
    let mut names = keys(core);
    for k in keys(front) {
        if !names.contains(&k) {
            names.push(k);
        }
    }
    let mut out = Vec::new();
    for k in names {
        let (a, b) = (core.get(&k), front.get(&k));
        if a == b {
            continue;
        }
        match (a, b) {
            (JValue::Obj(_), JValue::Obj(_)) => {
                out.extend(differing(a, b).into_iter().map(|f| format!("{k}.{f}")));
            }
            _ => out.push(k),
        }
    }
    out
}

/// `shadow`: builds the front's answer for the start core has just answered and logs whether they
/// agree. Core's tune state is taken as it is (the front does not read it).
pub async fn shadow(app: &Arc<App>, user: &str, record: &JValue, core: &JValue) {
    let settings = &app.config.code.facts;
    let r = read(app, user).await;
    let front = match r.answer(settings, record, core.get("paused").clone()) {
        Answer::Facts(f) => f,
        Answer::NotFound => JValue::from("notFound"),
        Answer::Unavailable(reason) => JValue::from(format!("unavailable:{reason}")),
    };
    let fields = differing(core, &front);
    log(&JValue::obj([
        ("event", JValue::from("code.engine_facts")),
        ("mode", JValue::from("shadow")),
        ("same", JValue::Bool(fields.is_empty())),
        (
            "differs",
            JValue::Arr(fields.into_iter().map(JValue::Str).collect()),
        ),
    ]));
}

/// Why the front's own answer could not be given (the start answers core's 503).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failed {
    /// core was asked for the tune state and did not answer.
    Core,
    /// the answer itself is core's 503 (logged with core's reason).
    Unavailable,
}

/// `rust`: the front's answer. `core_paused` asks core's bridge for the tune state; it is asked
/// only when the Code role is on this machine.
pub async fn native<F, Fut>(
    app: &Arc<App>,
    user: &str,
    record: &JValue,
    core_paused: F,
) -> Result<JValue, Failed>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Option<JValue>>,
{
    let settings = &app.config.code.facts;
    let r = read(app, user).await;
    let paused = match r.remote() {
        Ok(false) => core_paused().await.ok_or(Failed::Core)?,
        _ => JValue::Bool(false),
    };
    match r.answer(settings, record, paused) {
        Answer::Facts(f) => Ok(f),
        Answer::NotFound => Err(Failed::Unavailable),
        Answer::Unavailable(reason) => {
            // core's own line (rust-code-executor.cjs): the reason, never an input.
            println!("[code] engine facts failed: {reason}");
            Err(Failed::Unavailable)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(text: &str) -> JValue {
        wtf16::parse(text).unwrap_or(JValue::Undefined)
    }

    #[test]
    fn the_switch_is_off_unless_named() {
        let mode = |v: Option<&str>| {
            let v = v.map(str::to_string);
            Settings::from_lookup(&move |k: &str| (k == SWITCH).then(|| v.clone()).flatten()).mode
        };
        assert_eq!(mode(None), Mode::Core);
        assert_eq!(mode(Some("")), Mode::Core);
        assert_eq!(mode(Some("on")), Mode::Core);
        assert_eq!(mode(Some("Rust")), Mode::Core);
        assert_eq!(mode(Some("shadow")), Mode::Shadow);
        assert_eq!(mode(Some("rust")), Mode::Rust);
    }

    #[test]
    fn differences_are_named_by_field_never_by_value() {
        let core = j(
            r#"{"engine":{"baseUrl":"http://qa.invalid/v1","apiKey":"qa-secret-a","model":"qa-a","contextTokens":null},"machine":false,"paused":false,"context":""}"#,
        );
        let front = j(
            r#"{"engine":{"baseUrl":"http://qa.invalid/v1","apiKey":"qa-secret-b","model":"qa-b","contextTokens":null},"machine":false,"paused":false,"context":"x","role":null}"#,
        );
        let d = differing(&core, &front);
        assert_eq!(d, ["engine.apiKey", "engine.model", "context", "role"]);
        assert!(d.iter().all(|f| !f.contains("qa-secret")));
        assert!(differing(&core, &core).is_empty());
        assert_eq!(
            differing(&core, &JValue::from("unavailable:TypeError")).len(),
            4
        );
    }

    #[test]
    fn a_target_must_have_core_s_shape() {
        use serde_json::json;
        assert_eq!(checked(json!(null)), Ok(JValue::Null));
        let good = json!({"role":"code","machine":{"id":"qa-m"},"port":8080,"model":"qa","baseUrl":"http://qa.invalid:8080"});
        assert!(checked(good.clone()).is_ok_and(|t| t.truthy()));
        for broken in [
            json!({"machine":{"id":"qa-m"},"port":8080,"model":null,"baseUrl":"http://qa.invalid:8080"}),
            json!({"machine":{"id":"qa-m"},"port":"8080","model":"qa","baseUrl":"http://qa.invalid:8080"}),
            json!({"machine":{"id":"qa-m"},"port":80.5,"model":"qa","baseUrl":"http://qa.invalid:8080"}),
            json!({"machine":{},"port":8080,"model":"qa","baseUrl":"http://qa.invalid:8080"}),
            json!("qa"),
        ] {
            assert_eq!(checked(broken), Err(()));
        }
    }
}
