#![forbid(unsafe_code)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let implementation = std::env::var("NOEVIA_DOCLING_IMPL").unwrap_or_else(|_| "python".into());
    match implementation.as_str() {
        "python" => {
            let status = tokio::process::Command::new("python")
                .arg("server.py")
                .status()
                .await?;
            if !status.success() {
                return Err("Python Docling owner exited unsuccessfully".into());
            }
        }
        "rust" => {
            if !cfg!(feature = "native-docling") {
                return Err("native-docling feature required".into());
            }
            let port = std::env::var("DOCLING_PORT")
                .unwrap_or_else(|_| "8031".into())
                .parse::<u16>()?;
            let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
            let mut config = docling_service::Config::production(std::env::current_dir()?);
            for (key, target) in [
                ("DOCLING_DOCUMENT_TIMEOUT_SECONDS", &mut config.long_timeout),
                ("DOCLING_OTHER_TIMEOUT_SECONDS", &mut config.short_timeout),
            ] {
                if let Ok(value) = std::env::var(key) {
                    if let Ok(seconds) = value.parse::<f64>() {
                        if seconds > 0.0 {
                            if let Ok(duration) = std::time::Duration::try_from_secs_f64(seconds) {
                                *target = duration;
                            }
                        }
                    }
                }
            }
            docling_service::serve(listener, config).await?;
        }
        _ => return Err("NOEVIA_DOCLING_IMPL must be python or rust".into()),
    }
    Ok(())
}
