#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
//! Properties: the shared answer against a model of short-read.cjs over random interleavings of
//! polls, clears and time; `JSON.stringify` of strings against serde_json where both can hold the
//! string; `encodeURIComponent` round-trips or throws exactly for lone surrogates; the summary
//! adds what every row reports and nothing else.
use prompt_framing::json::Value;
use proptest::prelude::*;
use server_stats::js::{encode_uri_component, stringify, Fault};
use server_stats::metrics::{parse_metrics, summarize, Row};
use server_stats::{SharedRead, STATS_TTL};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
enum Op {
    /// A poll whose engine read (if it starts one) takes this long.
    Poll(u64),
    Clear,
    Advance(u64),
}

fn op() -> impl Strategy<Value = Op> {
    // Round values too, so polls land exactly on a landing or on the end of the TTL.
    let ms = || prop_oneof![(0u64..6).prop_map(|k| k * 500), 0u64..3000];
    prop_oneof![
        3 => ms().prop_map(Op::Poll),
        1 => Just(Op::Clear),
        3 => ms().prop_map(Op::Advance),
    ]
}

/// short-read.cjs as a model, stepped one op at a time: which load each poll gets, how many loads
/// started, and when each started load lands.
#[derive(Default)]
struct Model {
    now: u64,
    loads: usize,
    /// (load id, lands at)
    entry: Option<(usize, u64)>,
    landings: Vec<u64>,
}

impl Model {
    fn poll(&mut self, ms: u64) -> usize {
        let ttl = STATS_TTL.as_millis() as u64;
        let now = self.now;
        let fresh = self
            .entry
            .filter(|(_, lands)| now < *lands || now < lands + ttl);
        match fresh {
            Some((id, _)) => id,
            None => {
                self.loads += 1;
                self.entry = Some((self.loads, now + ms));
                self.landings.push(now + ms);
                self.loads
            }
        }
    }
}

async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn shared_read_matches_short_read(ops in proptest::collection::vec(op(), 1..40)) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().start_paused(true).build().unwrap();
        let (got, want, loads, want_loads) = rt.block_on(async {
            let read = Arc::new(SharedRead::new(STATS_TTL));
            let loads = Arc::new(AtomicUsize::new(0));
            let mut m = Model::default();
            let (mut polls, mut want) = (Vec::new(), Vec::new());
            for op in &ops {
                match op {
                    Op::Advance(ms) => {
                        // Step through every landing on the way, so each lands at its own time.
                        let target = m.now + ms;
                        let mut stops: Vec<u64> = m.landings.iter().copied().filter(|t| *t > m.now && *t <= target).collect();
                        stops.sort_unstable();
                        stops.push(target);
                        for t in stops {
                            tokio::time::advance(Duration::from_millis(t - m.now)).await;
                            m.now = t;
                            settle().await;
                        }
                    }
                    Op::Clear => {
                        read.clear();
                        m.entry = None;
                    }
                    Op::Poll(ms) => {
                        want.push(m.poll(*ms));
                        let (read, loads, ms) = (Arc::clone(&read), Arc::clone(&loads), *ms);
                        polls.push(tokio::spawn(async move {
                            read.get(move || {
                                let id = loads.fetch_add(1, Ordering::SeqCst) + 1;
                                async move {
                                    tokio::time::sleep(Duration::from_millis(ms)).await;
                                    id
                                }
                            })
                            .await
                        }));
                    }
                }
                settle().await;
            }
            tokio::time::advance(Duration::from_secs(10)).await;
            let mut got = Vec::new();
            for p in polls { got.push(p.await.unwrap().unwrap()); }
            (got, want, loads.load(Ordering::SeqCst), m.loads)
        });
        prop_assert_eq!(got, want);
        prop_assert_eq!(loads, want_loads);
    }

    #[test]
    fn strings_are_written_as_json_stringify(s in "\\PC{0,24}|[\\x00-\\x1f\"\\\\\u{2028}\u{7f}a]{0,12}") {
        let mut out = Vec::new();
        stringify(&mut out, &Value::Str(s.encode_utf16().collect())).unwrap();
        prop_assert_eq!(String::from_utf8(out).unwrap(), serde_json::to_string(&s).unwrap());
    }

    #[test]
    fn uri_components_round_trip(units in proptest::collection::vec(any::<u16>(), 0..16)) {
        let lone = char::decode_utf16(units.iter().copied()).any(|r| r.is_err());
        match encode_uri_component(Some(&Value::Str(units.clone()))) {
            Err(Fault::Throws) => prop_assert!(lone),
            Err(Fault::Unsupported) => prop_assert!(false),
            Ok(enc) => {
                prop_assert!(!lone);
                prop_assert!(enc.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.!~*'()%".contains(&b)));
                let mut bytes = Vec::new();
                let raw = enc.as_bytes();
                let mut i = 0;
                while i < raw.len() {
                    if raw[i] == b'%' {
                        bytes.push(u8::from_str_radix(std::str::from_utf8(&raw[i + 1..i + 3]).unwrap(), 16).unwrap());
                        i += 3;
                    } else {
                        bytes.push(raw[i]);
                        i += 1;
                    }
                }
                prop_assert_eq!(String::from_utf8(bytes).unwrap().encode_utf16().collect::<Vec<_>>(), units);
            }
        }
    }

    #[test]
    fn summary_adds_only_complete_counters(
        rows in proptest::collection::vec(proptest::collection::vec((0usize..3, 0u32..1000), 0..4), 0..5)
    ) {
        const NAMES: [&str; 3] = ["prompt_tokens_total", "tokens_predicted_total", "predicted_tokens_seconds"];
        let parsed: Vec<Row> = rows.iter().map(|lines| {
            let text: String = lines.iter().map(|(k, v)| format!("llamacpp:{} {v}\n", NAMES[*k])).collect();
            Row { model: Some(Value::Str("m".encode_utf16().collect())), values: parse_metrics(&text.encode_utf16().collect::<Vec<_>>()) }
        }).collect();
        let s = summarize(&parsed);
        for (k, got) in [(0, s.input_tokens_total), (1, s.output_tokens_total), (2, s.tokens_per_second)] {
            let want = if rows.is_empty() || rows.iter().any(|r| !r.iter().any(|(n, _)| *n == k)) {
                None
            } else {
                // The last line with the name wins within a row.
                Some(rows.iter().map(|r| f64::from(r.iter().rev().find(|(n, _)| *n == k).unwrap().1)).sum::<f64>())
            };
            prop_assert_eq!(got, want);
        }
        prop_assert!(s.mtp.is_empty());
    }
}
