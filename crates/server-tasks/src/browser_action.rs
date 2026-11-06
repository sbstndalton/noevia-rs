//! Native Browser action orchestration. The engine only supplies real page/DOM facts and
//! executes fixed primitives; policy, secret handling, cards and freshness remain here.
use crate::{
    browser_policy::{self, Secret},
    session::Context,
    Decision,
};
use js_json::{wtf16, JValue};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
pub type Reply = Pin<Box<dyn Future<Output = Result<JValue, EngineError>> + Send + 'static>>;
#[derive(Debug)]
pub struct EngineError;
/// Primitive engine commands only. No task lifecycle, model instructions or evaluation strings.
#[derive(Clone)]
pub enum Primitive {
    Current,
    Inspect {
        selector: Option<String>,
        focus: bool,
    },
    Recheck {
        page: String,
        element: Option<String>,
        focus: bool,
    },
    Dispose {
        element: String,
    },
    Screenshot {
        page: String,
    },
    Extract {
        page: String,
        selector: Option<String>,
    },
    Dispatch {
        page: String,
        element: Option<String>,
        kind: String,
        arguments: JValue,
    },
    Back {
        page: String,
    },
}
/// A submitted primitive retains its account/engine ownership through settlement even when the
/// returned waiter is dropped. The lifecycle owner independently starts close on cancellation.
pub trait BrowserEngine: Send + Sync {
    fn execute(&self, command: Primitive) -> Reply;
}
#[derive(Default)]
struct Navigation {
    blocked: Vec<JValue>,
    reason: Option<String>,
    downloads: Vec<JValue>,
}
pub struct State {
    navigation: Mutex<Navigation>,
    domains: Vec<String>,
    secrets: Arc<Vec<Secret>>,
}
fn object(rows: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(rows.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn string(value: &str) -> JValue {
    JValue::Str(value.into())
}
fn text(value: &JValue) -> String {
    js_json::to_js_string(value).unwrap_or_default()
}
fn nullish(value: &JValue) -> &JValue {
    if value.is_nullish() {
        &JValue::Null
    } else {
        value
    }
}
fn set(value: &mut JValue, key: &str, item: JValue) {
    if let JValue::Obj(rows) = value {
        if let Some(row) = rows.iter_mut().find(|(k, _)| k == key) {
            row.1 = item;
        } else {
            rows.push((key.into(), item));
        }
    }
}
impl State {
    pub fn new(domains: Vec<String>, secrets: Arc<Vec<Secret>>) -> Self {
        Self {
            navigation: Mutex::new(Navigation::default()),
            domains,
            secrets,
        }
    }
    /// Called for every request, including subresources, redirects observed by Playwright,
    /// popups and sockets. A failed callback always refuses; DNS rebinding stays at egress.
    pub fn route(&self, url: &str, main: bool, websocket: bool) -> bool {
        if !websocket
            && (url
                .get(..5)
                .is_some_and(|s| s.eq_ignore_ascii_case("data:"))
                || url
                    .get(..5)
                    .is_some_and(|s| s.eq_ignore_ascii_case("blob:")))
        {
            return true;
        }
        let projected = if websocket && url.get(..2).is_some_and(|s| s.eq_ignore_ascii_case("ws")) {
            format!("http{}", url.get(2..).unwrap_or_default())
        } else {
            url.into()
        };
        let answer = browser_policy::navigation(
            &string(&projected),
            &JValue::Arr(self.domains.iter().map(|d| string(d)).collect()),
        );
        if answer.get("ok") == &JValue::Bool(true) {
            return true;
        }
        let reason = answer
            .get("reason")
            .as_str()
            .unwrap_or("This address could not be double-checked, so it is not opened.");
        self.refuse(url, reason, main);
        false
    }
    fn refuse(&self, url: &str, reason: &str, main: bool) {
        if let Ok(mut nav) = self.navigation.lock() {
            if nav.blocked.len() < 100 {
                nav.blocked.push(object(vec![
                    (
                        "url",
                        string(&wtf16::slice(
                            &browser_policy::mask(url, &self.secrets),
                            300,
                        )),
                    ),
                    ("reason", string(reason)),
                ]));
            }
            if main {
                nav.reason = Some(reason.into());
            }
        }
    }
    pub fn downloaded(&self, name: String, from: String) {
        if let Ok(mut nav) = self.navigation.lock() {
            nav.downloads.push(object(vec![
                ("name", string(&name)),
                (
                    "from",
                    string(&wtf16::slice(
                        &browser_policy::mask(&from, &self.secrets),
                        300,
                    )),
                ),
            ]));
        }
    }
}
struct Handle {
    engine: Arc<dyn BrowserEngine>,
    context: Arc<dyn Context>,
    id: Option<String>,
}
impl Handle {
    async fn dispose(&mut self) {
        if let Some(id) = self.id.take() {
            let _ = self
                .engine
                .execute(Primitive::Dispose { element: id })
                .await;
        }
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let engine = self.engine.clone();
            let context = self.context.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = engine.execute(Primitive::Dispose { element: id }).await;
                    drop(context);
                });
            }
        }
    }
}
pub struct Actions {
    engine: Arc<dyn BrowserEngine>,
    context: Arc<dyn Context>,
    state: Arc<State>,
    task: String,
    secret_typed: bool,
}
impl Actions {
    pub fn new(
        engine: Arc<dyn BrowserEngine>,
        context: Arc<dyn Context>,
        state: Arc<State>,
        task: String,
    ) -> Self {
        Self {
            engine,
            context,
            state,
            task,
            secret_typed: false,
        }
    }
    fn result(&self, status: &str, origin: &str, extra: Vec<(&str, JValue)>) -> JValue {
        let mut out = object(vec![("status", string(status)), ("origin", string(origin))]);
        for (k, v) in extra {
            let v = if k == "reason" {
                string(&browser_policy::mask(&text(&v), &self.state.secrets))
            } else {
                v
            };
            set(&mut out, k, v);
        }
        out
    }
    async fn evidence(&self, page: &str) -> JValue {
        if self.secret_typed {
            return object(vec![
                ("screenshotOmitted", JValue::Bool(true)),
                (
                    "screenshotOmissionReason",
                    string("Screenshot omitted because this browser session has handled a secret."),
                ),
            ]);
        }
        match self
            .engine
            .execute(Primitive::Screenshot { page: page.into() })
            .await
        {
            Ok(reply) if reply.get("png").as_str().is_some() => {
                object(vec![("screenshot", reply.get("png").clone())])
            }
            _ => object(vec![
                ("screenshotOmitted", JValue::Bool(true)),
                (
                    "screenshotOmissionReason",
                    string("Screenshot omitted because capture failed."),
                ),
            ]),
        }
    }
    pub async fn act(&mut self, action: JValue) -> JValue {
        let mut handle = Handle {
            engine: self.engine.clone(),
            context: self.context.clone(),
            id: None,
        };
        let result = self.perform(&action, &mut handle).await;
        handle.dispose().await;
        result
    }
    async fn perform(&mut self, action: &JValue, handle: &mut Handle) -> JValue {
        let current = match self.engine.execute(Primitive::Current).await {
            Ok(v) => v,
            Err(_) => {
                return self.result(
                    "blocked",
                    "",
                    vec![("reason", string("No page is open in this session."))],
                )
            }
        };
        let origin = current.get("origin").as_str().unwrap_or("").to_owned();
        let page = current.get("page").as_str().unwrap_or("").to_owned();
        let kind = if action.get("type").truthy() {
            text(action.get("type"))
        } else {
            String::new()
        };
        if ![
            "navigate",
            "click",
            "type",
            "select",
            "press",
            "upload",
            "submit",
            "extract",
            "screenshot",
            "scroll",
            "hover",
            "wait",
        ]
        .contains(&kind.as_str())
        {
            return self.result(
                "blocked",
                &origin,
                vec![(
                    "reason",
                    string(&format!(
                        "Unknown action “{}”.",
                        wtf16::slice(&browser_policy::mask(&kind, &self.state.secrets), 40)
                    )),
                )],
            );
        }
        if kind == "navigate"
            && action.get("method").truthy()
            && text(action.get("method")).to_uppercase() != "GET"
        {
            return self.result(
                "blocked",
                &origin,
                vec![(
                    "reason",
                    string("Only plain navigation is supported; submit the page’s form instead."),
                )],
            );
        }
        let selector = if action.get("selector").truthy() && kind != "navigate" {
            Some(text(action.get("selector")))
        } else {
            None
        };
        let mut element = JValue::Null;
        if selector.is_some() || kind == "press" {
            match self
                .engine
                .execute(Primitive::Inspect {
                    selector: selector.clone(),
                    focus: selector.is_none(),
                })
                .await
            {
                Ok(inspected) => {
                    handle.id = inspected.get("element").as_str().map(str::to_owned);
                    element = inspected.get("facts").clone();
                }
                Err(_) if selector.is_some() => {
                    return self.result(
                        "blocked",
                        &origin,
                        vec![("reason", string("That element is not on the page."))],
                    )
                }
                Err(_) => {}
            }
            if selector.is_some() && handle.id.is_none() {
                return self.result(
                    "blocked",
                    &origin,
                    vec![("reason", string("That element is not on the page."))],
                );
            }
            if kind == "press"
                && selector.is_none()
                && ["iframe", "frame", "object", "embed"]
                    .contains(&element.get("tag").as_str().unwrap_or(""))
            {
                element = JValue::Null;
            }
        }
        let facts = object(vec![
            ("type", string(&kind)),
            ("url", action.get("url").clone()),
            ("method", action.get("method").clone()),
            ("key", action.get("key").clone()),
            (
                "element",
                if kind == "press" && element.is_nullish() {
                    object(vec![("inForm", JValue::Bool(true))])
                } else {
                    element.clone()
                },
            ),
        ]);
        let verdict = browser_policy::classify(
            &facts,
            &object(vec![
                ("origin", string(&origin)),
                (
                    "allowedDomains",
                    JValue::Arr(self.state.domains.iter().map(|d| string(d)).collect()),
                ),
            ]),
        );
        if verdict.get("status").as_str() == Some("blocked") {
            return self.result(
                "blocked",
                &origin,
                vec![("reason", verdict.get("reason").clone())],
            );
        }
        let mut typed = None;
        if kind == "type" || kind == "select" {
            let into = element
                .get("frameOrigin")
                .as_str()
                .filter(|s| s.starts_with("http:") || s.starts_with("https:"))
                .unwrap_or(&origin);
            let into = if into.is_empty() { "about:blank" } else { into };
            let value = if !action.get("text").is_nullish() {
                action.get("text")
            } else if !action.get("value").is_nullish() {
                action.get("value")
            } else {
                &JValue::Str(String::new())
            };
            match browser_policy::substitute(&text(value), &self.state.secrets, into) {
                Ok(value) => {
                    if !value.used.is_empty() {
                        self.secret_typed = true;
                    }
                    typed = Some(value);
                }
                Err(reason) => {
                    return self.result("blocked", &origin, vec![("reason", string(&reason))])
                }
            }
        }
        // Public browser tasks have no supplied upload files; the existing service passes [].
        if kind == "upload" {
            return self.result(
                "blocked",
                &origin,
                vec![(
                    "reason",
                    string("Only files given to this task can be uploaded."),
                )],
            );
        }
        if verdict.get("status").as_str() == Some("needs_approval") {
            let mut masked = element.clone();
            for key in ["text", "name", "value", "formAction"] {
                if !element.is_nullish() {
                    set(
                        &mut masked,
                        key,
                        string(&browser_policy::mask(
                            &if element.get(key).is_nullish() {
                                String::new()
                            } else {
                                text(element.get(key))
                            },
                            &self.state.secrets,
                        )),
                    );
                }
            }
            let mut card = object(vec![
                ("jobId", string(&self.task)),
                ("origin", string(&origin)),
                ("action", string(&kind)),
                ("reason", verdict.get("reason").clone()),
                ("element", masked),
                (
                    "url",
                    if action.get("url").truthy() {
                        string(&browser_policy::mask(
                            &text(action.get("url")),
                            &self.state.secrets,
                        ))
                    } else {
                        JValue::Null
                    },
                ),
                (
                    "typed",
                    typed.as_ref().map_or(JValue::Null, |t| {
                        string(&browser_policy::mask(t.value(), &self.state.secrets))
                    }),
                ),
                ("file", JValue::Null),
            ]);
            if let JValue::Obj(fields) = self.evidence(&page).await {
                for (k, v) in fields {
                    set(&mut card, &k, v);
                }
            }
            let answer = match self.context.request_approval(card) {
                Ok(wait) => wait.await,
                Err(_) => Decision::Deny,
            };
            let allow = matches!(answer, Decision::Approve | Decision::ApproveAll);
            if self
                .context
                .event(
                    "approval.decided",
                    object(vec![
                        ("decision", string(if allow { "approve" } else { "deny" })),
                        ("action", string(&kind)),
                    ]),
                )
                .is_err()
            {
                return self.result("blocked", &origin, vec![("reason", string("Declined."))]);
            }
            if !allow || self.context.active().is_err() {
                return self.result("blocked", &origin, vec![("reason", string("Declined."))]);
            }
            let checked = self
                .engine
                .execute(Primitive::Recheck {
                    page: page.clone(),
                    element: handle.id.clone(),
                    focus: kind == "press" && selector.is_none(),
                })
                .await;
            let checked = checked.unwrap_or(JValue::Null);
            if checked.get("samePage") != &JValue::Bool(true)
                || checked.get("origin").as_str() != Some(&origin)
            {
                return self.result(
                    "blocked",
                    checked.get("origin").as_str().unwrap_or(&origin),
                    vec![(
                        "reason",
                        string(
                            "The page changed while approval was pending; review the action again.",
                        ),
                    )],
                );
            }
            if handle.id.is_some()
                && (checked.get("connected") != &JValue::Bool(true)
                    || checked.get("focused") == &JValue::Bool(false)
                    || [
                        "tag",
                        "type",
                        "role",
                        "name",
                        "text",
                        "value",
                        "inForm",
                        "formMethod",
                        "formAction",
                        "frameOrigin",
                    ]
                    .iter()
                    .any(|k| {
                        let a = nullish(element.get(k));
                        let b = nullish(checked.get("facts").get(k));
                        let a = if a.is_nullish() {
                            string("")
                        } else {
                            a.clone()
                        };
                        let b = if b.is_nullish() {
                            string("")
                        } else {
                            b.clone()
                        };
                        a != b
                    }))
            {
                return self.result("blocked",&origin,vec![("reason",string("The approved target changed while approval was pending; review the action again."))]);
            }
        }
        if self.context.active().is_err() {
            return self.result(
                "blocked",
                &origin,
                vec![("reason", string("The browser session is closed."))],
            );
        }
        if let Ok(mut state) = self.state.navigation.lock() {
            state.reason = None;
        }
        if kind == "screenshot" {
            return self.result(
                "done",
                &origin,
                vec![("evidence", self.evidence(&page).await)],
            );
        }
        if kind == "extract" {
            return match self
                .engine
                .execute(Primitive::Extract { page, selector })
                .await
            {
                Ok(answer) => self.result(
                    "done",
                    &origin,
                    vec![(
                        "evidence",
                        object(vec![(
                            "text",
                            string(&browser_policy::mask(
                                &wtf16::slice(&text(answer.get("text")), 20000),
                                &self.state.secrets,
                            )),
                        )]),
                    )],
                ),
                Err(_) => self.result(
                    "blocked",
                    &origin,
                    vec![("reason", string("Browser action failed."))],
                ),
            };
        }
        let mut arguments = action.clone();
        set(&mut arguments, "url", string(&text(action.get("url"))));
        set(&mut arguments, "key", string(&text(action.get("key"))));
        if let Some(value) = typed.as_ref() {
            set(&mut arguments, "text", string(value.value()));
            set(&mut arguments, "values", string(value.value()));
        }
        // Number coercion matches Node before clamping; the engine receives a primitive number.
        for (key, default, cap) in [("dy", 600.0, f64::INFINITY), ("ms", 500.0, 5000.0)] {
            let n = crate::meta::numeric(action.get(key));
            let n = if n == 0.0 || n.is_nan() { default } else { n };
            set(&mut arguments, key, JValue::Num(n.min(cap)));
        }
        let effected = [
            "navigate", "click", "type", "select", "press", "upload", "submit",
        ]
        .contains(&kind.as_str());
        let outcome = self
            .engine
            .execute(Primitive::Dispatch {
                page: page.clone(),
                element: handle.id.clone(),
                kind,
                arguments,
            })
            .await;
        drop(typed);
        let landed = self
            .engine
            .execute(Primitive::Current)
            .await
            .unwrap_or(JValue::Null);
        let landed_origin = landed.get("origin").as_str().unwrap_or("");
        let url = landed.get("url").as_str().unwrap_or("");
        if outcome.is_ok()
            && (url
                .get(..5)
                .is_some_and(|s| s.eq_ignore_ascii_case("http:"))
                || url
                    .get(..6)
                    .is_some_and(|s| s.eq_ignore_ascii_case("https:")))
        {
            self.state.route(url, true, false);
        }
        let reason = self
            .state
            .navigation
            .lock()
            .ok()
            .and_then(|n| n.reason.clone());
        if let Some(reason) = reason {
            if landed_origin.is_empty() || !self.state.route(url, false, false) {
                let _ = self
                    .engine
                    .execute(Primitive::Back {
                        page: landed.get("page").as_str().unwrap_or(&page).into(),
                    })
                    .await;
            }
            let after = self
                .engine
                .execute(Primitive::Current)
                .await
                .unwrap_or(JValue::Null);
            return self.result(
                "blocked",
                after.get("origin").as_str().unwrap_or(""),
                vec![("reason", string(&reason))],
            );
        }
        if outcome.is_err() {
            return self.result(
                if effected { "uncertain" } else { "blocked" },
                landed_origin,
                vec![("reason", string("Browser action failed."))],
            );
        }
        let downloads = self
            .state
            .navigation
            .lock()
            .map(|n| JValue::Arr(n.downloads.clone()))
            .unwrap_or(JValue::Arr(vec![]));
        self.result("done", landed_origin, vec![("downloads", downloads)])
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    struct FakeEngine {
        row: JValue,
        url: Mutex<String>,
        disposed: Mutex<Vec<JValue>>,
    }
    impl BrowserEngine for FakeEngine {
        fn execute(&self, command: Primitive) -> Reply {
            let result = match command {
                Primitive::Current => {
                    let url = self.url.lock().unwrap().clone();
                    let origin = url.split('/').take(3).collect::<Vec<_>>().join("/");
                    Ok(object(vec![
                        ("page", string("page")),
                        ("origin", string(&origin)),
                        ("url", string(&url)),
                    ]))
                }
                Primitive::Inspect { selector, .. } => match selector {
                    Some(selector) if self.row.get("dom").get(&selector) != &JValue::Undefined => {
                        Ok(object(vec![
                            ("element", string(&selector)),
                            ("facts", self.row.get("dom").get(&selector).clone()),
                        ]))
                    }
                    _ => Err(EngineError),
                },
                Primitive::Recheck { element, .. } => {
                    let mode = self.row.get("mode").as_str().unwrap_or("");
                    Ok(object(vec![
                        ("samePage", JValue::Bool(true)),
                        (
                            "origin",
                            string(if mode == "page" {
                                "https://other.example.test"
                            } else {
                                "https://shop.example.test"
                            }),
                        ),
                        ("connected", JValue::Bool(mode != "target")),
                        ("focused", JValue::Bool(true)),
                        (
                            "facts",
                            element
                                .as_ref()
                                .map_or(JValue::Null, |e| self.row.get("dom").get(e).clone()),
                        ),
                    ]))
                }
                Primitive::Screenshot { .. } => Ok(object(vec![(
                    "png",
                    string("cG5nIHN5bnRoZXRpYy1zZWNyZXQtNzczMQ=="),
                )])),
                Primitive::Dispose { element } => {
                    self.disposed.lock().unwrap().push(string(&element));
                    Ok(object(vec![]))
                }
                Primitive::Extract { selector, .. } => Ok(object(vec![(
                    "text",
                    selector.map_or_else(
                        || string("body text"),
                        |s| self.row.get("dom").get(&s).get("text").clone(),
                    ),
                )])),
                Primitive::Dispatch {
                    kind, arguments, ..
                } => {
                    if kind == "navigate" {
                        *self.url.lock().unwrap() = text(arguments.get("url"));
                    }
                    Ok(object(vec![]))
                }
                Primitive::Back { .. } => Ok(object(vec![])),
            };
            Box::pin(async move { result })
        }
    }
    struct FakeContext {
        cards: Mutex<Vec<JValue>>,
        decision: Decision,
        events: Mutex<Vec<JValue>>,
    }
    impl Context for FakeContext {
        fn active(&self) -> Result<(), crate::acp::RpcError> {
            Ok(())
        }
        fn event(&self, kind: &str, data: JValue) -> Result<(), crate::acp::RpcError> {
            if kind == "approval.requested" {
                self.cards.lock().unwrap().push(data.clone());
            }
            self.events
                .lock()
                .unwrap()
                .push(object(vec![("kind", string(kind)), ("data", data)]));
            Ok(())
        }
        fn approval(&self, _: JValue) -> crate::session::Answer {
            let decision = self.decision;
            Box::pin(async move { decision })
        }
        fn now(&self) -> i64 {
            1
        }
    }
    #[tokio::test]
    async fn native_actions_match_node_cards_results_and_handle_disposal() {
        let fixture =
            wtf16::parse(include_str!("../tests/fixtures/browser-actions-node.json")).unwrap();
        let JValue::Arr(rows) = fixture else {
            panic!("fixture");
        };
        for row in rows {
            let engine = Arc::new(FakeEngine {
                row: row.clone(),
                url: Mutex::new("https://shop.example.test/".into()),
                disposed: Mutex::new(vec![]),
            });
            let context = Arc::new(FakeContext {
                cards: Mutex::new(vec![]),
                decision: if row.get("answer").as_str() == Some("approve") {
                    Decision::Approve
                } else {
                    Decision::Deny
                },
                events: Mutex::new(vec![]),
            });
            let secrets = Arc::new(vec![Secret::new(
                "token".into(),
                vec!["shop.example.test".into()],
                "synthetic-secret-7731".into(),
            )]);
            let state = Arc::new(State::new(vec!["shop.example.test".into()], secrets));
            let mut actions = Actions::new(engine.clone(), context.clone(), state, "job".into());
            let result = actions.act(row.get("action").clone()).await;
            assert_eq!(
                &result,
                row.get("result"),
                "result {}",
                text(row.get("name"))
            );
            assert_eq!(
                &JValue::Arr(context.cards.lock().unwrap().clone()),
                row.get("cards"),
                "cards {}",
                text(row.get("name"))
            );
            assert_eq!(
                &JValue::Arr(engine.disposed.lock().unwrap().clone()),
                row.get("disposals"),
                "disposal {}",
                text(row.get("name"))
            );
        }
    }
    #[test]
    fn native_routes_never_open_private_or_unlisted_requests() {
        let state = State::new(vec!["synthetic.invalid".into()], Arc::new(vec![]));
        assert!(state.route("https://synthetic.invalid/a", true, false));
        assert!(state.route("data:text/plain,synthetic", false, false));
        assert!(!state.route("https://127.1/", false, false));
        assert!(state.route("wss://synthetic.invalid/socket", false, true));
        assert!(!state.route("wss://blocked.invalid/socket", false, true));
        assert!(!state.route("https://blocked.invalid/", true, false));
        assert!(state.navigation.lock().unwrap().reason.is_some());
    }
}
