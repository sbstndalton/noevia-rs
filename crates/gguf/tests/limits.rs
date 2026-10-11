#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use gguf::{read_raw_bytes, read_raw_stream_with_limits, LimitedError, ReadLimits};
use std::io::Cursor;
fn header(value: &str) -> Vec<u8> {
    let mut bytes = b"GGUF".to_vec();
    bytes.extend(3u32.to_le_bytes());
    bytes.extend(0u64.to_le_bytes());
    bytes.extend(1u64.to_le_bytes());
    bytes.extend(1u64.to_le_bytes());
    bytes.push(b'x');
    bytes.extend(8u32.to_le_bytes());
    bytes.extend((value.len() as u64).to_le_bytes());
    bytes.extend(value.as_bytes());
    bytes
}
fn limits() -> ReadLimits {
    ReadLimits {
        header_bytes: 1024,
        retained_string_bytes: 100,
        retained_values: 10,
        kv_count: 10,
        string_bytes: 32,
        key_bytes: 32,
    }
}
#[test]
fn bounded_metadata_text_preflights_dialect_depth_nodes_and_output() {
    use gguf::{metadata_text_with_limit as text, Value};
    let array = Value::List(vec![Value::Str("yarn".into())]);
    assert_eq!(text(&array, 64).unwrap(), "['yarn']");
    assert!(matches!(text(&array, 4), Err(LimitedError::Budget)));
    assert_eq!(
        text(&Value::Str("\u{1c}none\u{1f}😀".into()), 32).unwrap(),
        "\u{1c}none\u{1f}😀"
    );
    assert!(matches!(
        text(&Value::List(vec![Value::Str("😀".into())]), 64),
        Err(LimitedError::Budget)
    ));
    let mut deep = Value::Bool(true);
    for _ in 0..9 {
        deep = Value::List(vec![deep]);
    }
    assert!(matches!(text(&deep, 65536), Err(LimitedError::Budget)));
    let nodes = Value::List(vec![Value::Bool(true); 1025]);
    assert!(matches!(text(&nodes, 65536), Err(LimitedError::Budget)));
    assert!(matches!(
        text(&Value::Float(f64::NAN), 64),
        Err(LimitedError::Budget)
    ));
    assert!(matches!(text(&array, 65537), Err(LimitedError::Budget)));
}
#[test]
fn tighter_components_refuse_distinctly_without_changing_default_reader() {
    let bytes = header("hello");
    let ordinary = read_raw_bytes(&bytes).unwrap();
    assert_eq!(
        ordinary,
        read_raw_stream_with_limits(Cursor::new(&bytes), limits()).unwrap()
    );
    for limit in [
        ReadLimits {
            header_bytes: 4,
            ..limits()
        },
        ReadLimits {
            retained_string_bytes: 3,
            ..limits()
        },
        ReadLimits {
            retained_values: 0,
            ..limits()
        },
        ReadLimits {
            kv_count: 0,
            ..limits()
        },
        ReadLimits {
            string_bytes: 4,
            ..limits()
        },
        ReadLimits {
            key_bytes: 0,
            ..limits()
        },
    ] {
        assert!(matches!(
            read_raw_stream_with_limits(Cursor::new(&bytes), limit),
            Err(LimitedError::Budget)
        ));
        assert_eq!(ordinary, read_raw_bytes(&bytes).unwrap());
    }
    assert!(matches!(
        read_raw_stream_with_limits(Cursor::new(b"NOPE"), limits()),
        Err(LimitedError::Metadata(_))
    ));
    assert!(matches!(
        read_raw_stream_with_limits(
            Cursor::new(&bytes),
            ReadLimits {
                kv_count: gguf::MAX_KV_COUNT + 1,
                ..limits()
            }
        ),
        Err(LimitedError::Budget)
    ));
}
#[test]
fn utf8_decode_reservation_is_enforced_before_aggregate_growth() {
    for count in 0..32 {
        let text = "x".repeat(count);
        let bytes = header(&text);
        let result = read_raw_stream_with_limits(
            Cursor::new(&bytes),
            ReadLimits {
                retained_string_bytes: 3 * (count as u64 + 1),
                ..limits()
            },
        );
        assert_eq!(result.unwrap(), read_raw_bytes(&bytes).unwrap());
        if count > 0 {
            assert!(matches!(
                read_raw_stream_with_limits(
                    Cursor::new(bytes),
                    ReadLimits {
                        retained_string_bytes: 3 * count as u64,
                        ..limits()
                    }
                ),
                Err(LimitedError::Budget)
            ));
        }
    }
}
