//! Source-only native candidate for the isolated Code sandbox supervisor.
#![forbid(unsafe_code)]

use code_sandbox_supervisor::{serve, Config};
use tokio::net::TcpListener;
use tokio::sync::watch;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (config, port) = Config::from_env().map_err(std::io::Error::other)?;
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    let address = listener.local_addr()?;
    println!(
        "[code-sandbox] listening on {}:{}, workspaces under {}",
        address.ip(),
        address.port(),
        config.root.display()
    );
    let (tx, rx) = watch::channel(false);
    let server = tokio::spawn(serve(listener, config, rx));
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = terminate.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    let _ = tx.send(true);
    server.await??;
    Ok(())
}
