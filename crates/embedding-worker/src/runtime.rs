//! Native lifecycle owner, driven only after explicit Core pairing proof.
use crate::{
    fill::Filler, http::Http, sqlite::SqlStore, Config, Control, Host, Index, Options, Target,
    Task, Worker,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
fn parse_target(value: &Value) -> Result<Target, String> {
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| {
            !s.is_empty()
                && s.chars().count() <= 200
                && !s.chars().any(|c| c.is_whitespace() || c.is_control())
        })
        .ok_or("invalid embedding target")?;
    let profile = value
        .get("profile")
        .and_then(Value::as_str)
        .unwrap_or("nomic-v1")
        .trim();
    let profile = if profile.is_empty() {
        "nomic-v1"
    } else {
        profile
    };
    prompt_framing::embedding_input::Profile::try_from(profile)
        .map_err(|_| "invalid embedding profile")?;
    let base = value
        .get("baseUrl")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(base) = base {
        let url = reqwest::Url::parse(base).map_err(|_| "invalid embedding address")?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("invalid embedding address".into());
        }
    }
    Ok(Target {
        model: model.into(),
        profile: profile.into(),
        base_url: base.map(|b| b.trim_end_matches('/').to_owned()),
    })
}
pub fn load_config(root: &Path, seed: &Config) -> Result<Config, String> {
    match std::fs::read(root.join("embedding-config.json")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            for target in std::iter::once(&seed.active)
                .chain(seed.next.iter())
                .chain(seed.previous.iter())
            {
                parse_target(
                    &serde_json::json!({"model":target.model,"profile":target.profile,"baseUrl":target.base_url}),
                )?;
            }
            Ok(seed.clone())
        }
        Err(_) => Err("embedding config unavailable".into()),
        Ok(bytes) => {
            if bytes.len() > 1024 * 1024 {
                return Err("embedding config too large".into());
            }
            let value: Value =
                serde_json::from_slice(&bytes).map_err(|_| "damaged embedding config")?;
            if value.get("version").and_then(Value::as_u64) != Some(1) {
                return Err("damaged embedding config".into());
            }
            let hash = Sha256::digest(&bytes);
            let mut version = [0; 8];
            for (to, from) in version.iter_mut().zip(hash.iter()) {
                *to = *from;
            }
            let optional = |key: &str| {
                value
                    .get(key)
                    .filter(|v| !v.is_null())
                    .map(parse_target)
                    .transpose()
            };
            Ok(Config {
                version: u64::from_le_bytes(version),
                active: parse_target(value.get("active").ok_or("damaged embedding config")?)?,
                next: optional("next")?,
                previous: optional("previous")?,
            })
        }
    }
}
pub fn invalid_seed() -> Config {
    Config {
        version: 0,
        active: Target {
            model: "default".into(),
            profile: "invalid-environment".into(),
            base_url: None,
        },
        next: None,
        previous: None,
    }
}
pub fn seed(get: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
    let pick = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            get(key)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        })
    };
    let active = parse_target(
        &serde_json::json!({"model":pick(&["EMBEDDING_MODEL","EMBED_MODEL"]).unwrap_or("default".into()),"profile":pick(&["EMBEDDING_PROFILE"]).unwrap_or("nomic-v1".into()),"baseUrl":pick(&["EMBEDDING_BASE_URL"])}),
    )?;
    let next=pick(&["EMBEDDING_NEXT_MODEL"]).map(|model|parse_target(&serde_json::json!({"model":model,"profile":pick(&["EMBEDDING_NEXT_PROFILE"]).unwrap_or("nomic-v1".into()),"baseUrl":pick(&["EMBEDDING_NEXT_BASE_URL"])}))).transpose()?.filter(|next|next!=&active);
    Ok(Config {
        version: 0,
        active,
        next,
        previous: None,
    })
}
pub(crate) struct NativeHost {
    pub(crate) store: SqlStore,
    pub(crate) http: Http,
    valid: Arc<AtomicBool>,
    off: Arc<AtomicBool>,
    pub(crate) service_base: Option<String>,
    pub(crate) core: String,
    pub(crate) token: zeroize::Zeroizing<String>,
    pub(crate) start: String,
}
#[cfg(test)]
pub(crate) fn media_test_host(store: SqlStore, http: Http, core: String) -> NativeHost {
    NativeHost {
        store,
        http,
        core,
        valid: Arc::new(AtomicBool::new(true)),
        off: Arc::new(AtomicBool::new(false)),
        service_base: None,
        token: zeroize::Zeroizing::new("synthetic-token".into()),
        start: "11111111111111111111111111111111".into(),
    }
}
impl NativeHost {
    fn off(&self, target: &Target) -> bool {
        self.off.load(Ordering::Acquire)
            && (target.base_url.is_none()
                || self.service_base.as_ref().is_some_and(|base| {
                    crate::service::same_address(Some(base), target.base_url.as_deref())
                }))
    }
}
impl Host for NativeHost {
    fn config(&self) -> Config {
        match self.store.config() {
            Ok(c) => c,
            Err(_) => {
                self.valid.store(false, Ordering::Release);
                self.store.seed.clone()
            }
        }
    }
    fn now_ms(&self) -> u64 {
        now()
    }
    fn indexes(&self) -> Vec<Index> {
        if self.valid.load(Ordering::Acquire) {
            self.store.indexes()
        } else {
            vec![]
        }
    }
    fn served<'a>(&'a self, target: &'a Target) -> Task<'a, Result<Vec<String>, String>> {
        Box::pin(async move {
            if !self.valid.load(Ordering::Acquire) || self.off(target) {
                return Err("embedding off".into());
            }
            self.http.served(target).await
        })
    }
    fn fill<'a>(
        &'a self,
        index: &'a Index,
        target: &'a Target,
        limit: usize,
        exclude: &'a BTreeSet<i64>,
        version: u64,
        control: &'a Control,
    ) -> Task<'a, Result<Option<crate::Batch>, String>> {
        Box::pin(async move {
            if !self.valid.load(Ordering::Acquire) || self.off(target) {
                return Err("embedding unavailable".into());
            }
            if self
                .http
                .ownership(&self.core, &self.token, &self.start)
                .await
                .is_err()
            {
                control.suspend();
                return Err("embedding ownership lost".into());
            }
            // This host runs on a blocking worker thread; no SQLite lock is held
            // across the await inside Filler.
            Filler {
                store: self.store.clone(),
                embedder: self.http.clone(),
            }
            .fill(index, target, limit, exclude, version, control)
            .await
        })
    }
    fn drop_slot(&self, index: &Index, target: &Target) -> Result<(), String> {
        if !self.valid.load(Ordering::Acquire) {
            return Err("embedding unavailable".into());
        }
        self.store.drop_slot(index, target)
    }
    fn sleep<'a>(&'a self, ms: u64, control: &'a Control) -> Task<'a, ()> {
        Box::pin(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
            while !control.stopped() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep_until(
                    deadline.min(tokio::time::Instant::now() + Duration::from_millis(25)),
                )
                .await;
            }
        })
    }
}
pub struct Background {
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
    control: Control,
}
impl Background {
    /// The worker's control: a kick asks for a pass (native index writes kick it after an index run).
    pub fn control(&self) -> Control {
        self.control.clone()
    }
    pub async fn shutdown(mut self) {
        self.control.stop();
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for Background {
    fn drop(&mut self) {
        self.control.stop();
        let _ = self.stop.send(true);
    }
}
/// The process owns a nonblocking flock for its whole lifetime: another native
/// front cannot run workers against this data directory at the same time.
pub fn start(
    store: SqlStore,
    http: Http,
    core: String,
    token: String,
    start: String,
    options: Options,
    service_base: Option<String>,
) -> Result<Background, String> {
    start_with_documents(store, http, core, token, start, options, service_base, None)
}
#[allow(clippy::too_many_arguments)]
pub fn start_with_documents(
    store: SqlStore,
    http: Http,
    core: String,
    token: String,
    start: String,
    options: Options,
    service_base: Option<String>,
    documents: Option<crate::media_runtime::EngineFactory>,
) -> Result<Background, String> {
    let http = http.with_ownership(&core, &token, &start);
    let token = zeroize::Zeroizing::new(token);
    let (stop, mut rx) = watch::channel(false);
    let handle = tokio::runtime::Handle::current();
    let mut worker = Worker::new(options);
    let control = worker.control();
    let background_control = control.clone();
    let task = tokio::task::spawn_blocking(move || {
        handle.block_on(async move {
            // Another draining front may still own the lock. Stay alive and
            // retry only this optional subsystem; never write its snapshot until
            // the lifetime owner lock has actually been acquired.
            let mut lock_reported=false;
            let _owner=loop {
                if *rx.borrow() {return;}
                if let Ok(file)=std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(store.root.join("embedding-worker.lock")) {
                    if file.try_lock().is_ok() {break file;}
                }
                if !lock_reported {eprintln!("noevia-server: optional embedding owner idle while lifetime lock is unavailable");lock_reported=true;}
                tokio::select! {_=tokio::time::sleep(Duration::from_secs(2))=>{},_=rx.changed()=>{return;}}
            };
            let host=NativeHost {store,http,valid:Arc::new(AtomicBool::new(true)),off:Arc::new(AtomicBool::new(false)),service_base,core:core.clone(),token:token.clone(),start:start.clone()};
            let state=Arc::new(std::sync::Mutex::new(serde_json::json!({"protocol":"rust-embedding-worker-v1","start":start,"service":null,"serviceAt":null})));
            let heartbeat_state=state.clone();
            let heartbeat_root=host.store.root.clone();
            let mut heartbeat_stop=rx.clone();
            // Separate runtime task: synchronous SQLite fence waits and long
            // passes on this blocking host must not delay liveness snapshots.
            let heartbeat=tokio::spawn(async move {
                let mut ticks=tokio::time::interval(Duration::from_secs(2));
                ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {_=ticks.tick()=>{},_=heartbeat_stop.changed()=>break}
                    if *heartbeat_stop.borrow() {break;}
                    update_snapshot(&heartbeat_root,&heartbeat_state,serde_json::json!({}));
                }
            });
            let mut service_rx=rx.clone();
            // Refresh runs alongside the migration await, so a slow embeddings
            // batch cannot starve the ten-second model-loader refresh.
            let refresh=async {
                if !host.http.loader_configured() {let _=service_rx.wait_for(|stop|*stop).await;return;}
                let mut service=crate::service::Service::default();
                let mut ticks=tokio::time::interval(Duration::from_secs(10));
                ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {_=ticks.tick()=>{},_=service_rx.changed()=>{break;}}
                    if *service_rx.borrow() {break;}
                    let paired=tokio::select! {r=host.http.ownership(&core,&token,&start)=>r.is_ok(),_=service_rx.changed()=>false};
                    if *service_rx.borrow() {break;}
                    if !paired {continue;}
                    tokio::select! {_=service.refresh(&host.http,now)=>{},_=service_rx.changed()=>{break;}}
                    host.off.store(service.managed()&&!service.enabled(),Ordering::Release);
                    update_snapshot(&host.store.root,&state,serde_json::json!({"service":service.status(now()),"serviceAt":service.observed_at_ms()}));
                }
            };
            let mut outbox_rx=rx.clone();
            let outbox=async {
                use server_projects::outbox::{self,JobDiagnostic,JobOperation};
                let mut recovered=BTreeSet::new();
                let mut pending=std::collections::BTreeMap::<String,RagCompletion>::new();
                loop {
                    if *outbox_rx.borrow() {break;}
                    pending.retain(|_,completion|completion.apply().is_err());
                    if host.http.outbox_ready(&core,&token,&start).await.unwrap_or(false) {
                        let users=match host.store.users() {Ok(users)=>users,Err(_)=>{eprintln!("[index-outbox] operation=users reason=unavailable");Vec::new()}};
                        for user in users {
                            if *outbox_rx.borrow() {break;}
                            if pending.contains_key(&user) {continue;}
                            let Ok(dir)=server_projects::store::user_dir(&host.store.root,&user) else {continue;};
                            let rows=match outbox::pending_rag(&dir,!recovered.contains(&user)) {
                                Ok(rows)=>rows,
                                Err(_)=>{eprintln!("[index-outbox] operation=scan reason=unavailable");continue;}
                            };
                            if outbox::path(&dir).exists() {recovered.insert(user.clone());}
                            for row in rows {
                                if *outbox_rx.borrow() || !host.http.outbox_ready(&core,&token,&start).await.unwrap_or(false) {break;}
                                match outbox::claim_rag(&dir,&row,now() as i64) {
                                    Ok(true)=>{},Ok(false)=>continue,
                                    Err(error)=>{
                                        let error=JobDiagnostic::new(&row,JobOperation::Claim,&format!("{error:?}"));
                                        eprintln!("{}",error.warning());
                                        if outbox::record_error(&dir,&row,&error).is_err() {eprintln!("[index-outbox] operation=record-error reason=unavailable");}
                                        break;
                                    }
                                }
                                let attempt=host.run_rag(&user,&row,&mut outbox_rx).await;
                                let status=if *outbox_rx.borrow() {Ok(())} else {host.store.mark_index_source(&user,&row,attempt.result.is_ok() && !attempt.deferred,attempt.prepared)};
                                let completion=RagCompletion::from_attempt(dir.clone(),row,attempt,status);
                                if completion.apply().is_err() {pending.insert(user.clone(),completion);break;}
                                tokio::task::yield_now().await;
                            }
                        }
                    }
                    tokio::select! {_=tokio::time::sleep(Duration::from_secs(2))=>{},_=outbox_rx.changed()=>break}
                }
            };
            let media_rx = rx.clone();
            let migrate=async {
                let begun=tokio::time::Instant::now();let mut initial=false;
                let mut seen=None;let mut config_version=None;let mut request_stamp=0;
                loop {
                    if *rx.borrow() {break;}
                    let paired=tokio::select! {r=host.http.ownership(&core,&token,&start)=>r.is_ok(),_=rx.changed()=>false};
                    if *rx.borrow() {break;}
                    if paired {
                        control.resume();
                        let valid=host.store.config().is_ok(); let was_valid=host.valid.swap(valid,Ordering::AcqRel); let recovered=valid && !was_valid; if recovered {control.kick();}
                        let stamp=index_stamp(&host.store);let config=host.store.config();
                        if begun.elapsed()>=Duration::from_secs(30) && !initial {initial=true;if config.as_ref().is_ok_and(|c|c.next.is_some()) {control.kick();}}
                        if seen.as_ref().is_some_and(|s|s!=&stamp)&&config.as_ref().is_ok_and(|c|c.next.is_some()) {control.kick();}
                        seen=Some(stamp);
                        if let Ok(c)=&config {if config_version.is_some_and(|v|v!=c.version)&&c.next.is_some() {control.kick();}config_version=Some(c.version);}
                        let request=std::fs::read(host.store.root.join("embedding-worker-request.json")).ok().filter(|b|b.len()<8192).and_then(|b|serde_json::from_slice::<Value>(&b).ok());
                        if let Some(r)=request.filter(|r|r.get("start").and_then(Value::as_str)==Some(&start)) {let sequence=r.get("sequence").and_then(Value::as_u64).unwrap_or(0);if sequence!=request_stamp {request_stamp=sequence;control.kick();}}
                        tokio::select! {_=worker.tick(&host)=>{},_=rx.changed()=>{control.stop();}}
                        let held=worker.held.as_ref().map(|(target,reason)|serde_json::json!({"model":target.model,"profile":target.profile,"baseUrl":target.base_url,"code":match reason {crate::Held::EndpointUnverified=>"endpointUnverified",crate::Held::ModelNotServed=>"modelNotServed"}}));
                        let next_served=host.store.config().ok().and_then(|c|c.next).and_then(|t|if worker.verified(&t) {Some(true)} else if worker.held.as_ref().is_some_and(|(held,_)|held==&t) {Some(false)} else {None});
                        let pass=worker.last_pass.as_ref().map(|p|serde_json::json!({"at":p.at_ms,"embedded":p.embedded,"failed":p.failed,"refused":p.refused,"error":p.error,"aborted":p.aborted}));
                        update_snapshot(&host.store.root,&state,serde_json::json!({"lastPass":pass,"nextHeld":held,"nextServed":next_served,"configValid":host.valid.load(Ordering::Acquire),"configInvalid":!host.valid.load(Ordering::Acquire),"ownershipUnconfirmedSince":control.unconfirmed_since(),"ownershipStatus":control.unconfirmed_since().map(|at|format!("ownership unconfirmed since {at}")).unwrap_or_else(||"confirmed".into())}));
                    } else {
                        control.suspend();
                        update_snapshot(&host.store.root,&state,serde_json::json!({"ownershipUnconfirmedSince":control.unconfirmed_since(),"ownershipStatus":format!("ownership unconfirmed since {}",control.unconfirmed_since().unwrap_or(0))}));
                    }
                    tokio::select! {_=tokio::time::sleep(Duration::from_secs(2))=>{},_=rx.changed()=>{}}
                }
                control.stop();
            };
            let media = crate::media_runtime::run(&host, control.clone(), &state, documents, media_rx);
            tokio::join!(refresh,migrate,outbox,media);
            let _=heartbeat.await;
        });
    });
    Ok(Background {
        stop,
        task: Some(task),
        control: background_control,
    })
}
/// A file/WAL stamp notices Node's 2s outbox index completion without claiming,
/// recovering or acknowledging any Node job. It also catches native reindex writes;
/// these coalesce into a harmless subsequent coverage-only pass.
fn index_stamp(store: &SqlStore) -> Vec<(Index, u64, u64)> {
    store
        .indexes()
        .into_iter()
        .map(|index| {
            let path = store.index_path(&index).unwrap_or_default();
            let stamp = |p: &Path| {
                std::fs::metadata(p)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0)
            };
            (index, stamp(&path), stamp(&path.with_extension("db-wal")))
        })
        .collect()
}

