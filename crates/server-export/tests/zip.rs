#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use base64::{engine::general_purpose::STANDARD, Engine};
use proptest::prelude::*;
use server_export::{
    zip_store, Entry, Error, MAX_ENTRIES, MAX_NAME_BYTES, MAX_OUTPUT_BYTES, MAX_PAYLOAD_BYTES,
};
fn u16at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes(b[i..i + 2].try_into().unwrap())
}
fn u32at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
// Independent bitwise CRC oracle (production uses crc32fast's table implementation).
fn independent_crc(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}
#[test]
fn actual_node_complete_zip_bytes() {
    let f: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/node-zip.v1.json")).unwrap();
    assert_eq!(f["core"], "965598462d64dc04e1df55c3bb7a25d4343380e1");
    assert_eq!(
        f["sourceSha256"],
        "5cfbfd9353b3963a903f0da70fb191e50c7af9d6f215742c222143e402d62d7c"
    );
    for case in f["cases"].as_array().unwrap() {
        let data: Vec<(String, Vec<u8>)> = case["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["name"].as_str().unwrap().into(),
                    STANDARD.decode(e["data"].as_str().unwrap()).unwrap(),
                )
            })
            .collect();
        let entries: Vec<_> = data
            .iter()
            .map(|(name, data)| Entry { name, data })
            .collect();
        assert_eq!(
            zip_store(&entries, case["when"].as_i64().unwrap()).unwrap(),
            STANDARD.decode(case["zip"].as_str().unwrap()).unwrap()
        );
    }
}
#[test]
fn limits_are_checked_before_zip_output() {
    let empty = Entry {
        name: "",
        data: &[],
    };
    let too_many: Vec<_> = (0..MAX_ENTRIES + 1)
        .map(|_| Entry {
            name: empty.name,
            data: empty.data,
        })
        .collect();
    assert_eq!(zip_store(&too_many, 1800000000000), Err(Error::Limit));
    let data = vec![0; MAX_PAYLOAD_BYTES + 1];
    assert_eq!(
        zip_store(
            &[Entry {
                name: "a",
                data: &data
            }],
            1800000000000
        ),
        Err(Error::Limit)
    );
    let long = "a".repeat(u16::MAX as usize + 1);
    assert_eq!(
        zip_store(
            &[Entry {
                name: &long,
                data: &[]
            }],
            1800000000000
        ),
        Err(Error::Limit)
    );
    let name = "a".repeat(65_000);
    let names: Vec<_> = (0..MAX_NAME_BYTES / 65_000 + 1)
        .map(|_| Entry {
            name: &name,
            data: &[],
        })
        .collect();
    assert_eq!(zip_store(&names, 1800000000000), Err(Error::Limit));
    assert_eq!(zip_store(&[], i64::MAX), Err(Error::Timestamp));
    assert_eq!(zip_store(&[], i64::MIN), Err(Error::Timestamp));
    assert_eq!(zip_store(&[], 4_355_942_400_000), Err(Error::Timestamp)); // 2108-01-13 beyond classic DOS year.
}
#[test]
fn exact_payload_and_entry_limits_are_accepted() {
    let longest_name = "a".repeat(u16::MAX as usize);
    assert_eq!(
        zip_store(
            &[Entry {
                name: &longest_name,
                data: &[]
            }],
            1800000000000
        )
        .unwrap()
        .len(),
        2 * longest_name.len() + 98
    );
    let data = vec![0; MAX_PAYLOAD_BYTES];
    assert_eq!(
        zip_store(
            &[Entry {
                name: "a",
                data: &data
            }],
            1800000000000
        )
        .unwrap()
        .len(),
        MAX_PAYLOAD_BYTES + 100
    );
    let short = "a".repeat(52);
    let tail = "a".repeat(MAX_NAME_BYTES - 52 * (MAX_ENTRIES - 1));
    let maximal: Vec<_> = (0..MAX_ENTRIES)
        .map(|i| Entry {
            name: if i == MAX_ENTRIES - 1 { &tail } else { &short },
            data: if i == 0 { &data } else { &[] },
        })
        .collect();
    assert_eq!(
        zip_store(&maximal, 1800000000000).unwrap().len(),
        MAX_OUTPUT_BYTES
    );
    let entries: Vec<_> = (0..MAX_ENTRIES)
        .map(|_| Entry {
            name: "",
            data: &[],
        })
        .collect();
    assert_eq!(
        zip_store(&entries, 1800000000000).unwrap().len(),
        MAX_ENTRIES * 76 + 22
    );
}
proptest! {
    #[test]
    fn archive_records_remain_consistent(
        input in prop::collection::vec(("[a-zÅé👨_-]{0,16}", prop::collection::vec(any::<u8>(), 0..1024)), 0..16),
        ms in 315_532_800_000i64..4_354_819_198_000i64,
    ) {
        let entries: Vec<_> = input.iter().map(|(name, data)| Entry { name, data }).collect();
        let zip = zip_store(&entries, ms).unwrap();
        let mut cursor = 0;
        let mut offsets = Vec::new();
        for (name, data) in &input {
            offsets.push(cursor);
            prop_assert_eq!(u32at(&zip, cursor), 0x04034b50);
            prop_assert_eq!(u16at(&zip, cursor + 6), 0x0800);
            prop_assert_eq!(u16at(&zip, cursor + 8), 0);
            prop_assert_eq!(u32at(&zip, cursor + 14), independent_crc(data));
            prop_assert_eq!(u32at(&zip, cursor + 18), data.len() as u32);
            prop_assert_eq!(u32at(&zip, cursor + 22), data.len() as u32);
            prop_assert_eq!(u16at(&zip, cursor + 26), name.len() as u16);
            prop_assert_eq!(u16at(&zip, cursor + 28), 0);
            prop_assert_eq!(&zip[cursor + 30..cursor + 30 + name.len()], name.as_bytes());
            prop_assert_eq!(&zip[cursor + 30 + name.len()..cursor + 30 + name.len() + data.len()], data);
            cursor += 30 + name.len() + data.len();
        }
        let central_start = cursor;
        for ((name, data), offset) in input.iter().zip(offsets) {
            prop_assert_eq!(u32at(&zip, cursor), 0x02014b50);
            prop_assert_eq!(u16at(&zip, cursor + 8), 0x0800);
            prop_assert_eq!(u16at(&zip, cursor + 10), 0);
            prop_assert_eq!(u32at(&zip, cursor + 16), independent_crc(data));
            prop_assert_eq!(u32at(&zip, cursor + 20), data.len() as u32);
            prop_assert_eq!(u32at(&zip, cursor + 24), data.len() as u32);
            prop_assert_eq!(u16at(&zip, cursor + 28), name.len() as u16);
            prop_assert_eq!(u16at(&zip, cursor + 30), 0);
            prop_assert_eq!(u16at(&zip, cursor + 32), 0);
            prop_assert_eq!(u32at(&zip, cursor + 42), offset as u32);
            prop_assert_eq!(&zip[cursor + 46..cursor + 46 + name.len()], name.as_bytes());
            cursor += 46 + name.len();
        }
        prop_assert_eq!(u32at(&zip, cursor), 0x06054b50);
        prop_assert_eq!(u16at(&zip, cursor + 4), 0);
        prop_assert_eq!(u16at(&zip, cursor + 6), 0);
        prop_assert_eq!(u16at(&zip, cursor + 8), input.len() as u16);
        prop_assert_eq!(u16at(&zip, cursor + 10), input.len() as u16);
        prop_assert_eq!(u32at(&zip, cursor + 12), (cursor - central_start) as u32);
        prop_assert_eq!(u32at(&zip, cursor + 16), central_start as u32);
        prop_assert_eq!(u16at(&zip, cursor + 20), 0);
        prop_assert_eq!(cursor + 22, zip.len());
    }
}
