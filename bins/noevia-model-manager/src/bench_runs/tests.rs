#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use proptest::prelude::*;

#[test]
fn canonical_ids_only() {
    assert_eq!(canonical_id("7"), Some(7));
    assert_eq!(canonical_id("007"), Some(7));
    assert_eq!(canonical_id("-3"), Some(-3));
    assert_eq!(canonical_id("9223372036854775807"), Some(i64::MAX));
    assert_eq!(canonical_id("-9223372036854775808"), Some(i64::MIN));
    for bad in [
        "",
        "-",
        "+1",
        " 1",
        "1 ",
        "1.0",
        "1_0",
        "9223372036854775808",
        "%31",
        "abc",
    ] {
        assert_eq!(canonical_id(bad), None, "{bad:?}");
    }
}

#[test]
fn round_matches_python() {
    // Expected values are CPython 3.12 round(x, n) (ties to even on the exact binary value).
    for (x, n, want) in [
        (0.125, 2, 0.12),
        (0.375, 2, 0.38),
        (2.5, 0, 2.0),
        (3.5, 0, 4.0),
        (0.25, 1, 0.2),
        (1.05, 1, 1.1),
        (2.675, 2, 2.67),
        (-0.5, 0, -0.0),
        (123.456, 1, 123.5),
        (1e20, 1, 1e20),
    ] {
        assert_eq!(
            py_round(Num::Float(x), n),
            Some(Num::Float(want)),
            "{x} {n}"
        );
    }
    assert_eq!(py_round(Num::Int(5), 2), Some(Num::Int(5)));
    // noevia#1935: `round(nan|inf, n)` is the value itself, and FastAPI writes it as null.
    assert!(matches!(py_round(Num::Float(f64::NAN), 1), Some(Num::Float(f)) if f.is_nan()));
    assert_eq!(
        py_round(Num::Float(f64::INFINITY), 2),
        Some(Num::Float(f64::INFINITY))
    );
    assert_eq!(
        Num::Float(f64::NEG_INFINITY).render().as_deref(),
        Some("null")
    );
    assert_eq!(Num::Float(f64::NAN).render().as_deref(), Some("null"));
}

#[test]
fn median_is_helpers_median() {
    assert_eq!(
        median(&[Num::Float(3.0), Num::Float(1.0), Num::Float(2.0)]),
        Some(Num::Float(2.0))
    );
    assert_eq!(median(&[Num::Int(1), Num::Int(4)]), Some(Num::Float(2.5)));
    assert_eq!(
        median(&[Num::Int(2), Num::Float(9.0), Num::Int(1)]),
        Some(Num::Int(2))
    );
    assert_eq!(median(&[Num::Float(0.0)]), Some(Num::Float(0.0)));
}

#[test]
fn peak_sums_follow_python_sum_and_json_loads() {
    let t = |s: &str| peak_sum(Some(&Val::Text(s.into())));
    assert_eq!(t("[1, 2]"), Ok(Some(Num::Int(3))));
    assert_eq!(t("[1, 2.5]"), Ok(Some(Num::Float(3.5))));
    assert_eq!(t("[true, 1]"), Ok(Some(Num::Int(2))));
    assert_eq!(t("[]"), Ok(Some(Num::Int(0))));
    assert_eq!(t("{}"), Ok(Some(Num::Int(0))));
    assert_eq!(t("\"\""), Ok(Some(Num::Int(0))));
    assert_eq!(t(""), Ok(Some(Num::Int(0))));
    assert_eq!(t("not json"), Ok(None));
    assert_eq!(t("5"), Err(()));
    assert_eq!(t("[\"a\"]"), Err(()));
    assert_eq!(t("null"), Err(()));
    assert_eq!(peak_sum(None), Ok(Some(Num::Int(0))));
    assert_eq!(peak_sum(Some(&Val::Null)), Ok(Some(Num::Int(0))));
    assert_eq!(peak_sum(Some(&Val::Int(3))), Err(()));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // Hostile peak JSON never panics.
    #[test]
    fn hostile_peaks_are_total(text in "\\PC{0,200}") {
        let _ = peak_sum(Some(&Val::Text(text)));
    }

    // Rounding is total and idempotent on finite floats.
    #[test]
    fn round_is_idempotent(x in -1e12f64..1e12, n in 0usize..4) {
        let once = py_round(Num::Float(x), n).unwrap();
        prop_assert_eq!(py_round(once, n), Some(once));
    }
}

/// noevia#2006: a request dropped while the store is read (the client went away) keeps its
/// admission permit until the read has returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_request_keeps_its_permit_until_the_read_ends() {
    use crate::test_support::{all_permits_back, dropped_in_blocking_work, test_state};
    let dir = tempfile::tempdir().unwrap();
    let mut state = test_state(crate::LEGACY_BASE);
    state.bench_runs = Some(std::sync::Arc::new(BenchRuns::new(dir.path())));
    let state = std::sync::Arc::new(state);
    let request = axum::http::Request::get("/api/v1/benchmark/runs/1")
        .body(axum::body::Body::empty())
        .unwrap();
    let open = dropped_in_blocking_work(&state, request).await;
    open.send(()).unwrap();
    all_permits_back(&state).await;
}

/// noevia#2006: the same for `GET /api/v1/benchmark`, whose models.ini and store reads run on
/// one blocking task. (Here because CI runs the overview's lib tests through `bench_runs::`.)
#[cfg(feature = "native-benchmark-overview")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_overview_keeps_its_permit_until_its_reads_end() {
    use crate::test_support::{all_permits_back, dropped_in_blocking_work, test_state};
    let dir = tempfile::tempdir().unwrap();
    let mut state = test_state(crate::LEGACY_BASE);
    state.benchmark_overview = Some(std::sync::Arc::new(
        crate::benchmark_overview::BenchmarkOverview::new(
            dir.path(),
            &dir.path().join("models.ini"),
        ),
    ));
    let state = std::sync::Arc::new(state);
    let request = axum::http::Request::get("/api/v1/benchmark")
        .body(axum::body::Body::empty())
        .unwrap();
    let open = dropped_in_blocking_work(&state, request).await;
    open.send(()).unwrap();
    all_permits_back(&state).await;
}
