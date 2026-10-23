//! `noevia-inference-gate`: see the library docs. Configuration comes from the environment
//! (`GATE_*`, see `env.rs`); an invalid value stops the process before it listens.

#[tokio::main]
async fn main() {
    let config = match noevia_inference_gate::env::from_env(|key| std::env::var(key).ok()) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("noevia-inference-gate: {message}");
            std::process::exit(2);
        }
    };
    let listener = match tokio::net::TcpListener::bind(config.listen).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!(
                "noevia-inference-gate: cannot listen on {}: {err}",
                config.listen
            );
            std::process::exit(1);
        }
    };
    let shutdown = async {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(term) => term,
                    Err(_) => {
                        let _ = ctrl_c.await;
                        return;
                    }
                };
            tokio::select! {
                _ = ctrl_c => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
        }
    };
    if let Err(err) = noevia_inference_gate::run(listener, config, shutdown).await {
        eprintln!("noevia-inference-gate: {err}");
        std::process::exit(1);
    }
}
