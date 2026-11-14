//! Differential check against the Node detector itself, reasons included. Opt-in: set RS_CAPABILITY_NODE_LIB to
//! the absolute path of noevia's deploy/tools/lib/rs-capability.cjs (and have `node` on PATH). Every recorded case
//! and production fixture is mutated deterministically (deleted, duplicated and inserted characters, swapped lines)
//! and both implementations must print the same verdict string and derive the same feature set.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, Stdio};

use rs_capability::{derive_features, detect};
use serde_json::{json, Value};

const HARNESS: &str = r#"
const lib = require(process.env.RS_CAPABILITY_NODE_LIB);
let input = '';
process.stdin.on('data', d => input += d).on('end', () => {
  const out = JSON.parse(input).map(c => c.kind === 'detect'
    ? lib.detect(c.source, c.declaration, c.capability, c.features === null ? null : new Set(c.features))
    : (s => s === null ? null : [...s].sort())(lib.deriveFeatures(c.build, c.cargo)));
  process.stdout.write(JSON.stringify(out));
});
"#;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

const INSERTS: [&str; 22] = [
    "#", "[", "]", "{", "}", "(", ")", "\"", "'", "\\", ";", ",", "&", "::", "r#\"", "/*", "*/",
    "//", "\u{a0}", "\u{feff}", "\r", "\\u{61}",
];

fn mutate(rng: &mut Rng, s: &str) -> String {
    let mut chars: Vec<char> = s.chars().collect();
    for _ in 0..1 + rng.below(3) {
        let at = rng.below(chars.len() + 1);
        match rng.below(4) {
            0 if !chars.is_empty() && at < chars.len() => {
                chars.remove(at);
            }
            1 if at < chars.len() => {
                let c = chars[at];
                chars.insert(at, c);
            }
            _ => {
                let ins = INSERTS[rng.below(INSERTS.len())];
                for (k, c) in ins.chars().enumerate() {
                    chars.insert(at + k, c);
                }
            }
        }
    }
    chars.into_iter().collect()
}

fn mutate_lines(rng: &mut Rng, s: &str) -> String {
    let mut lines: Vec<String> = s.split('\n').map(str::to_string).collect();
    match rng.below(3) {
        0 if lines.len() > 1 => {
            let (a, b) = (rng.below(lines.len()), rng.below(lines.len()));
            lines.swap(a, b);
        }
        1 if !lines.is_empty() => {
            let a = rng.below(lines.len());
            lines.remove(a);
        }
        _ => {
            let a = rng.below(lines.len());
            lines[a] = mutate(rng, &lines[a]);
        }
    }
    lines.join("\n")
}

fn node(batch: &[Value]) -> Vec<Value> {
    let mut child = Command::new("node")
        .args(["-e", HARNESS])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("node on PATH");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(batch).unwrap().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "node harness failed");
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn rust_and_node_agree_on_mutated_inputs() {
    if std::env::var("RS_CAPABILITY_NODE_LIB").map_or(true, |v| v.is_empty()) {
        eprintln!("RS_CAPABILITY_NODE_LIB not set: Node differential check skipped");
        return;
    }
    let rounds: usize = std::env::var("RS_CAPABILITY_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let cases: Vec<Value> = serde_json::from_str(&fixture("cases.json")).unwrap();
    let build = fixture("production.Dockerfile");
    let cargo = fixture("production-Cargo.toml");
    let web = fixture("web.Dockerfile");
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut batch = Vec::new();
    for c in &cases {
        let features = c
            .get("features")
            .cloned()
            .filter(|f| !f.is_null())
            .unwrap_or(Value::Null);
        for round in 0..=rounds {
            let source = c["source"].as_str().unwrap();
            let source = if round == 0 {
                source.to_string()
            } else {
                mutate(&mut rng, source)
            };
            batch.push(
                json!({"kind": "detect", "source": source, "declaration": c["declaration"],
                "capability": c["capability"], "features": features}),
            );
        }
    }
    for round in 0..=rounds * 10 {
        let (b, k) = match round {
            0 => (build.clone(), cargo.clone()),
            1 => (web.clone(), cargo.clone()),
            _ if round % 3 == 0 => (build.clone(), mutate_lines(&mut rng, &cargo)),
            _ if round % 3 == 1 => (mutate_lines(&mut rng, &web), cargo.clone()),
            _ => (mutate_lines(&mut rng, &build), cargo.clone()),
        };
        batch.push(json!({"kind": "derive", "build": b, "cargo": k}));
    }
    let mut differ = Vec::new();
    let mut detects = 0usize;
    let mut derives = 0usize;
    for chunk in batch.chunks(2000) {
        let expected = node(chunk);
        for (input, want) in chunk.iter().zip(expected) {
            let got = if input["kind"] == "detect" {
                detects += 1;
                let features: Option<BTreeSet<String>> = input["features"]
                    .as_array()
                    .map(|a| a.iter().map(|f| f.as_str().unwrap().to_string()).collect());
                Value::String(
                    detect(
                        input["source"].as_str().unwrap(),
                        input["declaration"].as_str().unwrap(),
                        input["capability"].as_str().unwrap(),
                        features.as_ref(),
                    )
                    .to_string(),
                )
            } else {
                derives += 1;
                match derive_features(
                    input["build"].as_str().unwrap(),
                    input["cargo"].as_str().unwrap(),
                ) {
                    None => Value::Null,
                    Some(set) => json!(set.into_iter().collect::<Vec<_>>()),
                }
            };
            if got != want {
                differ.push(format!(
                    "node {want} rust {got} input {}",
                    serde_json::to_string(input).unwrap()
                ));
            }
        }
    }
    assert!(
        differ.is_empty(),
        "{} of {} inputs differ:\n{}",
        differ.len(),
        batch.len(),
        differ
            .iter()
            .take(25)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    println!(
        "Node differential: {detects} detector inputs and {derives} feature derivations agree"
    );
}
