#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use diary_workspace_import_apply::format_python_float;
#[test]
fn actual_python_float_strings() {
    let rows: Vec<(String, String)> =
        serde_json::from_str(include_str!("python-floats.json")).unwrap();
    for (bits, expected) in rows {
        let value = f64::from_bits(u64::from_str_radix(&bits, 16).unwrap());
        assert_eq!(format_python_float(value), Some(expected), "{bits}");
    }
    assert_eq!(format_python_float(f64::INFINITY), None);
}
