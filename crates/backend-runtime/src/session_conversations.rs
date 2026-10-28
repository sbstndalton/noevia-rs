//! Live HTTP-session fence around history disclosure and final transcript authority.
//! Lock order is account Writer -> original project/transcript locks. Revocation cannot
//! commit between the last session check and the native transcript commit.
use crate::{approvals::Binding, state::Conversations, Control, Fault, Result};
use serde_json::Value;
use server_auth::{Authenticator, Creds, Refusal};
use server_store::Writer;
use std::sync::Arc;
pub struct SessionConversations {
    inner: Arc<dyn Conversations>,
    writer: Arc<Writer>,
    auth: Arc<Authenticator>,
    creds: Creds,
    user: String,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}
impl SessionConversations {
    pub fn new(
        inner: Arc<dyn Conversations>,
        writer: Arc<Writer>,
        auth: Arc<Authenticator>,
        creds: Creds,
        user: String,
        clock: impl Fn() -> i64 + Send + Sync + 'static,
    ) -> Self {
        Self {
            inner,
            writer,
            auth,
            creds,
            user,
            clock: Arc::new(clock),
        }
    }
    fn fenced<T>(&self, user: &str, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        if user != self.user {
            return Err(Fault::Forbidden);
        }
        let mut outcome = None;
        self.writer
            .write(|tx| {
                let gate =
                    self.auth
                        .gate_in(&tx.reader(), &self.creds, "/api/chat", (self.clock)())?;
                outcome = Some(match gate {
                    Ok(identity) if identity.user.id == self.user => operation(),
                    Err(Refusal::Unauthorized) => Err(Fault::Unauthorized),
                    _ => Err(Fault::Forbidden),
                });
                Ok(())
            })
            .map_err(|_| Fault::Unavailable)?;
        outcome.ok_or(Fault::Unavailable)?
    }
    pub fn check(&self) -> Result<()> {
        self.fenced(&self.user, || Ok(()))
    }
}
impl Conversations for SessionConversations {
    fn context(&self, user: &str, chat: &str) -> Result<Binding> {
        self.fenced(user, || self.inner.context(user, chat))
    }
    fn begin(
        &self,
        binding: &Binding,
        id: &str,
        request: &Value,
        control: &Control,
    ) -> Result<(String, Vec<Value>)> {
        self.fenced(&binding.user, || {
            self.inner.begin(binding, id, request, control)
        })
    }
    fn finish(
        &self,
        binding: &Binding,
        id: &str,
        revision: &str,
        messages: &[Value],
        control: &Control,
    ) -> Result<()> {
        self.fenced(&binding.user, || {
            self.inner.finish(binding, id, revision, messages, control)
        })
    }
    fn fail(&self, user: &str, id: &str, fault: Fault) -> Result<()> {
        // Cleanup of this owner's admitted intent must remain possible after revocation.
        if user != self.user {
            return Err(Fault::Forbidden);
        }
        self.inner.fail(user, id, fault)
    }
}
