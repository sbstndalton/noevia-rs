//! noevia-web-supervisor: native port of noevia build/web-supervisor.sh (inert: no image starts it yet).
//! `--healthcheck` is the image's HEALTHCHECK probe without Node (src/health.rs).
fn main() {
    if std::env::args_os().nth(1).is_some_and(|a| a == "--healthcheck") {
        std::process::exit(web_supervisor::health::run());
    }
    std::process::exit(web_supervisor::run());
}
