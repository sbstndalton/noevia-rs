//! One pinned model per task, shared by Planner, reviewer and Auditor. Engine bindings supply
//! only snapshots/primitive requests; native code refuses model/machine switches and controls calls.
use crate::role_stream::{self, Call, Error, Response, Transport, Wait};
use js_json::JValue;
use std::sync::Arc;
use tokio::sync::watch;
#[derive(Clone)]
pub struct Machine {
    pub key: String,
    pub model: String,
    pub name: String,
}
#[derive(Clone)]
pub struct Endpoint {
    pub configured: bool,
    pub external: bool,
    pub model: Option<String>,
    pub machine: Option<Machine>,
    pub schema_supported: bool,
    pub reasoning_model: bool,
}
#[derive(Clone)]
pub struct Fault {
    pub code: String,
    pub reason: String,
    pub constraint: Option<JValue>,
}
impl std::fmt::Debug for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoleRefusal")
            .field("code", &self.code)
            .finish_non_exhaustive()
    }
}
fn fail(code: &str, reason: impl Into<String>) -> Fault {
    Fault {
        code: code.into(),
        reason: reason.into(),
        constraint: None,
    }
}
fn aborted() -> Fault {
    fail("aborted", "The task was cancelled.")
}
fn unavailable() -> Fault {
    fail(
        "unavailable",
        "No model is configured for the task pipeline.",
    )
}
/// Account-scoped model facts and inference-engine I/O. Never invokes a Planner or task executor.
pub trait Authority: Send + Sync {
    fn endpoint(&self) -> Result<Endpoint, Fault>;
    /// Only chat rows reported loaded/loading, after the sidecar keep list is excluded.
    fn loaded(
        &self,
        endpoint: &Endpoint,
        cancel: watch::Receiver<bool>,
    ) -> Wait<'_, Result<Vec<String>, Fault>>;
    fn ready(
        &self,
        endpoint: &Endpoint,
        cancel: watch::Receiver<bool>,
    ) -> Wait<'_, Result<(), Fault>>;
    /// The authority's own fixed public refusal, never a remote response message/body.
    fn budget(
        &self,
        model: &str,
        cancel: watch::Receiver<bool>,
    ) -> Wait<'_, Result<Option<String>, Fault>>;
    fn send(
        &self,
        endpoint: &Endpoint,
        payload: JValue,
        cancel: watch::Receiver<bool>,
    ) -> Wait<'_, Result<Response, Error>>;
}
async fn covered<T>(
    future: Wait<'_, Result<T, Fault>>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<T, Fault> {
    if *cancel.borrow() {
        return Err(aborted());
    }
    tokio::select! {biased;_=cancel.changed()=>Err(aborted()),result=future=>result}
}
pub struct Session {
    authority: Arc<dyn Authority>,
    pub task: String,
    pub model: String,
    pub machine: Option<String>,
    pub thinking: bool,
    pub roles: Vec<String>,
}
pub async fn pin(
    authority: Arc<dyn Authority>,
    task: &str,
    model: Option<&str>,
    thinking: bool,
    roles: Vec<String>,
    mut cancel: watch::Receiver<bool>,
) -> Result<Session, Fault> {
    if task.is_empty() {
        return Err(fail("invalid", "A task id is required to pin a model."));
    }
    if *cancel.borrow() {
        return Err(aborted());
    }
    let endpoint = authority.endpoint().unwrap_or(Endpoint {
        configured: false,
        external: false,
        model: None,
        machine: None,
        schema_supported: false,
        reasoning_model: false,
    });
    if endpoint.external {
        return Err(fail(
            "external",
            "The task pipeline runs only on a local model.",
        ));
    }
    if !endpoint.configured {
        return Err(unavailable());
    }
    let wanted = if let Some(machine) = &endpoint.machine {
        if let Some(model) = model.filter(|s| !s.is_empty()) {
            if model != machine.model {
                return Err(fail("model_mismatch",format!("This task names {model}, but the Code role runs {} on {}. One task runs on one model.",machine.model,if machine.name.is_empty(){"its machine"}else{&machine.name})));
            }
        }
        covered(authority.ready(&endpoint, cancel.clone()), &mut cancel).await?;
        machine.model.clone()
    } else {
        let loaded=covered(authority.loaded(&endpoint,cancel.clone()),&mut cancel).await.map_err(|_|if *cancel.borrow(){aborted()}else{fail("router_unavailable","The model router could not say which model is loaded, so no model was pinned.")})?;
        let wanted = model
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| endpoint.model.clone().filter(|s| !s.is_empty()))
            .or_else(|| {
                if loaded.len() == 1 {
                    loaded.first().cloned()
                } else {
                    None
                }
            })
            .ok_or_else(|| fail("no_model", "No model is configured for the task pipeline."))?;
        if loaded.iter().any(|id| id != &wanted) {
            return Err(fail("model_mismatch","A different model is loaded. The task was not started, because switching models would unload it."));
        }
        if let Some(reason) =
            covered(authority.budget(&wanted, cancel.clone()), &mut cancel).await?
        {
            return Err(fail("over_budget", reason));
        }
        wanted
    };
    Ok(Session {
        authority,
        task: task.into(),
        model: wanted,
        machine: endpoint.machine.as_ref().map(|m| m.key.clone()),
        thinking,
        roles,
    })
}
struct PinnedTransport {
    authority: Arc<dyn Authority>,
    endpoint: Endpoint,
    model: String,
}
impl Transport for PinnedTransport {
    fn begin_attempt(&self, cancel: watch::Receiver<bool>) -> Wait<'_, Result<(), Error>> {
        Box::pin(async move {
            if self.endpoint.machine.is_some() {
                return Ok(());
            }
            let mut cancellation = cancel;
            if let Some(reason) = covered(
                self.authority.budget(&self.model, cancellation.clone()),
                &mut cancellation,
            )
            .await
            .map_err(|_| {
                if *cancellation.borrow() {
                    Error::Aborted
                } else {
                    Error::Transport
                }
            })? {
                return Err(Error::OverBudget(reason));
            }
            let loaded = covered(
                self.authority.loaded(&self.endpoint, cancellation.clone()),
                &mut cancellation,
            )
            .await
            .map_err(|_| {
                if *cancellation.borrow() {
                    Error::Aborted
                } else {
                    Error::RouterUnavailable
                }
            })?;
            if loaded.iter().any(|id| id != &self.model) {
                return Err(Error::ModelMismatch);
            }
            Ok(())
        })
    }
    fn send(
        &self,
        payload: JValue,
        cancel: watch::Receiver<bool>,
    ) -> Wait<'_, Result<Response, Error>> {
        self.authority.send(&self.endpoint, payload, cancel)
    }
}
impl Session {
    pub async fn call(
        &self,
        input: Call<'_>,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<role_stream::Answer, Fault> {
        if !self.roles.iter().any(|role| role == input.role) {
            return Err(fail(
                "role_not_pinned",
                "That role is not part of this task.",
            ));
        }
        if *cancel.borrow() {
            return Err(aborted());
        }
        if input.state.get("taskId").as_str() != Some(self.task.as_str()) {
            return Err(fail(
                "task_mismatch",
                "That call belongs to a different task.",
            ));
        }
        let endpoint = self.authority.endpoint().map_err(|_| unavailable())?;
        if endpoint.external {
            return Err(fail(
                "external",
                "The task pipeline runs only on a local model.",
            ));
        }
        if !endpoint.configured {
            return Err(unavailable());
        }
        if endpoint.machine.as_ref().map(|m| &m.key) != self.machine.as_ref() {
            return Err(fail("model_mismatch","The Code role moved to another machine or model during this task. This step was not sent, because one task runs on one model."));
        }
        if endpoint.machine.is_some() {
            covered(self.authority.ready(&endpoint, cancel.clone()), &mut cancel).await?;
        }
        let input = Call {
            role: input.role,
            state: input.state,
            roles: &self.roles,
            model: &self.model,
            thinking: self.thinking,
            instructions: input.instructions,
            schema: input.schema,
            schema_name: input.schema_name,
            constrain: input.constrain,
            schema_supported: endpoint.schema_supported,
            reasoning_model: endpoint.reasoning_model,
            max_tokens: input.max_tokens,
            max_bytes: input.max_bytes,
        };
        let transport = PinnedTransport {
            authority: self.authority.clone(),
            endpoint,
            model: self.model.clone(),
        };
        role_stream::call_detailed(&transport,input,cancel).await.map_err(|e|{let mut fault=match e.kind{
   Error::Aborted=>aborted(),Error::ModelMismatch=>fail("model_mismatch","A different model is loaded. This step was not sent, because switching models would unload it."),
   Error::RouterUnavailable=>fail("router_unavailable","The model router could not say which model is loaded, so this step was not sent."),Error::OverBudget(reason)=>fail("over_budget",reason),
   Error::ContextRefused=>fail("context_refused","The task was not sent because its context would have carried private data."),Error::TooLarge=>fail("too_large","The task is too large for this step."),
   Error::ContextInvalid=>fail("context_invalid","The task could not be prepared for this step."),Error::Invalid=>fail("invalid","The answer did not match the expected format, even after one correction."),Error::Transport=>fail("error","The model could not be reached.")
  };fault.constraint=e.constraint;fault})
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };
    struct Fake {
        endpoint: Mutex<Endpoint>,
        loaded: Mutex<Vec<String>>,
        budget: Mutex<Option<String>>,
        ready: AtomicUsize,
        refused_ready: Mutex<Option<Fault>>,
        requests: AtomicUsize,
    }
    impl Authority for Fake {
        fn endpoint(&self) -> Result<Endpoint, Fault> {
            Ok(self.endpoint.lock().unwrap().clone())
        }
        fn loaded(
            &self,
            _: &Endpoint,
            _: watch::Receiver<bool>,
        ) -> Wait<'_, Result<Vec<String>, Fault>> {
            Box::pin(async { Ok(self.loaded.lock().unwrap().clone()) })
        }
        fn ready(&self, _: &Endpoint, _: watch::Receiver<bool>) -> Wait<'_, Result<(), Fault>> {
            Box::pin(async {
                self.ready.fetch_add(1, Ordering::SeqCst);
                if let Some(fault) = self.refused_ready.lock().unwrap().clone() {
                    return Err(fault);
                }
                Ok(())
            })
        }
        fn budget(
            &self,
            _: &str,
            _: watch::Receiver<bool>,
        ) -> Wait<'_, Result<Option<String>, Fault>> {
            Box::pin(async { Ok(self.budget.lock().unwrap().clone()) })
        }
        fn send(
            &self,
            _: &Endpoint,
            _: JValue,
            _: watch::Receiver<bool>,
        ) -> Wait<'_, Result<Response, Error>> {
            Box::pin(async {
                self.requests.fetch_add(1, Ordering::SeqCst);
                Err(Error::Transport)
            })
        }
    }
    fn fake() -> Arc<Fake> {
        Arc::new(Fake {
            endpoint: Mutex::new(Endpoint {
                configured: true,
                external: false,
                model: Some("synthetic".into()),
                machine: None,
                schema_supported: false,
                reasoning_model: false,
            }),
            loaded: Mutex::new(vec![]),
            budget: Mutex::new(None),
            ready: AtomicUsize::new(0),
            refused_ready: Mutex::new(None),
            requests: AtomicUsize::new(0),
        })
    }
    fn input<'a>(state: &'a JValue, schema: &'a JValue) -> Call<'a> {
        Call {
            role: "planner",
            state,
            roles: &[],
            model: "untrusted-caller-model",
            thinking: true,
            instructions: "Plan",
            schema,
            schema_name: "answer",
            constrain: false,
            schema_supported: false,
            reasoning_model: false,
            max_tokens: 1400,
            max_bytes: 32768,
        }
    }
    fn state() -> JValue {
        JValue::Obj(vec![
            ("taskId".into(), JValue::Str("task".into())),
            ("tenantId".into(), JValue::Str("tenant".into())),
            ("request".into(), JValue::Str("Edit widget".into())),
        ])
    }
    #[tokio::test]
    async fn cold_pin_does_not_load_and_later_model_switch_refuses_before_send() {
        let f = fake();
        let (_tx, rx) = watch::channel(false);
        let s = pin(
            f.clone(),
            "task",
            None,
            false,
            vec!["planner".into()],
            rx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(s.model, "synthetic");
        assert_eq!(f.requests.load(Ordering::SeqCst), 0);
        *f.loaded.lock().unwrap() = vec!["other".into()];
        let error = s
            .call(input(&state(), &JValue::Obj(vec![])), rx)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, "model_mismatch");
        assert_eq!(f.requests.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn one_resident_is_fallback_and_budget_is_checked_again_per_call() {
        let f = fake();
        f.endpoint.lock().unwrap().model = None;
        *f.loaded.lock().unwrap() = vec!["resident".into()];
        let (_tx, rx) = watch::channel(false);
        let s = pin(
            f.clone(),
            "task",
            None,
            false,
            vec!["planner".into()],
            rx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(s.model, "resident");
        *f.budget.lock().unwrap() =
            Some("The model does not fit the inference memory budget.".into());
        assert_eq!(
            s.call(input(&state(), &JValue::Obj(vec![])), rx)
                .await
                .err()
                .unwrap()
                .code,
            "over_budget"
        );
        assert_eq!(f.requests.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn remote_pin_bypasses_local_residency_and_refuses_changed_machine() {
        let f = fake();
        let machine = Machine {
            key: "machine|url|served".into(),
            model: "served".into(),
            name: "Synthetic machine".into(),
        };
        f.endpoint.lock().unwrap().machine = Some(machine);
        *f.loaded.lock().unwrap() = vec!["different-local-model".into()];
        *f.budget.lock().unwrap() = Some("local budget refuses".into());
        let (_tx, rx) = watch::channel(false);
        let s = pin(
            f.clone(),
            "task",
            None,
            false,
            vec!["planner".into()],
            rx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(s.model, "served");
        assert_eq!(f.ready.load(Ordering::SeqCst), 1);
        f.endpoint.lock().unwrap().machine.as_mut().unwrap().key = "changed".into();
        assert_eq!(
            s.call(input(&state(), &JValue::Obj(vec![])), rx)
                .await
                .err()
                .unwrap()
                .code,
            "model_mismatch"
        );
        assert_eq!(f.ready.load(Ordering::SeqCst), 1);
        assert_eq!(f.requests.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn wrong_role_task_external_and_preaborted_pin_never_send() {
        let f = fake();
        let (tx, rx) = watch::channel(false);
        let s = pin(
            f.clone(),
            "task",
            None,
            false,
            vec!["reviewer".into()],
            rx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            s.call(input(&state(), &JValue::Obj(vec![])), rx.clone())
                .await
                .err()
                .unwrap()
                .code,
            "role_not_pinned"
        );
        tx.send_replace(true);
        assert_eq!(
            pin(f.clone(), "task", None, false, vec![], rx)
                .await
                .err()
                .unwrap()
                .code,
            "aborted"
        );
        assert_eq!(f.requests.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn node_pin_vectors_match_success_and_refusal_shapes() {
        let fixtures =
            js_json::wtf16::parse(include_str!("../tests/fixtures/role-pin-node.json")).unwrap();
        let JValue::Arr(fixtures) = fixtures else {
            panic!()
        };
        for f in fixtures {
            let raw = f.get("input");
            let authority = fake();
            {
                let mut endpoint = authority.endpoint.lock().unwrap();
                endpoint.external = raw.get("external") == &JValue::Bool(true);
                endpoint.configured = raw.get("configured") != &JValue::Bool(false);
                if raw.get("model") != &JValue::Undefined {
                    endpoint.model = raw.get("model").as_str().map(str::to_owned);
                }
                if raw.get("machine") == &JValue::Bool(true) {
                    endpoint.machine = Some(Machine {
                        key: "m|http://engine.invalid/v1|served".into(),
                        model: "served".into(),
                        name: "Synthetic machine".into(),
                    });
                }
            }
            if let JValue::Arr(loaded) = raw.get("loaded") {
                *authority.loaded.lock().unwrap() =
                    loaded.iter().map(|s| s.as_str().unwrap().into()).collect();
            }
            *authority.budget.lock().unwrap() = raw.get("budget").as_str().map(str::to_owned);
            if raw.get("ready") == &JValue::Bool(false) {
                *authority.refused_ready.lock().unwrap() = Some(fail(
                    "machineUnreachable",
                    "The Code role’s machine is unreachable.",
                ));
            }
            let (_tx, rx) = watch::channel(false);
            let result = pin(
                authority.clone(),
                raw.get("task").as_str().unwrap_or("task"),
                raw.get("wanted").as_str(),
                false,
                vec!["planner".into()],
                rx,
            )
            .await;
            let text = |s: String| JValue::Str(s);
            let result = match result {
                Ok(s) => JValue::obj([
                    ("ok", JValue::Bool(true)),
                    ("model", text(s.model)),
                    ("thinking", JValue::Bool(s.thinking)),
                    ("machine", s.machine.map_or(JValue::Null, text)),
                    (
                        "roles",
                        JValue::Arr(s.roles.into_iter().map(text).collect()),
                    ),
                ]),
                Err(e) => JValue::obj([
                    ("ok", JValue::Bool(false)),
                    ("code", text(e.code)),
                    ("reason", text(e.reason)),
                ]),
            };
            assert_eq!(result, *f.get("result"));
            assert_eq!(authority.requests.load(Ordering::SeqCst), 0);
        }
    }
}
