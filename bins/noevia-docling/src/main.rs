#![forbid(unsafe_code)]
//! The document extraction service: the native HTTP server and warm-worker supervisor
//! (docling-service), always. sbstndalton/noevia#1071 retired NOEVIA_DOCLING_IMPL and the Python
//! HTTP server it could select: a leftover value in an older environment file only earns one
//! warning, and nothing here starts `server.py`. A build without the `native-docling` capability
//! refuses before it listens or starts a worker.

/// The retired selector, and the one line a leftover value earns.
const RETIRED: &str = "NOEVIA_DOCLING_IMPL";
const WARNING: &str = "NOEVIA_DOCLING_IMPL is retired; the native server is always used";

/// The warning for a leftover selector value, if it needs one: unset, empty and `rust` (the old
/// native value, any case and surrounding whitespace) are silent; anything else, `python`
/// included, is ignored with a warning. The value itself is never echoed.
fn retired_warning(value: Option<std::ffi::OsString>) -> Option<&'static str> {
    let value = value?;
    let value = value.to_string_lossy();
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("rust") {
        None
    } else {
        Some(WARNING)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(warning) = retired_warning(std::env::var_os(RETIRED)) {
        eprintln!("{warning}");
    }
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
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    eprintln!("Docling Rust front ready");
    docling_service::serve_with_shutdown(listener, config, async move {
        tokio::select! {_=terminate.recv()=>(), _=tokio::signal::ctrl_c()=>()}
    })
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{retired_warning, WARNING};
    #[test]
    fn only_a_value_other_than_rust_warns() {
        for silent in [
            None,
            Some(""),
            Some("  "),
            Some("rust"),
            Some("RUST"),
            Some(" Rust\t"),
        ] {
            assert_eq!(retired_warning(silent.map(Into::into)), None, "{silent:?}");
        }
        for leftover in ["python", "PYTHON", " python ", "node", "rust2", "1"] {
            assert_eq!(
                retired_warning(Some(leftover.into())),
                Some(WARNING),
                "{leftover}"
            );
        }
    }
    #[test]
    fn a_non_unicode_value_warns_and_is_not_echoed() {
        use std::os::unix::ffi::OsStringExt;
        let value = std::ffi::OsString::from_vec(vec![0xff, b's', b'e', b'c', b'r', b'e', b't']);
        assert_eq!(retired_warning(Some(value)), Some(WARNING));
        assert!(!WARNING.contains("secret"));
    }
}
