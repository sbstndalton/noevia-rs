#![cfg(feature = "native-diagnose")]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#[path = "support/diagnose.rs"]
mod support;
use noevia_model_manager::{
    diagnose_docker::{Context, DiagnosisRead, Outcome, ReadJob},
    Container,
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
struct HeldReader {
    releases: Mutex<Vec<std::sync::mpsc::Receiver<()>>>,
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    ready: tokio::sync::mpsc::UnboundedSender<()>,
}
impl DiagnosisRead for HeldReader {
    fn inventory<'a>(&'a self, _context: &'a Context) -> ReadJob<'a, Vec<Container>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn current_logs<'a>(&'a self, _name: &'a str, context: &'a Context) -> ReadJob<'a, Vec<u8>> {
        Box::pin(async move {
            let release = self.releases.lock().unwrap().pop().unwrap();
            let entered = self.entered.clone();
            let (driver_ready, observed) = tokio::sync::oneshot::channel();
            context.own_driver(async move {
                struct BlockDrop {
                    release: std::sync::mpsc::Receiver<()>,
                    entered: tokio::sync::mpsc::UnboundedSender<()>,
                }
                impl Drop for BlockDrop {
                    fn drop(&mut self) {
                        let _ = self.entered.send(());
                        let _ = self.release.recv();
                    }
                }
                let _drop = BlockDrop { release, entered };
                let _ = driver_ready.send(());
                std::future::pending::<()>().await;
            })?;
            observed.await.unwrap();
            self.ready.send(()).unwrap();
            std::future::pending().await
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_jobs_retain_http_admission_until_observed_driver_joins() {
    let (release_a, wait_a) = std::sync::mpsc::channel();
    let (release_b, wait_b) = std::sync::mpsc::channel();
    let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ready, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
    let reader = Arc::new(HeldReader {
        releases: Mutex::new(vec![wait_a, wait_b]),
        entered,
        ready,
    });
    let state = support::state(reader, vec!["synthetic-backend".into()]);
    let diagnosis = state.diagnosis.as_ref().unwrap().clone();
    let mut requests = Vec::new();
    for _ in 0..2 {
        let state = state.clone();
        let diagnosis = diagnosis.clone();
        requests.push(tokio::spawn(async move {
            diagnosis
                .request(
                    state,
                    "synthetic-backend".into(),
                    Instant::now() + Duration::from_secs(60),
                )
                .await
        }));
        ready_rx.recv().await.unwrap();
    }
    for request in &requests {
        request.abort();
    }
    for request in requests {
        let _ = request.await;
    }
    entered_rx.recv().await.unwrap();
    entered_rx.recv().await.unwrap();
    let retained = state.permits.available_permits();
    let third = diagnosis
        .request(
            state.clone(),
            "synthetic-backend".into(),
            Instant::now() + Duration::from_secs(60),
        )
        .await;
    // Always release owned destructor barriers before any semantic assertion can panic.
    release_a.send(()).unwrap();
    release_b.send(()).unwrap();
    diagnosis.shutdown().await;
    assert_eq!(retained, 2);
    assert!(matches!(third, Outcome::Busy));
    assert_eq!(state.permits.available_permits(), 4);
}
#[tokio::test]
async fn one_expired_arrival_deadline_is_best_effort_empty() {
    let fake = Arc::new(support::Fake::new(b"fatal".to_vec()));
    let state = support::state(fake, vec!["synthetic-backend".into()]);
    let diagnosis = state.diagnosis.as_ref().unwrap().clone();
    let outcome = diagnosis
        .request(
            state.clone(),
            "synthetic-backend".into(),
            Instant::now() - Duration::from_secs(1),
        )
        .await;
    diagnosis.shutdown().await;
    assert!(matches!(outcome,Outcome::Json(bytes) if bytes==b"{\"failures\":[]}"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timed_out_response_retains_join_and_shutdown_until_driver_drop_completes() {
    let (release, wait) = std::sync::mpsc::channel();
    let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ready, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = support::state(
        Arc::new(HeldReader {
            releases: Mutex::new(vec![wait]),
            entered,
            ready,
        }),
        vec!["synthetic-backend".into()],
    );
    let diagnosis = state.diagnosis.as_ref().unwrap().clone();
    let request_state = state.clone();
    let request_diagnosis = diagnosis.clone();
    let request = tokio::spawn(async move {
        request_diagnosis
            .request(
                request_state,
                "synthetic-backend".into(),
                Instant::now() + Duration::from_millis(100),
            )
            .await
    });
    ready_rx.recv().await.unwrap();
    let response = request.await.unwrap();
    entered_rx.recv().await.unwrap();
    let retained = state.permits.available_permits();
    let shutdown_owner = diagnosis.clone();
    let shutdown = tokio::spawn(async move { shutdown_owner.shutdown().await });
    tokio::task::yield_now().await;
    let still_retained = !shutdown.is_finished();
    release.send(()).unwrap();
    shutdown.await.unwrap();
    assert!(matches!(response,Outcome::Json(bytes) if bytes==b"{\"failures\":[]}"));
    assert_eq!(retained, 3);
    assert!(still_retained);
    assert_eq!(state.permits.available_permits(), 4);
}
struct PanicReader {
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    panicking: tokio::sync::mpsc::UnboundedSender<()>,
}
struct CleanupReader {
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    entered: tokio::sync::mpsc::UnboundedSender<()>,
}
impl DiagnosisRead for CleanupReader {
    fn inventory<'a>(&'a self, _: &'a Context) -> ReadJob<'a, Vec<Container>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn current_logs<'a>(&'a self, _: &'a str, context: &'a Context) -> ReadJob<'a, Vec<u8>> {
        Box::pin(async move {
            let release = self.release.lock().unwrap().take().unwrap();
            let entered = self.entered.clone();
            let (ready, observed) = tokio::sync::oneshot::channel();
            context.own_driver(async move {
                struct Held {
                    release: std::sync::mpsc::Receiver<()>,
                    entered: tokio::sync::mpsc::UnboundedSender<()>,
                }
                impl Drop for Held {
                    fn drop(&mut self) {
                        let _ = self.entered.send(());
                        let _ = self.release.recv();
                    }
                }
                let _held = Held { release, entered };
                let _ = ready.send(());
                std::future::pending::<()>().await;
            })?;
            observed.await.unwrap();
            // Injected reader cancellation interrupts cleanup while the driver join
            // remains pending. Production UnixDocker never invokes this method.
            context.cleanup().await;
            Ok(Vec::new())
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_reader_cleanup_keeps_driver_handle_under_context_owner() {
    let (release, wait) = std::sync::mpsc::channel();
    let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = support::state(
        Arc::new(CleanupReader {
            release: Mutex::new(Some(wait)),
            entered,
        }),
        vec!["synthetic-backend".into()],
    );
    let diagnosis = state.diagnosis.as_ref().unwrap().clone();
    let request_owner = diagnosis.clone();
    let request_state = state.clone();
    let request = tokio::spawn(async move {
        request_owner
            .request(
                request_state,
                "synthetic-backend".into(),
                Instant::now() + Duration::from_secs(60),
            )
            .await
    });
    entered_rx.recv().await.unwrap();
    request.abort();
    let _ = request.await;
    let shutdown_owner = diagnosis.clone();
    let mut shutdown = tokio::spawn(async move { shutdown_owner.shutdown().await });
    let observed = tokio::time::timeout(Duration::from_millis(500), &mut shutdown).await;
    let retained = state.permits.available_permits();
    let shutdown_pending = observed.is_err();
    // Release the owned driver barrier on both old and repaired branches before
    // evaluating any assertion, then observe shutdown if still retained.
    release.send(()).unwrap();
    if shutdown_pending {
        shutdown.await.unwrap();
    }
    assert_eq!(retained, 3);
    assert!(shutdown_pending);
    assert_eq!(state.permits.available_permits(), 4);
}
impl DiagnosisRead for PanicReader {
    fn inventory<'a>(&'a self, _: &'a Context) -> ReadJob<'a, Vec<Container>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    #[allow(clippy::panic)]
    fn current_logs<'a>(&'a self, _: &'a str, context: &'a Context) -> ReadJob<'a, Vec<u8>> {
        Box::pin(async move {
            let release = self.release.lock().unwrap().take().unwrap();
            let entered = self.entered.clone();
            let (ready, observed) = tokio::sync::oneshot::channel();
            context.own_driver(async move {
                struct Held {
                    release: std::sync::mpsc::Receiver<()>,
                    entered: tokio::sync::mpsc::UnboundedSender<()>,
                }
                impl Drop for Held {
                    fn drop(&mut self) {
                        let _ = self.entered.send(());
                        let _ = self.release.recv();
                    }
                }
                let _held = Held { release, entered };
                let _ = ready.send(());
                std::future::pending::<()>().await;
            })?;
            observed.await.unwrap();
            self.panicking.send(()).unwrap();
            panic!("closed synthetic diagnosis reader panic");
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reader_panic_retains_http_permit_until_registered_driver_join() {
    let (release, wait) = std::sync::mpsc::channel();
    let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (panicking, mut panic_rx) = tokio::sync::mpsc::unbounded_channel();
    let state = support::state(
        Arc::new(PanicReader {
            release: Mutex::new(Some(wait)),
            entered,
            panicking,
        }),
        vec!["synthetic-backend".into()],
    );
    let diagnosis = state.diagnosis.as_ref().unwrap().clone();
    let request_state = state.clone();
    let request_owner = diagnosis.clone();
    let request = tokio::spawn(async move {
        request_owner
            .request(
                request_state,
                "synthetic-backend".into(),
                Instant::now() + Duration::from_secs(60),
            )
            .await
    });
    panic_rx.recv().await.unwrap();
    let shutdown_owner = diagnosis.clone();
    let shutdown = tokio::spawn(async move { shutdown_owner.shutdown().await });
    entered_rx.recv().await.unwrap();
    let retained = state.permits.available_permits();
    release.send(()).unwrap();
    let response = request.await.unwrap();
    shutdown.await.unwrap();
    assert_eq!(retained, 3);
    assert!(matches!(response, Outcome::Refused));
    assert_eq!(state.permits.available_permits(), 4);
}
