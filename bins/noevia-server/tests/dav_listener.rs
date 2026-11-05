//! Synthetic SQLite accounts and a signed loopback Diary; no production state.
#![allow(clippy::unwrap_used,clippy::expect_used,clippy::panic)]
use axum::{body::{Body,to_bytes},http::{Request,Response},Router};
use base64::Engine;
use noevia_server::dav::{bind_host,NativeBackend};
use server_dav::{Config,DavService,Scope};
use server_store::{Store,Writer,RustAuth,rusqlite::{Connection,params}};
use std::{collections::HashMap,sync::{Arc,Mutex},time::Duration};
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
const USER:&str="11111111-1111-4111-8111-111111111111";
const PASSWORD:&str="nv_dav_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.T__o4qEYEzGvCBRCkWGgkJUiECUVkENvm8M2ypgmjQ4";
const HASH:&str="$argon2id$v=19$m=19456,t=2,p=1$htTnVq45aTFtSZ4j6uYg2Q$0dpnt5pAek0kwLdm/v1/iGBKzQ1bxZlOeOT9W9kbBlQ";
fn version(s:&str)->String{format!("{:x}",Sha256::digest(s.as_bytes()))}
fn seed(path:&std::path::Path)->Connection{
 let db=Connection::open(path.join("cowork.db")).unwrap();
 db.execute_batch(include_str!("../../../crates/server-store/tests/fixtures/node-schema.sql")).unwrap();
 db.execute_batch("CREATE TABLE dav_settings(user_id TEXT PRIMARY KEY REFERENCES users(id),scope TEXT NOT NULL,acknowledged_at INTEGER,updated_at INTEGER NOT NULL);").unwrap();
 db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,'alice','alice','Synthetic','member','x','synthetic',1,1)",[USER]).unwrap();
 db.execute("INSERT INTO user_features(user_id,diary_enabled,updated_at) VALUES(?1,1,1)",[USER]).unwrap();
 db.execute("INSERT INTO dav_settings VALUES(?1,'lan',NULL,1)",[USER]).unwrap();
 db.execute("INSERT INTO app_passwords(id,user_id,name,scope,password_hash,created_at) VALUES('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',?1,'Synthetic','lan',?2,1)",params![USER,HASH]).unwrap();db
}
struct Rig { _dir:tempfile::TempDir, db:Connection, service:Arc<DavService<NativeBackend>>, files:Arc<Mutex<HashMap<String,String>>>, calls:Arc<Mutex<Vec<Value>>>, task:tokio::task::JoinHandle<()> }
impl Drop for Rig {fn drop(&mut self){self.task.abort();}}
impl Rig {
 async fn new()->Self{
  let dir=tempfile::tempdir().unwrap();let db=seed(dir.path());
  let files=Arc::new(Mutex::new(HashMap::from([("entry.md".into(),"Synthetic".into())])));let calls=Arc::new(Mutex::new(Vec::new()));
  let state=Arc::clone(&files);let records=Arc::clone(&calls);
  let router=Router::new().fallback(move|req:Request<Body>|{let files=Arc::clone(&state);let records=Arc::clone(&records);async move{
   assert_eq!(req.headers().get("x-cowork-user-id").unwrap(),USER);
   assert!(req.headers().contains_key("x-cowork-tenant-assertion"));
   assert_eq!(req.headers().get("authorization").unwrap(),"Bearer synthetic-service");
   let method=req.method().to_string();let uri=req.uri().clone();
   let body=to_bytes(req.into_body(),1024*1024).await.unwrap();
   let body:Value=if body.is_empty(){Value::Null}else{serde_json::from_slice(&body).unwrap()};
   records.lock().unwrap().push(json!({"method":method,"path":uri.path(),"body":body}));
   let path=body.get("path").and_then(Value::as_str).unwrap_or("");let mut files=files.lock().unwrap();
   let mut status=200;
   let value=match (method.as_str(),uri.path()) {
    ("GET","/api/files")=>{json!({"files":files.keys().map(|p|json!({"path":p,"name":p,"isDir":false,"modified":1})).collect::<Vec<_>>()})},
    ("POST","/api/file")=>match files.get(path){Some(s)=>json!({"content":s,"version":version(s)}),None=>json!({"content":null})},
    ("PUT","/api/file")=>{
     let expected=body.get("version").and_then(Value::as_str);let current=files.get(path).map(|s|version(s));
     if current.as_deref()!=expected{status=409;json!({"detail":"changed"})}else{let content=body["content"].as_str().unwrap();files.insert(path.into(),content.into());json!({"version":version(content)})}
    },
    ("POST","/api/directory")=>json!({}),
    ("POST","/api/workspace-ops")=>match body["op"].as_str().unwrap(){
     "stat"=>match files.get(path){Some(s)=>json!({"version":version(s),"isDir":false}),None=>{status=404;json!({"detail":"missing"})}},
     "preserve"=>json!({"trash":"synthetic-trash"}),
     "delete"=>{files.remove(path);json!({"trash":["synthetic-trash"]})},
     "move"|"copy"=>{let content=files.get(path).unwrap().clone();let target=body["destination"].as_str().unwrap();let replaced=files.insert(target.into(),content).is_some();if body["op"]=="move"{files.remove(path);}json!({"replaced":replaced})},
     _=>{status=400;json!({"detail":"bad op"})}
    },
    _=>{status=404;json!({"detail":"missing"})}
   };
   Response::builder().status(status).header("content-type","application/json").body(Body::from(value.to_string())).unwrap()
  }});
  let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let address=listener.local_addr().unwrap();
  let task=tokio::spawn(async move{axum::serve(listener,router).await.unwrap();});
  let backend=NativeBackend::new(Arc::new(Store::open(dir.path()).unwrap()),Arc::new(Writer::open(dir.path(),RustAuth::owned()).unwrap()),server_diary::DiaryConfig::new(format!("http://{address}"),"synthetic-service","synthetic-key",String::new),dir.path().into(),Scope::Lan).unwrap();
  let service=Arc::new(DavService::new(Arc::new(backend),Config::new("http://sharing.test:8031",Scope::Lan,None).unwrap()));
  Self{_dir:dir,db,service,files,calls,task}
 }
 async fn call(&self,method:&str,path:&str,body:&str,headers:&[(&str,&str)])->Response<Body>{
  let auth=base64::engine::general_purpose::STANDARD.encode(format!("alice:{PASSWORD}"));
  let mut req=Request::builder().method(method).uri(path).header("host","sharing.test:8031").header("authorization",format!("Basic {auth}"));
  for (key,value) in headers{req=req.header(*key,*value);}
  tokio::time::timeout(Duration::from_secs(10),self.service.handle(req.body(Body::from(body.to_owned())).unwrap(),"192.0.2.1".parse().unwrap())).await.unwrap()
 }
}
#[test]
fn bind_fallback_matches_original_without_new_exposure(){
 for (values,expected) in [(vec![],"0.0.0.0"),(vec![("UI_HOST","localhost")],"localhost"),(vec![("UI_HOST","127.0.0.1"),("COWORK_DAV_HOST","::1")],"::1")] {
  assert_eq!(bind_host(&|k|values.iter().find(|(key,_)|*key==k).map(|(_,v)|v.to_string())),expected);
 }
}
#[tokio::test]
async fn native_listener_uses_scoped_signed_operations_and_audits(){
 let r=Rig::new().await;
 let options=r.call("OPTIONS","/dav/alice/","",&[]).await;
 assert_eq!(options.status(),200);assert_eq!(options.headers()["dav"],"1");assert!(options.headers()["allow"].to_str().unwrap().ends_with("DELETE, MOVE, COPY"));
 assert!(r.db.query_row("SELECT last_used_at FROM app_passwords",[],|row|row.get::<_,i64>(0)).unwrap()>0);
 assert_eq!(r.call("GET","/dav/alice/entry.md","",&[]).await.status(),200);
 assert_eq!(r.call("PUT","/dav/alice/new.md","New",&[("if-none-match","*")]).await.status(),201);
 assert_eq!(r.call("PUT","/dav/alice/new.md","Replacement",&[]).await.status(),204);
 assert_eq!(r.files.lock().unwrap()["new.md"],"Replacement");
 assert!(r.calls.lock().unwrap().iter().any(|v|v["body"]["op"]=="preserve"));
 assert_eq!(r.call("COPY","/dav/alice/new.md","",&[("destination","/dav/alice/copy.md"),("overwrite","F")]).await.status(),201);
 assert_eq!(r.call("MOVE","/dav/alice/copy.md","",&[("destination","/dav/alice/moved.md")]).await.status(),201);
 assert_eq!(r.call("DELETE","/dav/alice/moved.md","",&[]).await.status(),204);
 assert_eq!(r.call("MKCOL","/dav/alice/folder","",&[]).await.status(),201);
 let events:i64=r.db.query_row("SELECT count(*) FROM audit_events WHERE action LIKE 'dav.%'",[],|r|r.get(0)).unwrap();assert_eq!(events,6);
 assert!(r.db.query_row("SELECT detail FROM audit_events WHERE action='dav.write' AND detail LIKE '%unconditional%'",[],|r|r.get::<_,String>(0)).unwrap().contains("synthetic-trash"));
}
#[tokio::test]
async fn optin_scope_revocation_and_path_errors_never_reach_diary(){
 let r=Rig::new().await;
 for (sql,status) in [("UPDATE dav_settings SET scope='off'",403),("UPDATE dav_settings SET scope='public'",403),("UPDATE user_features SET diary_enabled=0",403),("UPDATE users SET disabled_at=1",401)] {
  r.db.execute("UPDATE dav_settings SET scope='lan'",[]).unwrap();r.db.execute("UPDATE user_features SET diary_enabled=1",[]).unwrap();
  r.db.execute(sql,[]).unwrap();assert_eq!(r.call("GET","/dav/alice/entry.md","",&[]).await.status(),status);
 }
 assert!(r.calls.lock().unwrap().is_empty());
 for path in ["/dav/alice/%2e%2e/entry.md","/dav/alice/a%2fb.md","/dav/alice/a%5cb.md","/dav/alice/%GG"] {assert_eq!(r.call("GET",path,"",&[]).await.status(),400);}
}
#[tokio::test]
async fn revoking_while_a_put_body_waits_blocks_every_mutation(){
 use futures_util::stream;
 let r=Rig::new().await;let service=Arc::clone(&r.service);
 let (tx,rx)=tokio::sync::oneshot::channel::<()>();
 let body=Body::from_stream(stream::once(async move {let _=rx.await;Ok::<_,std::io::Error>(bytes::Bytes::from_static(b"changed"))}));
 let auth=base64::engine::general_purpose::STANDARD.encode(format!("alice:{PASSWORD}"));
 let req=Request::builder().method("PUT").uri("/dav/alice/entry.md").header("host","sharing.test:8031").header("authorization",format!("Basic {auth}")).header("if-match",format!("\"{}\"",version("Synthetic"))).body(body).unwrap();
 let task=tokio::spawn(async move{service.handle(req,"192.0.2.1".parse().unwrap()).await});
 let until=tokio::time::Instant::now()+Duration::from_secs(10);
 while r.db.query_row("SELECT last_used_at FROM app_passwords",[],|row|row.get::<_,Option<i64>>(0)).unwrap().is_none(){assert!(tokio::time::Instant::now()<until);tokio::task::yield_now().await;}
 r.db.execute("DELETE FROM app_passwords",[]).unwrap();let _=tx.send(());
 assert_eq!(tokio::time::timeout(Duration::from_secs(10),task).await.unwrap().unwrap().status(),403);
 assert_eq!(r.files.lock().unwrap()["entry.md"],"Synthetic");assert!(!r.calls.lock().unwrap().iter().any(|v|v["method"]=="PUT"));
}
