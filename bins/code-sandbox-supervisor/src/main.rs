//! Source-only native candidate for the isolated Code sandbox supervisor.
#![forbid(unsafe_code)]

use code_sandbox_supervisor::{serve, Config};
use tokio::net::TcpListener;
use tokio::sync::watch;

// Keep the server future owned throughout signal handling. A completed server
// must finish main immediately; a signal must request and await its cleanup.
async fn supervise(
    server: impl std::future::Future<Output = std::io::Result<()>>,
    signal: impl std::future::Future<Output = std::io::Result<()>>,
    stop: watch::Sender<bool>,
) -> std::io::Result<()> {
    tokio::pin!(server);
    let signal_result = tokio::select! {
        result = &mut server => return result,
        result = signal => result,
    };
    let _ = stop.send(true);
    server.await?;
    signal_result
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (config, port) = Config::from_env().map_err(std::io::Error::other)?;
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    let address = listener.local_addr()?;
    println!(
        "[code-sandbox] listening on {}:{}, workspaces under {}",
        address.ip(),
        address.port(),
        config.root.display()
    );
    let (tx, rx) = watch::channel(false);
    let signal = async {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = terminate.recv() => Ok(()),
                result = tokio::signal::ctrl_c() => result,
            }
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c().await
    };
    supervise(serve(listener, config, rx), signal, tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::Duration;

    #[tokio::test]
    async fn server_failure_does_not_wait_for_a_signal() -> Result<(), Box<dyn std::error::Error>> {
        let (stop, _) = watch::channel(false);
        let server = async { Err(std::io::Error::other("synthetic accept failure")) };
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            supervise(server, std::future::pending(), stop),
        )
        .await?;
        assert_eq!(
            result.err().map(|e| e.to_string()).as_deref(),
            Some("synthetic accept failure")
        );
        Ok(())
    }

    #[tokio::test]
    async fn signal_failure_still_awaits_server_cleanup() -> Result<(), Box<dyn std::error::Error>>
    {
        let (stop, mut rx) = watch::channel(false);
        let cleaned = Arc::new(AtomicBool::new(false));
        let did_clean = Arc::clone(&cleaned);
        let server = async move {
            rx.changed().await.map_err(std::io::Error::other)?;
            assert!(*rx.borrow());
            did_clean.store(true, Ordering::SeqCst);
            Ok(())
        };
        let signal = async {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "synthetic signal failure",
            ))
        };
        let result =
            tokio::time::timeout(Duration::from_secs(1), supervise(server, signal, stop)).await?;
        assert!(cleaned.load(Ordering::SeqCst));
        assert_eq!(
            result.err().map(|e| e.kind()),
            Some(std::io::ErrorKind::PermissionDenied)
        );
        Ok(())
    }
}
