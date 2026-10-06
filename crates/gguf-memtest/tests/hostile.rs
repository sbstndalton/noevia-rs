//! Port of noevia services/model-manager/tests/test_gguf_hostile_input.py (#869, #870, #877):
//! hostile or corrupt metadata degrades to `_error`, never to memory exhaustion or a hang.
//! Synthetic bytes only.
//!
//! The counting allocator below is the only `unsafe` in the workspace. It lives in this
//! test-only crate so the shipped crates can keep `#![forbid(unsafe_code)]`.
#![allow(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[path = "../../gguf/tests/common/mod.rs"]
mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::*;
use gguf::{
    read_raw, read_raw_bytes, summarize, Raw, Value, MAX_KV_COUNT, MAX_RETAINED_VALUES,
    MAX_STRING_LEN,
};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwards to the system allocator unchanged.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwards to the system allocator unchanged.
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Serialises the tests in this file so one test's allocations don't count toward another's.
static SERIAL: Mutex<()> = Mutex::new(());

fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let out = f();
    let peak = PEAK.load(Ordering::SeqCst).saturating_sub(base);
    (out, peak)
}

fn err_of(raw: &Raw) -> &str {
    match raw.get("_error") {
        Some(Value::Str(s)) => s,
        other => panic!("expected a string _error, got {other:?}"),
    }
}

#[test]
fn huge_string_length_in_a_local_file_sets_error_without_reading_it() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    for n in [1u64 << 33, 1 << 40, 1 << 63, u64::MAX] {
        let p = dir.join(format!("evil-{n}.gguf"));
        std::fs::write(
            &p,
            cat(&[
                &header(2, 0),
                &s("general.architecture"),
                &u32le(STRING),
                &u64le(n),
                &[b'x'; 64],
            ]),
        )
        .unwrap();
        let (raw, peak) = peak_of(|| read_raw(&p).unwrap());
        assert!(raw.contains_key("_error"), "n={n}");
        assert!(peak < MB2, "n={n} peak={peak}");
        std::fs::remove_file(&p).unwrap();
    }
}

#[test]
fn huge_array_count_is_not_walked_or_seeked_past_the_end() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    for count in [1u64 << 40, 1 << 63, u64::MAX] {
        for subtype in [U32, STRING] {
            let tail = if subtype == STRING {
                vec![0u8; 8 * 4]
            } else {
                vec![0u8; 4 * 16]
            };
            let buf = cat(&[
                &header(2, 0),
                &s("tokenizer.ggml.tokens"),
                &u32le(ARRAY),
                &iq(subtype, count),
                &tail,
            ]);
            let t0 = Instant::now();
            let (raw, peak) = peak_of(|| read_raw_bytes(&buf).unwrap());
            assert!(t0.elapsed() < Duration::from_secs(1));
            assert!(peak < MB2, "count={count} subtype={subtype} peak={peak}");
            assert!(raw.contains_key("_error"));
        }
    }
}

#[test]
fn deeply_nested_arrays_do_not_raise_recursion_error() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut body = Vec::new();
    for _ in 0..5000 {
        body.extend_from_slice(&iq(ARRAY, 1));
    }
    let buf = cat(&[&header(1, 0), &s("evil"), &u32le(ARRAY), &body]);
    let raw = read_raw_bytes(&buf).unwrap();
    assert!(err_of(&raw).contains("nested"));
}

#[test]
fn one_level_of_nested_arrays_is_still_accepted() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let inner = cat(&[&iq(U32, 2), &u32le(7), &u32le(8)]);
    let buf = cat(&[&header(1, 0), &s("k"), &u32le(ARRAY), &iq(ARRAY, 1), &inner]);
    let raw = read_raw_bytes(&buf).unwrap();
    assert_eq!(
        raw.get("k"),
        Some(&Value::List(vec![Value::List(vec![
            Value::Int(7),
            Value::Int(8)
        ])]))
    );
    assert!(!raw.contains_key("_error"));
}

#[test]
fn implausible_kv_count_is_refused_up_front() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let buf = header(u64::MAX, 0);
    let (raw, peak) = peak_of(|| read_raw_bytes(&buf).unwrap());
    assert!(err_of(&raw).contains("implausible kv_count"));
    assert!(peak < MB2);
}

