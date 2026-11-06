//! A separate browser connection suspends new targets before their first request.
//! The session proxy enforces WebSocket destinations; response bodies and headers
//! remain owned by Chromium.
use super::*;
use chromiumoxide::{cdp::browser_protocol::target::SessionId, Connection};
use chromiumoxide_types::{CallId, EventMessage, Message};
use std::collections::{HashMap, VecDeque};
#[derive(Debug, Deserialize)]
struct GuardEvent {
    method: MethodId,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    params: Value,
}
impl Method for GuardEvent {
    fn identifier(&self) -> MethodId {
        self.method.clone()
    }
}
impl EventMessage for GuardEvent {
    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}
type Steps = VecDeque<(&'static str, Value)>;

pub(super) struct Guard {
    pub blocked: Arc<tokio::sync::Mutex<Vec<Value>>>,
    pub nav_blocked: Arc<tokio::sync::Mutex<Option<String>>>,
    pub trace: Arc<tokio::sync::Mutex<Vec<Value>>>,
    pub shutdown: tokio::sync::mpsc::UnboundedSender<()>,
    task: JoinHandle<()>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        // The guard owns its connection until Browser.close is acknowledged.
        let _ = self.shutdown.send(());
    }
}
impl Guard {
    pub async fn start(
        endpoint: &str,
        domains: Vec<String>,
        errors: Arc<tokio::sync::Mutex<Option<String>>>,
        secrets: BTreeMap<String, Secret>,
        log: AuditLog,
        job_id: String,
        proxy: Option<(String, ProxyCredentials)>,
    ) -> Result<Self> {
        let mut connection = Connection::<GuardEvent>::connect(endpoint).await?;
        connection.submit_command("Fetch.enable".into(), None, json!({"handleAuthRequests":true,"patterns":[{"urlPattern":"*","requestStage":"Request"}]}))?;
        let initial = connection.submit_command("Target.setAutoAttach".into(), None, attach())?;
        let (ready, started) = tokio::sync::oneshot::channel();
        let (shutdown, mut stop) = tokio::sync::mpsc::unbounded_channel();
        let trace = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let events = trace.clone();
        let blocked = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let nav_blocked = Arc::new(tokio::sync::Mutex::new(None));
        let denied = blocked.clone();
        let denied_navigation = nav_blocked.clone();
        let task = tokio::spawn(async move {
            let mut ready = Some(ready);
            let mut pending: HashMap<CallId, (Option<SessionId>, Steps)> = HashMap::new();
            let mut authenticated = std::collections::HashSet::new();
            let mut pages = std::collections::HashSet::new();
            let mut closing = None;

            let outcome: Result<()> = async {
                loop {
                    let message = tokio::select! {
                        _ = stop.recv(), if closing.is_none() => {
                            closing = Some(connection.submit_command("Browser.close".into(),None,json!({}))?);
                            continue;
                        }
                        m = connection.next() => m.ok_or_else(||anyhow!("Browser policy connection closed"))??,
                    };
                    {
                        let mut trace = events.lock().await;
                        if trace.len() < 300 {
                            trace.push(match &message {
                                Message::Event(e) => json!({"method":e.method,"session":e.session_id,"targetType":e.params.pointer("/targetInfo/type"),"responseStage":e.params.get("responseStatusCode").is_some()}),
                                Message::Response(r) => json!({"reply":r.id,"error":r.error.is_some()}),
                            });
                        }
                    }
                    match message {
                        Message::Response(r) => {
                            if closing == Some(r.id) { return Ok(()); }
                            if r.error.is_some() { bail!("Browser policy command failed"); }
                            if r.id == initial {
                                if let Some(ready) = ready.take() { let _ = ready.send(()); }
                            }
                            if let Some((session, mut steps)) = pending.remove(&r.id) {
                                if let Some((method, params)) = steps.pop_front() {
                                    let id = connection.submit_command(method.into(),session.clone(),params)?;
                                    pending.insert(id,(session,steps));
                                }
                            }
                        }
                        Message::Event(e) if e.method == "Target.attachedToTarget" => {
                            if e.params.pointer("/targetInfo/type").and_then(Value::as_str)==Some("page") {
                                if let Some(id)=e.params.pointer("/targetInfo/targetId").and_then(Value::as_str) {pages.insert(id.to_owned());}
                            }
                            let session = string(&e.params,"sessionId");
                            if session.is_empty() { bail!("Missing policy target session"); }
                            let session = Some(SessionId::from(session));
                            let mut steps = VecDeque::new();
                            if e.params.pointer("/targetInfo/type").and_then(Value::as_str)==Some("page") {
                                steps.extend([
                                    ("Page.enable",json!({})),
                                    ("Network.enable",json!({})),
                                    ("Network.setBypassServiceWorker",json!({"bypass":true})),
                                    ("Page.addScriptToEvaluateOnNewDocument",json!({"source":WORKER_SCRIPT})),
                                ]);
                            }
                            steps.extend([
                                ("Target.setAutoAttach",attach()),
                                ("Runtime.runIfWaitingForDebugger",json!({})),
                            ]);
                            // Page.enable may reply only after the initial navigation resumes.
                            // Submit initialization in order, then resume, like the retained driver.
                            // Awaiting each reply here deadlocks a newly-created popup.
                            for (method,params) in steps {
                                let id=connection.submit_command(method.into(),session.clone(),params)?;
                                pending.insert(id,(session.clone(),VecDeque::new()));
                            }
                        }
                        Message::Event(e) if e.method == "Network.webSocketCreated" => {
                            let url=string(&e.params,"url");
                            let http=url.replacen("ws","http",1);
                            if !navigation(&http,&domains) {
                                let reason=policy(2,json!([http,domains])).ok().map(|v|mask(&string(&v,"reason"),&secrets)).unwrap_or_else(||"Navigation refused".into());
                                (log)(json!({"event":"browser.blocked","jobId":job_id,"url":mask(&url,&secrets).chars().take(300).collect::<String>(),"reason":reason}));
                                let mut denied=denied.lock().await;
                                if denied.len()<100 {denied.push(json!({"url":mask(&url,&secrets).chars().take(300).collect::<String>(),"reason":reason}));}
                            }
                        }
                        Message::Event(e) if e.method == "Fetch.authRequired" => {
                            let session = e.session_id.map(SessionId::from);
                            let id = string(&e.params,"requestId");
                            let challenge = e.params.get("authChallenge").cloned().unwrap_or(Value::Null);
                            let response = match &proxy {
                                Some((server,credentials)) if string(&challenge,"source")=="Proxy" && origin(&string(&challenge,"origin"))==*server && authenticated.insert(id.clone()) => json!({"response":"ProvideCredentials","username":credentials.username,"password":credentials.password}),
                                _ => json!({"response":"CancelAuth"}),
                            };
                            connection.submit_command("Fetch.continueWithAuth".into(),session,json!({"requestId":id,"authChallengeResponse":response}))?;
                        }
                        Message::Event(e) if e.method == "Fetch.requestPaused" => {
                            let session = e.session_id.map(SessionId::from);
                            let id = string(&e.params,"requestId");
                            let url = e.params.pointer("/request/url").and_then(Value::as_str).unwrap_or("");
                            let allowed = navigation(url,&domains) || url == "about:blank" || url.starts_with("data:") || url.starts_with("blob:");
                            { let mut trace=events.lock().await; if trace.len()<300 {trace.push(json!({"allowed":allowed,"resourceType":e.params.get("resourceType"),"scheme":url::Url::parse(url).ok().map(|u|u.scheme().to_owned()),"networkError":e.params.get("responseErrorReason")}));} }
                            let (method, params) = if !allowed {
                                let reason = policy(2,json!([url,domains])).ok().map(|v|string(&v,"reason")).filter(|s|!s.is_empty()).unwrap_or_else(||"Navigation refused".into());
                                let reason=mask(&reason,&secrets);
                                (log)(json!({"event":"browser.blocked","jobId":job_id,"url":mask(url,&secrets).chars().take(300).collect::<String>(),"reason":reason}));
                                let mut denied = denied.lock().await;
                                if denied.len()<100 {denied.push(json!({"url":mask(url,&secrets).chars().take(300).collect::<String>(),"reason":reason}));}
                                drop(denied);
                                if string(&e.params,"resourceType")=="Document" && pages.contains(&string(&e.params,"frameId")) {
                                    *denied_navigation.lock().await=Some(reason);
                                }
                                ("Fetch.failRequest",json!({"requestId":id,"errorReason":"BlockedByClient"}))
                            } else {
                                ("Fetch.continueRequest",json!({"requestId":id}))
                            };
                            let call = connection.submit_command(method.into(),session,params)?;
                            pending.insert(call,(None,VecDeque::new()));
                        }
                        _ => {}
                    }
                }
            }.await;
            if outcome.is_err() {
                *errors.lock().await = Some("Browser policy coverage lost".into());
                // Never resume or continue after losing enforcement.
                if let Ok(id) = connection.submit_command("Browser.close".into(), None, json!({})) {
                    let _ = timeout(LIMIT, async {
                        while let Some(Ok(message)) = connection.next().await {
                            if matches!(message,Message::Response(r) if r.id==id) {
                                break;
                            }
                        }
                    })
                    .await;
                }
            }
        });
        let guard = Self {
            shutdown,
            task,
            trace,
            blocked,
            nav_blocked,
        };
        timeout(LIMIT, started)
            .await
            .map_err(|_| anyhow!("Browser policy setup timed out"))?
            .map_err(|_| anyhow!("Browser policy setup failed"))?;
        Ok(guard)
    }
    pub fn finished(&self) -> bool {
        self.task.is_finished()
    }
}
fn attach() -> Value {
    json!({"autoAttach":true,"waitForDebuggerOnStart":true,"flatten":true,"filter":[{"type":"browser","exclude":true},{"type":"tab","exclude":true},{}]})
}
