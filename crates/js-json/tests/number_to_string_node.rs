//! `number_to_string` against `String(x)` from real Node (V8): tools/gen-js-number-fixtures.cjs.
//! The values are mostly binary64 numbers whose shortest round-trip digits are ambiguous, where a
//! formatter's tie choice shows (2^-25 is `2.9802322387695312e-8`, not `…313e-8`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

#[test]
fn number_to_string_writes_v8s_digits() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/number-to-string-node.json")).unwrap();
    let mut wrong = Vec::new();
    let values = fixture["values"].as_array().unwrap();
    for pair in values {
        let bits = u64::from_str_radix(pair[0].as_str().unwrap(), 16).unwrap();
        let expected = pair[1].as_str().unwrap();
        let got = js_json::number_to_string(f64::from_bits(bits));
        if got != expected {
            wrong.push(format!("{bits:016x}: {got} (V8 {expected})"));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} differ from V8, e.g. {:?}",
        wrong.len(),
        values.len(),
        &wrong[..wrong.len().min(5)]
    );
    // The review's example, spelled out.
    assert_eq!(
        js_json::number_to_string(2f64.powi(-25)),
        "2.9802322387695312e-8"
    );
}
