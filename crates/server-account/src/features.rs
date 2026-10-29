//! Public feature owner. The live producer lease, settings, authentication and
//! mutations share the same existing SQLite writer and App controller.
use crate::{
    feature_context::{startup, BrowserFact, ChoiceReason, View},
    Account, Fault, Outcome, Reply, Request, BODY_LIMIT,
};
use js_json::JValue;
use server_auth::{Identity, Role};
use server_features::{Availability, AvailabilityKind, Evaluation, Feature};
use server_store::{Reader, RustFeatureSettings, StoreError};

pub fn owns(path: &str) -> bool {
    matches!(path, "/api/features" | "/api/admin/features")
        || path.starts_with("/api/admin/features/")
}
/// decodeURIComponent: percent bytes must form UTF-8, including encoded '/'.
pub fn decode_suffix(path: &str) -> Result<String, Fault> {
    let text = path
        .strip_prefix("/api/admin/features/")
        .ok_or(Fault::Internal)?;
    let mut bytes = Vec::with_capacity(text.len());
    let mut iter = text.bytes();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let hi = iter
                .next()
                .and_then(|v| (v as char).to_digit(16))
                .ok_or(Fault::Internal)?;
            let lo = iter
                .next()
                .and_then(|v| (v as char).to_digit(16))
                .ok_or(Fault::Internal)?;
            bytes.push((hi * 16 + lo) as u8);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).map_err(|_| Fault::Internal)
}
fn unavailable() -> StoreError {
    StoreError::NotReady("feature runtime is unavailable".into())
}
fn context_error() -> Outcome {
    Reply::json(
        503,
        &JValue::obj([
            (
                "error",
                JValue::from("Feature runtime context is unavailable."),
            ),
            ("code", JValue::from("FEATURE_RUNTIME_CONTEXT_UNAVAILABLE")),
        ]),
    )
    .into()
}
fn reason(view: &View, feature: Feature) -> Option<(&'static str, Option<&'static str>)> {
    use AvailabilityKind::*;
    match server_features::spec(feature).availability {
        Static => None,
        DecisionChoice => view.snapshot.choice.map(|why| {
            let (text,id) = match why {
                ChoiceReason::Unconfigured => ("Set up the decision service below before enabling this experiment.", "decisionSetup"),
                ChoiceReason::Unsupported => ("The configured decision service does not support choice decisions, so this experiment cannot run.", "decisionUnsupported"),
            };
            (text, matches!(feature, Feature::StepSupervision | Feature::ToolGate).then_some(id))
        }),
        SystemOne => view.snapshot.system_one_unconfigured.then_some(("Not configured. The operator needs to point it at a dedicated local decision-model endpoint, then restart noevia.", Some("systemOneUrl"))),
        Browser => match view.facts.browser {
            BrowserFact::Available => None,
            BrowserFact::MissingPlaywright => Some(("Needs Playwright installed on this server; it is not in this image.", Some("browserPlaywright"))),
            BrowserFact::MissingChromium => Some(("Needs a Chromium browser installed for Playwright on this server.", Some("browserChromium"))),
        },
        CodeSandbox => (!view.facts.code).then_some(("Needs Code mode running in the code sandbox (CODE_HARNESS_ENDPOINT).", Some("codeSandbox"))),
        TrustProxy => (!view.facts.trust_proxy).then_some(("Needs TRUST_PROXY on so sign-in limits can tell clients apart.", Some("trustProxy"))),
    }
}
pub(crate) fn evaluate(
    reader: &Reader<'_>,
    view: &View,
    feature: Feature,
) -> Result<Evaluation, StoreError> {
    let index = Feature::ALL
        .iter()
        .position(|f| *f == feature)
        .ok_or_else(unavailable)?;
    let stored = startup::selected(reader, feature)?;
    let boot = match feature {
        Feature::DiaryMcpWrite => Some(view.facts.restart[0]),
        Feature::Kiwix => Some(view.facts.restart[1]),
        _ => None,
    };
    Ok(server_features::evaluate_with_context(
        feature,
        view.facts.env.get(index).copied().flatten(),
        stored.map(|v| if v { "true" } else { "false" }),
        if reason(view, feature).is_none() {
            Availability::Available
        } else {
            Availability::Unavailable
        },
        boot,
    ))
}
fn describe(reader: &Reader<'_>, view: &View, feature: Feature) -> Result<JValue, StoreError> {
    let spec = server_features::spec(feature);
    let evaluation = evaluate(reader, view, feature)?;
    let mut fields = vec![
        ("id".into(), JValue::from(spec.name)),
        ("name".into(), JValue::from(spec.name)),
        ("label".into(), JValue::from(spec.label)),
        ("description".into(), JValue::from(spec.description)),
        ("enabled".into(), JValue::Bool(evaluation.value)),
        ("source".into(), JValue::from(evaluation.source.as_str())),
        ("locked".into(), JValue::Bool(evaluation.locked)),
        ("env".into(), JValue::from(spec.env)),
    ];
    if spec.experimental {
        fields.push(("experimental".into(), JValue::Bool(true)));
    }
    if spec.experimental || spec.availability != AvailabilityKind::Static {
        let why = reason(view, feature);
        fields.push((
            "unavailable".into(),
            why.map(|v| JValue::from(v.0)).unwrap_or(JValue::Null),
        ));
        fields.push((
            "unavailableId".into(),
            why.and_then(|v| v.1)
                .map(JValue::from)
                .unwrap_or(JValue::Null),
        ));
    }
    fields.push((
        "pendingRestart".into(),
        JValue::Bool(evaluation.pending_restart.unwrap_or(false)),
    ));
    Ok(JValue::Obj(fields))
}
pub(crate) fn refuse(why: server_auth::identity::Refusal) -> Outcome {
    use server_auth::identity::Refusal;
    match why {
        Refusal::Unauthorized => {
            let mut reply = Reply::error(401, "unauthorized");
            reply
                .headers
                .push(("WWW-Authenticate".into(), "Bearer realm=\"cowork\"".into()));
            reply.into()
        }
        Refusal::Csrf => Reply::error(403, "invalid CSRF token").into(),
        Refusal::BrowserOnly => Reply::json(
            403,
            &JValue::obj([
                (
                    "error",
                    JValue::from("This needs a signed-in browser session."),
                ),
                ("code", JValue::from("browser_session_required")),
            ]),
        )
        .into(),
    }
}
fn gate(
    acct: &Account,
    req: &Request,
    initial: &Identity,
    reader: &Reader<'_>,
    now: i64,
) -> Result<Result<Identity, Outcome>, StoreError> {
    Ok(
        match acct.auth.gate_in(reader, &req.creds, &req.path, now)? {
            Ok(current) if current.user.id != initial.user.id => {
                Err(refuse(server_auth::identity::Refusal::Unauthorized))
            }
            Ok(current) if req.path != "/api/features" && current.user.role != Role::Admin => {
                Err(Reply::error(403, "Administrator required").into())
            }
            Ok(current) => Ok(current),
            Err(why) => Err(refuse(why)),
        },
    )
}

