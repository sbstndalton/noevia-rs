#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

//! One chunker for both native writers. `crates/embedding-worker` already indexes the front's durable
//! source jobs (outbox.rs `chunks`); this crate ports the same `chunkText` for the writes Node's
//! projects.cjs still makes (p1 verifier, noevia#2677 item 1). Both write the same index files, so they
//! must cut every text the same way, or a file indexed by one and re-indexed by the other would keep
//! stale rows (dedupe is by chunk hash). Checked here on Node's recorded edge cases and on a generated
//! corpus whose Node answer is recorded as hashes (tests/generate_chunk_corpus.cjs, same generator).

use project_index::chunk::{chunk_str, chunk_text, units};
use serde_json::Value;
use sha2::{Digest, Sha256};

fn fixture(name: &str) -> Value {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn recorded() -> Vec<(String, String)> {
    let f = fixture("chunk-text.json");
    f["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap_or("?").to_string(),
                c["text"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn both_chunkers_agree_on_nodes_recorded_cases() {
    let cases = recorded();
    assert!(cases.len() >= 10);
    for (name, text) in cases {
        assert_eq!(
            chunk_str(&text),
            embedding_worker::outbox::chunks(&text),
            "case {name}"
        );
    }
}

/// A small deterministic generator (no external crate): texts made of words, sentence and paragraph
/// ends, CRLF, JS white space, surrogate pairs and page markers in and out of place.
struct Gen(u64);

impl Gen {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() as usize) % xs.len()]
    }
    fn text(&mut self) -> String {
        const PIECES: &[&str] = &[
            "qa",
            "notes",
            "sample",
            "word",
            "x",
            "longerwordwithoutspaces",
            " ",
            " ",
            " ",
            ". ",
            "! ",
            "? ",
            ".",
            "\n",
            "\n\n",
            "\r\n",
            "\r\n\r\n",
            "  ",
            "\t",
            "\u{a0}",
            "\u{2028}",
            "\u{3000}",
            "\u{feff}",
            "\u{1f600}",
            "\u{e9}",
            "[Page 2]\n",
            "[Page 10]",
            "[Page ]\n",
            "[Page 7] ",
        ];
        let target = (self.next() % 7000) as usize;
        let mut s = String::new();
        if self.next().is_multiple_of(3) {
            s.push_str(self.pick(&["[Page 1]\n", "[Page 1]", "[Page 12]\n\n", "  [Page 1]\n"]));
        }
        while s.encode_utf16().count() < target {
            s.push_str(self.pick(PIECES));
            if self.next().is_multiple_of(400) {
                let n = self.next() % 30;
                s.push_str(&format!("\n[Page {n}]\n"));
            }
        }
        s
    }
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The recorder's chunk hash: each chunk's UTF-16LE code units, then two zero bytes.
fn chunks_hash(chunks: &[Vec<u16>]) -> String {
    let mut b = Vec::new();
    for c in chunks {
        for u in c {
            b.extend_from_slice(&u.to_le_bytes());
        }
        b.extend_from_slice(&[0, 0]);
    }
    sha(&b)
}

#[test]
fn both_chunkers_match_node_on_the_generated_corpus() {
    let f = fixture("chunk-corpus.json");
    let cases = f["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 600);
    let mut g = Gen(f["seed"].as_u64().unwrap());
    let mut checked = 0;
    for (i, case) in cases.iter().enumerate() {
        let text = g.text();
        assert_eq!(
            sha(text.as_bytes()),
            case["text"].as_str().unwrap(),
            "case {i}: the generator drifted from the recorder's"
        );
        let ours = chunk_text(&units(&text));
        assert_eq!(ours.len() as u64, case["n"].as_u64().unwrap(), "case {i}");
        assert_eq!(
            chunks_hash(&ours),
            case["chunks"].as_str().unwrap(),
            "case {i}"
        );
        let a = chunk_str(&text);
        let b = embedding_worker::outbox::chunks(&text);
        if a != b {
            let first = a.iter().zip(&b).position(|(x, y)| x != y);
            panic!(
                "case {i}: {} vs {} chunks, first difference at {first:?}; text {:?}",
                a.len(),
                b.len(),
                text.chars().take(200).collect::<String>()
            );
        }
        checked += a.len();
    }
    assert!(
        checked > 1500,
        "the corpus holds long texts too ({checked} chunks)"
    );
}
