//! Closed private context protocol shared with Core's existing store client.
//! HTTP token/admission/body timeout and the Account's migration/revocation
//! callback remain the caller's responsibilities; this opens no listener.
use super::{
    unavailable, BootFacts, BrowserFact, ChoiceReason, Control, Controller, Registration, Reply,
    Snapshot,
};
use serde::Deserialize;
use serde_json::json;
use server_store::{StoreError, WriteTx, Writer};
use uuid::Uuid;

pub const BODY_LIMIT: usize = 32 * 1024;
const MAX_SEQUENCE: u64 = 9_007_199_254_740_991;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Facts {
    env: [Option<bool>; 23],
    restart: [bool; 2],
    browser: Browser,
    code: bool,
    trust_proxy: bool,
}
#[derive(Deserialize)]
enum Browser {
    #[serde(rename = "available")]
    Available,
    #[serde(rename = "playwright")]
    Playwright,
    #[serde(rename = "chromium")]
    Chromium,
}
impl Facts {
    fn into_native(self) -> BootFacts {
        BootFacts {
            env: self.env,
            restart: self.restart,
            browser: match self.browser {
                Browser::Available => BrowserFact::Available,
                Browser::Playwright => BrowserFact::MissingPlaywright,
                Browser::Chromium => BrowserFact::MissingChromium,
            },
            code: self.code,
            trust_proxy: self.trust_proxy,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sources {
    decision: String,
    machines: String,
}
#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
enum Choice {
    #[serde(rename = "decision-service-unconfigured")]
    Unconfigured,
    #[serde(rename = "choice-unsupported")]
    Unsupported,
}
#[derive(Deserialize)]
enum SystemOne {
    #[serde(rename = "system-one-unconfigured")]
    Unconfigured,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Availability {
    #[serde(deserialize_with = "nullable")]
    step_supervision: Option<Choice>,
    #[serde(deserialize_with = "nullable")]
    tool_gate: Option<Choice>,
    #[serde(deserialize_with = "nullable")]
    chat_framing: Option<Choice>,
    #[serde(deserialize_with = "nullable")]
    framing_reasoner: Option<Choice>,
    #[serde(deserialize_with = "nullable")]
    system_one_routing: Option<SystemOne>,
}
fn nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Context {
    sources: Sources,
    availability: Availability,
}
impl Context {
    fn into_native(self) -> Result<Snapshot, StoreError> {
        let a = self.availability;
        if [a.tool_gate, a.chat_framing, a.framing_reasoner]
            .iter()
            .any(|v| *v != a.step_supervision)
        {
            return Err(unavailable());
        }
        let snapshot = Snapshot {
            decision: self.sources.decision,
            machines: self.sources.machines,
            choice: a.step_supervision.map(|v| match v {
                Choice::Unconfigured => ChoiceReason::Unconfigured,
                Choice::Unsupported => ChoiceReason::Unsupported,
            }),
            system_one_unconfigured: a.system_one_routing.is_some(),
        };
        snapshot.validate()?;
        Ok(snapshot)
    }
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
enum Message {
    Hello {
        request: String,
        start: String,
    },
    Register {
        request: String,
        start: String,
        incarnation: String,
        boot: String,
        session: String,
        sequence: u64,
        facts: Facts,
        snapshot: Context,
    },
    Renew {
        request: String,
        start: String,
        incarnation: String,
        boot: String,
        session: String,
        sequence: u64,
        snapshot: Context,
    },
    Release {
        request: String,
        start: String,
        incarnation: String,
        boot: String,
        session: String,
        sequence: u64,
    },
}
fn uuid(value: &str) -> Result<Uuid, StoreError> {
    let id = Uuid::parse_str(value).map_err(|_| unavailable())?;
    if id.is_nil() || id.to_string() != value {
        return Err(unavailable());
    }
    Ok(id)
}
/// Parse the closed, bounded envelope before constructing the handler's Control.
pub fn request_id(bytes: &[u8]) -> Result<Uuid, StoreError> {
    if bytes.len() > BODY_LIMIT {
        return Err(unavailable());
    }
    let message: Message = serde_json::from_slice(bytes).map_err(|_| unavailable())?;
    let request = match &message {
        Message::Hello { request, .. }
        | Message::Register { request, .. }
        | Message::Renew { request, .. }
        | Message::Release { request, .. } => request,
    };
    uuid(request)
}
/// Dispatch only after the existing private store capability is authenticated.
/// The cleanup callback MUST implement the Account's startup migration/revoke
/// work in the provided transaction. Its failure cannot acknowledge Ready.
pub fn dispatch(
    bytes: &[u8],
    start: &str,
    controller: &Controller,
    writer: &Writer,
    control: &Control,
    initialize: impl FnOnce(&WriteTx<'_>, &BootFacts) -> Result<(), StoreError>,
) -> Result<String, StoreError> {
    let runtime = controller.runtime();
    if bytes.len() > BODY_LIMIT
        || start.len() != 32
        || !start
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(unavailable());
    }
    control.check()?;
    let message: Message = serde_json::from_slice(bytes).map_err(|_| unavailable())?;
    let (op, request, supplied_start, incarnation, boot, session, sequence) = match &message {
        Message::Hello { request, start } => ("hello", request, start, None, None, None, 0),
        Message::Register {
            request,
            start,
            incarnation,
            boot,
            session,
            sequence,
            ..
        } => (
            "register",
            request,
            start,
            Some(incarnation),
            Some(boot),
            Some(session),
            *sequence,
        ),
        Message::Renew {
            request,
            start,
            incarnation,
            boot,
            session,
            sequence,
            ..
        } => (
            "renew",
            request,
            start,
            Some(incarnation),
            Some(boot),
            Some(session),
            *sequence,
        ),
        Message::Release {
            request,
            start,
            incarnation,
            boot,
            session,
            sequence,
        } => (
            "release",
            request,
            start,
            Some(incarnation),
            Some(boot),
            Some(session),
            *sequence,
        ),
    };
    if supplied_start != start
        || uuid(request)? != control.request
        || sequence > MAX_SEQUENCE
        || (op != "renew" && sequence != 0)
        || incarnation.is_some_and(|v| v != runtime.incarnation())
    {
        return Err(unavailable());
    }
    let mut response =
        json!({"op":op,"request":request,"start":start,"incarnation":runtime.incarnation()});
    let boot = boot.map(|v| uuid(v)).transpose()?;
    let session = session.map(|v| uuid(v)).transpose()?;
    if op == "hello" {
        runtime.initialize(writer)?;
        control.check()?;
        return Ok(response.to_string());
    }
    let boot = boot.ok_or_else(unavailable)?;
    let session = session.ok_or_else(unavailable)?;
    let reply = match message {
        Message::Register {
            facts, snapshot, ..
        } => {
            let facts = facts.into_native();
            let startup_facts = facts.clone();
            controller.register(
                writer,
                control,
                Registration {
                    boot,
                    session,
                    facts,
                    snapshot: snapshot.into_native()?,
                },
                |tx| initialize(tx, &startup_facts),
            )?
        }
        Message::Renew { snapshot, .. } => controller.renew(
            writer,
            control,
            boot,
            session,
            sequence,
            snapshot.into_native()?,
        )?,
        Message::Release { .. } => controller.release(writer, control, boot, session)?,
        Message::Hello { .. } => return Err(unavailable()),
    };
    control.check()?;
    let status = match reply {
        Reply::Ready(_) if op != "release" => "ready",
        Reply::Released if op == "release" => "released",
        _ => return Err(unavailable()),
    };
    let object = response.as_object_mut().ok_or_else(unavailable)?;
    object.insert("boot".into(), json!(boot.to_string()));
    object.insert("session".into(), json!(session.to_string()));
    object.insert("sequence".into(), json!(sequence));
    object.insert("status".into(), json!(status));
    if op != "release" {
        object.insert("leaseMs".into(), json!(15000));
    }
    Ok(response.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature_context::{source_fingerprint, ContextSource, FeatureRuntime};
    use server_store::{db_path, rusqlite::Connection, RustAuth};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};
    type Error = Box<dyn std::error::Error>;
    const START: &str = "11111111111111111111111111111111";
    fn fixture() -> Result<(tempfile::TempDir, Writer, Controller), Error> {
        let dir = tempfile::tempdir()?;
        let db = Connection::open(db_path(dir.path()))?;
        db.execute_batch(include_str!(
            "../../../server-store/tests/fixtures/node-schema.sql"
        ))?;
        drop(db);
        let writer = Writer::open(dir.path(), RustAuth::owned())?;
        let controller = Controller::new(Arc::new(FeatureRuntime::new()?), [None; 23]);
        Ok((dir, writer, controller))
    }
    fn control(id: Uuid) -> Control {
        Control {
            request: id,
            deadline: Instant::now() + Duration::from_secs(2),
            cancelled: Arc::default(),
        }
    }
    fn snapshot(writer: &Writer) -> Result<serde_json::Value, StoreError> {
        writer.read(|r| Ok(json!({
            "sources": {"decision":source_fingerprint(r, ContextSource::Decision)?,"machines":source_fingerprint(r, ContextSource::InferenceMachines)?},
            "availability":{"stepSupervision":"decision-service-unconfigured","toolGate":"decision-service-unconfigured","chatFraming":"decision-service-unconfigured","framingReasoner":"decision-service-unconfigured","systemOneRouting":"system-one-unconfigured"}
        })))
    }
    fn register(
        writer: &Writer,
        controller: &Controller,
        id: Uuid,
    ) -> Result<serde_json::Value, StoreError> {
        Ok(
            json!({"op":"register","request":id.to_string(),"start":START,
            "incarnation":controller.runtime().incarnation(),"boot":Uuid::new_v4().to_string(),
            "session":Uuid::new_v4().to_string(),"sequence":0,
            "facts":{"env":[null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null,null],"restart":[false,true],"browser":"playwright","code":false,"trustProxy":true},"snapshot":snapshot(writer)?}),
        )
    }
    #[test]
    fn real_controller_correlates_closed_messages_retries_and_retirement() -> Result<(), Error> {
        let (_dir, writer, controller) = fixture()?;
        let hello_id = Uuid::new_v4();
        let hello = json!({"op":"hello","request":hello_id.to_string(),"start":START});
        let response = dispatch(
            hello.to_string().as_bytes(),
            START,
            &controller,
            &writer,
            &control(hello_id),
            |_, _| Err(unavailable()),
        )?;
        let response: serde_json::Value = serde_json::from_str(&response)?;
        assert_eq!(
            response.get("incarnation"),
            Some(&json!(controller.runtime().incarnation()))
        );
        let request = Uuid::new_v4();
        let message = register(&writer, &controller, request)?;
        let initialized = AtomicUsize::new(0);
        for _ in 0..2 {
            let reply = dispatch(
                message.to_string().as_bytes(),
                START,
                &controller,
                &writer,
                &control(request),
                |_, _| {
                    initialized.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )?;
            let reply: serde_json::Value = serde_json::from_str(&reply)?;
            assert_eq!(reply.get("status"), Some(&json!("ready")));
            assert_eq!(reply.get("leaseMs"), Some(&json!(15000)));
        }
        assert_eq!(initialized.load(Ordering::SeqCst), 1);
        let mut renewal = message.clone();
        let fields = renewal.as_object_mut().ok_or("object")?;
        fields.remove("facts");
        fields.insert("op".into(), json!("renew"));
        fields.insert("sequence".into(), json!(1));
        let renew_id = Uuid::new_v4();
        fields.insert("request".into(), json!(renew_id.to_string()));
        dispatch(
            renewal.to_string().as_bytes(),
            START,
            &controller,
            &writer,
            &control(renew_id),
            |_, _| Err(unavailable()),
        )?;
        let mut release = renewal.clone();
        let fields = release.as_object_mut().ok_or("object")?;
        fields.remove("snapshot");
        fields.insert("op".into(), json!("release"));
        fields.insert("sequence".into(), json!(0));
        let release_id = Uuid::new_v4();
        fields.insert("request".into(), json!(release_id.to_string()));
        for _ in 0..2 {
            let reply = dispatch(
                release.to_string().as_bytes(),
                START,
                &controller,
                &writer,
                &control(release_id),
                |_, _| Err(unavailable()),
            )?;
            let reply: serde_json::Value = serde_json::from_str(&reply)?;
            assert_eq!(reply.get("status"), Some(&json!("released")));
            assert!(reply.get("leaseMs").is_none());
        }
        assert!(dispatch(
            renewal.to_string().as_bytes(),
            START,
            &controller,
            &writer,
            &control(renew_id),
            |_, _| Ok(())
        )
        .is_err());
        Ok(())
    }
    #[test]
    fn rejects_unknown_duplicate_missing_nullable_and_incoherent_owner_facts() -> Result<(), Error>
    {
        let (_dir, writer, controller) = fixture()?;
        let id = Uuid::new_v4();
        let valid = register(&writer, &controller, id)?;
        let calls = AtomicUsize::new(0);
        let mut cases = Vec::new();
        for pointer in [
            "/snapshot/availability/stepSupervision",
            "/snapshot/availability/systemOneRouting",
        ] {
            let mut v = valid.clone();
            let (parent, key) = pointer.rsplit_once('/').ok_or("pointer")?;
            v.pointer_mut(parent)
                .and_then(|p| p.as_object_mut())
                .ok_or("object")?
                .remove(key);
            cases.push(v.to_string());
        }
        let mut v = valid.clone();
        v.as_object_mut()
            .ok_or("object")?
            .insert("extra".into(), json!(true));
        cases.push(v.to_string());
        let mut v = valid.clone();
        *v.pointer_mut("/snapshot/availability/toolGate")
            .ok_or("field")? = json!("choice-unsupported");
        cases.push(v.to_string());
        let mut v = valid.clone();
        *v.get_mut("sequence").ok_or("sequence")? = json!(9_007_199_254_740_992_u64);
        cases.push(v.to_string());
        let mut v = valid.clone();
        *v.get_mut("incarnation").ok_or("epoch")? = json!("0".repeat(32));
        cases.push(v.to_string());
        cases.push(format!(
            "{{\"op\":\"register\",{}",
            valid.to_string().strip_prefix('{').ok_or("object")?
        ));
        for wire in cases {
            assert!(dispatch(
                wire.as_bytes(),
                START,
                &controller,
                &writer,
                &control(id),
                |_, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            )
            .is_err());
        }
        assert!(dispatch(
            &vec![b' '; BODY_LIMIT + 1],
            START,
            &controller,
            &writer,
            &control(id),
            |_, _| Ok(())
        )
        .is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        writer.read(|r| {
            assert!(r
                .row(
                    "SELECT value FROM settings WHERE key=?1",
                    [server_store::FEATURE_RUNTIME_INCARNATION_KEY],
                    |r| r.get::<_, String>(0)
                )?
                .is_none());
            Ok(())
        })?;
        Ok(())
    }
    #[test]
    fn failed_owner_initialization_never_acknowledges_or_repeats_cleanup() -> Result<(), Error> {
        let (_dir, writer, controller) = fixture()?;
        let id = Uuid::new_v4();
        let message = register(&writer, &controller, id)?;
        let count = AtomicUsize::new(0);
        assert!(dispatch(
            message.to_string().as_bytes(),
            START,
            &controller,
            &writer,
            &control(id),
            |_, _| {
                count.fetch_add(1, Ordering::SeqCst);
                Err(unavailable())
            }
        )
        .is_err());
        assert!(dispatch(
            message.to_string().as_bytes(),
            START,
            &controller,
            &writer,
            &control(id),
            |_, _| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        )
        .is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        writer.read(|r| {
            assert!(controller.admission(r).is_err());
            Ok(())
        })?;
        Ok(())
    }
}
