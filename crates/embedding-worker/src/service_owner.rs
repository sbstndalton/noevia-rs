//! One asynchronous owner for refresh, service changes and their restore paths.
//! Accepted jobs settle even if their caller disconnects. Dropping all clients
//! drains the bounded queue; shutdown must join the returned task before the
//! process releases its embedding lifetime ownership lock.
//!
//! This queue is not a remote effect-settlement authority. A transport timeout
//! must not be treated as proof that a loader mutation finished. An integrating
//! operation must retain its turn through an authority-confirmed settle/restore
//! (or use the shared authority fence) before returning from its callback.
use crate::{
    service::{Loader, Service},
    Task,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

type Operation<L> = Box<dyn for<'a> FnOnce(&'a mut Service, &'a L) -> Task<'a, Value> + Send>;
struct Job<L> {
    operation: Operation<L>,
    reply: oneshot::Sender<Value>,
}
pub struct Client<L> {
    sender: mpsc::Sender<Job<L>>,
}
impl<L> Clone for Client<L> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}
impl<L: Loader + Send + 'static> Client<L> {
    /// Admission is cancellation safe: cancellation before send reserves nothing;
    /// after send the actor owns all effects and the entire restoration sequence.
    pub async fn execute<F>(&self, operation: F) -> Result<Value, &'static str>
    where
        F: for<'a> FnOnce(&'a mut Service, &'a L) -> Task<'a, Value> + Send + 'static,
    {
        let (reply, result) = oneshot::channel();
        self.sender
            .send(Job {
                operation: Box::new(operation),
                reply,
            })
            .await
            .map_err(|_| "embedding service unavailable")?;
        result.await.map_err(|_| "embedding service unavailable")
    }
}
pub fn start<L: Loader + Send + 'static>(loader: L) -> (Client<L>, tokio::task::JoinHandle<()>) {
    let (sender, mut receiver) = mpsc::channel::<Job<L>>(32);
    let task = tokio::spawn(async move {
        let mut service = Service::default();
        while let Some(job) = receiver.recv().await {
            let result = (job.operation)(&mut service, &loader).await;
            let _ = job.reply.send(result);
        }
    });
    (Client { sender }, task)
}
