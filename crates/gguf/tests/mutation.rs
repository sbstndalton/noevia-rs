//! Mutation test: every truncation and thousands of random byte corruptions of the fixture
//! corpus must come back as Ok or Err, never a panic, hang or runaway allocation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::{Duration, Instant};

use gguf::{read_raw_bytes, summarize};

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % (n.max(1) as u64)) as usize
    }
}

fn exercise(label: &str, bytes: &[u8]) {
    let t0 = Instant::now();
    let outcome = std::panic::catch_unwind(|| {
        if let Ok(raw) = read_raw_bytes(bytes) {
            if let Ok(summary) = summarize(&raw) {
                let text = summary.to_string();
                serde_json::from_str::<serde_json::Value>(&text)
                    .unwrap_or_else(|e| panic!("invalid JSON {e}: {text}"));
            }
        }
    });
    assert!(outcome.is_ok(), "panic on {label}");
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "{label} took {:?}",
        t0.elapsed()
    );
}

const INTERESTING: [u64; 8] = [0, 1, 8, 9, 0xffff_ffff, 1 << 40, 1 << 63, u64::MAX];

#[test]
fn every_truncation_of_every_fixture_is_handled() {
    let mut runs = 0;
    for (name, _, bytes) in common::fixtures() {
        for cut in 0..bytes.len() {
            exercise(&format!("{name} cut at {cut}"), &bytes[..cut]);
            runs += 1;
        }
    }
    assert!(runs > 1000);
}

#[test]
fn random_corruptions_of_every_fixture_are_handled() {
    let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
    let mut runs = 0;
    for (name, _, bytes) in common::fixtures() {
        if bytes.is_empty() {
            continue;
        }
        for round in 0..400 {
            let mut m = bytes.clone();
            match rng.below(4) {
                // flip a few random bits
                0 => {
                    for _ in 0..=rng.below(4) {
                        let i = rng.below(m.len());
                        m[i] ^= 1 << rng.below(8);
                    }
                }
                // overwrite a random byte
                1 => {
                    let i = rng.below(m.len());
                    m[i] = rng.next() as u8;
                }
                // plant an extreme little-endian u64/u32 (lengths, counts, types)
                2 => {
                    let v = INTERESTING[rng.below(INTERESTING.len())];
                    let width = if rng.below(2) == 0 { 8 } else { 4 };
                    let i = rng.below(m.len());
                    for (k, b) in v.to_le_bytes().iter().take(width).enumerate() {
                        if let Some(slot) = m.get_mut(i + k) {
                            *slot = *b;
                        }
                    }
                }
                // corrupt and truncate
                _ => {
                    let i = rng.below(m.len());
                    m[i] = m[i].wrapping_add(1 + rng.below(255) as u8);
                    m.truncate(rng.below(m.len() + 1));
                }
            }
            exercise(&format!("{name} round {round}"), &m);
            runs += 1;
        }
    }
    assert!(runs > 20_000);
}
