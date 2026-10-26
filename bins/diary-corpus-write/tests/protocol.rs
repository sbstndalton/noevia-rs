#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

fn call(input: &[u8]) -> (bool, Vec<u8>, Vec<u8>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_diary-corpus-write-cli"))
        .arg("transform-v1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    (output.status.success(), output.stdout, output.stderr)
}
fn frame(v: &Value) -> Vec<u8> {
    raw_frame(&serde_json::to_vec(v).unwrap())
}
fn raw_frame(data: &[u8]) -> Vec<u8> {
    let mut out = b"DCW1".to_vec();
    out.extend_from_slice(&(data.len() as u64).to_be_bytes());
    out.extend_from_slice(&data);
    out.extend_from_slice(b"END1");
    out
}
fn unframe(out: &[u8]) -> Value {
    assert!(out.starts_with(b"DWR1") && out.ends_with(b"END1"));
    let len = u64::from_be_bytes(out[4..12].try_into().unwrap()) as usize;
    assert_eq!(out.len(), len + 16);
    serde_json::from_slice(&out[12..12 + len]).unwrap()
}
#[test]
fn cpython_oracle() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("oracle.json")).unwrap();
    for (i, case) in cases.iter().enumerate() {
        let (ok, output, error) = call(&frame(&case["request"]));
        assert!(ok, "case {i}: {}", String::from_utf8_lossy(&error));
        assert_eq!(unframe(&output), case["expected"], "case {i}");
    }
}
#[test]
fn framing_and_shape_fail_closed() {
    let request = serde_json::json!({"version":1,"op":"apply_edit","current":null,"xid":"x","new_me":"m","new_assistant":"a"});
    let good = frame(&request);
    for bad in [
        b"bad".to_vec(),
        raw_frame(br#"{"version":2,"version":1,"op":"apply_edit","current":null,"xid":"x","new_me":"m","new_assistant":"a"}"#),
        [b"BAD1".as_slice(), &good[4..]].concat(),
        good[..good.len() - 1].to_vec(),
        [good.as_slice(), b"extra"].concat(),
        [b"DCW1".as_slice(), &u64::MAX.to_be_bytes()].concat(),
        frame(
            &serde_json::json!({"version":1,"op":"apply_edit","current":null,"xid":"x","new_me":10,"new_assistant":"a"}),
        ),
    ] {
        let (ok, output, _) = call(&bad);
        assert!(!ok && output.is_empty());
    }
}