fn publish_snapshot(root: &Path, value: &Value) -> Result<(), String> {
    use std::io::Write;
    let temp = root.join("embedding-worker-status.json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .map_err(|_| "embedding snapshot unavailable")?;
    file.write_all(value.to_string().as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|_| "embedding snapshot unavailable")?;
    std::fs::rename(temp, root.join("embedding-worker-status.json"))
        .map_err(|_| "embedding snapshot unavailable".into())
}

fn update_snapshot(root: &Path, state: &std::sync::Mutex<Value>, fields: Value) {
    if let Ok(mut state) = state.lock() {
        if let (Some(object), Some(fields)) = (state.as_object_mut(), fields.as_object()) {
            for (key, value) in fields {
                object.insert(key.clone(), value.clone());
            }
            object.insert("at".into(), Value::from(now()));
        }
        let _ = publish_snapshot(root, &state);
    }
}

struct RagAttempt {
    result: Result<(), server_projects::outbox::JobDiagnostic>,
    prepared: bool,
    deferred: bool,
}
impl NativeHost {
    async fn run_rag(
        &self,
        user: &str,
        row: &server_projects::outbox::IndexClaim,
        stop: &mut watch::Receiver<bool>,
    ) -> RagAttempt {
        use server_projects::outbox::{JobDiagnostic as Error, JobOperation as Op};
        let mut attempt = RagAttempt {
            result: Ok(()),
            prepared: false,
            deferred: false,
        };
        let result:Result<(),Error>=async {
            // Match Node: require an active workspace and a current tenant-local
            // source, then notify before fallible text eligibility/preparation.
            // Scheduling errors stay outside text retries and diagnostics.
            if row.kind=="index" && self.store.index_source_member(user,row).map_err(|e|Error::new(row,Op::Prepare,&e))? {
                if let Err(error)=self.http.source_changed(&self.core,&self.token,&self.start,user,&row.project,row.file.as_deref().unwrap_or_default()).await {
                    eprintln!("{}",Error::new(row,Op::Notify,&error).warning());
                }
            }
            let ctl=Control::default();
            crate::fill::Embed::authorize(&self.http,&ctl).await.map_err(|e|Error::new(row,Op::Prepare,&e))?;
            let preparation=self.store.index_source(user,row,&ctl);
            let source=preparation.map_err(|e|Error::new(row,Op::Prepare,&e))?;
            attempt.prepared=true;
            let Some(source)=source else {return Ok(())};
            let config=self.store.config().map_err(|e|Error::new(row,Op::Prepare,&e))?;
            if self.off(&config.active) {return Ok(());}
            let filler=Filler{store:crate::sqlite::SourceStore{store:&self.store,row,version:Some(&source.version)},embedder:self.http.clone()};
            loop {
                if *stop.borrow() || !self.http.outbox_ready(&self.core,&self.token,&self.start).await.unwrap_or(false) {attempt.deferred=true;return Ok(());}
                let excluded=BTreeSet::new();
                let batch=tokio::select! {
                    r=filler.fill(&source.index,&config.active,4,&excluded,config.version,&ctl)=>r.map_err(|e|Error::new(row,Op::Embed,&e))?,
                    _=stop.changed()=>{ctl.stop();attempt.deferred=true;return Ok(());}
                };
                match batch {
                    Some(batch) if batch.remaining>0 && batch.embedded>0=>{},
                    Some(batch) if batch.remaining>0=>return Err(Error::new(row,Op::Embed,"incomplete vectors")),
                    _=>return Ok(())
                }
                tokio::select! {_=tokio::time::sleep(Duration::from_millis(50))=>{},_=stop.changed()=>{ctl.stop();attempt.deferred=true;return Ok(());}}
            }
        }.await;
        attempt.result = result;
        attempt
    }
}

enum RagCompletionAction {
    Defer,
    Finish(Option<server_projects::outbox::JobDiagnostic>),
}
struct RagCompletion {
    dir: std::path::PathBuf,
    row: server_projects::outbox::IndexClaim,
    action: RagCompletionAction,
}
impl RagCompletion {
    fn from_attempt(
        dir: std::path::PathBuf,
        row: server_projects::outbox::IndexClaim,
        mut attempt: RagAttempt,
        status: Result<(), String>,
    ) -> Self {
        use server_projects::outbox::{JobDiagnostic as Error, JobOperation as Op};
        if let Err(error) = &attempt.result {
            eprintln!("{}", error.warning());
        }
        if let Err(error) = status {
            let error = Error::new(&row, Op::Status, &error);
            eprintln!("{}", error.warning());
            attempt.result = Err(error);
            attempt.deferred = false;
        }
        let action = if attempt.deferred {
            RagCompletionAction::Defer
        } else {
            RagCompletionAction::Finish(attempt.result.err())
        };
        Self { dir, row, action }
    }
    /// A failed status/ack never disappears into `let _`: keep the completion in
    /// memory for the next tick, with the durable running row as restart recovery.
    fn apply(&self) -> Result<(), server_projects::outbox::JobDiagnostic> {
        use server_projects::outbox::{self, JobDiagnostic as Error, JobOperation as Op};
        let (operation, result) = match &self.action {
            RagCompletionAction::Defer => (Op::Defer, outbox::defer_index(&self.dir, &self.row)),
            RagCompletionAction::Finish(error) => (
                Op::Ack,
                outbox::finish_index_with_error(&self.dir, &self.row, error.as_ref()),
            ),
        };
        if let Err(error) = result {
            let error = Error::new(&self.row, operation, &format!("{error:?}"));
            eprintln!("{}", error.warning());
            if outbox::record_error(&self.dir, &self.row, &error).is_err() {
                eprintln!("[index-outbox] operation=record-error reason=unavailable");
            }
            Err(error)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod rag_completion_tests {
    use super::*;
    use server_projects::outbox::{self, Job};
    fn claimed() -> (tempfile::TempDir, outbox::IndexClaim) {
        let dir = tempfile::tempdir().unwrap();
        outbox::enqueue(
            dir.path(),
            &[Job {
                kind: "index",
                project: "p",
                file: Some("synthetic.md"),
                payload: None,
            }],
            1,
        )
        .unwrap();
        let row = outbox::claim_index(dir.path(), false, 2).unwrap().unwrap();
        (dir, row)
    }
    #[test]
    fn status_and_ack_errors_are_recorded_sanitized_and_ack_is_retried() {
        let (dir, mut row) = claimed();
        let db = rusqlite::Connection::open(outbox::path(dir.path())).unwrap();
        db.execute("UPDATE jobs SET attempts=5", []).unwrap();
        row.attempts = 4;
        let completion = RagCompletion::from_attempt(
            dir.path().into(),
            row,
            RagAttempt {
                result: Ok(()),
                prepared: true,
                deferred: false,
            },
            Err("private credential https://secret.invalid/token".repeat(1000)),
        );
        completion.apply().unwrap();
        let (state, error): (String, String) = db
            .query_row("SELECT state,last_error FROM jobs", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(state, "failed");
        assert!(error.len() < 256);
        assert!(error.contains("status"));
        assert!(!error.contains("credential"));
        assert!(!error.contains("secret.invalid"));
        outbox::enqueue(
            dir.path(),
            &[Job {
                kind: "index",
                project: "p",
                file: Some("synthetic.md"),
                payload: None,
            }],
            3,
        )
        .unwrap();
        let row = outbox::claim_index(dir.path(), false, 4).unwrap().unwrap();
        db.execute_batch("CREATE TRIGGER synthetic_ack_failure BEFORE DELETE ON jobs BEGIN SELECT RAISE(ABORT,'private token must never be logged'); END;").unwrap();
        let completion = RagCompletion::from_attempt(
            dir.path().into(),
            row,
            RagAttempt {
                result: Ok(()),
                prepared: true,
                deferred: false,
            },
            Ok(()),
        );
        let error = completion.apply().unwrap_err();
        assert_eq!(error.operation, "ack");
        assert!(!error.warning().contains("token"));
        assert!(error.warning().len() < 256);
        let stored: String = db
            .query_row("SELECT last_error FROM jobs", [], |r| r.get(0))
            .unwrap();
        assert!(stored.contains("ack"));
        assert!(!stored.contains("token"));
        db.execute_batch("DROP TRIGGER synthetic_ack_failure")
            .unwrap();
        completion.apply().unwrap();
        assert!(outbox::jobs(dir.path()).unwrap().is_empty());
    }
}

#[cfg(test)]
#[path = "runtime_membership_tests.rs"]
mod membership_tests;