#[test]
fn truncated_fixed_header_is_a_meta_error_not_a_struct_error() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let e = read_raw_bytes(b"GGUF\x03\x00").unwrap_err();
    assert_eq!(
        e.0,
        "header is truncated or declares a length past the end of the data"
    );
}

#[test]
fn oversized_but_present_string_is_truncated_and_parsing_continues() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let big = vec![b'a'; MAX_STRING_LEN as usize + 5000];
    let buf = cat(&[
        &header(2, 0),
        &s("tokenizer.chat_template"),
        &u32le(STRING),
        &u64le(big.len() as u64),
        &big,
        &kv_u32("llama.block_count", 32),
    ]);
    let raw = read_raw_bytes(&buf).unwrap();
    let Some(Value::Str(t)) = raw.get("tokenizer.chat_template") else {
        panic!("template missing")
    };
    assert!(t.ends_with("…[truncated]"));
    assert!((t.chars().count() as u64) < MAX_STRING_LEN + 50);
    assert_eq!(raw.get("llama.block_count"), Some(&Value::Int(32)));
    assert!(!raw.contains_key("_error"));
}

#[test]
fn a_range_cut_tokenizer_array_keeps_its_count_for_vocab_size() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let strings: Vec<u8> = (0..20).flat_map(|i| s(&format!("t{i}"))).collect();
    let buf = cat(&[
        &header(3, 0),
        &kv_u32("llama.block_count", 32),
        &s("tokenizer.ggml.tokens"),
        &u32le(ARRAY),
        &iq(STRING, 150_000),
        &strings,
    ]);
    let raw = read_raw_bytes(&buf).unwrap();
    assert_eq!(raw.get("llama.block_count"), Some(&Value::Int(32)));
    match raw.get("tokenizer.ggml.tokens") {
        Some(Value::ArraySummary { count, .. }) => assert_eq!(*count, 150_000),
        other => panic!("{other:?}"),
    }
    assert!(raw.contains_key("_error"));
    let mut raw_with_arch = raw.clone();
    raw_with_arch.insert("general.architecture".into(), Value::Str("llama".into()));
    let summary = summarize(&raw_with_arch).unwrap().to_string();
    assert!(summary.contains("\"vocab_size\":150000"), "{summary}");
    // and without an architecture the count still wins
    let summary = summarize(&raw).unwrap().to_string();
    assert!(summary.contains("\"vocab_size\":150000"), "{summary}");
}

#[test]
fn error_surfaces_in_the_summary() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let raw = read_raw_bytes(&cat(&[&header(5, 0), &kv_u32("a", 1)])).unwrap();
    let summary = summarize(&raw).unwrap();
    let he = summary.get("general").and_then(|g| g.get("header_error"));
    assert!(matches!(he, Some(gguf::Json::Str(s)) if !s.is_empty()));
}

// --- beyond the Python suite: the remaining #877 caps --------------------------------------

#[test]
fn retained_string_data_is_capped() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // Values of 200,000 characters: keys count too, so the 80th value crosses
    // MAX_RETAINED_CHARS (16,000,000).
    let n = 81u64;
    let val = vec![b'a'; 200_000];
    let mut buf = header(n, 0);
    for i in 0..n {
        buf.extend_from_slice(&s(&format!("k{i}")));
        buf.extend_from_slice(&u32le(STRING));
        buf.extend_from_slice(&u64le(val.len() as u64));
        buf.extend_from_slice(&val);
    }
    let raw = read_raw_bytes(&buf).unwrap();
    assert_eq!(
        err_of(&raw),
        "stopped at KV read: header holds implausibly much string data"
    );
    assert!(raw.contains_key("k78") && !raw.contains_key("k79"));
}

#[test]
fn oversized_key_is_truncated_at_max_key_len() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let key = "k".repeat(70_000);
    let buf = cat(&[&header(1, 0), &kv_u32(&key, 3)]);
    let raw = read_raw_bytes(&buf).unwrap();
    let want = format!("{}…[truncated]", "k".repeat(65_535));
    assert_eq!(raw.get(&want), Some(&Value::Int(3)));
}

/// A synthetic header followed by `len - head.len()` zero bytes, never held in memory.
struct HeadThenZeros {
    head: Vec<u8>,
    len: u64,
    pos: u64,
}

