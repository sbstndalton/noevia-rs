//! Parity with noevia deploy/tools/test-rs-capability.cjs: every recorded case of fixtures/cases.json, the
//! branch-marker mutation matrix, and the production feature-derivation cases.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;

use rs_capability::{derive_features, detect};
use serde_json::Value;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

struct Case {
    name: String,
    source: String,
    declaration: String,
    capability: String,
    origin: String,
    expected: String,
    features: Option<BTreeSet<String>>,
}

fn cases() -> Vec<Case> {
    let raw: Vec<Value> = serde_json::from_str(&fixture("cases.json")).unwrap();
    raw.iter()
        .map(|c| Case {
            name: c["name"].as_str().unwrap().to_string(),
            source: c["source"].as_str().unwrap().to_string(),
            declaration: c["declaration"].as_str().unwrap().to_string(),
            capability: c["capability"].as_str().unwrap().to_string(),
            origin: c["origin"].as_str().unwrap().to_string(),
            expected: c["expected"].as_str().unwrap().to_string(),
            features: c
                .get("features")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(|f| f.as_str().unwrap().to_string()).collect()),
        })
        .collect()
}

#[test]
fn recorded_cases_and_branch_matrix() {
    let mut all = cases();
    let recorded = all.len();
    assert_eq!(
        recorded, 448,
        "fixture size changed: re-copy it from noevia and update this count"
    );
    type Row = (String, String, String, String, Option<BTreeSet<String>>);
    let matrix: Vec<Row> = all
        .iter()
        .filter(|c| {
            c.origin == "new" && c.name != "new:static-lifetime" && c.name != "new:char-lifetime"
        })
        .map(|c| {
            (
                c.name.clone(),
                c.source.clone(),
                c.declaration.clone(),
                c.expected.clone(),
                c.features.clone(),
            )
        })
        .collect();
    let markers: Vec<String> = serde_json::from_str(&fixture("branch-markers.json")).unwrap();
    for marker in &markers {
        let first = marker.chars().next().unwrap();
        let hex = format!("{:x}", first as u32);
        let tail: String = marker.chars().skip(1).collect();
        for (name, source, declaration, expected, features) in &matrix {
            let source = source
                .replace("cap", marker)
                .replace("c\\x61p", &format!("\\x{hex}{tail}"))
                .replace("c\\u{61}p", &format!("\\u{{{hex}}}{tail}"));
            all.push(Case {
                name: format!("branch-matrix:{marker}:{name}"),
                source,
                declaration: declaration.clone(),
                capability: marker.clone(),
                origin: "matrix".to_string(),
                expected: expected.clone(),
                features: features.clone(),
            });
        }
    }
    let mut failures = Vec::new();
    for c in &all {
        let got = detect(
            &c.source,
            &c.declaration,
            &c.capability,
            c.features.as_ref(),
        );
        if got.class() != c.expected {
            failures.push(format!(
                "{} expected {} got {got} source {:?}",
                c.name, c.expected, c.source
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        all.len(),
        failures.join("\n")
    );
    // noevia#2712: the whole matrix, not only the recorded cases.
    assert_eq!(all.len(), 4244, "448 recorded + 3,796 matrix cases");
    println!(
        "Rust capability fixtures: {} passed ({recorded} recorded, {} matrix)",
        all.len(),
        all.len() - recorded
    );
}

fn has(set: &Option<BTreeSet<String>>, f: &str) -> bool {
    set.as_ref().is_some_and(|s| s.contains(f))
}

#[test]
fn production_feature_derivation() {
    let build = fixture("production.Dockerfile");
    let cargo = fixture("production-Cargo.toml");
    let derived = derive_features(&build, &cargo);
    let no_default = build.replacen(
        "cargo build --release",
        "cargo build --no-default-features --release",
        1,
    );
    let mut checks: Vec<(&str, bool)> = vec![
        (
            "actual production features",
            has(&derived, "rust-storage-reads")
                && has(&derived, "rust-project-chats")
                && derived.is_some()
                && !has(&derived, "absent"),
        ),
        (
            "current production build",
            has(
                &derive_features(&fixture("web.Dockerfile"), &cargo),
                "rust-storage-reads",
            ),
        ),
        (
            "no default flag excludes default",
            derive_features(&no_default, &cargo).is_some_and(|s| !s.contains("default")),
        ),
        (
            "all features flag refuses",
            derive_features(
                &build.replacen(
                    "cargo build --release",
                    "cargo build --all-features --release",
                    1,
                ),
                &cargo,
            )
            .is_none(),
        ),
        (
            "unknown selector refuses",
            derive_features(
                &build.replacen("features=rust-reasoning-settings", "features=$CUSTOM", 1),
                &cargo,
            )
            .is_none(),
        ),
        (
            "extra assignment refuses",
            derive_features(
                &build.replacen(
                    "cargo build --release",
                    "features=other; cargo build --release",
                    1,
                ),
                &cargo,
            )
            .is_none(),
        ),
        (
            "extra env refuses",
            derive_features(
                &build.replace(
                    "ENV RUSTUP_TOOLCHAIN",
                    "ENV CARGO_ENCODED_RUSTFLAGS=custom\nENV RUSTUP_TOOLCHAIN",
                ),
                &cargo,
            )
            .is_none(),
        ),
        (
            "unknown build flags refuse",
            derive_features(
                &build.replacen("--locked -p", "--locked --config other -p", 1),
                &cargo,
            )
            .is_none(),
        ),
        (
            "duplicate features table refuses",
            derive_features(&build, &format!("{cargo}\n[features]\ndefault=[]\n")).is_none(),
        ),
        (
            "malformed manifest refuses",
            derive_features(&build, "[features]\ndefault = [broken]").is_none(),
        ),
        (
            "missing build refuses",
            derive_features("", &cargo).is_none(),
        ),
        (
            "missing manifest refuses",
            derive_features(&build, "").is_none(),
        ),
    ];
    let graph = "[features]\ndefault = [\"via-default\"]\nvia-default = [\"transitive\"]\ntransitive = []\nrust-reasoning-settings=[]\nrust-routing-mode=[]\n";
    let transitive = derive_features(&build, graph);
    checks.push(("default feature closure", has(&transitive, "transitive")));
    let disabled = derive_features(&build, &graph.replacen("[\"via-default\"]", "[]", 1));
    checks.push(("disabled default feature", !has(&disabled, "transitive")));
    let src = "pub const FEATURES: &[&str] = &[#[cfg(feature = \"transitive\")] \"cap\"];";
    checks.push((
        "derived default queried cfg",
        detect(src, "FEATURES", "cap", transitive.as_ref()).to_string() == "v1",
    ));
    checks.push((
        "underivable queried cfg",
        detect(src, "FEATURES", "cap", derive_features("", &cargo).as_ref()).to_string()
            == "invalid(production-features-underivable)",
    ));
    let nd = derive_features(&no_default, graph);
    checks.push((
        "no-default excludes default transitive closure",
        nd.is_some() && !has(&nd, "default") && !has(&nd, "transitive"),
    ));
    let minimal = "[features]\nrust-reasoning-settings=[]\nrust-routing-mode=[]\n";
    let default_src = "pub const FEATURES: &[&str] = &[#[cfg(feature = \"default\")] \"cap\"];";
    let mut owned: Vec<(String, bool)> = Vec::new();
    for manifest in [minimal.to_string(), format!("{minimal}default=[]\n")] {
        let set = derive_features(&build, &manifest);
        let declared = manifest.contains("default=");
        owned.push((
            format!("default declaration presence {}", manifest.len()),
            set.is_some() && has(&set, "default") == declared,
        ));
        let want = if declared { "v1" } else { "legacy" };
        owned.push((
            format!("queried default declaration {}", manifest.len()),
            detect(default_src, "FEATURES", "cap", set.as_ref()).to_string() == want,
        ));
    }
    checks.push((
        "undeclared explicit feature refuses",
        derive_features(
            &build.replacen(
                "features=rust-reasoning-settings,",
                "features=undeclared-feature,",
                1,
            ),
            minimal,
        )
        .is_none(),
    ));
    let failed: Vec<String> = checks
        .iter()
        .map(|(n, ok)| (n.to_string(), *ok))
        .chain(owned)
        .filter(|(_, ok)| !ok)
        .map(|(n, _)| n)
        .collect();
    assert!(failed.is_empty(), "feature derivation: {failed:?}");
    assert_eq!(checks.len() + 4, 22, "the 22 derivation cases");
    println!("Production feature derivation: {} passed", checks.len() + 4);
}
