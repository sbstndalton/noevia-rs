#![allow(clippy::unwrap_used,clippy::expect_used,clippy::indexing_slicing)]
mod support;
use support::*;
use embedding_worker::{runtime,http::Http,Options,fill::Store,Index};
use std::sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}};
#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn regression_p2_transient_batch_proof_failure_resumes_without_restart() {
    let (dir,store)=seed();let once=Arc::new(AtomicBool::new(true));let fail=Arc::new(AtomicBool::new(false));let embeds=Arc::new(AtomicUsize::new(0));let (o,f,e)=(once.clone(),fail.clone(),embeds.clone());
    let stub=server(move |path,_| {if path=="/v1/models"&&o.swap(false,Ordering::SeqCst) {f.store(true,Ordering::SeqCst);}if path=="/internal/embedding-worker-owner"&&f.swap(false,Ordering::SeqCst) {return (404,"{}".into());}if path=="/v1/embeddings" {e.fetch_add(1,Ordering::SeqCst);}default_reply(path)}).await;
    let verify=store.clone();signal(dir.path(),1);let http=Http::new(&stub.base,None,&stub.base,None).unwrap();let worker=runtime::start(store,http,stub.base.clone(),"synthetic-token".into(),START.into(),Options::default(),None).unwrap();
    wait_until(||std::fs::read(dir.path().join("embedding-worker-status.json")).ok().and_then(|b|serde_json::from_slice::<serde_json::Value>(&b).ok()).is_some_and(|v|v.get("ownershipUnconfirmedSince").and_then(serde_json::Value::as_u64).is_some())).await;
    wait_until(||verify.remaining(&Index {user:None,project:"p".into()},&target("next")).ok()==Some(0)).await;
    assert!(embeds.load(Ordering::SeqCst)>=2);worker.shutdown().await;
}
#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn regression_p3_revoked_proof_after_embedding_prevents_publication() {
    let (dir,store)=seed();let verify=store.clone();let revoked=Arc::new(AtomicBool::new(false));let r=revoked.clone();
    let stub=server(move |path,_| {if path=="/v1/embeddings" {r.store(true,Ordering::SeqCst);}if path=="/internal/embedding-worker-owner"&&r.load(Ordering::SeqCst) {return (404,"{}".into());}default_reply(path)}).await;
    signal(dir.path(),1);let http=Http::new(&stub.base,None,&stub.base,None).unwrap();let worker=runtime::start(store,http,stub.base.clone(),"synthetic-token".into(),START.into(),Options::default(),None).unwrap();
    wait_until(||std::fs::read(dir.path().join("embedding-worker-status.json")).ok().and_then(|b|serde_json::from_slice::<serde_json::Value>(&b).ok()).is_some_and(|v|v.get("lastPass").is_some_and(|p|!p.is_null()))).await;
    assert!(revoked.load(Ordering::SeqCst));assert_eq!(verify.remaining(&Index {user:None,project:"p".into()},&target("active")).unwrap(),1);worker.shutdown().await;
}