impl std::io::Read for HeadThenZeros {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = (self.len.saturating_sub(self.pos)).min(buf.len() as u64) as usize;
        for (i, b) in buf[..n].iter_mut().enumerate() {
            *b = self.head.get((self.pos as usize) + i).copied().unwrap_or(0);
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl std::io::Seek for HeadThenZeros {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.pos = match pos {
            std::io::SeekFrom::Start(p) => p,
            std::io::SeekFrom::End(d) => self.len.saturating_add_signed(d),
            std::io::SeekFrom::Current(d) => self.pos.saturating_add_signed(d),
        };
        Ok(self.pos)
    }
}

#[test]
fn total_bytes_read_is_capped_for_endless_empty_strings() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // A string array declaring 2**40 elements, then 1 GiB of zeros (each zero u64 is one more
    // empty string). Skipping them reads 8 bytes apiece; the read cap stops the walk well
    // before the end of the data.
    let head = cat(&[
        &header(1, 0),
        &s("tokenizer.ggml.tokens"),
        &u32le(ARRAY),
        &iq(STRING, 1 << 40),
    ]);
    let stream = HeadThenZeros {
        head,
        len: 1 << 30,
        pos: 0,
    };
    let t0 = Instant::now();
    let (raw, peak) = peak_of(|| gguf::read_raw_stream(stream).unwrap());
    assert!(err_of(&raw).contains("read limit"), "{raw:?}");
    assert!(peak < MB2);
    eprintln!("read-cap walk took {:?}", t0.elapsed());
}

/// Peak-heap budget for the worst shapes below; DaServer has no swap (noevia#697).
const MB64: usize = 64 * 1024 * 1024;

#[test]
fn many_kv_pairs_of_nested_byte_lists_stay_under_the_value_cap() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // MAX_KV_COUNT pairs, each a list of 8 lists of 8 u8: 73 values per pair, 7.3M in all.
    // Without a cap on retained values this builds several hundred MB.
    let mut inner = iq(0, 8); // u8 subtype, 8 elements
    inner.extend_from_slice(&[7u8; 8]);
    let mut value = u32le(ARRAY);
    value.extend_from_slice(&iq(ARRAY, 8));
    for _ in 0..8 {
        value.extend_from_slice(&inner);
    }
    let mut buf = header(MAX_KV_COUNT, 0);
    for i in 0..MAX_KV_COUNT {
        buf.extend_from_slice(&s(&format!("k{i:06}")));
        buf.extend_from_slice(&value);
    }
    let t0 = Instant::now();
    let (raw, peak) = peak_of(|| read_raw_bytes(&buf).unwrap());
    let elapsed = t0.elapsed();
    let kept = raw.len();
    let (summary, summary_peak) = peak_of(|| summarize(&raw).unwrap().to_string());
    drop(raw);
    eprintln!(
        "worst-case nested lists: {} input bytes, {kept} keys kept, peak heap {} bytes \
         ({:.1} MiB) in {elapsed:?}; summarize peak {} bytes",
        buf.len(),
        peak,
        peak as f64 / 1048576.0,
        summary_peak
    );
    assert!(
        summary.contains("header holds implausibly many values"),
        "{summary}"
    );
    // ~MAX_RETAINED_VALUES / 73 pairs fit before the cap.
    assert!(kept < (MAX_RETAINED_VALUES / 73) as usize + 10);
    assert!(peak < MB64, "peak {peak}");
    assert!(elapsed < Duration::from_secs(5));
}

#[test]
fn many_kv_pairs_of_byte_array_summaries_stay_under_the_value_cap() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // MAX_KV_COUNT pairs, each a u8 array of 9 (summarised: 1 summary + 8 sample values).
    let mut value = u32le(ARRAY);
    value.extend_from_slice(&iq(0, 9));
    value.extend_from_slice(&[1u8; 9]);
    let mut buf = header(MAX_KV_COUNT, 0);
    for i in 0..MAX_KV_COUNT {
        buf.extend_from_slice(&s(&format!("k{i:06}")));
        buf.extend_from_slice(&value);
    }
    let (raw, peak) = peak_of(|| read_raw_bytes(&buf).unwrap());
    eprintln!(
        "worst-case array summaries: peak heap {peak} bytes ({:.1} MiB), {} keys kept",
        peak as f64 / 1048576.0,
        raw.len()
    );
    assert!(peak < MB64, "peak {peak}");
}
