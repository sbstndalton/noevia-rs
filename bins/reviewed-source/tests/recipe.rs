//! The image recipe (noevia-core `code-sandbox/Dockerfile`): with `REVIEWED_SOURCE_DOCKERFILE` set, check that
//! the native source check is opt-in, uses this binary, and pins the same verifier source as the default.
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

fn stage<'a>(dockerfile: &'a str, name: &str) -> &'a str {
    let start = dockerfile
        .find(&format!(" AS {name}\n"))
        .unwrap_or_else(|| panic!("stage {name}"));
    let rest = &dockerfile[start..];
    let end = rest[1..].find("\nFROM ").map_or(rest.len(), |at| at + 1);
    &rest[..end]
}

fn arg<'a>(text: &'a str, name: &str) -> &'a str {
    let key = format!("ARG {name}=");
    let line = text
        .lines()
        .find(|line| line.starts_with(&key))
        .unwrap_or_else(|| panic!("{name}"));
    &line[key.len()..]
}

#[test]
fn recipe_is_opt_in_and_pins_the_same_source() {
    let Ok(path) = std::env::var("REVIEWED_SOURCE_DOCKERFILE") else {
        println!("skipped: REVIEWED_SOURCE_DOCKERFILE not set");
        return;
    };
    let dockerfile = std::fs::read_to_string(path).unwrap();
    let head = &dockerfile[..dockerfile.find("\nFROM ").unwrap()];
    assert_eq!(
        arg(head, "NATIVE_VERIFIER_SOURCE_CHECK"),
        "python",
        "the default stays the reference"
    );
    assert!(dockerfile.contains(
        "\nFROM native-verifier-src-${NATIVE_VERIFIER_SOURCE_CHECK} AS native-verifier-src\n"
    ));
    assert!(dockerfile.contains("COPY --from=native-verifier-src /verifier-rs /src"));
    let python = stage(&dockerfile, "native-verifier-src-python");
    let rust = stage(&dockerfile, "native-verifier-src-rust");
    let fetch = stage(&dockerfile, "reviewed-source-src");
    for name in ["NATIVE_VERIFIER_RS_REF", "NATIVE_VERIFIER_SOURCE_SHA256"] {
        assert_eq!(
            arg(python, name),
            arg(rust, name),
            "{name} differs between the python and rust stages"
        );
    }
    assert_eq!(
        arg(python, "NATIVE_VERIFIER_RS_REF"),
        arg(fetch, "NATIVE_VERIFIER_RS_REF")
    );
    assert!(reviewed_source::is_ref(arg(
        fetch,
        "REVIEWED_SOURCE_RS_REF"
    )));
    assert!(reviewed_source::is_digest(arg(
        fetch,
        "REVIEWED_SOURCE_RS_SHA256"
    )));
    assert!(
        fetch.contains("sha256sum -c -"),
        "the checker's own source is sha256-checked"
    );
    assert!(rust.contains("cargo build --locked --release -p reviewed-source"));
    assert!(rust.contains(
        "target/release/reviewed-source extract-ref /verifier-rs.tgz \"$NATIVE_VERIFIER_RS_REF\" \"$NATIVE_VERIFIER_SOURCE_SHA256\" /verifier-rs"
    ));
    assert!(
        !rust.contains("python"),
        "the native stage has no interpreter step"
    );
}
