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
struct NativeHost {
    store: SqlStore,
    http: Http,
    valid: Arc<AtomicBool>,
    off: Arc<AtomicBool>,
    service_base: Option<String>,
    core: String,
    token: zeroize::Zeroizing<String>,
    start: String,
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
                let mut recovered=BTreeSet::new();
                loop {
                    if *outbox_rx.borrow() {break;}
                    if host.http.outbox_ready(&core,&token,&start).await.unwrap_or(false) {
                        for user in host.store.users().unwrap_or_default() {
                            if *outbox_rx.borrow() {break;}
                            let Ok(dir)=server_projects::store::user_dir(&host.store.root,&user) else {continue;};
                            let recover=!recovered.contains(&user);
                            if let Ok(rows)=server_projects::outbox::pending_rag(&dir,recover) {
                                    // A missing database may appear later. Do not mark it recovered yet.
                                    if server_projects::outbox::path(&dir).exists() {recovered.insert(user.clone());}
                                    for row in rows {
                                        if *outbox_rx.borrow() || !host.http.outbox_ready(&core,&token,&start).await.unwrap_or(false) {break;}
                                        if !server_projects::outbox::claim_rag(&dir,&row,now() as i64).unwrap_or(false) {continue;}
                                        let ctl=Control::default();
                                        let result=match crate::fill::Embed::authorize(&host.http,&ctl).await {
                                            Ok(())=>host.store.index_source(&user,&row,&ctl),
                                            Err(e)=>Err(e),
                                        };
                                        let prepared=result.is_ok();
                                        let ok=match result {
                                            Ok(Some(index))=> {
                                                if let Ok(config)=host.store.config() {
                                                    if host.off(&config.active) {true} else {
                                                    // Bounded batches and a pause let foreground admission win.
                                                    let mut success=true;
                                                    loop {
                                                        if !host.http.outbox_ready(&core,&token,&start).await.unwrap_or(false) {success=false;break;}
                                                        let excluded=BTreeSet::new();
                                                        let filler=Filler{store:crate::sqlite::SourceStore{store:&host.store,row:&row},embedder:host.http.clone()};
                                                        let batch=tokio::select! {
                                                            r=filler.fill(&index,&config.active,4,&excluded,config.version,&ctl)=>r,
                                                            _=outbox_rx.changed()=>{ctl.stop();Err("source indexing stopped".into())}
                                                        };
                                                        match batch {
                                                            Ok(Some(batch)) if batch.remaining>0 && batch.embedded>0 => {},
                                                            Ok(Some(batch))=>{success=batch.remaining==0;break;},
                                                            Ok(None)=>break,
                                                            Err(_)=>{success=false;break;}
                                                        }
                                                        tokio::select! {_=tokio::time::sleep(Duration::from_millis(50))=>{},_=outbox_rx.changed()=>{success=false;break;}}
                                                    }
                                                    success
                                                    }
                                                } else {false}
                                            },
                                            Ok(None)=>true,
                                            Err(_)=>false,
                                        };
                                        if !*outbox_rx.borrow() {let _=host.store.mark_index_source(&user,&row,ok,prepared);}
                                        if !ok && !host.http.outbox_ready(&core,&token,&start).await.unwrap_or(false) {
                                            let _=server_projects::outbox::defer_index(&dir,&row);
                                        } else {let _=server_projects::outbox::finish_index(&dir,&row,ok);}
                                    }
                            }
                            tokio::task::yield_now().await;
                        }
                    }
                    tokio::select! {_=tokio::time::sleep(Duration::from_secs(2))=>{},_=outbox_rx.changed()=>{break;}}
                }
            };
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
            tokio::join!(refresh,migrate,outbox);
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
