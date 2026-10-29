//! Candidate execution store. This is a separate, explicitly initialized database;
//! it does not migrate or silently reinterpret the existing project JSON stores.
use crate::{approvals::Binding, Control, Fault, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::Path, sync::Mutex, time::Duration};
const SCHEMA:&str="CREATE TABLE runtime_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE chats(user TEXT NOT NULL,id TEXT NOT NULL,binding TEXT NOT NULL,revision INTEGER NOT NULL DEFAULT 0,messages TEXT NOT NULL DEFAULT '[]',PRIMARY KEY(user,id));
CREATE TABLE turns(user TEXT NOT NULL,id TEXT NOT NULL,chat TEXT NOT NULL,base_revision INTEGER NOT NULL,request TEXT NOT NULL,state TEXT NOT NULL,output TEXT,error TEXT,PRIMARY KEY(user,id));
CREATE TABLE jobs(user TEXT NOT NULL,id TEXT NOT NULL,kind TEXT NOT NULL,payload TEXT NOT NULL,state TEXT NOT NULL,attempt INTEGER NOT NULL DEFAULT 0,lease TEXT,result TEXT,error TEXT,PRIMARY KEY(user,id));";
pub struct StateStore {
    db: Mutex<Connection>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub id: String,
    pub state: String,
    pub output: Option<Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub kind: String,
    pub payload: Value,
    pub attempt: i64,
    pub lease: String,
}
impl StateStore {
    pub fn create(path: &Path) -> Result<Self> {
        let db = crate::database::open(path, crate::database::Open::New)?;
        db.execute_batch(SCHEMA)?;
        db.execute("INSERT INTO runtime_meta VALUES('version','1')", [])?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { db: Mutex::new(db) })
    }
    pub fn open(path: &Path) -> Result<Self> {
        let db = crate::database::open(path, crate::database::Open::Existing)?;
        let version: String = db.query_row(
            "SELECT value FROM runtime_meta WHERE key='version'",
            [],
            |r| r.get(0),
        )?;
        if version != "1" {
            return Err(Fault::Unavailable);
        }
        db.pragma_update(None, "synchronous", "FULL")?;
        db.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { db: Mutex::new(db) })
    }
    pub fn create_chat(&self, binding: &Binding) -> Result<()> {
        binding.validate()?;
        let serialized = serde_json::to_string(binding)?;
        self.db.lock().map_err(|_| Fault::Database)?.execute(
            "INSERT INTO chats(user,id,binding) VALUES(?,?,?)",
            params![binding.user, binding.canonical_chat_id, serialized],
        )?;
        Ok(())
    }
    pub fn replace_context(&self, old: &Binding, new: &Binding) -> Result<()> {
        if old.user != new.user
            || old.canonical_chat_id != new.canonical_chat_id
            || old.start != new.start
            || new.generation.parse::<u64>().ok()
                != old
                    .generation
                    .parse::<u64>()
                    .ok()
                    .and_then(|v| v.checked_add(1))
        {
            return Err(Fault::Invalid);
        }
        let mut db = self.db.lock().map_err(|_| Fault::Database)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute(
            "UPDATE chats SET binding=? WHERE user=? AND id=? AND binding=?",
            params![
                serde_json::to_string(new)?,
                old.user,
                old.canonical_chat_id,
                serde_json::to_string(old)?
            ],
        )? != 1
        {
            return Err(Fault::Conflict);
        }
        tx.execute("UPDATE turns SET state='interrupted',error='chat context changed' WHERE user=? AND chat=? AND state='running'",params![old.user,old.canonical_chat_id])?;
        tx.commit()?;
        Ok(())
    }
    pub fn context(&self, user: &str, id: &str) -> Result<Binding> {
        let db = self.db.lock().map_err(|_| Fault::Database)?;
        let raw: Option<String> = db
            .query_row(
                "SELECT binding FROM chats WHERE user=? AND id=?",
                params![user, id],
                |r| r.get(0),
            )
            .optional()?;
        let binding: Binding = serde_json::from_str(&raw.ok_or(Fault::Missing)?)?;
        binding.validate()?;
        Ok(binding)
    }
    pub fn history(&self, binding: &Binding) -> Result<(i64, Vec<Value>)> {
        let db = self.db.lock().map_err(|_| Fault::Database)?;
        let row: Option<(String, i64, String)> = db
            .query_row(
                "SELECT binding,revision,messages FROM chats WHERE user=? AND id=?",
                params![binding.user, binding.canonical_chat_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (raw, revision, messages) = row.ok_or(Fault::Missing)?;
        if serde_json::from_str::<Binding>(&raw)? != *binding {
            return Err(Fault::Conflict);
        }
        Ok((revision, serde_json::from_str(&messages)?))
    }
    pub fn begin(
        &self,
        binding: &Binding,
        id: &str,
        request: &Value,
        control: &Control,
    ) -> Result<(i64, Vec<Value>)> {
        control.check()?;
        binding.validate()?;
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(Fault::Invalid);
        }
        let raw = serde_json::to_string(request)?;
        if raw.len() > 8 * 1024 * 1024 {
            return Err(Fault::Capacity);
        }
        let mut db = self.db.lock().map_err(|_| Fault::Database)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        control.check()?;
        let (context, revision, messages): (String, i64, String) = tx
            .query_row(
                "SELECT binding,revision,messages FROM chats WHERE user=? AND id=?",
                params![binding.user, binding.canonical_chat_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or(Fault::Missing)?;
        if serde_json::from_str::<Binding>(&context)? != *binding {
            return Err(Fault::Conflict);
        }
        let running: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM turns WHERE user=? AND chat=? AND state='running')",
            params![binding.user, binding.canonical_chat_id],
            |r| r.get(0),
        )?;
        if running {
            return Err(Fault::Conflict);
        }
        tx.execute("INSERT INTO turns(user,id,chat,base_revision,request,state) VALUES(?,?,?,?,?,'running')",params![binding.user,id,binding.canonical_chat_id,revision,raw])?;
        control.check()?;
        tx.commit()?;
        Ok((revision, serde_json::from_str(&messages)?))
    }
    pub fn finish(
        &self,
        binding: &Binding,
        id: &str,
        revision: i64,
        messages: &[Value],
        control: &Control,
    ) -> Result<()> {
        let raw = serde_json::to_string(messages)?;
        if raw.len() > 16 * 1024 * 1024 {
            return Err(Fault::Capacity);
        }
        let mut db = self.db.lock().map_err(|_| Fault::Database)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        control.check()?;
        let changed=tx.execute("UPDATE chats SET messages=?,revision=revision+1 WHERE user=? AND id=? AND revision=? AND binding=?",params![raw,binding.user,binding.canonical_chat_id,revision,serde_json::to_string(binding)?])?;
        if changed != 1 {
            return Err(Fault::Conflict);
        }
        let changed=tx.execute("UPDATE turns SET state='complete',output=? WHERE user=? AND id=? AND chat=? AND state='running' AND base_revision=?",params![raw,binding.user,id,binding.canonical_chat_id,revision])?;
        if changed != 1 {
            return Err(Fault::Conflict);
        }
        control.check()?;
        tx.commit()?;
        Ok(())
    }
    pub fn fail(&self, user: &str, id: &str, fault: Fault) -> Result<()> {
        self.db.lock().map_err(|_| Fault::Database)?.execute(
            "UPDATE turns SET state='failed',error=? WHERE user=? AND id=? AND state='running'",
            params![format!("{fault:?}"), user, id],
        )?;
        Ok(())
    }
    pub fn recover_interrupted(&self) -> Result<usize> {
        let mut db = self.db.lock().map_err(|_| Fault::Database)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let turns=tx.execute("UPDATE turns SET state='interrupted',error='process interrupted' WHERE state='running'",[])?;
        // Never automatically rerun an execution whose remote side effects are uncertain.
        tx.execute("UPDATE jobs SET state='uncertain',lease=NULL,error='process interrupted' WHERE state='running'",[])?;
        tx.commit()?;
        Ok(turns)
    }
    pub fn enqueue_job(&self, user: &str, id: &str, kind: &str, payload: &Value) -> Result<()> {
        if user.is_empty()
            || uuid::Uuid::parse_str(id).is_err()
            || kind.is_empty()
            || kind.len() > 128
        {
            return Err(Fault::Invalid);
        }
        let raw = serde_json::to_string(payload)?;
        if raw.len() > 8 * 1024 * 1024 {
            return Err(Fault::Capacity);
        }
        let mut db = self.db.lock().map_err(|_| Fault::Database)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT kind,payload FROM jobs WHERE user=? AND id=?",
                params![user, id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((old_kind, old)) = existing {
            if old_kind != kind || serde_json::from_str::<Value>(&old)? != *payload {
                return Err(Fault::Conflict);
            }
        } else {
            tx.execute(
                "INSERT INTO jobs(user,id,kind,payload,state) VALUES(?,?,?,?,'queued')",
                params![user, id, kind, raw],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn claim_job(&self, user: &str, id: &str, control: &Control) -> Result<Job> {
        let mut db = self.db.lock().map_err(|_| Fault::Database)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        control.check()?;
        let row: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT kind,payload,attempt FROM jobs WHERE user=? AND id=? AND state='queued'",
                params![user, id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (kind, payload, attempt) = row.ok_or(Fault::Missing)?;
        let lease = uuid::Uuid::new_v4().to_string();
        let attempt = attempt.checked_add(1).ok_or(Fault::Capacity)?;
        tx.execute("UPDATE jobs SET state='running',lease=?,attempt=? WHERE user=? AND id=? AND state='queued'",params![lease,attempt,user,id])?;
        control.check()?;
        tx.commit()?;
        Ok(Job {
            id: id.to_owned(),
            kind,
            payload: serde_json::from_str(&payload)?,
            attempt,
            lease,
        })
    }
    pub fn complete_job(
        &self,
        user: &str,
        job: &Job,
        result: &Value,
        control: &Control,
    ) -> Result<()> {
        let raw = serde_json::to_string(result)?;
        if raw.len() > 8 * 1024 * 1024 {
            return Err(Fault::Capacity);
        }
        let mut db = self.db.lock().map_err(|_| Fault::Database)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        control.check()?;
        if tx.execute("UPDATE jobs SET state='complete',result=?,lease=NULL WHERE user=? AND id=? AND state='running' AND lease=? AND attempt=?",params![raw,user,job.id,job.lease,job.attempt])?!=1{return Err(Fault::Conflict);}
        control.check()?;
        tx.commit()?;
        Ok(())
    }
    pub fn cancel_job(&self, user: &str, id: &str) -> Result<bool> {
        Ok(self.db.lock().map_err(|_|Fault::Database)?.execute("UPDATE jobs SET state=CASE WHEN state='running' THEN 'uncertain' ELSE 'cancelled' END,lease=NULL WHERE user=? AND id=? AND state IN ('running','queued')",params![user,id])?==1)
    }
}

/// Execution persistence contract. Production adapters must fence the actual holder and
/// generation together with the transcript write, not just compare before an await.
pub trait Conversations: Send + Sync {
    fn partial(&self, _binding: &Binding, _id: &str, _entry: Option<&Value>) -> Result<()> {
        Ok(())
    }

    fn tool_mode(
        &self,
        _binding: &Binding,
        _name: &str,
        write: bool,
    ) -> Result<policy_leaves::Mode> {
        Ok(policy_leaves::mode(None, write))
    }
    fn sampling_enabled(&self, _binding: &Binding) -> Result<bool> {
        Ok(true)
    }
    fn supports_project_files(&self) -> bool {
        false
    }
    fn sources(&self, _binding: &Binding, _control: &Control) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
    fn read_project_file(
        &self,
        _binding: &Binding,
        _args: &Value,
        _control: &Control,
    ) -> Result<Value> {
        Err(Fault::Unavailable)
    }

    fn project(&self, _binding: &Binding) -> Result<Option<Value>> {
        Ok(None)
    }
    fn system(&self, _binding: &Binding) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
    fn context(&self, user: &str, chat: &str) -> Result<Binding>;
    fn begin(
        &self,
        binding: &Binding,
        id: &str,
        request: &Value,
        control: &Control,
    ) -> Result<(String, Vec<Value>)>;
    /// Browser history is an admission assertion, never permission to replace server state.
    fn begin_browser(
        &self,
        _binding: &Binding,
        _id: &str,
        _request: &Value,
        _history: &[Value],
        _control: &Control,
    ) -> Result<(String, Vec<Value>)> {
        Err(Fault::Unavailable)
    }
    fn finish(
        &self,
        binding: &Binding,
        id: &str,
        revision: &str,
        messages: &[Value],
        control: &Control,
    ) -> Result<()>;
    fn finish_tools(
        &self,
        binding: &Binding,
        id: &str,
        revision: &str,
        messages: &[Value],
        _tools: &[Value],
        control: &Control,
    ) -> Result<()> {
        self.finish(binding, id, revision, messages, control)
    }
    fn fail(&self, user: &str, id: &str, fault: Fault) -> Result<()>;
}
impl Conversations for StateStore {
    fn context(&self, user: &str, chat: &str) -> Result<Binding> {
        StateStore::context(self, user, chat)
    }
    fn begin(
        &self,
        binding: &Binding,
        id: &str,
        request: &Value,
        control: &Control,
    ) -> Result<(String, Vec<Value>)> {
        let (revision, history) = StateStore::begin(self, binding, id, request, control)?;
        Ok((revision.to_string(), history))
    }
    fn finish(
        &self,
        binding: &Binding,
        id: &str,
        revision: &str,
        messages: &[Value],
        control: &Control,
    ) -> Result<()> {
        StateStore::finish(
            self,
            binding,
            id,
            revision.parse().map_err(|_| Fault::Invalid)?,
            messages,
            control,
        )
    }
    fn fail(&self, user: &str, id: &str, fault: Fault) -> Result<()> {
        StateStore::fail(self, user, id, fault)
    }
}
