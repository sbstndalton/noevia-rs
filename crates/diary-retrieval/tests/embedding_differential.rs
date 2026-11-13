//! Differential test: replays every case of `embedding-fixtures.json` (recorded from the frozen
//! Python `diary/agent/embedding.py` by `generate_embedding_fixtures.py`) on the Rust module and
//! compares the answers, including float bits, saved-file bytes and log lines.

// serde_json `Value[...]` on fixture data: a missing key reads as Null and fails the comparison;
// a failed assertion is the test reporting a difference.
#![allow(clippy::indexing_slicing, clippy::panic)]

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::Path;

use diary_retrieval::embedding::address::{ip_address, nfkc_netloc_chars, Ip};
use diary_retrieval::embedding::config::{ChangeError, Snapshot};
use diary_retrieval::embedding::guard::CheckFailure;
use diary_retrieval::embedding::pystr::{is_space, str_or_empty};
use diary_retrieval::embedding::{
    check_embedding_address, clean_title, embed_fitting, frame, guard_seeded_addresses, mean,
    model_matches, non_local_reason, normalize_base_url, normalize_target, recheck_seeded,
    seed_from_cfg, seed_warning, split_text, state_path_for, AddressCheck, EmbeddingConfig,
    EmbeddingConfigError, FitError, Kinds, LogLevel, LogLine, Pending, RecheckNext, ResolveError,
    SeededReasons, SendError, Source, Which, KINDS, PROFILES, RECHECK_S, RETRY_DELAYS_S,
};
use diary_retrieval::Target;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn fixtures() -> TestResult<Value> {
    Ok(serde_json::from_str(include_str!(
        "embedding-fixtures.json"
    ))?)
}

fn arr(v: &Value) -> &[Value] {
    v.as_array().map_or(&[], Vec::as_slice)
}

fn strs(v: &Value) -> Vec<&str> {
    arr(v).iter().filter_map(Value::as_str).collect()
}

fn bits(v: &Value) -> f64 {
    let hex = v.as_str().unwrap_or("0");
    f64::from_bits(u64::from_str_radix(hex, 16).unwrap_or(0))
}

fn vector_bits(v: &Value) -> Vec<f64> {
    arr(v).iter().map(bits).collect()
}

fn vectors_bits(v: &Value) -> Vec<Vec<f64>> {
    arr(v).iter().map(vector_bits).collect()
}

fn same_float(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
}

fn same_vector(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_float(*x, *y))
}

fn sha12(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .filter_map(|i| u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok())
        .collect()
}

fn error_json(e: &EmbeddingConfigError) -> Value {
    json!({"message": e.message, "code": e.code, "status": e.status})
}

fn ok_or_err<T>(r: Result<T, EmbeddingConfigError>, ok: impl Fn(T) -> Value) -> Value {
    match r {
        Ok(v) => json!({"ok": ok(v)}),
        Err(e) if e.code == "badArguments" => json!({"index_error": true}),
        Err(e) => json!({"error": error_json(&e)}),
    }
}

#[test]
fn constants_and_character_classes_match_python() -> TestResult {
    let fx = fixtures()?;
    let c = &fx["constants"];
    assert_eq!(json!(PROFILES), c["PROFILES"]);
    assert_eq!(json!(KINDS), c["KINDS"]);
    assert_eq!(
        json!(diary_retrieval::DEFAULT_PROFILE),
        c["DEFAULT_PROFILE"]
    );
    assert_eq!(json!(RETRY_DELAYS_S), c["RETRY_DELAYS_S"]);
    assert_eq!(json!(RECHECK_S), c["RECHECK_S"]);
    assert_eq!(
        json!(diary_retrieval::embedding::SPLIT_MARGIN),
        c["SPLIT_MARGIN"]
    );
    assert_eq!(
        json!(diary_retrieval::embedding::SPLIT_MAX_DEPTH),
        c["SPLIT_MAX_DEPTH"]
    );
    assert_eq!(
        json!(diary_retrieval::embedding::SPLIT_MIN_CHARS),
        c["SPLIT_MIN_CHARS"]
    );
    assert_eq!(
        json!(diary_retrieval::embedding::SPLIT_MAX_PARTS),
        c["SPLIT_MAX_PARTS"]
    );

    // str.isspace and re \s, over every code point
    let ranges: Vec<(u32, u32)> = arr(&fx["classes"]["space_ranges"])
        .iter()
        .map(|r| {
            (
                r[0].as_u64().unwrap_or(0) as u32,
                r[1].as_u64().unwrap_or(0) as u32,
            )
        })
        .collect();
    for cp in 0..=0x10_FFFFu32 {
        let Some(ch) = char::from_u32(cp) else {
            continue;
        };
        let want = ranges.iter().any(|&(a, b)| (a..=b).contains(&cp));
        assert_eq!(is_space(ch), want, "U+{cp:04X}");
    }
    let table: Vec<u64> = arr(&fx["classes"]["nfkc_netloc_chars"])
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    let ours: Vec<u64> = nfkc_netloc_chars().into_iter().map(u64::from).collect();
    assert_eq!(
        ours, table,
        "NFKC netloc table (Unicode {})",
        fx["classes"]["unicode_version"]
    );
    Ok(())
}

