//! Parity with noevia deploy/tools/test-rs-feature-proof.cjs: the probe classes of the production feature-proof
//! review (extra producers, output rewrites, unproven copies, stage-chain tricks, heredoc data).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use rs_capability::{derive_features, detect, dockerfile_instructions};

const MINIMAL: &str = "[features]\nrust-reasoning-settings=[]\nrust-routing-mode=[]\n";
const BUILD: &str = "cargo build --release --locked -p noevia-server --features rust-reasoning-settings,rust-routing-mode";

fn cargo() -> String {
    format!("{MINIMAL}default=[\"default-only\"]\ndefault-only=[]\n")
}

struct Df<'a> {
    build: &'a str,
    extra: &'a str,
    runtime_extra: &'a str,
    copy: &'a str,
    tail: &'a str,
}

impl Default for Df<'_> {
    fn default() -> Self {
        Df { build: BUILD, extra: "", runtime_extra: "", copy: "COPY --from=front /out/ /usr/local/bin/", tail: "FROM runtime AS production" }
    }
}

fn df(d: Df<'_>) -> String {
    [
        "FROM rust:1.99-slim-bookworm AS front",
        "ENV RUSTUP_TOOLCHAIN=1.99.0 CARGO_TERM_COLOR=never",
        "COPY rs/ /src/",
        "WORKDIR /src",
        &format!("RUN mkdir -p /out && {} && cp target/release/noevia-server /out/noevia-server", d.build),
        d.extra,
        "FROM node:22-bookworm AS runtime",
        "WORKDIR /app",
        d.copy,
        d.runtime_extra,
        d.tail,
        "",
    ]
    .join("\n")
}

enum Want<'a> {
    Refuse,
    Has { yes: &'a [&'a str], no: &'a [&'a str] },
}

fn check(failures: &mut Vec<String>, count: &mut usize, name: &str, text: &str, want: Want<'_>, manifest: &str) {
    let got = derive_features(text, manifest);
    let ok = match (&want, &got) {
        (Want::Refuse, None) => true,
        (Want::Has { yes, no }, Some(set)) => yes.iter().all(|f| set.contains(*f)) && no.iter().all(|f| !set.contains(*f)),
        _ => false,
    };
    if !ok {
        failures.push(format!("{name}: got {got:?}"));
    }
    *count += 1;
}

