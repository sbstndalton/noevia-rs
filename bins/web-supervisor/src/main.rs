//! noevia-web-supervisor: native port of noevia build/web-supervisor.sh (inert: no image starts it yet).
//! `--healthcheck` is the image's HEALTHCHECK probe without Node (src/health.rs);
//! `--image-entrypoint` picks the script or this supervisor by NOEVIA_SUPERVISOR_IMPL (src/entry.rs).
fn main() {
    let first = std::env::args_os().nth(1);
    if first.as_deref().is_some_and(|a| a == "--healthcheck") {
        std::process::exit(web_supervisor::health::run());
    }
    if first.as_deref().is_some_and(|a| a == "--image-entrypoint") {
        std::process::exit(web_supervisor::entry::run());
    }
    std::process::exit(web_supervisor::run());
}
