//! One native sandbox executor turn. The existing ACP engine receives pinned configuration;
//! native code owns callbacks, checkpoints, bounded output and resource/relay teardown.
use crate::{
    acp::{self, Client},
    code::{Failure, Program, Run, Task},
    context::JournalContext,
    launch, meta, pin,
    resources::Resources,
    session::{Context, Session},
};
use js_json::{wtf16, JValue};
use sha2::{Digest, Sha256};
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc, time::Duration};
use zeroize::Zeroizing;
pub struct EngineLease {
    pub base_url: Option<String>,
    pub api_key: Option<Zeroizing<String>>,
    pub model: Option<String>,
    pub context_tokens: Option<f64>,
    pub no_proxy: String,
    _guard: Option<Box<dyn Send + Sync>>,
}
impl EngineLease {
    pub fn new(
        base_url: Option<String>,
        api_key: Option<String>,
        model: Option<String>,
        context_tokens: Option<f64>,
        no_proxy: String,
        guard: Option<Box<dyn Send + Sync>>,
    ) -> Self {
        Self {
            base_url,
            api_key: api_key.map(Zeroizing::new),
            model,
            context_tokens,
            no_proxy,
            _guard: guard,
        }
    }
}
pub type Open<'a> = Pin<Box<dyn Future<Output = Result<EngineLease, Failure>> + Send + 'a>>;
pub trait Engine: Send + Sync {
    /// The issuer of a relay/model lease owns cleanup from acquisition onward, including when
    /// its opening future is dropped. A lost incarnation is refused, never silently reopened.
    /// Machine expectation: None is an unpinned direct task; Some(null) pins this machine;
    /// a string pins the remote target key. Never collapse pinned-local into unpinned.
    fn open<'a>(
        &'a self,
        context: Arc<JournalContext>,
        model: Option<&'a str>,
        machine: Option<&'a JValue>,
    ) -> Open<'a>;
}
pub trait Review: Send + Sync {
    fn review<'a>(
        &'a self,
        context: Arc<JournalContext>,
        resources: &'a Resources,
        task: &'a Task,
        result: JValue,
    ) -> Run<'a>;
}
pub struct Spawn {
    pub command: String,
    pub args: Vec<String>,
}
pub struct Executor {
    pub engine: Arc<dyn Engine>,
    pub endpoint: String,
    pub spawn: Option<Spawn>,
    pub owner: Option<(u32, u32)>,
    pub sandboxed: bool,
    pub path: Option<String>,
    pub lang: Option<String>,
    pub review: Option<Arc<dyn Review>>,
}
pub struct Invocation {
    pub prompt: String,
    pub label: String,
    pub model: Option<String>,
    pub machine: Option<JValue>,
    pub context: String,
    pub preparation: String,
    pub guarded: bool,
    pub harness: String,
    pub capabilities: Vec<String>,
    pub domains: Vec<String>,
}
fn object(rows: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(rows.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn text(s: &str) -> JValue {
    JValue::Str(s.into())
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
fn internal(error: acp::RpcError) -> Failure {
    Failure::internal(error.message)
}
fn acp_error(error: acp::Error) -> Failure {
    match error {
        acp::Error::Cancelled => {
            let mut error = Failure::internal("Cancelled");
            error.public_message = Some("The task was cancelled.".into());
            error
        }
        acp::Error::HandshakeTimeout => {
            let mut error = Failure::internal("The coding harness did not answer.");
            error.public_message = Some("The coding harness did not answer.".into());
            error
        }
        acp::Error::NoSession => {
            let mut error = Failure::internal("The harness did not open a session.");
            error.public_message = Some("The coding harness did not open a session.".into());
            error
        }
        acp::Error::Remote(error) => Failure::internal(error.message),
        _ => {
            let mut error = Failure::internal("The coding harness stopped.");
            error.public_message = Some("The coding harness stopped unexpectedly.".into());
            error
        }
    }
}
fn blocked(value: JValue) -> Failure {
    Failure {
        status: None,
        message: value.get("message").as_str().unwrap_or("blocked").into(),
        public_message: value.get("publicMessage").as_str().map(str::to_owned),
        result: Some(value.get("result").clone()),
    }
}
fn identity(
    harness: JValue,
    version: JValue,
    model: Option<&str>,
    protocol: JValue,
    capabilities: &[String],
    preparation: &str,
    sandboxed: bool,
) -> Result<JValue, Failure> {
    let mut capabilities = capabilities.to_vec();
    capabilities.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    let identity = object(vec![
        ("backend", text("acp")),
        ("harness", harness),
        ("harnessVersion", version),
        ("model", model.map_or(JValue::Null, text)),
        ("provider", JValue::Null),
        ("protocolVersion", protocol),
        (
            "capabilities",
            JValue::Arr(capabilities.iter().map(|s| text(s)).collect()),
        ),
        ("promptPreparation", text(preparation)),
        ("sandbox", text(if sandboxed { "sandbox" } else { "spawn" })),
    ]);
    let JValue::Obj(mut canonical) = identity.clone() else {
        return Err(Failure::internal("Invalid coding identity"));
    };
    canonical.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
    let json = wtf16::stringify(&JValue::Obj(canonical))
        .ok_or_else(|| Failure::internal("Invalid coding identity"))?;
    let hash = Sha256::digest(json.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    Ok(object(vec![
        ("identity", identity),
        ("identityHash", text(&hash)),
    ]))
}
impl Executor {
    pub async fn execute(
        &self,
        context: Arc<JournalContext>,
        resources: &mut Resources,
        invocation: Invocation,
    ) -> Result<JValue, Failure> {
        let workspace = resources.workspace.clone();
        let branch = workspace.get("branch").clone();
        let base = if workspace.get("baseSha").is_nullish() {
            JValue::Null
        } else {
            workspace.get("baseSha").clone()
        };
        let title = wtf16::slice(&invocation.label, 120);
        let mut lease = None;
        let mut session: Option<Arc<Session>> = None;
        let mut client: Option<Client> = None;
        let mut scope = None;
        let mut reported = None;
        let mut result = None;
        let executed:Result<(),Failure>=async{
            context.active().map_err(internal)?;let mut cancelled=context.cancellation();let already_cancelled=*cancelled.borrow();if already_cancelled{return Err(acp_error(acp::Error::Cancelled));}
            lease=Some(tokio::select!{biased;_=cancelled.changed()=>return Err(acp_error(acp::Error::Cancelled)),opened=self.engine.open(context.clone(),invocation.model.as_deref(),invocation.machine.as_ref())=>opened?});
            context.active().map_err(internal)?;let endpoint=lease.as_ref().ok_or_else(Failure::unavailable)?;let chosen=invocation.model.as_deref().or(endpoint.model.as_deref());
            let callbacks=Arc::new(if invocation.guarded{Session::new_guarded(context.task().into(),PathBuf::from(workspace.get("path").as_str().unwrap_or("")),invocation.capabilities.clone(),invocation.domains.clone(),context.clone())}else{Session::new(context.task().into(),PathBuf::from(workspace.get("path").as_str().unwrap_or("")),invocation.capabilities.clone(),invocation.domains.clone(),context.clone())});session=Some(callbacks.clone());
            context.event("checkpoint.created",object(vec![("branch",branch.clone()),("task",text(&title)),("baseSha",base.clone())])).map_err(internal)?;
            context.event("step.started",object(vec![("id",text("harness.config")),("title",text("Pin the harness configuration"))])).map_err(internal)?;
            let cwd=PathBuf::from(workspace.get("path").as_str().unwrap_or(""));let home=workspace.get("home").as_str().map(PathBuf::from);
            let pinned=pin::Rendered::render(pin::Options{harness:&invocation.harness,model:chosen.unwrap_or(""),engine:endpoint.base_url.as_deref().unwrap_or(""),api_key:endpoint.api_key.as_deref().map(|s|s.as_str()),context_tokens:endpoint.context_tokens,output_tokens:None}).and_then(|p|p.write(&cwd,home.as_deref(),self.owner));
            let pinned=match pinned{Ok(pinned)=>pinned,Err(error)=>{context.event("step.completed",object(vec![("id",text("harness.config")),("failed",JValue::Bool(true)),("error",text(&wtf16::slice(&error.message,300)))])).map_err(internal)?;return Err(error.into());}};
            let mut pin_event=object(vec![("id",text("harness.config"))]);if let JValue::Obj(fields)=pinned{for (key,value) in fields{set(&mut pin_event,&key,value);}}context.event("step.completed",pin_event).map_err(internal)?;
            let proxy=resources.grant().map_or(JValue::Null,|grant|object(vec![("url",text(&format!("http://task:{}@{}",grant.token(),resources.proxy_endpoint()))),("noProxy",text(&endpoint.no_proxy))]));
            let env=launch::environment(self.path.as_deref(),self.lang.as_deref(),home.as_deref().and_then(|p|p.to_str()),&object(vec![]),&proxy);
            let settings=if invocation.harness=="claude-code"{
                let Some(home)=home.as_deref().filter(|p|p.is_absolute())else{return Err(Failure::public(409,"Claude Code needs the task private HOME for its pinned settings."));};
                crate::files::read(home,".claude/settings.json",crate::files::MAX_FILE_BYTES).ok().and_then(|s|wtf16::parse(&s).ok()).filter(JValue::is_object).ok_or_else(||Failure::public(409,"Claude Code’s pinned settings are missing or invalid."))?
            }else{JValue::Null};
            let meta=launch::metadata(&object(vec![("edit",text("ask")),("bash",text("ask")),("webfetch",text("ask"))]),Some(&invocation.harness),&settings).map_err(|_|Failure::public(409,"Claude Code needs noevia’s pinned session settings."))?;
            let options=acp::Options{endpoint:self.endpoint.clone(),cwd:cwd.to_string_lossy().into_owned(),env,session_meta:if meta==JValue::Null{None}else{Some(meta)},model:chosen.map(str::to_owned),handshake_timeout:Duration::from_secs(30)};
            client=Some(if !self.endpoint.is_empty(){Client::connect(options,callbacks.clone(),context.cancellation()).await}else if let Some(spawn)=&self.spawn{Client::spawn(options,&spawn.command,&spawn.args,callbacks.clone(),context.cancellation(),Duration::from_secs(5)).await}else{return Err(Failure::public(409,"No coding harness is configured on this server."));}.map_err(acp_error)?);
            if context.active().is_ok(){context.event("progress",object(vec![("stage",text("running"))])).map_err(internal)?;}
            let agent=client.as_mut().ok_or_else(Failure::unavailable)?;let agent_info=agent.agent.clone();let prompt=if invocation.context.is_empty(){invocation.prompt.clone()}else{format!("{}\n\nTask:\n{}",invocation.context,invocation.prompt)};
            let outcome=agent.turn(&prompt).await;callbacks.flush().map_err(internal)?;
            if let Some(error)=callbacks.blocked_error().map_err(internal)?{return Err(blocked(error));}
            let outcome=outcome.map_err(acp_error)?;let measured=callbacks.metadata(agent_info,meta::usage(outcome.get("_meta"))).map_err(internal)?;
            let harness=if measured.get("harness").truthy(){measured.get("harness").clone()}else{text(&invocation.harness)};
            let identity=identity(harness,measured.get("harnessVersion").clone(),chosen,measured.get("protocolVersion").clone(),&invocation.capabilities,&invocation.preparation,self.sandboxed)?;
            scope=Some(identity.clone());reported=Some(measured.clone());
            let head=context.account().workspaces.head_sha(context.task(),&std::sync::atomic::AtomicBool::new(false)).map_or(JValue::Null,|s|text(&s));
            context.event("checkpoint.created",object(vec![("branch",branch.clone()),("task",text(&title)),("identityHash",identity.get("identityHash").clone()),("identity",identity.get("identity").clone()),("meta",measured.clone()),("baseSha",base.clone()),("headSha",head)])).map_err(internal)?;
            let mut completed=object(vec![("stopReason",if outcome.get("stopReason").truthy(){outcome.get("stopReason").clone()}else{text("end_turn")}),("branch",branch.clone()),("identityHash",identity.get("identityHash").clone()),("meta",measured)]);if let JValue::Obj(summary)=callbacks.summary().map_err(internal)?{for (key,value) in summary{set(&mut completed,&key,value);}}result=Some(completed);Ok(())
        }.await;
        if let Some(client) = client.as_mut() {
            client.close().await;
        } else if let Some(session) = &session {
            session.end();
        }
        drop(lease);
        let released = resources.release().await;
        if let Some(completed) = result.as_mut() {
            if resources.grant().is_some() {
                set(
                    completed,
                    "network",
                    released.network.clone().unwrap_or(JValue::Null),
                );
            }
        }
        if let Some(identity) = scope {
            let _ = context.event(
                "checkpoint.created",
                object(vec![
                    ("branch", branch),
                    ("task", text(&title)),
                    ("identityHash", identity.get("identityHash").clone()),
                    ("identity", identity.get("identity").clone()),
                    ("meta", reported.unwrap_or(JValue::Null)),
                    ("baseSha", base),
                    (
                        "headSha",
                        released
                            .workspace
                            .as_ref()
                            .map(|r| r.get("headSha").clone())
                            .unwrap_or(JValue::Null),
                    ),
                ]),
            );
        }
        executed?;
        if resources.grant().is_some() && !released.confirmed {
            let mut error = Failure::internal("Native network cleanup could not be confirmed.");
            error.result = result;
            return Err(error);
        }
        result.ok_or_else(|| Failure::internal("Code task returned no result"))
    }
}
impl Program for Executor {
    fn run<'a>(
        &'a self,
        context: Arc<JournalContext>,
        resources: &'a mut Resources,
        task: &'a Task,
    ) -> Run<'a> {
        Box::pin(async move {
            let invocation = Invocation {
                prompt: task.request.prompt.clone(),
                label: task.request.prompt.clone(),
                model: task.request.model.clone(),
                machine: None,
                context: task.context.clone(),
                preparation: task.request.preparation.clone(),
                guarded: task.settings.guarded,
                harness: task.request.harness.clone(),
                capabilities: task.request.capabilities.clone(),
                domains: task.request.domains.clone(),
            };
            let result = self.execute(context.clone(), resources, invocation).await?;
            if *context.cancellation().borrow() || !task.settings.reviewed {
                return Ok(result);
            }
            let review = self
                .review
                .as_ref()
                .ok_or_else(|| Failure::public(503, "Code review is unavailable."))?;
            review.review(context, resources, task, result).await
        })
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{
        account::Account,
        context::SystemClock,
        facts::{CodeConfig, Repository},
        Scope,
    };
    use server_projects::task_journals::Surface;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::{
            tcp::{OwnedReadHalf, OwnedWriteHalf},
            TcpListener,
        },
    };
    const USER: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const TASK: &str = "11111111-1111-4111-8111-111111111111";
    struct Endpoint;
    impl Engine for Endpoint {
        fn open<'a>(
            &'a self,
            _context: Arc<JournalContext>,
            _model: Option<&'a str>,
            _machine: Option<&'a JValue>,
        ) -> Open<'a> {
            Box::pin(async {
                Ok(EngineLease::new(
                    Some("http://engine.invalid/v1".into()),
                    Some("synthetic-key".into()),
                    Some("synthetic".into()),
                    None,
                    "engine.invalid".into(),
                    None,
                ))
            })
        }
    }
    async fn read(reader: &mut BufReader<OwnedReadHalf>) -> Option<JValue> {
        let mut line = String::new();
        if reader.read_line(&mut line).await.unwrap() == 0 {
            return None;
        }
        Some(wtf16::parse(&line).unwrap())
    }
    async fn send(writer: &mut OwnedWriteHalf, value: JValue) {
        let _ = writer
            .write_all((wtf16::stringify(&value).unwrap() + "\n").as_bytes())
            .await;
    }
    fn reply(request: &JValue, result: JValue) -> JValue {
        object(vec![
            ("jsonrpc", text("2.0")),
            ("id", request.get("id").clone()),
            ("result", result),
        ])
    }
    async fn sidecar(listener: TcpListener, mode: String) -> (JValue, Vec<JValue>) {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut writer) = stream.into_split();
        let mut reader = BufReader::new(r);
        assert_eq!(
            read(&mut reader).await.unwrap().get("noevia").as_str(),
            Some("start")
        );
        let init = read(&mut reader).await.unwrap();
        send(
            &mut writer,
            reply(
                &init,
                object(vec![
                    ("protocolVersion", JValue::Num(1.0)),
                    (
                        "agentInfo",
                        object(vec![
                            ("name", text("synthetic-agent")),
                            ("version", text("1")),
                        ]),
                    ),
                ]),
            ),
        )
        .await;
        let new = read(&mut reader).await.unwrap();
        send(
            &mut writer,
            reply(
                &new,
                if mode == "missing_session" {
                    object(vec![])
                } else {
                    object(vec![("sessionId", text("session"))])
                },
            ),
        )
        .await;
        if mode == "missing_session" {
            assert!(read(&mut reader).await.is_none());
            return (JValue::Null, vec![]);
        }
        let prompt = read(&mut reader).await.unwrap();
        let sent = match prompt.get("params").get("prompt") {
            JValue::Arr(parts) => parts.first().unwrap().get("text").clone(),
            _ => panic!(),
        };
        if mode == "closed_prompt" {
            return (sent, vec![]);
        }
        if mode == "remote_error" {
            send(
                &mut writer,
                object(vec![
                    ("jsonrpc", text("2.0")),
                    ("id", prompt.get("id").clone()),
                    (
                        "error",
                        object(vec![
                            ("code", JValue::Num(-32000.0)),
                            ("message", text("synthetic engine refused")),
                        ]),
                    ),
                ]),
            )
            .await;
        } else if mode == "guarded_malformed" {
            for (n, tool) in [
                JValue::Null,
                object(vec![("kind", text("execute"))]),
                object(vec![("kind", text("edit"))]),
            ]
            .into_iter()
            .enumerate()
            {
                send(
                    &mut writer,
                    object(vec![
                        ("jsonrpc", text("2.0")),
                        ("id", JValue::Num(80.0 + n as f64)),
                        ("method", text("session/request_permission")),
                        (
                            "params",
                            object(vec![
                                ("toolCall", tool),
                                (
                                    "options",
                                    JValue::Arr(vec![
                                        object(vec![
                                            ("optionId", text("allow")),
                                            ("kind", text("allow_once")),
                                        ]),
                                        object(vec![
                                            ("optionId", text("reject")),
                                            ("kind", text("reject_once")),
                                        ]),
                                    ]),
                                ),
                            ]),
                        ),
                    ]),
                )
                .await;
            }
        } else {
            if mode == "bad_json_then_reply" {
                writer.write_all(b"not-json\n").await.unwrap();
            }
            send(
                &mut writer,
                object(vec![
                    ("jsonrpc", text("2.0")),
                    ("method", text("session/update")),
                    (
                        "params",
                        object(vec![(
                            "update",
                            object(vec![
                                ("sessionUpdate", text("agent_message_chunk")),
                                (
                                    "content",
                                    object(vec![("type", text("text")), ("text", text("hello"))]),
                                ),
                            ]),
                        )]),
                    ),
                ]),
            )
            .await;
            send(
                &mut writer,
                object(vec![
                    ("jsonrpc", text("2.0")),
                    ("method", text("session/update")),
                    (
                        "params",
                        object(vec![(
                            "update",
                            object(vec![
                                ("sessionUpdate", text("usage_update")),
                                (
                                    "usage",
                                    object(vec![
                                        ("inputTokens", JValue::Num(3.0)),
                                        ("outputTokens", JValue::Num(4.0)),
                                        ("totalTokens", JValue::Num(7.0)),
                                    ]),
                                ),
                            ]),
                        )]),
                    ),
                ]),
            )
            .await;
            send(
                &mut writer,
                reply(&prompt, object(vec![("stopReason", text("end_turn"))])),
            )
            .await;
        }
        let mut answers = vec![];
        while let Some(message) = read(&mut reader).await {
            if message.get("result") != &JValue::Undefined {
                answers.push(message.get("result").clone());
            }
        }
        (sent, answers)
    }
    fn normalize(value: JValue, cwd: &str, home: &str) -> JValue {
        match value {
            JValue::Str(value) => {
                if value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
                    text(&"a".repeat(40))
                } else {
                    text(
                        &value
                            .replace(cwd, "/synthetic/tree")
                            .replace(home, "/synthetic/home"),
                    )
                }
            }
            JValue::Obj(rows) => JValue::Obj(
                rows.into_iter()
                    .map(|(k, v)| (k, normalize(v, cwd, home)))
                    .collect(),
            ),
            JValue::Arr(rows) => {
                JValue::Arr(rows.into_iter().map(|v| normalize(v, cwd, home)).collect())
            }
            v => v,
        }
    }
    #[tokio::test]
    async fn complete_executor_turns_and_sidecar_failures_match_node() {
        let fixture = wtf16::parse(include_str!("../tests/fixtures/execution-node.json")).unwrap();
        let JValue::Arr(cases) = fixture.get("cases") else {
            panic!()
        };
        assert_eq!(cases.len(), 6);
        for case in cases {
            let mode = case.get("mode").as_str().unwrap();
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(root.path().join("users").join(USER)).unwrap();
            let repo = root.path().join("repo");
            std::fs::create_dir(&repo).unwrap();
            for args in [
                vec!["init", "-q"],
                vec!["config", "user.email", "synthetic@example.invalid"],
                vec!["config", "user.name", "Synthetic"],
                vec![
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "synthetic",
                ],
            ] {
                assert!(std::process::Command::new("git")
                    .args(args)
                    .current_dir(&repo)
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_SYSTEM", "/dev/null")
                    .status()
                    .unwrap()
                    .success());
            }
            let account = Arc::new(
                Account::open(
                    root.path(),
                    USER,
                    &CodeConfig::from_env(|_| None),
                    "epoch".into(),
                    1,
                )
                .unwrap(),
            );
            let caps: Vec<String> = vec!["read_repository".into(), "edit_file".into()];
            account
                .create(
                    Surface::Code,
                    TASK,
                    "project",
                    caps.iter().map(|s| text(s)).collect(),
                    1,
                )
                .unwrap();
            let guard = account
                .control
                .begin(
                    Scope {
                        user: USER.into(),
                        project: "project".into(),
                        surface: Surface::Code,
                    },
                    TASK.into(),
                )
                .unwrap();
            let context = Arc::new(
                JournalContext::new(
                    account.clone(),
                    guard,
                    Arc::new(SystemClock),
                    Duration::from_secs(30),
                )
                .unwrap(),
            );
            context.event("job.started", object(vec![])).unwrap();
            let request = crate::start::Request {
                repository: Repository {
                    id: "repo".into(),
                    path: repo,
                },
                prompt: "synthetic task".into(),
                capabilities: caps.clone(),
                domains: vec![],
                harness: "opencode".into(),
                preparation: "direct".into(),
                model: None,
                approve_plan: false,
            };
            let mut resources = Resources::claim(
                context.clone(),
                &request,
                None,
                false,
                &std::sync::atomic::AtomicBool::new(false),
            )
            .await
            .unwrap();
            let cwd = resources.workspace.get("path").as_str().unwrap().to_owned();
            let home = resources.workspace.get("home").as_str().unwrap().to_owned();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = listener.local_addr().unwrap().to_string();
            let server = tokio::spawn(sidecar(listener, mode.into()));
            let executor = Executor {
                spawn: None,
                engine: Arc::new(Endpoint),
                endpoint,
                owner: None,
                sandboxed: true,
                path: None,
                lang: None,
                review: None,
            };
            let outcome = executor
                .execute(
                    context.clone(),
                    &mut resources,
                    Invocation {
                        prompt: request.prompt.clone(),
                        label: request.prompt.clone(),
                        model: None,
                        machine: None,
                        context: "Reference context".into(),
                        preparation: "direct".into(),
                        guarded: mode == "guarded_malformed",
                        harness: request.harness.clone(),
                        capabilities: caps,
                        domains: vec![],
                    },
                )
                .await;
            let (prompt, answers) = server.await.unwrap();
            let (result, error) = match outcome {
                Ok(result) => (result, JValue::Null),
                Err(error) => (
                    JValue::Null,
                    object(vec![
                        ("error", text(&error.recorded())),
                        ("result", error.result.unwrap_or(JValue::Null)),
                    ]),
                ),
            };
            let journal = std::fs::read_to_string(
                root.path()
                    .join("users")
                    .join(USER)
                    .join("jobs")
                    .join(format!("{TASK}.jsonl")),
            )
            .unwrap();
            let events = journal
                .lines()
                .map(|line| wtf16::parse(line).unwrap())
                .filter(|row| {
                    !matches!(
                        row.get("type").as_str(),
                        Some("job.created" | "job.started")
                    )
                })
                .map(|row| {
                    object(vec![
                        ("type", row.get("type").clone()),
                        ("data", row.get("data").clone()),
                    ])
                })
                .collect();
            let actual = normalize(
                object(vec![
                    ("mode", text(mode)),
                    ("events", JValue::Arr(events)),
                    ("result", result),
                    ("error", error),
                    ("cleaned", JValue::Num(1.0)),
                    ("prompt", prompt),
                    ("answers", JValue::Arr(answers)),
                ]),
                &cwd,
                &home,
            );
            assert_eq!(actual, *case, "mode {mode}");
        }
    }
}
