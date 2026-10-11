#![forbid(unsafe_code)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let selector = match std::env::var("NOEVIA_LAYA_IMPL") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => "python".into(),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err("NOEVIA_LAYA_IMPL must be UTF-8 python or rust".into());
        }
    };
    match selector.as_str() {
        "python" => {
            use std::os::unix::process::CommandExt;
            return Err(std::process::Command::new("python")
                .arg("/service/server.py")
                .exec()
                .into());
        }
        "rust" => {
            if !cfg!(feature = "native-laya") {
                return Err("native-laya feature required".into());
            }
            let config = laya_service::Config::production();
            // Install shutdown handlers before the90-second engine startup.
            // Cancelling Pool::start drops its owned process-group guards.
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            let pool = tokio::select! {
                result=laya_service::Pool::start(config)=>result?,
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
            laya_service::serve_with_shutdown(listener, pool, async move {
                tokio::select! { _=terminate.recv()=>(), _=tokio::signal::ctrl_c()=>() }
            })
            .await?;
        }
        _ => return Err("NOEVIA_LAYA_IMPL must be python or rust".into()),
    }
    Ok(())
}
