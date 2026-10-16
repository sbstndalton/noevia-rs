#![allow(dead_code)]
use noevia_model_manager::{
    diagnose_docker::{Context, Diagnosis, DiagnosisRead, Error, ReadJob},
    Container, DockerRead, Redactor, StateData,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
pub const TOKEN: &str = "synthetic-shared-token-00000000000";
type Job<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, &'static str>> + Send + 'a>>;
pub struct Fake {
    pub text: Vec<u8>,
    pub discovered: Vec<Container>,
    pub error: Option<Error>,
    pub lists: AtomicUsize,
    pub logs: AtomicUsize,
}
impl Fake {
    pub fn new(text: Vec<u8>) -> Self {
        Self {
            text,
            discovered: Vec::new(),
            error: None,
            lists: AtomicUsize::new(0),
            logs: AtomicUsize::new(0),
        }
    }
}
impl DiagnosisRead for Fake {
    fn inventory<'a>(&'a self, _context: &'a Context) -> ReadJob<'a, Vec<Container>> {
        Box::pin(async move {
            self.lists.fetch_add(1, Ordering::SeqCst);
            Ok(self.discovered.clone())
        })
    }
    fn current_logs<'a>(&'a self, _name: &'a str, _context: &'a Context) -> ReadJob<'a, Vec<u8>> {
        Box::pin(async move {
            self.logs.fetch_add(1, Ordering::SeqCst);
            match self.error {
                Some(error) => Err(error),
                None => Ok(self.text.clone()),
            }
        })
    }
}
struct OldDenied;
impl DockerRead for OldDenied {
    fn containers(&self) -> Job<'_, Vec<Container>> {
        Box::pin(async { Err("old capability refused") })
    }
    fn logs<'a>(&'a self, _name: &'a str, _tail: usize) -> Job<'a, Vec<u8>> {
        Box::pin(async { Err("old capability refused") })
    }
}
pub fn state(reader: Arc<dyn DiagnosisRead>, configured: Vec<String>) -> Arc<StateData> {
    Arc::new(StateData {
        diagnosis: Some(Arc::new(Diagnosis::new(reader).unwrap())),
        #[cfg(feature = "native-prompts")]
        prompts: None,
        #[cfg(feature = "native-settings")]
        settings: None,
        #[cfg(feature = "native-model-updates")]
        model_updates: None,
        #[cfg(feature = "native-model-detail")]
        model_detail: None,
        sections: None,
        embedding: None,
        host: None,
        token: TOKEN.into(),
        configured,
        docker: Arc::new(OldDenied),
        redactor: Redactor::new().unwrap(),
        permits: tokio::sync::Semaphore::new(4),
        legacy: noevia_model_manager::legacy_client().unwrap(),
        legacy_base: noevia_model_manager::LEGACY_BASE.to_owned(),
        idle_unload: None,
    })
}
/// A finite Rust-owned synthetic Unix Docker script. The caller observes the daemon join.
pub async fn wire(
    responses: Vec<Vec<u8>>,
) -> (
    tempfile::TempDir,
    Arc<noevia_model_manager::docker::UnixDocker>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = tempfile::Builder::new()
        .prefix("s1-diagnose-test-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = root.path().join("docker.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (mut socket, _) =
                tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 8192);
                let byte =
                    tokio::time::timeout(std::time::Duration::from_secs(2), socket.read_u8())
                        .await
                        .unwrap()
                        .unwrap();
                request.push(byte);
            }
            requests.push(String::from_utf8(request).unwrap());
            socket.write_all(&response).await.unwrap();
            socket.shutdown().await.unwrap();
        }
        requests
    });
    (
        root,
        Arc::new(noevia_model_manager::docker::UnixDocker { socket }),
        task,
    )
}
pub fn response(status: u16, raw: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status} Synthetic\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        raw.len()
    )
    .into_bytes();
    response.extend_from_slice(raw);
    response
}
pub fn metadata(id: &str, started: &str, tty: bool) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"Id":id,"Name":"/synthetic-backend","Image":"sha256:synthetic","State":{"StartedAt":started},"Config":{"Tty":tty}})).unwrap()
}
pub fn frame(kind: u8, raw: &[u8]) -> Vec<u8> {
    let mut frame = vec![kind, 0, 0, 0];
    frame.extend_from_slice(&u32::try_from(raw.len()).unwrap().to_be_bytes());
    frame.extend_from_slice(raw);
    frame
}