#[test]
fn clean_title_and_frame_match_python() -> TestResult {
    let fx = fixtures()?;
    let titles = arr(&fx["framing"]["clean_title"]);
    assert!(titles.len() >= 60);
    for case in titles {
        let got = clean_title(case["title"].as_str());
        assert_eq!(json!(got), case["expected"], "{}", case["title"]);
    }
    let frames = arr(&fx["framing"]["frame"]);
    assert!(
        titles.len() + frames.len() >= 150,
        "{} frame cases",
        frames.len()
    );
    for case in frames {
        let texts = strs(&case["texts"]);
        let kind_list: Vec<&str> = strs(&case["kind"]);
        let kind = match case["kind"].as_str() {
            Some(k) => Kinds::One(k),
            None => Kinds::Each(&kind_list),
        };
        let titles: Option<Vec<Option<&str>>> = case["titles"]
            .as_array()
            .map(|a| a.iter().map(Value::as_str).collect());
        let got = ok_or_err(
            frame(
                case["profile"].as_str().unwrap_or(""),
                &texts,
                kind,
                titles.as_deref(),
            ),
            |v| json!(v),
        );
        assert_eq!(got, case["expected"], "{case}");
    }
    Ok(())
}

#[test]
fn split_text_matches_python() -> TestResult {
    let fx = fixtures()?;
    let cases = arr(&fx["split_text"]);
    assert!(cases.len() >= 120);
    for case in cases {
        let got = split_text(
            case["text"].as_str().unwrap_or(""),
            case["parts"].as_i64().unwrap_or(0),
        );
        assert_eq!(json!(got), case["pieces"], "parts {}", case["parts"]);
    }
    Ok(())
}

#[test]
fn weighted_mean_matches_python_bit_for_bit() -> TestResult {
    let fx = fixtures()?;
    let cases = arr(&fx["mean"]);
    assert!(cases.len() >= 50);
    for case in cases {
        let vectors = vectors_bits(&case["vectors"]);
        let weights: Vec<usize> = arr(&case["weights"])
            .iter()
            .filter_map(|w| w.as_u64().map(|w| w as usize))
            .collect();
        let got = mean(&vectors, &weights);
        let want = &case["expected"];
        match (got, want.get("ok")) {
            (Some(got), Some(ok)) => {
                let want = vector_bits(ok);
                assert!(
                    same_vector(&got, &want),
                    "{case}\n got {got:?}\nwant {want:?}"
                );
            }
            (None, None) => assert_eq!(want["index_error"], json!(true)),
            (got, _) => panic!("{case}: got {got:?}"),
        }
    }
    Ok(())
}

// ---- embed_fitting ----

fn fake_vec(text: &str) -> Vec<f64> {
    Sha256::digest(text.as_bytes())
        .iter()
        .take(6)
        .map(|b| f64::from(*b) / 255.0)
        .collect()
}

type SendOutcome = Result<Vec<Vec<f64>>, SendError<(String, String)>>;

