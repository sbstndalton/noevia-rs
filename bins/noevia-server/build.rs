//! Reads contracts/http/routes.toml at build time and writes the route ownership table the server
//! dispatches on (`$OUT_DIR/routes_gen.rs`). A malformed entry fails the build.

use serde::Deserialize;
use std::error::Error;
use std::fmt::Write as _;
use std::path::PathBuf;

#[derive(Deserialize)]
struct Doc {
    route: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    owner: String,
    #[serde(default)]
    methods: Vec<String>,
    #[serde(default)]
    catch_all: bool,
    #[serde(default = "yes")]
    client: bool,
}

fn yes() -> bool {
    true
}

const METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

fn main() -> Result<(), Box<dyn Error>> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let file = manifest.join("../../contracts/http/routes.toml");
    println!("cargo:rerun-if-changed={}", file.display());
    let doc: Doc = toml::from_str(&std::fs::read_to_string(&file)?)?;
    let mut out = String::from("pub static ROUTES: &[Route] = &[\n");
    let mut catch_alls = 0;
    for r in &doc.route {
        let owner = match r.owner.as_str() {
            "rust" => "Owner::Rust",
            "node" => "Owner::Node",
            other => return Err(format!("routes.toml {}: owner {other:?}", r.path).into()),
        };
        if !r.path.starts_with('/') {
            return Err(format!("routes.toml {}: path must start with /", r.path).into());
        }
        if let Some(m) = r.methods.iter().find(|m| !METHODS.contains(&m.as_str())) {
            return Err(format!("routes.toml {}: unknown method {m:?}", r.path).into());
        }
        if r.catch_all {
            catch_alls += 1;
            if r.path.starts_with("/api") {
                return Err(
                    format!("routes.toml {}: catch_all is for non-/api paths", r.path).into(),
                );
            }
        }
        let _ = r.client;
        writeln!(
            out,
            "    Route {{ path: {:?}, owner: {owner}, methods: &{:?}, catch_all: {} }},",
            r.path, r.methods, r.catch_all
        )?;
    }
    if catch_alls > 1 {
        return Err("routes.toml: more than one catch_all route".into());
    }
    out.push_str("];\n");
    let dest = PathBuf::from(std::env::var("OUT_DIR")?).join("routes_gen.rs");
    std::fs::write(dest, out)?;
    Ok(())
}