pub(crate) fn handle(
    acct: &Account,
    req: &Request,
    initial: &Identity,
    clock: &dyn Fn() -> i64,
) -> Result<Option<Outcome>, Fault> {
    if !owns(&req.path) {
        return Ok(None);
    }
    if req.path != "/api/features" && initial.user.role != Role::Admin {
        return Ok(Some(Reply::error(403, "Administrator required").into()));
    }
    let suffix = req
        .path
        .starts_with("/api/admin/features/")
        .then(|| decode_suffix(&req.path))
        .transpose()?;
    if req.method != if suffix.is_some() { "PUT" } else { "GET" } {
        return Ok(Some(Reply::error(405, "method not allowed").into()));
    }
    // Legacy readJson failures precede feature-name/type validation.
    let body = if suffix.is_some() {
        if req.body.over || req.body.bytes.len() > BODY_LIMIT {
            return Ok(Some(Reply::error(400, "invalid JSON").into()));
        }
        let text = String::from_utf8_lossy(&req.body.bytes);
        match crate::framing_preferences::project_booleans(
            if text.is_empty() { "{}" } else { &text },
            &["enabled"],
        ) {
            Ok(body) => body,
            Err(()) => return Ok(Some(Reply::error(400, "invalid JSON").into())),
        }
    } else {
        JValue::Null
    };
    let Some(controller) = acct.feature_context.as_ref() else {
        return Ok(Some(context_error()));
    };
    if let Some(name) = suffix {
        let feature = Feature::from_name(if name == "astraReview" {
            "plannerReview"
        } else {
            &name
        });
        let Some(feature) = feature else {
            return Ok(Some(Reply::error(404, "Unknown feature").into()));
        };
        let JValue::Bool(enabled) = body.get("enabled") else {
            return Ok(Some(
                Reply::error(400, "enabled must be true or false").into(),
            ));
        };
        let admission = match acct.writer.read(|reader| controller.admission(reader)) {
            Ok(value) => value,
            Err(_) => return Ok(Some(context_error())),
        };
        // Error replies must ROLLBACK, not commit and increment generation.
        let mut refusal = None;
        let result = controller.mutate(&acct.writer, admission, feature == Feature::NativeClientAuth && !enabled, |tx| {
            let reader = tx.reader();
            let actor = match gate(acct, req, initial, &reader, clock())? {
                Ok(actor) => actor,
                Err(reply) => { refusal = Some(reply); return Err(unavailable()); }
            };
            let view = controller.view(&reader)?;
            let spec = server_features::spec(feature);
            let evaluation = evaluate(&reader, &view, feature)?;
            if evaluation.locked {
                refusal = Some(Reply::error(409, &format!("Set by the operator ({}); change it in the deployment configuration.", spec.env)).into());
                return Err(unavailable());
            }
            if *enabled {
                if let Some((why,_)) = reason(&view, feature) {
                    refusal = Some(Reply::error(409, why).into()); return Err(unavailable());
                }
            }
            tx.set_feature_enabled(RustFeatureSettings::owned(), feature, *enabled)?;
            crate::users::audit(tx, "feature.set", Some(&actor.user.id), None, &JValue::obj([
                ("name", JValue::from(spec.name)), ("enabled", JValue::Bool(*enabled)),
            ]), clock())?;
            if feature == Feature::NativeClientAuth && !enabled { startup::revoke_all(tx, Some(&actor.user.id), clock())?; }
            let record = describe(&tx.reader(), &view, feature)?;
            if let Err(reply) = gate(acct, req, initial, &tx.reader(), clock())? {
                refusal = Some(reply); return Err(unavailable());
            }
            Ok(Reply::json(200, &record).into())
        });
        return Ok(Some(match result {
            Ok(reply) => reply,
            Err(_) if refusal.is_some() => refusal.unwrap_or_else(context_error),
            Err(StoreError::Sqlite(_)) => Reply::error(500, "Could not save the feature").into(),
            Err(_) => context_error(),
        }));
    }
    let result = acct.writer.read(|reader| {
        if let Err(reply) = gate(acct, req, initial, reader, clock())? {
            return Ok(reply);
        }
        let view = controller.view(reader)?;
        let body = if req.path == "/api/features" {
            let mut flags = Vec::new();
            for feature in Feature::ALL {
                flags.push((
                    server_features::spec(feature).name.into(),
                    JValue::Bool(evaluate(reader, &view, feature)?.enabled),
                ));
            }
            JValue::obj([("flags", JValue::Obj(flags))])
        } else {
            let descriptors = Feature::ALL
                .into_iter()
                .map(|f| describe(reader, &view, f))
                .collect::<Result<Vec<_>, _>>()?;
            JValue::obj([("features", JValue::Arr(descriptors))])
        };
        if let Err(reply) = gate(acct, req, initial, reader, clock())? {
            return Ok(reply);
        }
        if controller.admission(reader)? != view.admission {
            return Err(unavailable());
        }
        Ok(Reply::json(200, &body).into())
    });
    Ok(Some(result.unwrap_or_else(|_| context_error())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature_context::{
        source_fingerprint, BootFacts, Cancellation, ContextSource, Control, Controller,
        FeatureRuntime, Registration, Snapshot,
    };
    use crate::{Body, Settings};
    use server_auth::{AuthConfig, Authenticator, Creds};
    use server_store::{rusqlite::Connection, RustAuth, Writer};
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    use uuid::Uuid;
    type Error = Box<dyn std::error::Error>;
    const T0: i64 = 1_800_000_000_000;
    const UID: &str = "00000000-0000-4000-8000-000000000001";
    const ORIGIN: &str = "https://noevia.example.test";
    struct Fixture {
        _dir: tempfile::TempDir,
        node: Connection,
        acct: Account,
        req: Request,
        initial: Identity,
    }
    fn fixture() -> Result<Fixture, Error> {
        fixture_for(None)
    }
    fn fixture_for(input: Option<&serde_json::Value>) -> Result<Fixture, Error> {
        let dir = tempfile::tempdir()?;
        let node = Connection::open(server_store::db_path(dir.path()))?;
        node.execute_batch(include_str!(
            "../../server-store/tests/fixtures/node-schema.sql"
        ))?;
        node.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,'Synthetic','synthetic','Synthetic','admin','unused','synthetic-wa',?2,?2)", (UID,T0))?;
        node.execute("INSERT INTO sessions(id_hash,user_id,csrf_hash,created_at,last_seen_at,expires_at,user_agent,ip) VALUES(?1,?2,?3,?4,?4,?5,'Synthetic','127.0.0.1')",
            (server_auth::js::digest("synthetic-session"),UID,server_auth::js::digest("synthetic-csrf"),T0,T0+86_400_000))?;
        if let Some(seed) = input
            .and_then(|v| v.get("seed"))
            .and_then(serde_json::Value::as_object)
        {
            for (key, value) in seed {
                node.execute(
                    "INSERT INTO settings(key,value) VALUES(?1,?2)",
                    (key, value.as_str().ok_or("seed")?),
                )?;
            }
        }
        if input
            .and_then(|v| v.get("fail"))
            .and_then(serde_json::Value::as_str)
            == Some("migration")
        {
            node.execute_batch("CREATE TRIGGER reject_migration BEFORE INSERT ON settings WHEN NEW.key='feature:plannerReview' BEGIN SELECT RAISE(ABORT,'synthetic migration failure'); END;")?;
        }
        let switch = RustAuth::owned();
        let writer = Arc::new(Writer::open(dir.path(), switch)?);
        let auth = Arc::new(Authenticator::new(AuthConfig {
            public_origin: ORIGIN.into(),
            additional_origins: vec![],
            legacy_token: String::new(),
            legacy_compat: false,
            trust_proxy: false,
            native_client_auth_env: Some(false),
            origin_from_settings: false,
        }));
        let settings = Settings {
            public_origin: ORIGIN.into(),
            webauthn_rp_id: String::new(),
            trust_proxy: false,
            data_dir: dir.path().to_path_buf(),
            dav_available: false,
            dav: crate::DavEnv::default(),
        };
        let mut env = [None; 23];
        for (slot, feature) in env.iter_mut().zip(Feature::ALL) {
            *slot = server_features::parse_env(
                input
                    .and_then(|v| v.get("env"))
                    .and_then(|v| v.get(server_features::spec(feature).env))
                    .and_then(serde_json::Value::as_str),
            )
            .map_err(str::to_owned)?;
        }
        let controller = Arc::new(Controller::new(Arc::new(FeatureRuntime::new()?), env));
        let cancellation = Cancellation::default();
        let control = Control {
            request: Uuid::from_u128(3),
            deadline: Instant::now() + Duration::from_secs(30),
            cancelled: cancellation.signal(),
        };
        let mut facts = BootFacts {
            env,
            restart: if input.is_some() {
                ["feature:diaryMcpWrite", "feature:kiwix"].map(|key| {
                    input
                        .and_then(|v| v.get("seed"))
                        .and_then(|v| v.get(key))
                        .and_then(serde_json::Value::as_str)
                        == Some("true")
                })
            } else {
                [false, true]
            },
            browser: BrowserFact::MissingChromium,
            code: false,
            trust_proxy: true,
        };
        let unavailable = input
            .and_then(|v| v.get("unavailable"))
            .and_then(serde_json::Value::as_str);
        if input.is_some() {
            facts.browser = if unavailable == Some("browserExecutor") {
                BrowserFact::MissingPlaywright
            } else {
                BrowserFact::Available
            };
            facts.code = !matches!(unavailable, Some("codePipeline" | "codeMerge"));
            facts.trust_proxy = unavailable != Some("nativeClientAuth");
        }
        let snapshot = writer.read(|r| {
            Ok(Snapshot {
                decision: source_fingerprint(r, ContextSource::Decision)?,
                machines: source_fingerprint(r, ContextSource::InferenceMachines)?,
                choice: (input.is_none()
                    || matches!(
                        unavailable,
                        Some("stepSupervision" | "toolGate" | "chatFraming" | "framingReasoner")
                    ))
                .then_some(ChoiceReason::Unconfigured),
                system_one_unconfigured: input.is_none() || unavailable == Some("systemOneRouting"),
            })
        })?;
        let registered = controller.register(
            &writer,
            &control,
            Registration {
                boot: Uuid::from_u128(1),
                session: Uuid::from_u128(2),
                facts: facts.clone(),
                snapshot,
            },
            |tx| startup::initialize(tx, &facts, T0),
        )?;
        assert!(matches!(
            registered,
            crate::feature_context::Reply::Ready(_)
        ));
        let acct =
            Account::new(writer, auth, settings, switch, T0).with_feature_context(controller);
        let req = Request {
            method: "PUT".into(),
            path: "/api/admin/features/previews".into(),
            creds: Creds {
                cookie: Some("cowork_session=synthetic-session; cowork_csrf=synthetic-csrf".into()),
                csrf: Some("synthetic-csrf".into()),
                origin: Some(ORIGIN.into()),
                method: "PUT".into(),
                authorization: None,
            },
            user_agent: "Synthetic".into(),
            content_type: "application/json".into(),
            client_ip: "127.0.0.1".into(),
            body: Body {
                bytes: br#"{"enabled":true}"#.to_vec(),
                over: false,
            },
        };
        std::fs::create_dir_all(dir.path().join("users").join(UID))?;
        let initial = acct
            .writer
            .read(|r| acct.auth.authenticate_in(r, &req.creds, T0))?
            .ok_or("no synthetic identity")?;
        Ok(Fixture {
            _dir: dir,
            node,
            acct,
            req,
            initial,
        })
    }

    fn call(f: &Fixture) -> Result<Reply, Error> {
        match handle(&f.acct, &f.req, &f.initial, &|| T0)
            .map_err(|e| format!("{e:?}"))?
            .ok_or("not owned")?
        {
            Outcome::Reply(reply) => Ok(reply),
            _ => Err("passed".into()),
        }
    }
    #[test]
    fn frozen_original_routes_cover_all_160_cases_with_explicit_native_deltas() -> Result<(), Error>
    {
        let corpus: serde_json::Value = serde_json::from_str(include_str!(
            "../../server-features/tests/fixtures/core-feature-http-v1.json"
        ))?;
        let cases = corpus
            .get("cases")
            .and_then(serde_json::Value::as_array)
            .ok_or("cases")?;
        assert_eq!(cases.len(), 160);
        for case in cases {
            let input = case.get("input").ok_or("input")?;
            if case.get("id").and_then(serde_json::Value::as_str) == Some("invalid-startup-env") {
                assert!(fixture_for(Some(input)).is_err());
                continue;
            }
            let mut f = fixture_for(Some(input))?;
            let text = |key: &str| {
                input
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .ok_or("text")
            };
            f.req.path = text("path")?.into();
            f.req.method = text("method")?.into();
            f.req.creds.method = f.req.method.clone();
            f.req.body.bytes = text("raw")?.as_bytes().to_vec();
            if text("role")? == "member" {
                f.node.execute("UPDATE users SET role='member'", [])?;
                f.initial.user.role = Role::Member;
                f.initial.account_role = Role::Member;
            }
            match input.get("bodyMode").and_then(serde_json::Value::as_str) {
                Some("overflow" | "stream-error") => f.req.body.over = true,
                Some("invalid-utf8") => f.req.body.bytes = vec![255],
                _ => {}
            }
            let failure = input.get("fail").and_then(serde_json::Value::as_str);
            match failure {
                Some("set")=>f.node.execute_batch("CREATE TRIGGER reject_setting BEFORE INSERT ON settings WHEN NEW.key='feature:plannerReview' BEGIN SELECT RAISE(ABORT,'synthetic write failure'); END;")?,
                Some("audit")=>f.node.execute_batch("CREATE TRIGGER reject_audit BEFORE INSERT ON audit_events BEGIN SELECT RAISE(ABORT,'synthetic audit failure'); END;")?,
                _=>{},
            }
            if input.get("noStore").and_then(serde_json::Value::as_bool) == Some(true) {
                f.acct.feature_context = None;
            }
            let actual = match handle(&f.acct, &f.req, &f.initial, &|| T0) {
                Ok(Some(Outcome::Reply(reply))) => Some(reply),
                Ok(None) => None,
                Err(Fault::Internal) => Some(Reply::error(500, "Internal error")),
                other => return Err(format!("unexpected {other:?} for {input}").into()),
            };
            if input.get("noStore").and_then(serde_json::Value::as_bool) == Some(true) {
                // Native context availability is explicit, never an in-memory settings fallback.
                assert_eq!(actual.ok_or("reply")?.status, 503);
                continue;
            }
            if failure == Some("onChange") {
                // The oracle throws an arbitrary injected callback after committing plannerReview.
                // Native owns explicit device-OFF effects only; there is no arbitrary callback.
                assert_eq!(actual.ok_or("reply")?.status, 200);
                continue;
            }
            match actual {
                Some(reply) => {
                    assert_eq!(
                        Some(u64::from(reply.status)),
                        case.get("status").and_then(serde_json::Value::as_u64),
                        "{input}"
                    );
                    let body: serde_json::Value = serde_json::from_str(&reply.body)?;
                    assert_eq!(Some(&body), case.get("body"), "{input}");
                }
                None => assert_eq!(
                    case.get("handled").and_then(serde_json::Value::as_bool),
                    Some(false),
                    "{input}"
                ),
            }
            if failure == Some("audit") {
                assert_eq!(
                    f.node.query_row(
                        "SELECT count(*) FROM settings WHERE key='feature:plannerReview'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    0,
                    "native audit failure rolls back the original partial write"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn descriptors_flags_alias_and_closed_dynamic_reasons() -> Result<(), Error> {
        let mut f = fixture()?;
        f.req.path = "/api/admin/features".into();
        f.req.method = "GET".into();
        f.req.creds.method = "GET".into();
        let reply = call(&f)?;
        assert_eq!(reply.status, 200);
        let body: serde_json::Value = serde_json::from_str(&reply.body)?;
        let rows = body
            .get("features")
            .and_then(serde_json::Value::as_array)
            .ok_or("no rows")?;
        assert_eq!(rows.len(), 23);
        let by = |name: &str| rows.iter().find(|r| r["name"] == name).ok_or("missing");
        assert_eq!(by("kiwix")?["pendingRestart"], true);
        assert_eq!(by("stepSupervision")?["unavailableId"], "decisionSetup");
        assert_eq!(by("chatFraming")?["unavailableId"], serde_json::Value::Null);
        assert_eq!(by("browserExecutor")?["unavailableId"], "browserChromium");
        f.req.path = "/api/features".into();
        let flags: serde_json::Value = serde_json::from_str(&call(&f)?.body)?;
        assert_eq!(
            flags
                .get("flags")
                .and_then(serde_json::Value::as_object)
                .ok_or("flags")?
                .len(),
            23
        );
        assert_eq!(
            flags
                .get("flags")
                .and_then(|v| v.get("kiwix"))
                .ok_or("kiwix")?,
            true
        );
        f.req.path = "/api/admin/features/astraReview".into();
        f.req.method = "PUT".into();
        f.req.creds.method = "PUT".into();
        assert_eq!(call(&f)?.status, 200);
        assert_eq!(
            f.node.query_row(
                "SELECT value FROM settings WHERE key='feature:plannerReview'",
                [],
                |r| r.get::<_, String>(0)
            )?,
            "true"
        );
        f.req.path = "/api/admin/features/toolGate".into();
        assert_eq!(call(&f)?.status, 409);
        Ok(())
    }
    #[test]
    fn javascript_projection_and_route_error_precedence() -> Result<(), Error> {
        let mut f = fixture()?;
        for (path, method, body, status) in [
            ("/api/admin/features/%", "GET", "", 500),
            ("/api/admin/features/unknown", "PUT", "{", 400),
            ("/api/admin/features/unknown", "PUT", "{}", 404),
            ("/api/admin/features/previews", "PUT", "", 400),
            (
                "/api/admin/features/previews",
                "PUT",
                r#"{"ignored":"\ud800","n":1e999,"enabled":false,"enabled":true}"#,
                200,
            ),
        ] {
            f.req.path = path.into();
            f.req.method = method.into();
            f.req.creds.method = method.into();
            f.req.body.bytes = body.as_bytes().to_vec();
            let status_actual = match handle(&f.acct, &f.req, &f.initial, &|| T0) {
                Err(Fault::Internal) => 500,
                Ok(Some(Outcome::Reply(r))) => r.status,
                other => return Err(format!("unexpected {other:?}").into()),
            };
            assert_eq!(status_actual, status, "{path}");
        }
        assert!(owns("/api/admin/features/"));
        assert!(!owns("/api/features/"));
        assert!(decode_suffix("/api/admin/features/%ED%A0%80").is_err());
        assert_eq!(
            decode_suffix("/api/admin/features/%70reviews").map_err(|e| format!("{e:?}"))?,
            "previews"
        );
        Ok(())
    }
    #[test]
    fn off_commit_is_atomic_and_invalidates_admission_only_on_commit() -> Result<(), Error> {
        let mut f = fixture()?;
        f.req.path = "/api/admin/features/nativeClientAuth".into();
        f.req.body.bytes = br#"{"enabled":false}"#.to_vec();
        let controller = f.acct.feature_context.as_ref().ok_or("context")?;
        let before = f.acct.writer.read(|r| controller.admission(r))?;
        f.node.execute("INSERT INTO device_authorizations(device_code_hash,user_code_hash,client_name,ip,user_agent,status,created_at,expires_at,interval_ms,last_poll_at,user_id) VALUES('d','u','Synthetic','127.0.0.1','Synthetic','pending',?1,?2,5000,NULL,NULL)",(T0,T0+60_000))?;
        f.node.execute_batch("CREATE TRIGGER reject_feature_audit BEFORE INSERT ON audit_events WHEN NEW.action='feature.set' BEGIN SELECT RAISE(ABORT,'synthetic audit failure'); END;")?;
        assert_eq!(call(&f)?.status, 500);
        assert_eq!(
            f.node
                .query_row("SELECT count(*) FROM device_authorizations", [], |r| r
                    .get::<_, i64>(0))?,
            1
        );
        assert!(f.acct.writer.read(|r| controller.admission(r))? == before);
        assert_eq!(
            f.node.query_row(
                "SELECT count(*) FROM settings WHERE key='feature:nativeClientAuth'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        f.node.execute_batch("DROP TRIGGER reject_feature_audit")?;
        assert_eq!(call(&f)?.status, 200);
        assert_eq!(
            f.node
                .query_row("SELECT count(*) FROM device_authorizations", [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
        assert!(f.acct.writer.read(|r| controller.admission(r))? != before);
        assert_eq!(
            f.node.query_row(
                "SELECT count(*) FROM audit_events WHERE action='feature.set'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            1
        );
        Ok(())
    }
    #[test]
    fn auth_expiry_after_sql_rolls_back_feature_audit_and_off_generation() -> Result<(), Error> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut f = fixture()?;
        f.req.path = "/api/admin/features/nativeClientAuth".into();
        f.req.body.bytes = br#"{"enabled":false}"#.to_vec();
        let controller = f.acct.feature_context.as_ref().ok_or("context")?;
        let before = f.acct.writer.read(|r| controller.admission(r))?;
        let samples = AtomicUsize::new(0);
        let outcome = handle(&f.acct, &f.req, &f.initial, &|| {
            if samples.fetch_add(1, Ordering::SeqCst) >= 2 {
                T0 + 86_400_001
            } else {
                T0
            }
        })
        .map_err(|e| format!("{e:?}"))?
        .ok_or("reply")?;
        assert!(matches!(outcome, Outcome::Reply(Reply { status: 401, .. })));
        assert_eq!(
            f.node.query_row(
                "SELECT count(*) FROM settings WHERE key='feature:nativeClientAuth'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        assert_eq!(
            f.node.query_row(
                "SELECT count(*) FROM audit_events WHERE action='feature.set'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        assert!(f.acct.writer.read(|r| controller.admission(r))? == before);
        Ok(())
    }
    #[test]
    fn public_entry_counts_expiry_while_waiting_for_actual_writer() -> Result<(), Error> {
        use std::sync::{
            atomic::{AtomicI64, AtomicUsize, Ordering},
            Barrier,
        };
        let f = fixture()?;
        f.node
            .execute("UPDATE sessions SET expires_at=?1", [T0 + 100])?;
        let clock = AtomicI64::new(T0);
        let samples = AtomicUsize::new(0);
        let entered = Barrier::new(2);
        std::thread::scope(|scope| -> Result<(), Error> {
            let mut worker = None;
            f.acct.writer.write(|_| {
                let acct = &f.acct;
                let req = &f.req;
                let clock = &clock;
                let samples = &samples;
                let entered = &entered;
                worker = Some(scope.spawn(move || {
                    acct.handle_with_clock(req, || {
                        let value = clock.load(Ordering::SeqCst);
                        if samples.fetch_add(1, Ordering::SeqCst) == 0 {
                            entered.wait();
                        }
                        value
                    })
                }));
                entered.wait();
                clock.store(T0 + 101, Ordering::SeqCst);
                Ok(())
            })?;
            let result = worker.ok_or("worker")?.join().map_err(|_| "panic")?;
            assert!(matches!(result, Outcome::Reply(Reply { status: 401, .. })));
            Ok(())
        })?;
        assert_eq!(
            f.node.query_row(
                "SELECT count(*) FROM settings WHERE key='feature:previews'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        Ok(())
    }

    #[test]
    fn changed_source_epoch_missing_context_and_expired_credentials_never_succeed(
    ) -> Result<(), Error> {
        let mut f = fixture()?;
        f.node.execute(
            "INSERT INTO settings(key,value) VALUES('decision:configuration','{}')",
            [],
        )?;
        assert_eq!(call(&f)?.status, 503);
        f.node.execute(
            "DELETE FROM settings WHERE key='decision:configuration'",
            [],
        )?;
        f.node.execute("UPDATE sessions SET expires_at=?1", [T0])?;
        assert_eq!(call(&f)?.status, 401);
        f.node
            .execute("UPDATE sessions SET expires_at=?1", [T0 + 60_000])?;
        f.acct.feature_context = None;
        assert_eq!(call(&f)?.status, 503);
        Ok(())
    }
}