/// The `FakeSend` of the generator.
fn fake_send(
    spec: &Value,
    calls: &RefCell<Vec<Value>>,
    texts: &[String],
    kinds: &[String],
    titles: &[Option<String>],
) -> SendOutcome {
    calls.borrow_mut().push(json!({
        "texts": texts.iter().map(|t| json!([t.chars().count(), sha12(t)])).collect::<Vec<_>>(),
        "kinds": kinds,
        "titles": titles,
    }));
    let kind_refs: Vec<&str> = kinds.iter().map(String::as_str).collect();
    let title_refs: Vec<Option<&str>> = titles.iter().map(Option::as_deref).collect();
    let framed = frame(
        spec["profile"].as_str().unwrap_or(""),
        texts,
        Kinds::Each(&kind_refs),
        Some(&title_refs),
    )
    .map_err(|e| SendError::Other((e.message, e.code.to_owned())))?;
    if let Some(marker) = spec["other_marker"].as_str() {
        if texts.iter().any(|t| t.contains(marker)) {
            return Err(SendError::Other((
                "synthetic server failure".to_owned(),
                String::new(),
            )));
        }
    }
    let lens: Vec<i64> = framed.iter().map(|f| f.chars().count() as i64).collect();
    let longest = lens.iter().copied().max().unwrap_or(0);
    let total: i64 = lens.iter().sum();
    let mut refused = None;
    if let Some(cap) = spec["max_chars"].as_i64() {
        if longest > cap {
            refused = Some((longest, cap));
        }
    }
    if refused.is_none() {
        if let Some(cap) = spec["batch_chars"].as_i64() {
            if total > cap {
                refused = Some((total, cap));
            }
        }
    }
    if let Some((size, cap)) = refused {
        return Err(match spec["report"].as_str().unwrap_or("none") {
            "chars3" => SendError::TooLarge {
                tokens: Some(size / 3),
                limit: Some(cap / 3),
            },
            "fixed" => SendError::TooLarge {
                tokens: spec["tokens"].as_i64(),
                limit: spec["limit"].as_i64(),
            },
            _ => SendError::TooLarge {
                tokens: None,
                limit: None,
            },
        });
    }
    Ok(framed.iter().map(|f| fake_vec(f)).collect())
}

#[test]
fn embed_fitting_matches_python_including_every_batch_sent() -> TestResult {
    let fx = fixtures()?;
    let cases = arr(&fx["embed_fitting"]);
    assert!(cases.len() >= 60);
    for case in cases {
        let calls = RefCell::new(Vec::new());
        let spec = &case["spec"];
        let texts = strs(&case["texts"]);
        let kind_list = strs(&case["kind"]);
        let kind = match case["kind"].as_str() {
            Some(k) => Kinds::One(k),
            None => Kinds::Each(&kind_list),
        };
        let titles: Option<Vec<Option<&str>>> = case["titles"]
            .as_array()
            .map(|a| a.iter().map(Value::as_str).collect());
        let mut send =
            |t: &[String], k: &[String], ti: &[Option<String>]| fake_send(spec, &calls, t, k, ti);
        let got = embed_fitting(&mut send, &texts, kind, titles.as_deref());
        let want = &case["expected"];
        match (&got, want.get("ok"), want.get("error")) {
            (Ok(v), Some(ok), _) => {
                let want = vectors_bits(ok);
                assert_eq!(v.len(), want.len(), "{case}");
                for (a, b) in v.iter().zip(&want) {
                    assert!(same_vector(a, b), "{case}\n got {a:?}\nwant {b:?}");
                }
            }
            (Err(FitError::Send(SendError::TooLarge { tokens, limit })), _, Some(e)) => {
                assert_eq!(e["kind"], json!("too_large"), "{case}");
                assert_eq!(json!(tokens), e["tokens"], "{case}");
                assert_eq!(json!(limit), e["limit"], "{case}");
            }
            (Err(FitError::Send(SendError::Other((message, code)))), _, Some(e)) => {
                assert_eq!(e["kind"], json!("other"), "{case}");
                assert_eq!(json!(message), e["message"], "{case}");
                if let Some(want_code) = e.get("code") {
                    assert_eq!(json!(code), *want_code, "{case}");
                }
            }
            (Err(FitError::Malformed(_)), None, None) => {
                assert_eq!(want["index_error"], json!(true), "{case}");
            }
            _ => panic!("{case}\n got {got:?}"),
        }
        assert_eq!(
            json!(*calls.borrow()),
            case["calls"],
            "batches sent: {case}"
        );
    }
    Ok(())
}

