//! noevia-web-supervisor: native port of noevia build/web-supervisor.sh (inert: no image starts it yet).
fn main() {
    std::process::exit(web_supervisor::run());
}
