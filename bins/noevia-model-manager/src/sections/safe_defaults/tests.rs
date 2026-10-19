#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
#![cfg(target_os = "linux")]
use super::*;
use crate::section_writes::{Outcome, SectionWrites};
use serde_json::Value as V;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
fn fixture() -> V {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/safe-defaults-python.v1.json"
    ))
    .unwrap()
}
fn order_fixture() -> V {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/safe-defaults-projector-order-python.v1.json"
    ))
    .unwrap()
}
fn raw_folder_names(models: &Path) -> Vec<String> {
    std::fs::read_dir(models.join("syn-folder"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}
fn ordered_case<'a>(original: &'a V, orders: &'a V, names: &[String]) -> &'a V {
    assert_eq!(names.len(), 4);
    for known in [
        "a-mmproj.gguf",
        "z-mmproj.gguf",
        "syn-folder-Q4.gguf",
        "z-syn-folder-draft-Q8.gguf",
    ] {
        assert_eq!(
            names.iter().filter(|name| name.as_str() == known).count(),
            1,
            "unexpected synthetic directory input"
        );
    }
    let a = names
        .iter()
        .position(|name| name == "a-mmproj.gguf")
        .unwrap();
    let z = names
        .iter()
        .position(|name| name == "z-mmproj.gguf")
        .unwrap();
    let variant = if a < z { "a-first" } else { "z-first" };
    let case = &orders["variants"][variant];
    for key in [
        "id", "steps", "files", "ini", "roles", "cap", "hard", "impl",
    ] {
        assert_eq!(case[key], original[key], "ordered fixture input {key}");
    }
    case
}
fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}
fn build(models: &Path, case: &V) {
    std::fs::create_dir(models).unwrap();
    if let Some(ini) = case["ini"].as_str() {
        std::fs::write(models.join("models.ini"), ini).unwrap();
    }
    for spec in case["files"].as_array().unwrap() {
        let path = models.join(spec[0].as_str().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let data = if spec[1].is_object() {
            hex(spec[1]["gguf"].as_str().unwrap())
        } else {
            spec[1].as_str().unwrap().as_bytes().to_vec()
        };
        std::fs::write(&path, data).unwrap();
        let file = std::fs::File::options().write(true).open(path).unwrap();
        if let Some(size) = spec[1]["size"].as_u64() {
            file.set_len(size).unwrap();
        }
        file.set_modified(UNIX_EPOCH + Duration::from_secs(spec[2].as_u64().unwrap()))
            .unwrap();
    }
}
fn shown(plan: &Plan) -> V {
    match plan {
        Plan::Refused(status, detail) => serde_json::json!({"status":status,"detail":detail}),
        Plan::Values(values, mtp) => {
            let mut map = serde_json::Map::new();
            for (k, v) in values.0.iter() {
                map.insert(k.to_owned(), V::String(v.to_owned()));
            }
            serde_json::json!({"status":200,"values":map,"mtp":mtp})
        }
    }
}
fn listing(root: &Path, prefix: &str, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if entry.file_type().unwrap().is_dir() {
            listing(&entry.path(), &rel, out)
        } else {
            out.push(rel)
        }
    }
}
#[tokio::test]
async fn actual_python_plans_responses_and_final_files() {
    let fx = fixture();
    let orders = order_fixture();
    for key in ["oracle", "epoch", "stamp"] {
        assert_eq!(fx[key], orders[key], "ordered fixture provenance {key}");
    }
    let mut keys: Vec<_> = orders["variants"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(keys, vec!["a-first", "z-first"]);
    for original in fx["cases"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models");
        build(&models, original);
        // Directory enumeration is an input, observed before native output exists.
        let before =
            (original["id"] == "folder-head-projector-priority").then(|| raw_folder_names(&models));
        let case = match before.as_deref() {
            Some(names) => ordered_case(original, &orders, names),
            None => original,
        };
        let ini = models.join("models.ini");
        let epoch = fx["epoch"].as_u64().unwrap();
        let writes = SectionWrites::with_clock(
            &ini,
            (
                case["cap"].as_i64().unwrap(),
                case["hard"].as_i64().unwrap(),
            ),
            Arc::new(move || UNIX_EPOCH + Duration::from_secs(epoch)),
        );
        writes.set_safe_models(&models);
        for (step, want) in case["steps"]
            .as_array()
            .unwrap()
            .iter()
            .zip(case["answers"].as_array().unwrap())
        {
            let raw = step["path"].as_str().unwrap().split('/').nth(4).unwrap();
            let name = crate::lossy_unquote(raw);
            let guard = writes.lock().await;
            let native = plan(&models, &ini, &name).unwrap();
            assert_eq!(
                shown(&native),
                serde_json::from_str::<V>(want["plan"]["body"].as_str().unwrap()).unwrap(),
                "{} plan {name}",
                case["id"]
            );
            let got = crate::section_writes::safe_defaults_name(&name)
                .err()
                .unwrap_or_else(|| writes.native_defaults(&name));
            match got {
                Outcome::Json(status, body) => {
                    assert_eq!(
                        u64::from(status),
                        want["status"].as_u64().unwrap(),
                        "{}",
                        case["id"]
                    );
                    assert_eq!(
                        String::from_utf8(body).unwrap(),
                        want["body"].as_str().unwrap(),
                        "{}",
                        case["id"]
                    );
                }
                Outcome::Failed => panic!("{} failed", case["id"]),
            }
            drop(guard);
            if let Some(names) = &before {
                assert_eq!(
                    &raw_folder_names(&models),
                    names,
                    "companion order changed during native replay"
                );
            }
        }
        assert_eq!(
            std::fs::read_to_string(&ini).ok().as_deref(),
            case["final"]["ini"].as_str(),
            "{}",
            case["id"]
        );
        if let Some(names) = &before {
            assert_eq!(
                &raw_folder_names(&models),
                names,
                "companion order changed after native replay"
            );
        }
        let mut names = Vec::new();
        listing(&models, "", &mut names);
        names.sort();
        assert_eq!(
            serde_json::json!(names),
            case["final"]["listing"],
            "{}",
            case["id"]
        );
    }
}
#[tokio::test]
async fn a_link_leaving_the_models_root_never_creates_a_section() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    let outside = dir.path().join("outside.gguf");
    std::fs::write(&outside, b"synthetic outside sentinel").unwrap();
    symlink(&outside, models.join("syn.gguf")).unwrap();
    let ini = models.join("models.ini");
    let writes = SectionWrites::new(&ini, (1024, 2048));
    writes.set_safe_models(&models);
    let guard = writes.lock().await;
    let Outcome::Json(status, body) = writes.native_defaults("syn") else {
        panic!("no refusal")
    };
    assert_eq!(status, 503);
    assert_eq!(
        body,
        b"{\"detail\":\"Native safe defaults are unavailable\"}"
    );
    assert!(!ini.exists());
    assert_eq!(
        std::fs::read(outside).unwrap(),
        b"synthetic outside sentinel"
    );
    drop(guard);
}