// ---- targets ----

fn target_json(t: &Target) -> Value {
    t.to_json()
}

#[test]
fn normalize_target_matches_python() -> TestResult {
    let fx = fixtures()?;
    let cases = arr(&fx["normalize_target"]);
    assert!(cases.len() >= 80);
    for case in cases {
        let want = &case["expected"];
        match normalize_target(&case["raw"]) {
            Ok(t) => {
                assert_eq!(target_json(&t), want["ok"], "{case}");
                assert_eq!(json!(t.identity()), want["identity"], "{case}");
                assert_eq!(json!(t.slot_id()), want["slot_id"], "{case}");
            }
            Err(e) => assert_eq!(error_json(&e), want["error"], "{case}"),
        }
    }
    Ok(())
}

#[test]
fn model_pattern_base_url_and_str_conversion_match_python() -> TestResult {
    let fx = fixtures()?;
    for case in arr(&fx["model_re"]) {
        let model = case["model"].as_str().unwrap_or("");
        assert_eq!(
            model_matches(model),
            case["matches"] == json!(true),
            "{case}"
        );
    }
    for case in arr(&fx["base_url"]) {
        let got = match normalize_base_url(Some(&case["value"])) {
            Ok(v) => json!({"ok": v}),
            Err(e) => json!({"error": error_json(&e)}),
        };
        // a missing value and null are the same to Python's `value or ""`
        assert_eq!(got, case["expected"], "{case}");
    }
    for case in arr(&fx["str_or_empty"]) {
        let v: Value = serde_json::from_str(case["json"].as_str().unwrap_or("null"))?;
        assert_eq!(
            json!(str_or_empty(Some(&v))),
            case["str_or_empty"],
            "{case}"
        );
    }
    Ok(())
}

#[test]
fn seed_and_state_path_match_python() -> TestResult {
    let fx = fixtures()?;
    for case in arr(&fx["seed_from_cfg"]) {
        let cfg = &case["cfg"];
        let s = seed_from_cfg(cfg);
        let want = &case["expected"];
        assert_eq!(target_json(&s.active), want["active"], "{case}");
        assert_eq!(
            s.next.as_ref().map_or(Value::Null, target_json),
            want["next"],
            "{case}"
        );
        assert!(s.previous.is_none() && want["previous"].is_null());
        let warnings: Vec<Value> = seed_warning(cfg)
            .into_iter()
            .map(|m| json!(["WARNING", m]))
            .collect();
        assert_eq!(json!(warnings), case["warnings"], "{case}");
    }
    for case in arr(&fx["state_path_for"]) {
        let got = match state_path_for(&case["cfg"]) {
            Ok(p) => json!({"ok": p.to_string_lossy()}),
            Err(_) => json!({"type_error": true}),
        };
        assert_eq!(got, case["expected"], "{case}");
    }
    Ok(())
}

// ---- addresses ----

#[test]
fn address_classification_matches_python() -> TestResult {
    let fx = fixtures()?;
    let reasons = arr(&fx["addresses"]["non_local_reason"]);
    assert!(reasons.len() >= 400, "{} addresses", reasons.len());
    for case in reasons {
        let got = non_local_reason(case["address"].as_str().unwrap_or(""));
        assert_eq!(json!(got), case["reason"], "{}", case["address"]);
    }
    for case in arr(&fx["addresses"]["ip_address"]) {
        let got = match ip_address(case["text"].as_str().unwrap_or("")) {
            None => Value::Null,
            Some(Ip::V4(v)) => json!([4, v.to_string()]),
            Some(Ip::V6(v)) => json!([6, v.to_string()]),
        };
        assert_eq!(got, case["ip"], "{}", case["text"]);
    }
    Ok(())
}

