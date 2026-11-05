//! ACP framing to the existing sandbox supervisor. Dropping the connection stops the remote
//! process group. Policy, workspace access and approvals are native caller callbacks.
use js_json::{wtf16, JValue};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
    sync::watch,
};
const MAX_UNITS: usize = 16 * 1024 * 1024;
pub type HandlerFuture<'a> = Pin<Box<dyn Future<Output = Result<JValue, RpcError>> + Send + 'a>>;
pub trait Handlers: Send + Sync {
    // Prepare synchronously in receive order; only the returned wait may run concurrently.
    fn request(self: Arc<Self>, method: &str, params: JValue) -> HandlerFuture<'static>;
    fn update(&self, update: JValue);
    fn end(&self) {}
    fn halted(&self) -> bool {
        false
    }
    fn drain_on_end(&self) -> bool {
        false
    }
}
#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<JValue>,
}
#[derive(Debug)]
pub enum Error {
    Cancelled,
    Unavailable,
    Protocol,
    Remote(RpcError),
    HandshakeTimeout,
}
pub struct Options {
    pub endpoint: String,
    pub cwd: String,
    pub env: JValue,
    pub session_meta: Option<JValue>,
    pub model: Option<String>,
    pub handshake_timeout: Duration,
}
type CallbackReply = (JValue, Result<JValue, RpcError>);
pub struct Client {
    reader: BufReader<OwnedReadHalf>,
    line_bytes: Vec<u8>,
    callbacks: tokio::task::JoinSet<CallbackReply>,
    writer: OwnedWriteHalf,
    handlers: Arc<dyn Handlers>,
    cancel: watch::Receiver<bool>,
    next: u64,
    was_cancelled: bool,
    stopped: bool,
    session: JValue,
    model: Option<String>,
    pub agent: JValue,
}
fn obj(fields: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn text(s: &str) -> JValue {
    JValue::Str(s.into())
}
fn read_agent(result: &JValue) -> Result<JValue, Error> {
    let info = [
        result.get("agentInfo"),
        result.get("serverInfo"),
        result.get("_meta").get("agentInfo"),
    ]
    .into_iter()
    .find(|v| v.truthy())
    .unwrap_or(&JValue::Null);
    let clipped = |key, max| {
        info.get(key)
            .as_str()
            .map(|s| JValue::Str(wtf16::slice(s, max)))
            .unwrap_or(JValue::Null)
    };
    let version = result.get("protocolVersion");
    let number = match version {
        JValue::Undefined => f64::NAN,
        JValue::Null => 0.0,
        JValue::Bool(v) => {
            if *v {
                1.0
            } else {
                0.0
            }
        }
        JValue::Num(n) => *n,
        _ => server_account::util::js_number(
            &js_json::to_js_string(version).map_err(|_| Error::Protocol)?,
        ),
    };
    Ok(obj(vec![
        ("name", clipped("name", 80)),
        ("version", clipped("version", 40)),
        (
            "protocolVersion",
            if number.is_finite() && number >= 0.0 {
                JValue::Num(number)
            } else {
                JValue::Null
            },
        ),
    ]))
}
impl Client {
    pub async fn connect(
        options: Options,
        handlers: Arc<dyn Handlers>,
        cancel: watch::Receiver<bool>,
    ) -> Result<Self, Error> {
        let deadline = tokio::time::Instant::now()
            .checked_add(options.handshake_timeout)
            .ok_or(Error::Protocol)?;
        let mut cancelled = cancel.clone();
        if *cancelled.borrow() {
            return Err(Error::Cancelled);
        }
        let stream = tokio::select! {biased;
            _=cancelled.changed()=>return Err(Error::Cancelled),
            _=tokio::time::sleep_until(deadline)=>return Err(Error::HandshakeTimeout),
            stream=TcpStream::connect(&options.endpoint)=>stream.map_err(|_|Error::Unavailable)?,
        };
        let (read, write) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(read),
            line_bytes: Vec::new(),
            callbacks: tokio::task::JoinSet::new(),
            writer: write,
            handlers,
            cancel,
            next: 1,
            was_cancelled: false,
            stopped: false,
            session: JValue::Null,
            model: options.model.clone(),
            agent: JValue::Null,
        };
        let result = tokio::select! {biased;
            _=cancelled.changed()=>Err(Error::Cancelled),
            _=tokio::time::sleep_until(deadline)=>Err(Error::HandshakeTimeout),
            result=client.handshake(options)=>result,
        };
        if let Err(error) = result {
            client.was_cancelled = matches!(error, Error::Cancelled);
            client.close().await;
            return Err(error);
        }
        Ok(client)
    }
    async fn handshake(&mut self, options: Options) -> Result<(), Error> {
        // Must precede initialize: this is the existing supervisor's launch contract.
        self.send(obj(vec![
            ("noevia", text("start")),
            ("cwd", text(&options.cwd)),
            ("env", options.env),
        ]))
        .await?;
        self.agent = self
            .request(
                "initialize",
                obj(vec![
                    ("protocolVersion", JValue::Num(1.0)),
                    (
                        "clientCapabilities",
                        obj(vec![
                            (
                                "fs",
                                obj(vec![
                                    ("readTextFile", JValue::Bool(true)),
                                    ("writeTextFile", JValue::Bool(true)),
                                ]),
                            ),
                            ("terminal", JValue::Bool(false)),
                        ]),
                    ),
                    (
                        "clientInfo",
                        obj(vec![("name", text("noevia")), ("version", text("1"))]),
                    ),
                ]),
            )
            .await?;
        self.agent = read_agent(&self.agent)?;
        let mut params = vec![
            ("cwd", text(&options.cwd)),
            ("mcpServers", JValue::Arr(vec![])),
        ];
        if let Some(meta) = options.session_meta {
            params.push(("_meta", meta));
        }
        let session = self.request("session/new", obj(params)).await?;
        self.session = session.get("sessionId").clone();
        if !self.session.truthy() {
            return Err(Error::Protocol);
        }
        Ok(())
    }
    /// Consumes the connection, ensuring completion, error and cancellation all close the sandbox.
    pub async fn prompt(mut self, prompt: &str) -> Result<JValue, Error> {
        let result = self.turn(prompt).await;
        self.close().await;
        result
    }
    /// One turn stops the engine, retaining callbacks until the actor has recorded its
    /// checkpoint and calls close from finally, in the legacy journal order.
    pub async fn turn(&mut self, prompt: &str) -> Result<JValue, Error> {
        if self.stopped {
            return Err(Error::Unavailable);
        }
        let mut params = vec![
            ("sessionId", self.session.clone()),
            (
                "prompt",
                JValue::Arr(vec![obj(vec![
                    ("type", text("text")),
                    ("text", text(prompt)),
                ])]),
            ),
        ];
        if let Some(model) = &self.model {
            params.push((
                "_meta",
                obj(vec![("noevia", obj(vec![("model", text(model))]))]),
            ));
        }
        let result = self.request("session/prompt", obj(params)).await;
        self.stop().await;
        result
    }
    async fn send(&mut self, message: JValue) -> Result<(), Error> {
        let mut line = wtf16::stringify(&message).ok_or(Error::Protocol)?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|_| Error::Unavailable)
    }
    async fn request(&mut self, method: &str, params: JValue) -> Result<JValue, Error> {
        let mut cancel = self.cancel.clone();
        if *cancel.borrow() {
            self.was_cancelled = true;
            return Err(Error::Cancelled);
        }
        let result = tokio::select! {
            biased;
            _ = cancel.changed() => Err(Error::Cancelled),
            result = self.request_inner(method,params) => result,
        };
        if matches!(result, Err(Error::Cancelled)) {
            self.was_cancelled = true;
        }
        result
    }
    async fn request_inner(&mut self, method: &str, params: JValue) -> Result<JValue, Error> {
        let id = self.next;
        self.next = id.checked_add(1).ok_or(Error::Protocol)?;
        self.send(obj(vec![
            ("jsonrpc", text("2.0")),
            ("id", JValue::Num(id as f64)),
            ("method", text(method)),
            ("params", params),
        ]))
        .await?;
        loop {
            let line = tokio::select! {
                biased;
                reply = self.callbacks.join_next(), if !self.callbacks.is_empty() => {
                    if let Some(reply) = reply {
                        let (id, result) = reply.map_err(|_| Error::Protocol)?;
                        self.answer(id, true, result).await?;
                        if self.callbacks.is_empty()&&self.handlers.halted(){self.was_cancelled=true;return Err(Error::Cancelled);}
                    }
                    continue;
                },
                line = Self::line(&mut self.reader, &mut self.line_bytes) => line?,
            };
            let Ok(message) = wtf16::parse(&line) else {
                // The legacy line reader discards malformed JSON and continues.
                continue;
            };
            let raw_method = message.get("method");
            if raw_method == &JValue::Undefined {
                if message.get("id").as_num() != Some(id as f64) {
                    continue;
                }
                if message.get("error").truthy() {
                    let error = message.get("error");
                    return Err(Error::Remote(RpcError {
                        code: error.get("code").as_num().unwrap_or(-32603.0) as i64,
                        message: error
                            .get("message")
                            .as_str()
                            .unwrap_or("The harness reported an error.")
                            .into(),
                        data: None,
                    }));
                }
                return Ok(message.get("result").clone());
            }
            let incoming_method = raw_method.as_str().unwrap_or("");
            if incoming_method == "session/update" {
                let params = message.get("params");
                let update = if params.get("update").truthy() {
                    params.get("update")
                } else {
                    params
                };
                self.handlers.update(update.clone());
                continue;
            }
            if message.get("id") == &JValue::Undefined {
                continue;
            }
            let id = message.get("id").clone();
            if matches!(
                incoming_method,
                "session/request_permission" | "fs/read_text_file" | "fs/write_text_file"
            ) {
                let params = message.get("params");
                let params = if params.truthy() {
                    params.clone()
                } else {
                    obj(vec![])
                };
                let future = self.handlers.clone().request(incoming_method, params);
                let permission = incoming_method == "session/request_permission";
                let _ = self.callbacks.spawn(async move {
                    let result = future.await;
                    (
                        id,
                        if permission {
                            result.map(|outcome| obj(vec![("outcome", outcome)]))
                        } else {
                            result
                        },
                    )
                });
                continue;
            }
            self.answer(
                id,
                false,
                Err(RpcError {
                    code: -32601,
                    message: "Method not supported by this client".into(),
                    data: None,
                }),
            )
            .await?;
        }
    }
    async fn answer(
        &mut self,
        id: JValue,
        supported: bool,
        result: Result<JValue, RpcError>,
    ) -> Result<(), Error> {
        let payload = match result {
            Ok(value) => (
                "result",
                if value == JValue::Undefined {
                    JValue::Null
                } else {
                    value
                },
            ),
            Err(error) => {
                let code = if !supported {
                    -32601
                } else if error.code == -32602 {
                    -32602
                } else {
                    -32603
                };
                let mut fields = vec![
                    ("code", JValue::Num(code as f64)),
                    ("message", text(&error.message)),
                ];
                if code == -32602 {
                    if let Some(data) = error
                        .data
                        .filter(|d| matches!(d, JValue::Obj(_) | JValue::Arr(_)))
                    {
                        fields.push(("data", data));
                    }
                }
                ("error", obj(fields))
            }
        };
        self.send(obj(vec![("jsonrpc", text("2.0")), ("id", id), payload]))
            .await
    }
    async fn line<R: tokio::io::AsyncRead + Unpin>(
        reader: &mut BufReader<R>,
        bytes: &mut Vec<u8>,
    ) -> Result<String, Error> {
        loop {
            let chunk = reader.fill_buf().await.map_err(|_| Error::Unavailable)?;
            if chunk.is_empty() {
                return Err(Error::Unavailable);
            }
            let newline = chunk.iter().position(|b| *b == b'\n');
            let used = newline.map_or(chunk.len(), |i| i + 1);
            if bytes.len().saturating_add(used) > MAX_UNITS * 4 {
                return Err(Error::Protocol);
            }
            bytes.extend_from_slice(chunk.get(..used).ok_or(Error::Protocol)?);
            reader.consume(used);
            if newline.is_some() {
                let line =
                    server_account::util::js_trim(&String::from_utf8_lossy(bytes)).to_owned();
                if line.is_empty() {
                    bytes.clear();
                    continue;
                }
                bytes.clear();
                if line.encode_utf16().count() > MAX_UNITS {
                    return Err(Error::Protocol);
                }
                return Ok(line);
            }
        }
    }
}

