//! The CLI contract the service relies on: usage errors exit 2, refusals exit 1 with nothing on
//! stdout, a plan exits 0 with one JSON object.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::process::{Command, Stdio};

const REQUEST: &[u8] = br#"{"shape":{"gemma":false,"layers":32,"kv_heads":8,"k_dim":128,"v_dim":128,"hybrid_interval":null,"window":null,"k_swa":null,"v_swa":null,"shared":0,"period":null},"layers":32,"native_ctx":131072,"model_gb_raw":4.7,"moe_ratio":0.0,"is_moe":false,"mmproj_vram_gb":0.0,"n_sessions":1,"backends":[{"vram_gb":24.0,"gpu_count":1,"cards":[],"host_ram_gb":64.0,"same_as":0}],"preset":"","prompt_tps":0.0,"prompt_budget_s":120.0,"verified_ctx":0,"cache_ram_cap_mib":1024}"#;

fn run(args: &[&str], stdin: &[u8]) -> (i32, Vec<u8>, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_model-autoconfig"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    // A usage error may exit before reading stdin; a broken pipe there is fine.
    let _ = child.stdin.take().unwrap().write_all(stdin);
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        out.stdout,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn usage() {
    for args in [
        &["--help"][..],
        &[][..],
        &["size", "extra"][..],
        &["check", "size"][..],
        &["prep"][..],
    ] {
        let (code, out, err) = run(args, b"");
        assert_eq!(code, 2, "{args:?}");
        assert!(out.is_empty());
        assert!(err.contains("usage"));
    }
}

#[test]
fn refusal_leaves_stdout_empty() {
    let (code, out, err) = run(&["size"], b"{\"not\": \"a request\"}");
    assert_eq!(code, 1);
    assert!(out.is_empty());
    assert!(err.starts_with("model-autoconfig: schema"), "{err}");
}

#[test]
fn plan() {
    let (code, out, _) = run(&["size"], REQUEST);
    assert_eq!(code, 0);
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("{\"plans\":") && text.ends_with("}\n"));
    assert!(text.contains("\"cap\":\"unmeasured\""));
}

#[test]
fn check_answers_every_part_in_one_process() {
    let size = std::str::from_utf8(REQUEST).unwrap();
    let stdin = format!(
        "{{\"prep\":{{\"n_sessions\":1,\"arch\":\"llama\",\"model\":{{\"block_count\":32,\
\"attention_head_count\":32,\"embedding_length\":4096,\"attention_head_count_kv\":8,\"context_length\":131072}},\
\"file_size\":5046586572,\"backends\":[{{\"name\":\"a\",\"vram_gb\":24.0,\"host_ram_gb\":64.0}}],\
\"projector\":{{\"has_mmproj\":false,\"mmproj_gb\":0.0,\"mtp_gb\":0.0}}}},\"size\":{size},\
\"values\":{{\"model_rel\":\"\",\"n_sessions\":1,\"chat_template\":true,\"features\":null,\"section\":\"\",\
\"vision\":true,\"current\":{{}},\"mmproj_rel\":\"\",\"has_mmproj\":false,\"spec\":[],\"plan\":{{\"initial_ctx\":32768,\
\"sized\":true,\"ctx\":32768,\"ngl\":null,\"fit\":false,\"cache_ram\":8192}},\"rope\":{{}},\"native_ctx\":131072,\
\"rec_gpu_count\":1}}}}"
    );
    let (code, out, err) = run(&["check"], stdin.as_bytes());
    assert_eq!(code, 0, "{err}");
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.starts_with("{\"prep\":{\"refuse\":null,") && text.ends_with("]]}\n"),
        "{text}"
    );
    assert!(
        text.contains(",\"size\":{\"plans\":")
            && text.contains(",\"values\":[[\"ctx-size\",\"32768\"],")
    );
}

#[test]
fn check_refusal_leaves_stdout_empty() {
    let (code, out, err) = run(&["check"], b"{\"values\": {}}");
    assert_eq!(code, 1);
    assert!(out.is_empty());
    assert!(err.starts_with("model-autoconfig: schema"), "{err}");
    let (code, out, err) = run(&["check"], b"{\"prep\": {\"n_sessions\": 1, \"arch\": 5}}");
    assert_eq!(code, 1);
    assert!(out.is_empty());
    assert!(
        err.starts_with("model-autoconfig: python:AttributeError"),
        "{err}"
    );
}