#[test]
fn check_embedding_address_matches_python() -> TestResult {
    let fx = fixtures()?;
    let table = fx["urls"]["resolver_table"].clone();
    let cases = arr(&fx["urls"]["cases"]);
    assert!(cases.len() >= 60);
    for case in cases {
        let calls = RefCell::new(Vec::<String>::new());
        let mut resolver = |host: &str| -> Result<Vec<String>, ResolveError> {
            calls.borrow_mut().push(host.to_owned());
            match table.get(host) {
                Some(Value::Array(list)) => Ok(list
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()),
                Some(Value::Object(o)) => Err(ResolveError {
                    type_name: o["raise"].as_str().unwrap_or("").to_owned(),
                }),
                _ => Ok(Vec::new()),
            }
        };
        let got = check_embedding_address(case["url"].as_str().unwrap_or(""), &mut resolver);
        assert_eq!(got.ok, case["ok"] == json!(true), "{case}");
        assert_eq!(json!(got.reason), case["reason"], "{case}");
        assert_eq!(got.transient, case["transient"] == json!(true), "{case}");
        assert_eq!(json!(*calls.borrow()), case["resolver_calls"], "{case}");
    }
    Ok(())
}

// ---- EmbeddingConfig ----

fn clean(v: &Value, dir: &str) -> TestResult<Value> {
    let text = serde_json::to_string(v)?.replace(dir, "<DIR>");
    Ok(serde_json::from_str(&text)?)
}

