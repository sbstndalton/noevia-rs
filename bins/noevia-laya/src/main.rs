#![forbid(unsafe_code)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::var("NOEVIA_LAYA_IMPL") {
        Ok(value) if value == "rust" => {}
        Err(std::env::VarError::NotPresent) => {}
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err("NOEVIA_LAYA_IMPL must be UTF-8 rust".into());
        }
        _ => return Err("NOEVIA_LAYA_IMPL must be rust when set".into()),
    }
    if !cfg!(feature = "native-laya") {
        return Err("native-laya feature required".into());
    }
    let config = sensitivity_service::Config::production();
    // Install shutdown handlers before the90-second engine startup.
    // Cancelling Pool::start drops its owned process-group guards.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let pool = tokio::select! {
        result=sensitivity_service::Pool::start(config)=>result?,
        _=terminate.recv()=>return Ok(()),
        _=tokio::signal::ctrl_c()=>return Ok(()),
    };
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", 8040)).await {
        Ok(listener) => listener,
        Err(error) => {
            pool.stop().await;
            return Err(error.into());
        }
    };
    sensitivity_service::serve_with_shutdown(listener, pool, async move {
        tokio::select! { _=terminate.recv()=>(), _=tokio::signal::ctrl_c()=>() }
    })
    .await?;
    Ok(())
}
