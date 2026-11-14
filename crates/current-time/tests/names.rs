//! tools/chrono-tz-names.txt is chrono-tz's zone list, which the recorder offers Node as candidates.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[test]
fn chrono_tz_names_file_is_current() {
    let want: String = chrono_tz::TZ_VARIANTS
        .iter()
        .map(|z| format!("{}\n", z.name()))
        .collect();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tools/chrono-tz-names.txt");
    if std::env::var_os("CURRENT_TIME_WRITE_NAMES").is_some() {
        std::fs::write(path, &want).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        want,
        "rewrite with CURRENT_TIME_WRITE_NAMES=1"
    );
}