/// Masks the text of a Python/serde exception inside a damaged message.
fn mask(v: &mut Value) {
    match v {
        Value::String(s) => {
            let head = "The saved embedding configuration ";
            if s.starts_with(head) {
                if let (Some(a), Some(b)) =
                    (s.find(" cannot be read ("), s.rfind("). Fix or delete it."))
                {
                    let keep_head = s
                        .get(..a + " cannot be read (".len())
                        .unwrap_or("")
                        .to_owned();
                    let tail = s.get(b..).unwrap_or("").to_owned();
                    *s = format!("{keep_head}<EXC>{tail}");
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(mask),
        Value::Object(o) => o.values_mut().for_each(mask),
        _ => {}
    }
}

fn snapshot_json(s: &Snapshot) -> Value {
    s.to_json()
}

fn change_json(r: Result<Snapshot, ChangeError>) -> Value {
    match r {
        Ok(s) => json!({"ok": snapshot_json(&s)}),
        Err(ChangeError::Config(e)) => json!({"error": error_json(&e)}),
        Err(ChangeError::Io(_)) => json!({"os_error": true}),
    }
}

fn tmp_files(dir: &Path, out: &mut Vec<String>) {
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                tmp_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "tmp") {
                out.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
}

fn file_hex(path: Option<&Path>) -> Value {
    match path {
        Some(p) if p.is_file() => std::fs::read(p).map_or(Value::Null, |b| json!(hex(&b))),
        _ => Value::Null,
    }
}

fn expected_os_error(v: &mut Value) {
    if let Some(name) = v.get("os_error").cloned() {
        if name.is_string() {
            *v = json!({"os_error": true});
        }
    }
}

fn run_config_scenario(name: &str, spec: &Value) -> TestResult {
    let dir = tempfile::tempdir()?;
    let base = dir.path();
    let dir_text = base.to_string_lossy().into_owned();
    let state_file = spec["file"].as_str().map(|rel| base.join(rel));
    if let (Some(initial), Some(file)) = (spec["initial"].as_str(), &state_file) {
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if spec["initial_is_hex"] == json!(true) {
            std::fs::write(file, unhex(initial))?;
        } else {
            std::fs::write(file, initial)?;
        }
    }
    let masked = spec["masked_exception_text"] == json!(true);
    let check = |want: &Value, got: Value, what: &str| -> TestResult {
        let mut want = want.clone();
        let mut got = clean(&got, &dir_text)?;
        expected_os_error(&mut want);
        if masked {
            mask(&mut want);
            mask(&mut got);
        }
        assert_eq!(got, want, "{name}: {what}");
        Ok(())
    };
    let cfg = &spec["cfg"];
    let mut c = EmbeddingConfig::new(cfg, state_file.as_deref())?;
    let steps = arr(&spec["steps"]);
    check(&steps[0]["get"], snapshot_json(&c.get()), "init get")?;
    assert_eq!(
        file_hex(state_file.as_deref()),
        steps[0]["file"],
        "{name}: init file"
    );

    for (n, step) in steps.iter().enumerate().skip(1) {
        let op = &step["op"];
        let kind = op["op"].as_str().unwrap_or("");
        let result = match kind {
            "get" => json!({"ok": snapshot_json(&c.get())}),
            "set_next" => change_json(c.set_next(&op["raw"])),
            "clear_next" => change_json(c.clear_next()),
            "switch" => {
                let expected = &op["expected"];
                let target = if diary_retrieval::embedding::pystr::truthy(expected) {
                    match normalize_target(expected) {
                        Ok(t) => Some(t),
                        Err(e) => {
                            check(&step["result"], json!({"error": error_json(&e)}), "switch")?;
                            continue;
                        }
                    }
                } else {
                    None
                };
                change_json(c.switch_to_next(target.as_ref()))
            }
            "rollback" => change_json(c.rollback()),
            "finish" => change_json(c.finish()),
            "refuse" => {
                let r = &op["reasons"];
                let reasons = SeededReasons {
                    active: r["active"].as_str().map(str::to_owned),
                    next: r["next"].as_str().map(str::to_owned),
                };
                c.refuse_seeded(&reasons, op["transient"] == json!(true));
                json!({"ok": null})
            }
            "release" => {
                let which = if op["which"] == json!("active") {
                    Which::Active
                } else {
                    Which::Next
                };
                c.release_seeded(which);
                json!({"ok": null})
            }
            "reload" => {
                c = EmbeddingConfig::new(cfg, state_file.as_deref())?;
                json!({"ok": null})
            }
            "remove_file" => {
                if let Some(f) = &state_file {
                    std::fs::remove_file(f)?;
                }
                json!({"ok": null})
            }
            "make_blocker" => {
                if let Some(f) = &state_file {
                    if let Some(parent) = f.parent() {
                        std::fs::write(parent, "a file where the directory should be")?;
                    }
                }
                json!({"ok": null})
            }
            "make_dir" => {
                if let Some(f) = &state_file {
                    if f.exists() {
                        std::fs::remove_file(f)?;
                    }
                    std::fs::create_dir(f)?;
                }
                json!({"ok": null})
            }
            other => panic!("unknown op {other}"),
        };
        check(&step["result"], result, &format!("step {n} {op}"))?;
        check(
            &step["get"],
            snapshot_json(&c.get()),
            &format!("step {n} get"),
        )?;
        assert_eq!(
            file_hex(state_file.as_deref()),
            step["file"],
            "{name}: step {n} file bytes {op}"
        );
        let mut tmp = Vec::new();
        tmp_files(base, &mut tmp);
        assert_eq!(json!(tmp), step["tmp_left"], "{name}: step {n}");
    }
    Ok(())
}

#[test]
fn config_scenarios_match_python() -> TestResult {
    let fx = fixtures()?;
    let scenarios = fx["config"].as_object().ok_or("config")?;
    assert!(scenarios.len() >= 40);
    for (name, spec) in scenarios {
        run_config_scenario(name, spec)?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn the_saved_file_is_private_and_replaced_atomically() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("nested/embedding-config.json");
    let mut c = EmbeddingConfig::new(&json!({}), Some(&file))?;
    c.set_next(&json!({"model": "qa-embed-b"}))
        .map_err(|e| e.to_string())?;
    assert_eq!(
        std::fs::metadata(&file)?.permissions().mode() & 0o777,
        0o600
    );
    let mut left = Vec::new();
    tmp_files(dir.path(), &mut left);
    assert!(left.is_empty(), "{left:?}");
    Ok(())
}

// ---- guard ----

fn log_json(lines: &[LogLine]) -> Value {
    json!(lines
        .iter()
        .map(|l| {
            let level = match l.level {
                LogLevel::Info => "INFO",
                LogLevel::Warning => "WARNING",
                LogLevel::Error => "ERROR",
            };
            json!([level, l.message])
        })
        .collect::<Vec<_>>())
}

fn floats(v: &Value) -> Vec<f64> {
    arr(v).iter().filter_map(Value::as_f64).collect()
}

fn run_guard_scenario(name: &str, spec: &Value) -> TestResult {
    let dir = tempfile::tempdir()?;
    let dir_text = dir.path().to_string_lossy().into_owned();
    let file = dir.path().join("embedding-config.json");
    if let Some(initial) = spec["initial"].as_str() {
        std::fs::write(&file, initial)?;
    }
    let mut config = EmbeddingConfig::new(&spec["cfg"], Some(&file))?;
    let answers = &spec["answers"];
    let counters = RefCell::new(HashMap::<String, usize>::new());
    let calls = RefCell::new(Vec::<String>::new());
    let mut check = |url: &str| -> Result<AddressCheck, CheckFailure> {
        calls.borrow_mut().push(url.to_owned());
        let seq = arr(&answers[url]);
        let mut counters = counters.borrow_mut();
        let i = counters.entry(url.to_owned()).or_insert(0);
        let item = &seq[(*i).min(seq.len() - 1)];
        *i += 1;
        if let Some(kind) = item["raise"].as_str() {
            return Err(CheckFailure {
                type_name: kind.to_owned(),
            });
        }
        Ok(AddressCheck {
            ok: item["ok"] == json!(true),
            reason: item["reason"].as_str().unwrap_or("").to_owned(),
            transient: item["transient"] == json!(true),
        })
    };
    let mut sleeps = Vec::<f64>::new();
    let delays = floats(&spec["retry_delays"]);
    let steps = arr(&spec["steps"]);

    let first = &steps[0];
    let snapshot = config.get();
    let outcome = guard_seeded_addresses(
        &snapshot,
        &mut check,
        &mut |s| sleeps.push(s),
        &delays,
        RECHECK_S,
    );
    outcome.apply(&mut config);
    let mut queue: VecDeque<Pending> = outcome.rechecks.iter().cloned().collect();
    assert_eq!(
        clean(&snapshot_json(&config.get()), &dir_text)?,
        first["get"],
        "{name}: guard state"
    );
    assert_eq!(
        json!(*calls.borrow()),
        first["calls"],
        "{name}: guard checks"
    );
    assert_eq!(sleeps, floats(&first["sleeps"]), "{name}: sleeps");
    assert_eq!(
        vec![RECHECK_S; outcome.rechecks.len()],
        floats(&first["scheduled"]),
        "{name}: scheduled"
    );
    assert_eq!(log_json(&outcome.logs), first["logs"], "{name}: guard logs");
    calls.borrow_mut().clear();

    for (n, step) in steps.iter().enumerate().skip(1) {
        if let Some(admin) = step["step"].get("admin") {
            // an administrator's own choice: Python ignores a refusal here too
            let _ = config.set_next(&admin["raw"]);
            assert_eq!(
                clean(&snapshot_json(&config.get()), &dir_text)?,
                step["get"],
                "{name}: step {n}"
            );
            continue;
        }
        let Some(pending) = queue.pop_front() else {
            assert_eq!(step["queue_empty"], json!(true), "{name}: step {n}");
            continue;
        };
        let result = recheck_seeded(&mut config, &pending, &mut check);
        let scheduled = usize::from(result.next == RecheckNext::Again);
        if result.next == RecheckNext::Again {
            // Python reschedules `again`, a closure variable that the start-up loop has rebound to
            // the LAST held-back address by then (known difference 8): the step that just ran is
            // replaced by the last address's re-check. `fixed_rescheduling` below is the sound
            // loop; here the test reproduces Python so the recorded runs can be compared.
            queue.push_back(outcome.rechecks.last().cloned().unwrap_or(pending));
        }
        assert_eq!(
            clean(&snapshot_json(&config.get()), &dir_text)?,
            step["get"],
            "{name}: step {n} state"
        );
        assert_eq!(
            json!(*calls.borrow()),
            step["calls"],
            "{name}: step {n} checks"
        );
        assert_eq!(
            vec![RECHECK_S; scheduled],
            floats(&step["scheduled"]),
            "{name}: step {n} scheduled"
        );
        assert_eq!(json!(queue.len()), step["queue"], "{name}: step {n} queue");
        assert_eq!(
            log_json(&result.logs),
            step["logs"],
            "{name}: step {n} logs"
        );
        calls.borrow_mut().clear();
    }
    Ok(())
}

#[test]
fn guard_scenarios_match_python() -> TestResult {
    let fx = fixtures()?;
    let scenarios = fx["guard"].as_object().ok_or("guard")?;
    assert!(scenarios.len() >= 15);
    for (name, spec) in scenarios {
        run_guard_scenario(name, spec)?;
    }
    Ok(())
}

/// The sound scheduling loop: every held-back address is re-checked on its own until it is
/// released or refused for good. Python's closure bug (known difference 8) never re-checks the
/// first address again once the second is held back too.
#[test]
fn every_held_back_address_is_rechecked_on_its_own() -> TestResult {
    let fx = fixtures()?;
    let spec = &fx["guard"]["both_transient_released_apart"];
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("embedding-config.json");
    let mut config = EmbeddingConfig::new(&spec["cfg"], Some(&file))?;
    let counters = RefCell::new(HashMap::<String, usize>::new());
    let answers = &spec["answers"];
    let mut check = |url: &str| -> Result<AddressCheck, CheckFailure> {
        let mut counters = counters.borrow_mut();
        let i = counters.entry(url.to_owned()).or_insert(0);
        let seq = arr(&answers[url]);
        let item = &seq[(*i).min(seq.len() - 1)];
        *i += 1;
        Ok(AddressCheck {
            ok: item["ok"] == json!(true),
            reason: item["reason"].as_str().unwrap_or("").to_owned(),
            transient: item["transient"] == json!(true),
        })
    };
    let snapshot = config.get();
    let outcome = guard_seeded_addresses(
        &snapshot,
        &mut check,
        &mut |_| {},
        &floats(&spec["retry_delays"]),
        RECHECK_S,
    );
    outcome.apply(&mut config);
    assert_eq!(outcome.rechecks.len(), 2);
    let mut queue: VecDeque<Pending> = outcome.rechecks.iter().cloned().collect();
    let mut rounds = 0;
    while let Some(pending) = queue.pop_front() {
        rounds += 1;
        assert!(rounds < 40, "never settles");
        if recheck_seeded(&mut config, &pending, &mut check).next == RecheckNext::Again {
            queue.push_back(pending);
        }
    }
    let end = config.get();
    assert_eq!(end.damaged, None);
    assert_eq!(
        end.active.profile, "nomic-v1",
        "the active address was released"
    );
    assert!(end.next.is_some(), "the next address was released");
    Ok(())
}

// ---- documented differences ----

#[test]
fn known_differences_are_pinned() -> TestResult {
    let fx = fixtures()?;
    let cases = arr(&fx["known_differences"]);
    assert!(cases.len() >= 8);
    for case in cases {
        let name = case["name"].as_str().unwrap_or("");
        match case["kind"].as_str().unwrap_or("") {
            "normalize_target" => {
                let raw: Value = serde_json::from_str(case["json"].as_str().unwrap_or("null"))?;
                let got = match normalize_target(&raw) {
                    Ok(t) => json!({"ok": target_json(&t)}),
                    Err(e) => json!({"error": error_json(&e)}),
                };
                let rust = &case["rust"];
                let mut want = rust.clone();
                let alternative = want
                    .as_object_mut()
                    .and_then(|o| o.remove("also_accepted_message"));
                let accepted = got == want
                    || alternative.is_some_and(|m| {
                        let mut other = want.clone();
                        other["error"]["message"] = m;
                        got == other
                    });
                assert!(accepted, "{name}: got {got}, pinned {rust}");
                if alternative_free(rust) {
                    assert_ne!(
                        got, case["python"],
                        "{name} is no longer a difference: update the docs"
                    );
                }
            }
            "state_file" => {
                let dir = tempfile::tempdir()?;
                let file = dir.path().join("embedding-config.json");
                std::fs::write(&file, case["text"].as_str().unwrap_or(""))?;
                let c = EmbeddingConfig::new(&json!({}), Some(&file))?;
                assert_eq!(case["rust"], json!("damaged"));
                let snap = c.get();
                assert!(
                    snap.damaged.is_some() && snap.source == Source::Saved,
                    "{name}"
                );
                assert_eq!(snap.active, Target::new(None, "unavailable", "unavailable"));
                // Python loads it
                assert!(case["python"]["damaged"].is_null(), "{name}");
            }
            other => panic!("unknown kind {other}"),
        }
    }
    Ok(())
}

fn alternative_free(rust: &Value) -> bool {
    rust.get("also_accepted_message").is_none()
}