impl Client {
    fn cancel_notice(&mut self) {
        if !self.stopped && (self.was_cancelled || *self.cancel.borrow()) && self.session.truthy() {
            let message = obj(vec![
                ("jsonrpc", text("2.0")),
                ("method", text("session/cancel")),
                ("params", obj(vec![("sessionId", self.session.clone())])),
            ]);
            if let Some(mut line) = wtf16::stringify(&message) {
                line.push('\n');
                // EOF remains authoritative if the best-effort notification is backpressured.
                let _ = self.writer.try_write(line.as_bytes());
            }
        }
    }
    /// Call from the actor's finally path, before its terminal journal event. Stop the sidecar
    /// first, then let every aborted approval callback finish recording its decision.
    pub async fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.cancel_notice();
        let _ = self.writer.shutdown().await;
        self.stopped = true;
    }
    pub async fn close(&mut self) {
        self.stop().await;
        self.handlers.end();
        if self.handlers.drain_on_end() {
            while self.callbacks.join_next().await.is_some() {}
        } else {
            self.callbacks.abort_all();
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.cancel_notice();
        self.handlers.end();
    }
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    struct Handler {
        entered: tokio::sync::Notify,
        hang: bool,
        updates: std::sync::Mutex<Vec<JValue>>,
    }
    impl Handlers for Handler {
        fn request(self: Arc<Self>, _: &str, _: JValue) -> HandlerFuture<'static> {
            Box::pin(async move {
                self.entered.notify_one();
                if self.hang {
                    std::future::pending::<()>().await;
                }
                Ok(obj(vec![
                    ("outcome", text("selected")),
                    ("optionId", text("allow")),
                ]))
            })
        }
        fn update(&self, update: JValue) {
            self.updates.lock().unwrap().push(update);
        }
    }
    fn handler(hang: bool) -> Arc<Handler> {
        Arc::new(Handler {
            entered: tokio::sync::Notify::new(),
            hang,
            updates: std::sync::Mutex::new(vec![]),
        })
    }
    struct Fake {
        received: Vec<JValue>,
        read: BufReader<OwnedReadHalf>,
        write: OwnedWriteHalf,
    }
    impl Fake {
        async fn accept(listener: TcpListener) -> Self {
            let (s, _) = listener.accept().await.unwrap();
            let (r, w) = s.into_split();
            Self {
                received: Vec::new(),
                read: BufReader::new(r),
                write: w,
            }
        }
        async fn read(&mut self) -> JValue {
            let mut s = String::new();
            assert!(self.read.read_line(&mut s).await.unwrap() > 0);
            let value = wtf16::parse(&s).unwrap();
            self.received.push(value.clone());
            value
        }
        async fn send(&mut self, v: JValue) {
            let s = wtf16::stringify(&v).unwrap() + "\n";
            self.write.write_all(s.as_bytes()).await.unwrap();
        }
        async fn reply(&mut self, request: &JValue, result: JValue) {
            self.send(obj(vec![
                ("jsonrpc", text("2.0")),
                ("id", request.get("id").clone()),
                ("result", result),
            ]))
            .await;
        }
        async fn handshake(&mut self) {
            let start = self.read().await;
            assert_eq!(start.get("noevia").as_str(), Some("start"));
            assert_eq!(start.get("cwd").as_str(), Some("/synthetic/tree"));
            let init = self.read().await;
            assert_eq!(init.get("method").as_str(), Some("initialize"));
            self.reply(
                &init,
                obj(vec![
                    ("protocolVersion", JValue::Num(1.0)),
                    (
                        "agentInfo",
                        obj(vec![
                            ("name", text("synthetic-agent")),
                            ("version", text("1")),
                        ]),
                    ),
                ]),
            )
            .await;
            let new = self.read().await;
            assert_eq!(new.get("method").as_str(), Some("session/new"));
            assert_eq!(new.get("params").get("mcpServers"), &JValue::Arr(vec![]));
            self.reply(&new, obj(vec![("sessionId", text("session"))]))
                .await;
        }
        async fn closed(&mut self) {
            let mut s = String::new();
            let first = tokio::time::timeout(Duration::from_secs(2), self.read.read_line(&mut s))
                .await
                .unwrap()
                .unwrap();
            if first == 0 {
                return;
            }
            let cancel = wtf16::parse(&s).unwrap();
            assert_eq!(cancel.get("method").as_str(), Some("session/cancel"));
            s.clear();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), self.read.read_line(&mut s))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        }
    }
    async fn listener() -> (TcpListener, Options) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let options = Options {
            endpoint: l.local_addr().unwrap().to_string(),
            cwd: "/synthetic/tree".into(),
            env: oracle_env(),
            session_meta: None,
            model: None,
            handshake_timeout: Duration::from_secs(2),
        };
        (l, options)
    }
    fn oracle() -> JValue {
        wtf16::parse(include_str!("../tests/fixtures/acp-node.json")).unwrap()
    }
    fn oracle_env() -> JValue {
        let JValue::Arr(writes) = oracle().get("writes").clone() else {
            panic!()
        };
        writes.first().unwrap().get("env").clone()
    }
    #[tokio::test]
    async fn sandbox_start_handshake_permissions_updates_prompt_and_close() {
        let (l, options) = listener().await;
        let h = handler(false);
        let (_cancel, rx) = watch::channel(false);
        let server = tokio::spawn(async move {
            let mut f = Fake::accept(l).await;
            f.handshake().await;
            let prompt = f.read().await;
            assert_eq!(prompt.get("method").as_str(), Some("session/prompt"));
            assert_eq!(
                prompt.get("params").get("sessionId").as_str(),
                Some("session")
            );
            f.send(obj(vec![
                ("jsonrpc", text("2.0")),
                ("method", text("session/update")),
                (
                    "params",
                    obj(vec![(
                        "update",
                        obj(vec![("sessionUpdate", text("agent_message_chunk"))]),
                    )]),
                ),
            ]))
            .await;
            f.send(obj(vec![
                ("jsonrpc", text("2.0")),
                ("id", JValue::Num(99.0)),
                ("method", text("session/request_permission")),
                ("params", obj(vec![])),
            ]))
            .await;
            let answer = f.read().await;
            assert_eq!(
                answer.get("result").get("outcome").get("outcome").as_str(),
                Some("selected")
            );
            assert_eq!(
                answer.get("result").get("outcome").get("optionId").as_str(),
                Some("allow")
            );
            f.reply(&prompt, obj(vec![("stopReason", text("end_turn"))]))
                .await;
            f.closed().await;
            f.received
        });
        let client = Client::connect(options, h.clone(), rx).await.unwrap();
        assert_eq!(client.agent, oracle().get("agent").clone());
        assert_eq!(
            client
                .prompt("change this")
                .await
                .unwrap()
                .get("stopReason")
                .as_str(),
            Some("end_turn")
        );
        let received = server.await.unwrap();
        assert_eq!(JValue::Arr(received), oracle().get("writes").clone());
        assert_eq!(
            JValue::Arr(h.updates.lock().unwrap().clone()),
            oracle().get("updates").clone()
        );
    }
    #[tokio::test]
    async fn cancellation_during_initialize_closes_supervisor_socket() {
        let (l, options) = listener().await;
        let h = handler(false);
        let (cancel, rx) = watch::channel(false);
        let server = tokio::spawn(async move {
            let mut f = Fake::accept(l).await;
            f.read().await;
            f.read().await;
            cancel.send_replace(true);
            f.closed().await;
        });
        assert!(matches!(
            Client::connect(options, h, rx).await,
            Err(Error::Cancelled)
        ));
        server.await.unwrap();
    }
    #[tokio::test]
    async fn cancel_during_permission_callback_aborts_sidecar() {
        let (l, options) = listener().await;
        let h = handler(true);
        let (cancel, rx) = watch::channel(false);
        let server = tokio::spawn(async move {
            let mut f = Fake::accept(l).await;
            f.handshake().await;
            f.read().await;
            f.send(obj(vec![
                ("id", JValue::Num(99.0)),
                ("method", text("session/request_permission")),
                ("params", obj(vec![])),
            ]))
            .await;
            f.closed().await;
        });
        let client = Client::connect(options, h.clone(), rx).await.unwrap();
        let run = tokio::spawn(async move { client.prompt("change").await });
        h.entered.notified().await;
        cancel.send_replace(true);
        assert!(matches!(run.await.unwrap(), Err(Error::Cancelled)));
        server.await.unwrap();
    }
    #[tokio::test]
    async fn dropping_prompt_future_closes_supervisor_socket() {
        let (l, options) = listener().await;
        let h = handler(false);
        let (_cancel, rx) = watch::channel(false);
        let (seen, waiting) = oneshot_for_test();
        let server = tokio::spawn(async move {
            let mut f = Fake::accept(l).await;
            f.handshake().await;
            f.read().await;
            let _ = seen.send(());
            f.closed().await;
        });
        let client = Client::connect(options, h, rx).await.unwrap();
        let run = tokio::spawn(async move { client.prompt("change").await });
        waiting.await.unwrap();
        run.abort();
        let _ = run.await;
        server.await.unwrap();
    }
    fn oneshot_for_test() -> (
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        tokio::sync::oneshot::channel()
    }
    #[tokio::test]
    async fn a_non_string_method_cannot_masquerade_as_the_pending_reply() {
        let (l, options) = listener().await;
        let h = handler(false);
        let (_cancel, rx) = watch::channel(false);
        let server = tokio::spawn(async move {
            let mut f = Fake::accept(l).await;
            f.handshake().await;
            let prompt = f.read().await;
            f.send(obj(vec![
                ("id", prompt.get("id").clone()),
                ("method", JValue::Null),
                ("result", text("forged")),
            ]))
            .await;
            let refusal = f.read().await;
            assert_eq!(refusal.get("error").get("code").as_num(), Some(-32601.0));
            f.reply(&prompt, obj(vec![("stopReason", text("end_turn"))]))
                .await;
            f.closed().await;
        });
        let client = Client::connect(options, h, rx).await.unwrap();
        assert_eq!(
            client
                .prompt("change")
                .await
                .unwrap()
                .get("stopReason")
                .as_str(),
            Some("end_turn")
        );
        server.await.unwrap();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod concurrency_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;
    #[tokio::test]
    async fn partial_frames_survive_a_completed_callback_interrupting_the_read_wait() {
        let (mut writer, reader) = tokio::io::duplex(1);
        let (ack, written) = tokio::sync::oneshot::channel();
        let (release, proceed) = tokio::sync::oneshot::channel();
        let writing = tokio::spawn(async move {
            writer.write_all(b"{\"jsonrpc\":\"2.0\",").await.unwrap();
            ack.send(()).unwrap();
            proceed.await.unwrap();
            writer.write_all(b"\"result\":true}\n").await.unwrap();
        });
        let mut reader = BufReader::new(reader);
        let mut bytes = Vec::new();
        let mut partial = Box::pin(Client::line(&mut reader, &mut bytes));
        tokio::select! {
            _ = written => {},
            _ = &mut partial => panic!("a partial frame must still be waiting"),
        }
        drop(partial);
        assert!(
            !bytes.is_empty(),
            "prove bytes were consumed before the read future was interrupted"
        );
        release.send(()).unwrap();
        assert_eq!(
            Client::line(&mut reader, &mut bytes).await.unwrap(),
            r#"{"jsonrpc":"2.0","result":true}"#
        );
        writing.await.unwrap();
    }
    struct Concurrent {
        requests: AtomicUsize,
        updates: AtomicUsize,
        changed: tokio::sync::Notify,
    }
    impl Handlers for Concurrent {
        fn request(self: Arc<Self>, _: &str, _: JValue) -> HandlerFuture<'static> {
            Box::pin(async move {
                self.requests.fetch_add(1, Ordering::SeqCst);
                self.changed.notify_one();
                std::future::pending::<Result<JValue, RpcError>>().await
            })
        }
        fn update(&self, _: JValue) {
            self.updates.fetch_add(1, Ordering::SeqCst);
            self.changed.notify_one();
        }
    }
    async fn read(reader: &mut BufReader<OwnedReadHalf>) -> JValue {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).await.unwrap() > 0);
        wtf16::parse(&line).unwrap()
    }
    async fn reply(writer: &mut OwnedWriteHalf, id: JValue, result: JValue) {
        let row = obj(vec![
            ("jsonrpc", text("2.0")),
            ("id", id),
            ("result", result),
        ]);
        writer
            .write_all((wtf16::stringify(&row).unwrap() + "\n").as_bytes())
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn approval_waits_do_not_block_updates_or_other_permission_callbacks() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            read(&mut reader).await;
            let init = read(&mut reader).await;
            reply(
                &mut writer,
                init.get("id").clone(),
                obj(vec![("protocolVersion", JValue::Num(1.0))]),
            )
            .await;
            let new = read(&mut reader).await;
            reply(
                &mut writer,
                new.get("id").clone(),
                obj(vec![("sessionId", text("session"))]),
            )
            .await;
            read(&mut reader).await;
            for row in [
                obj(vec![
                    ("jsonrpc", text("2.0")),
                    ("id", JValue::Num(11.0)),
                    ("method", text("session/request_permission")),
                    ("params", obj(vec![])),
                ]),
                obj(vec![
                    ("jsonrpc", text("2.0")),
                    ("method", text("session/update")),
                    (
                        "params",
                        obj(vec![(
                            "update",
                            obj(vec![("sessionUpdate", text("usage_update"))]),
                        )]),
                    ),
                ]),
                obj(vec![
                    ("jsonrpc", text("2.0")),
                    ("id", JValue::Num(12.0)),
                    ("method", text("session/request_permission")),
                    ("params", obj(vec![])),
                ]),
            ] {
                writer
                    .write_all((wtf16::stringify(&row).unwrap() + "\n").as_bytes())
                    .await
                    .unwrap();
            }
            let cancel = read(&mut reader).await;
            assert_eq!(cancel.get("method").as_str(), Some("session/cancel"));
            let mut line = String::new();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
        });
        let handler = Arc::new(Concurrent {
            requests: AtomicUsize::new(0),
            updates: AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
        });
        let (cancel, receiver) = watch::channel(false);
        let options = Options {
            endpoint,
            cwd: "/synthetic/tree".into(),
            env: obj(vec![]),
            session_meta: None,
            model: None,
            handshake_timeout: Duration::from_secs(5),
        };
        let client = Client::connect(options, handler.clone(), receiver)
            .await
            .unwrap();
        let running = tokio::spawn(async move { client.prompt("synthetic").await });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let notified = handler.changed.notified();
                if handler.requests.load(Ordering::SeqCst) == 2
                    && handler.updates.load(Ordering::SeqCst) == 1
                {
                    break;
                }
                notified.await;
            }
        })
        .await
        .unwrap();
        let expected =
            wtf16::parse(include_str!("../tests/fixtures/acp-concurrent-node.json")).unwrap();
        assert_eq!(
            JValue::Num(handler.requests.load(Ordering::SeqCst) as f64),
            *expected.get("active")
        );
        assert_eq!(
            JValue::Num(handler.updates.load(Ordering::SeqCst) as f64),
            *expected.get("updates")
        );
        cancel.send_replace(true);
        assert!(matches!(running.await.unwrap(), Err(Error::Cancelled)));
        server.await.unwrap();
    }
}
