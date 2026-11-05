#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;
use embedding_worker::{fill::Embed, http::Http, *};
use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};
use support::*;
struct HostStub {
    http: Http,
    started: Instant,
    calls: AtomicUsize,
    pauses: Mutex<Vec<u64>>,
}
impl Host for HostStub {
    fn config(&self) -> Config {
        Config {
            version: 0,
            active: target("active"),
            next: None,
            previous: None,
        }
    }
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
    fn indexes(&self) -> Vec<Index> {
        vec![Index {
            user: None,
            project: "p".into(),
        }]
    }
    fn served<'a>(&'a self, _: &'a Target) -> Task<'a, Result<Vec<String>, String>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn fill<'a>(
        &'a self,
        _: &'a Index,
        target: &'a Target,
        _: usize,
        _: &'a BTreeSet<i64>,
        _: u64,
        control: &'a Control,
    ) -> Task<'a, Result<Option<Batch>, String>> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
                return Ok(Some(Batch::default()));
            }
            self.http.embed(target, &[vec![97]], control).await?;
            Ok(Some(Batch {
                embedded: 1,
                attempted: 1,
                remaining: 1,
                refused: vec![],
            }))
        })
    }
    fn drop_slot(&self, _: &Index, _: &Target) -> Result<(), String> {
        Ok(())
    }
    fn sleep<'a>(&'a self, ms: u64, _: &'a Control) -> Task<'a, ()> {
        Box::pin(async move {
            self.pauses.lock().unwrap().push(ms);
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_p3_admission_wait_does_not_inflate_duty_pause() {
    let engine = server(|path, _| {
        if path == "/v1/embeddings" {
            std::thread::sleep(Duration::from_millis(10));
        }
        default_reply(path)
    })
    .await;
    let core = server(|path, _| {
        if path == "/internal/inference-admission/lease" {
            std::thread::sleep(Duration::from_millis(200));
            (
                201,
                r#"{"lease":"0123456789abcdef0123456789abcdef","ttlMs":30000}"#.into(),
            )
        } else {
            (204, "null".into())
        }
    })
    .await;
    let http = Http::new(&engine.base, None, &engine.base, None)
        .unwrap()
        .with_admission(&core.base, "synthetic-token")
        .unwrap();
    let host = HostStub {
        http,
        started: Instant::now(),
        calls: AtomicUsize::new(0),
        pauses: Mutex::new(vec![]),
    };
    let mut worker = Worker::new(Options::default());
    worker.control().kick();
    worker.tick(&host).await;
    let pauses = host.pauses.lock().unwrap();
    assert_eq!(pauses.len(), 1);
    assert!(
        *pauses.first().unwrap() < 100,
        "queue time is not embeddings busy time: {pauses:?}"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_p3_stop_cancels_pending_admission() {
    use tokio::{io::AsyncReadExt, net::TcpListener};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (send, recv) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 4096];
        let _ = peer.read(&mut bytes).await.unwrap();
        send.send(()).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(1), peer.read(&mut bytes)).await;
    });
    let http = Http::new(&base, None, &base, None)
        .unwrap()
        .with_admission(&base, "synthetic-token")
        .unwrap();
    let control = Control::default();
    let c = control.clone();
    let mut task =
        tokio::spawn(async move { http.embed(&target("active"), &[vec![97]], &c).await });
    tokio::time::timeout(Duration::from_secs(1), recv)
        .await
        .unwrap()
        .unwrap();
    control.stop();
    let stopped = tokio::time::timeout(Duration::from_millis(200), &mut task)
        .await
        .is_ok();
    if !stopped {
        task.abort();
        let _ = task.await;
    }
    server.await.unwrap();
    assert!(stopped, "stop must cancel enter, not wait 150 seconds");
}
