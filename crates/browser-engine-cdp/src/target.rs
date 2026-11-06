//! A scoped CDP connection for a renderer target that the high-level library does not own.
use super::*;
use chromiumoxide::Connection;
use chromiumoxide_types::{CallId, CdpJsonEventMessage, Message};
use tokio::sync::{mpsc, oneshot};
#[async_trait]
pub(super) trait Control: Send + Sync {
    async fn raw_execute(&self, method: &'static str, params: Value) -> Result<Value>;
}
#[async_trait]
impl Control for Page {
    async fn raw_execute(&self, method: &'static str, params: Value) -> Result<Value> {
        Ok(self.execute(raw(method, params)).await?.result)
    }
}
struct Request {
    method: &'static str,
    params: Value,
    reply: oneshot::Sender<Result<Value>>,
}
pub(super) struct Renderer {
    tx: mpsc::UnboundedSender<Request>,
    task: JoinHandle<()>,
}
impl Drop for Renderer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Renderer {
    pub async fn connect(endpoint: &str, id: &str) -> Result<Self> {
        let mut endpoint = url::Url::parse(endpoint)?;
        endpoint.set_path(&format!("/devtools/page/{id}"));
        let mut connection = Connection::<CdpJsonEventMessage>::connect(endpoint.as_str()).await?;
        let (tx, mut rx) = mpsc::unbounded_channel::<Request>();
        let task = tokio::spawn(async move {
            let mut pending =
                std::collections::HashMap::<CallId, oneshot::Sender<Result<Value>>>::new();
            loop {
                tokio::select! {
                    request=rx.recv()=>{
                        let Some(request)=request else{break};
                        match connection.submit_command(request.method.into(),None,request.params) {
                            Ok(id)=>{pending.insert(id,request.reply);},
                            Err(_)=>{let _=request.reply.send(Err(anyhow!("Renderer connection unavailable")));break;}
                        }
                    },
                    message=connection.next()=>{
                        let Some(Ok(message))=message else{break};
                        if let Message::Response(response)=message {
                            if let Some(reply)=pending.remove(&response.id) {
                                let _=reply.send(if response.error.is_some(){Err(anyhow!("Renderer operation rejected"))}else{Ok(response.result.unwrap_or(Value::Null))});
                            }
                        }
                    }
                }
            }
        });
        Ok(Self { tx, task })
    }
}
#[async_trait]
impl Control for Renderer {
    async fn raw_execute(&self, method: &'static str, params: Value) -> Result<Value> {
        let (reply, result) = oneshot::channel();
        self.tx
            .send(Request {
                method,
                params,
                reply,
            })
            .map_err(|_| anyhow!("Renderer connection unavailable"))?;
        result
            .await
            .map_err(|_| anyhow!("Renderer connection closed"))?
    }
}
#[derive(Clone)]
pub(super) enum FrameTarget {
    Main(Page),
    Child(Arc<Renderer>),
}
#[async_trait]
impl Control for FrameTarget {
    async fn raw_execute(&self, method: &'static str, params: Value) -> Result<Value> {
        match self {
            Self::Main(page) => page.raw_execute(method, params).await,
            Self::Child(renderer) => renderer.raw_execute(method, params).await,
        }
    }
}