#[test]
fn shipped_binary_feature_proof() {
    let cargo = cargo();
    let actual = std::fs::read_to_string(format!("{}/fixtures/production.Dockerfile", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let mut f = Vec::new();
    let mut n = 0usize;
    let c = cargo.as_str();
    check(&mut f, &mut n, "plain literal build", &df(Df::default()), Want::Has { yes: &["rust-reasoning-settings", "default-only"], no: &[] }, c);
    check(&mut f, &mut n, "active comment/continuation",
        &df(Df { build: "cargo  build --release \\\n # --features imaginary\n -p noevia-server \\\n --features rust-routing-mode", ..Df::default() }),
        Want::Has { yes: &["rust-routing-mode", "default-only"], no: &["imaginary", "rust-reasoning-settings"] }, c);
    check(&mut f, &mut n, "no-feature build uses only defaults", &df(Df { build: "cargo build --release -p noevia-server", ..Df::default() }),
        Want::Has { yes: &["default-only"], no: &["rust-routing-mode"] }, c);
    check(&mut f, &mut n, "no defaults uses only listed features",
        &df(Df { build: "cargo build --release --package=noevia-server --bin noevia-server -F noevia-server/rust-routing-mode --no-default-features", ..Df::default() }),
        Want::Has { yes: &["rust-routing-mode"], no: &["default", "default-only", "rust-reasoning-settings"] }, c);
    check(&mut f, &mut n, "absent default never invented", &df(Df::default()), Want::Has { yes: &[], no: &["default"] }, MINIMAL);
    check(&mut f, &mut n, "explicit empty default exists", &df(Df::default()), Want::Has { yes: &["default"], no: &["default-only"] }, &format!("{MINIMAL}default=[]\n"));
    for extra in [
        "RUN cargo build --release -p noevia-server --no-default-features",
        "RUN cargo  build --release -p noevia-server --features rust-routing-mode",
        "RUN cargo\tbuild --release -p noevia-server",
        "RUN cargo build --release",
        "RUN cargo install --path bins/noevia-server",
        "COPY patched /src/target/release/noevia-server",
        "COPY rogue /out/",
        "RUN rm /out/noevia-server",
        "SHELL [\"/bin/bash\",\"-c\"]",
        "ONBUILD RUN true",
    ] {
        check(&mut f, &mut n, &format!("extra producer or rewrite {extra}"), &df(Df { extra, ..Df::default() }), Want::Refuse, c);
    }
    let builds = [
        format!("{BUILD}; cargo  build --release -p noevia-server --no-default-features"),
        format!("{BUILD} && cargo\tbuild --release -p noevia-server"),
        format!("{BUILD} && cp /legacy/noevia-server target/release/noevia-server"),
        "cargo build --release -p noevia-server --features ${FRONT_FEATURES}".to_string(),
        "cargo build --release -p noevia-server --features rust-routing-mode,${FRONT_FEATURES}".to_string(),
        "cargo build --release -p noevia-server --features \"rust-routing-mode,$FRONT_FEATURES\"".to_string(),
        "cargo build --release -p noevia-server --features `echo rust-routing-mode`".to_string(),
        "cargo build --release -p noevia-server --features rust-*".to_string(),
        "cargo build --release=true -p noevia-server".to_string(),
        "cargo build --release -p noevia-server --no-default-features=true".to_string(),
        "cargo build --release -p noevia-server --target x86_64-unknown-linux-musl".to_string(),
        "cargo build --release --workspace".to_string(),
        "cargo build --release -p noevia-server --all-features".to_string(),
        "cargo +nightly build --release -p noevia-server".to_string(),
        "cargo xtask build".to_string(),
        "cargo build --release".to_string(),
        "cargo install --path bins/noevia-server".to_string(),
        format!("sh -c \"{BUILD}\""),
        "make build".to_string(),
        format!("false || {BUILD}"),
        format!("({BUILD})"),
        format!("if true; then {BUILD}; fi"),
        format!("{BUILD} || true"),
        format!("{BUILD} &"),
        format!("true; {BUILD}"),
        format!("{BUILD} > target/release/noevia-server"),
    ];
    for build in &builds {
        check(&mut f, &mut n, &format!("unsupported build {build}"), &df(Df { build, ..Df::default() }), Want::Refuse, c);
    }
    for runtime_extra in [
        "COPY --from=front /out/ /usr/local/bin/",
        "ADD rogue /usr/local/bin/",
        "COPY rogue /usr/local/",
        "RUN cp /bin/true /usr/local/bin/noevia-server",
        "RUN ln -sf /bin/true /usr/local/bin/noevia-server",
        "ONBUILD COPY x y",
        "SHELL [\"/bin/sh\",\"-c\"]",
    ] {
        check(&mut f, &mut n, &format!("runtime overwrite {runtime_extra}"), &df(Df { runtime_extra, ..Df::default() }), Want::Refuse, c);
    }
    for copy in [
        "COPY --from=legacy-builder /out/ /usr/local/bin/",
        "COPY --from=0 /out/ /usr/local/bin/",
        "COPY /out/noevia-server /usr/local/bin/noevia-server",
        "COPY --from=front /legacy/noevia-server /usr/local/bin/noevia-server",
        "COPY --from=front /out/other /usr/local/bin/noevia-server",
        "COPY --from=${BUILDER} /out/ /usr/local/bin/",
    ] {
        check(&mut f, &mut n, &format!("unproven copy {copy}"), &df(Df { copy, ..Df::default() }), Want::Refuse, c);
    }
    check(&mut f, &mut n, "final stage does not inherit runtime", &df(Df { tail: "FROM node:22-bookworm AS production", ..Df::default() }), Want::Refuse, c);
    check(&mut f, &mut n, "unresolved final base", &df(Df { tail: "FROM ${RUNTIME} AS production", ..Df::default() }), Want::Refuse, c);
    check(&mut f, &mut n, "unknown front base",
        &df(Df::default()).replacen("FROM rust:1.99-slim-bookworm AS front", "FROM ${FRONT_BASE} AS front", 1), Want::Refuse, c);
    check(&mut f, &mut n, "unresolved platform", &df(Df::default()).replacen("FROM rust:", "FROM --platform=$PLATFORM rust:", 1), Want::Refuse, c);
    check(&mut f, &mut n, "renamed source", &df(Df::default()).replacen("AS front", "AS renamed", 1), Want::Refuse, c);
    check(&mut f, &mut n, "escape directive", &format!("# escape=`\n{}", df(Df::default())), Want::Refuse, c);
    check(&mut f, &mut n, "unfinished continuation", &format!("{}RUN true \\", df(Df::default())), Want::Refuse, c);
    check(&mut f, &mut n, "ancestor rebuild",
        &df(Df::default())
            .replacen("FROM node:22-bookworm AS runtime", "FROM front AS copied\nRUN cargo build --release -p noevia-server\nFROM node:22-bookworm AS runtime", 1)
            .replacen("COPY --from=front", "COPY --from=copied", 1),
        Want::Refuse, c);
    check(&mut f, &mut n, "inherited producer without rebuild",
        &df(Df::default())
            .replacen("FROM node:22-bookworm AS runtime", "FROM front AS copied\nFROM node:22-bookworm AS runtime", 1)
            .replacen("COPY --from=front", "COPY --from=copied", 1),
        Want::Has { yes: &["rust-routing-mode"], no: &[] }, c);
    check(&mut f, &mut n, "commented enabled build is not evidence",
        &df(Df { build: "cargo build --release -p noevia-server --no-default-features", ..Df::default() })
            .replacen("RUN mkdir", &format!("# RUN {BUILD}\nRUN mkdir"), 1),
        Want::Has { yes: &[], no: &["rust-routing-mode", "default"] }, c);
    let unused = Df {
        copy: "COPY --from=legacy-builder /out/ /usr/local/bin/",
        tail: "FROM node:22-bookworm AS unused\nCOPY --from=front /out/ /usr/local/bin/\nFROM runtime AS production",
        ..Df::default()
    };
    check(&mut f, &mut n, "unused valid copy does not prove runtime", &df(unused), Want::Refuse, c);
    check(&mut f, &mut n, "actual output replacement",
        &actual.replacen("cp target/release/noevia-server /out/noevia-server;", "cp /legacy/noevia-server /out/noevia-server;", 1), Want::Refuse, MINIMAL);
    check(&mut f, &mut n, "actual whitespace second build",
        &actual.replacen("cp target/release/noevia-server", "cargo  build --release --locked -p noevia-server --features rust-routing-mode; cp target/release/noevia-server", 1),
        Want::Refuse, MINIMAL);
    check(&mut f, &mut n, "actual runtime legacy copy while build-stage copy survives",
        &actual.replacen("COPY --from=front /out/ /usr/local/bin/", "COPY --from=legacy-builder /out/ /usr/local/bin/", 1), Want::Refuse, MINIMAL);
    let legacy_stage = [
        "FROM rust:1.99-slim-bookworm AS legacy-builder",
        "ENV RUSTUP_TOOLCHAIN=1.99.0 CARGO_TERM_COLOR=never",
        "COPY rs/ /src/",
        "WORKDIR /src",
        "RUN mkdir -p /out && cargo build --release -p noevia-server --no-default-features --features rust-routing-mode && cp target/release/noevia-server /out/noevia-server",
        "",
    ]
    .join("\n");
    let alternate = Df {
        copy: "COPY --from=legacy-builder /out/ /usr/local/bin/",
        tail: "FROM node:22-bookworm AS unused\nCOPY --from=front /out/ /usr/local/bin/\nFROM runtime AS production",
        ..Df::default()
    };
    check(&mut f, &mut n, "trace a real alternate producer rather than unused evidence",
        &df(alternate).replacen("FROM node:22-bookworm AS runtime", &format!("{legacy_stage}FROM node:22-bookworm AS runtime"), 1),
        Want::Has { yes: &["rust-routing-mode"], no: &["rust-reasoning-settings", "default-only"] }, c);
    let ins = dockerfile_instructions("# RUN bogus\nRUN cargo \\\n# continued comment\n build").unwrap();
    assert_eq!(ins[0].1, "cargo  build");
    let absent_default = derive_features(&df(Df::default()), MINIMAL);
    assert_eq!(detect("pub const FEATURES: &[&str] = &[#[cfg(feature = \"default\")] \"cap\"];", "FEATURES", "cap", absent_default.as_ref()).to_string(), "legacy");
    let heredocs = [
        df(Df { copy: "COPY <<'EOF' /tmp/example\nCOPY --from=front /out/ /usr/local/bin/\nEOF", ..Df::default() }),
        df(Df { copy: "RUN cat <<'EOF' > /tmp/example\nCOPY --from=front /out/ /usr/local/bin/\nEOF", ..Df::default() }),
        df(Df { copy: "ADD <<'EOF' /tmp/example\nCOPY --from=front /out/ /usr/local/bin/\nEOF", ..Df::default() }),
        format!("FROM debian AS unrelated\nRUN cat <<'EOF' > /tmp/example\nFROM scratch AS fake\nCOPY --from=front /out/ /usr/local/bin/\nEOF\n{}", df(Df::default())),
    ];
    for text in &heredocs {
        check(&mut f, &mut n, "heredoc data is never an active binary copy", text, Want::Refuse, c);
        let got = detect("pub const FEATURES: &[&str] = &[#[cfg(feature = \"rust-routing-mode\")] \"cap\"];", "FEATURES", "cap", derive_features(text, c).as_ref());
        assert_eq!(got.to_string(), "invalid(production-features-underivable)");
    }
    assert!(f.is_empty(), "{} of {n} probes differ:\n{}", f.len(), f.join("\n"));
    println!("Shipped binary feature proof: {} passed", n + 2);
}
