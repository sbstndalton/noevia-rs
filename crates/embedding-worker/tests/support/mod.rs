#![allow(dead_code,clippy::unwrap_used,clippy::expect_used,clippy::indexing_slicing)]
use embedding_worker::{sqlite::*,Config,Target};
use rusqlite::Connection;
use std::{sync::Arc,time::Duration};
use tokio::{io::{AsyncReadExt,AsyncWriteExt},net::TcpListener};
pub const START:&str="11111111111111111111111111111111";
pub const PROOF:&str=r#"{"protocol":"rust-embedding-worker-v1","start":"11111111111111111111111111111111","nodeWorkersStopped":true}"#;
pub fn target(model:&str)->Target {Target {profile:"nomic-v1".into(),model:model.into(),base_url:None}}
pub fn seed()->(tempfile::TempDir,SqlStore) {
    let dir=tempfile::tempdir().unwrap();let root=dir.path();let auth=Connection::open(root.join("cowork.db")).unwrap();auth.execute_batch(include_str!("../../../server-store/tests/fixtures/node-schema.sql")).unwrap();
    std::fs::create_dir(root.join("rag")).unwrap();std::fs::write(root.join("projects.json"),"{\"projects\":[{\"id\":\"p\"}]}").unwrap();
    let db=Connection::open(root.join("rag/p.db")).unwrap();db.execute_batch("PRAGMA journal_mode=WAL;CREATE TABLE chunks(id INTEGER PRIMARY KEY,file TEXT NOT NULL,chunk_no INTEGER NOT NULL,body TEXT NOT NULL,content_hash TEXT NOT NULL,updated_at TEXT,version TEXT,direct INTEGER,kind TEXT,meta TEXT);CREATE TABLE vec_meta(key TEXT PRIMARY KEY,value TEXT);INSERT INTO chunks VALUES(1,'synthetic.txt',0,'synthetic text','hash',NULL,NULL,0,'text',NULL)").unwrap();
    let store=SqlStore {rag_dir:None,root:root.into(),seed:Config {version:0,active:target("active"),next:Some(target("next")),previous:None},open_vec:open_vec_required};(dir,store)
}
pub fn signal(root:&std::path::Path,n:u64) {std::fs::write(root.join("embedding-worker-request.json"),serde_json::json!({"start":START,"sequence":n}).to_string()).unwrap();}
pub async fn wait_until(mut condition:impl FnMut()->bool) {tokio::time::timeout(Duration::from_secs(10),async {while !condition() {tokio::time::sleep(Duration::from_millis(20)).await;}}).await.unwrap();}
pub struct Server {pub base:String,task:tokio::task::JoinHandle<()>}
impl Drop for Server {fn drop(&mut self) {self.task.abort();}}
pub async fn server(reply:impl Fn(&str,&str)->(u16,String)+Send+Sync+'static)->Server {
    let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();let base=format!("http://{}",listener.local_addr().unwrap());let reply=Arc::new(reply);
    let task=tokio::spawn(async move {loop {let (mut stream,_)=listener.accept().await.unwrap();let mut bytes=vec![];let mut buf=[0;8192];
        // Serial request handling makes synthetic ownership/fallback transitions deterministic.
        loop {let n=tokio::time::timeout(Duration::from_secs(5),stream.read(&mut buf)).await.unwrap().unwrap();if n==0 {break;}bytes.extend_from_slice(&buf[..n]);let text=String::from_utf8_lossy(&bytes);if let Some((head,body))=text.split_once("\r\n\r\n") {let size=head.lines().find_map(|l|l.to_lowercase().strip_prefix("content-length:").map(|n|n.trim().parse::<usize>().unwrap())).unwrap_or(0);if body.len()>=size {break;}}}
        let text=String::from_utf8(bytes).unwrap();let path=text.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("");let body=text.split_once("\r\n\r\n").map(|(_,b)|b).unwrap_or("");let (status,body)=reply(path,body);let response=format!("HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());let _=stream.write_all(response.as_bytes()).await;
    }});Server {base,task}
}
pub fn default_reply(path:&str)->(u16,String) {match path {"/internal/embedding-worker-owner"=>(200,PROOF.into()),"/api/v1/embedding-service"=>(200,"{\"managed\":true,\"enabled\":true}".into()),"/v1/models"=>(200,"{\"data\":[{\"id\":\"next\"},{\"id\":\"active\"}]}".into()),"/v1/embeddings"=>(200,"{\"data\":[{\"embedding\":[1,0]}]}".into()),_=>(404,"{}".into())}}
