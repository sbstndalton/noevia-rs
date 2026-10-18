//! Differential tests against the actual services Python (`tools/gen-model-list-oracle.py`).
//! Every case's tree, models.ini and store are rebuilt here from the fixture declaration; the
//! synthetic root adapter stands in for the Linux-only kernel-contained opener
//! (`tests/model_list.rs` runs the same cases through the real opener, the router and a child
//! stand-in on Linux).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use serde_json::Value;

/// sha256 of the services sources the fixture came from (noevia-services 0863a9d63b664e70f40daa32a2ec239d8e2cca2f).
pub(crate) const ORACLE: [(&str, &str); 13] = [
    (
        "api.py",
        "034885124b805d513dd43fbbdade52c1cc13ca225233c576d1b1d42e2ad64680",
    ),
    (
        "autoconfig.py",
        "15cc464f19cb43a68752572f6fa91046fe34c37296decef348a84dcbe75dc1c6",
    ),
    (
        "autoconfig_core.py",
        "d9da1c88fe6ef8db5a0f3784ca5e024436c110d338deed9ea04bffb7870e15ac",
    ),
    (
        "config.py",
        "4708a112769dbc3b9da6c49eeb7057e02d06868ca4af0e0bf4567971a267581a",
    ),
    (
        "db.py",
        "7ac62912f2295d0c96387f55c21e6f42e5815800c44a3920b7ebc2f3a0217478",
    ),
    (
        "discover.py",
        "f92603267381ea40976fb9642034d18ee24048769f1dc2ab7e68aeac9d97b725",
    ),
    (
        "downloader.py",
        "b279254f87769a41643dcc7f5674ee6f5372f649a445d94b796313de4218edff",
    ),
    (
        "gguf_meta.py",
        "8ac984794fb9087fcd774cfe1a4e83eac847c1675224c8c09029fb9dcbd43b0a",
    ),
    (
        "helpers.py",
        "fcf2ccb14e5e068f97bb7a4b7985ab7f4f4855b940eb6d9e3838f239ad97918d",
    ),
    (
        "ini.py",
        "b3cb05fd382b5f408ad16430db8841d40c82a3714115233f2e35b2f7f3b61740",
    ),
    (
        "main.py",
        "badbfee8359e5a64b18d05e16c5ad48c800d18d7437cc871c4d002a88fb7e994",
    ),
    (
        "services.py",
        "8da3d667ce32729fba21c93b52968ef42a4ee77fab24f9412837f781a9bf83e8",
    ),
    (
        "utils.py",
        "3f56fb6b04094ec19c09c191a0d4267e2d233e3461884be0648e4875e6ef6426",
    ),
];

/// The generator's default disk numbers (`DISK`).
const DISK: (u128, u128) = (4_000_000_000_000, 1_234_567_890_123);

pub(crate) fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../tests/fixtures/model-list-python.v1.json"
    ))
    .unwrap()
}

/// Builds one case like the generator: the tree (model detail's builder), an ini outside the
/// models folder when the case has one, and the store. Returns (models, ini, data).
pub(crate) fn build(root: &Path, case: &Value) -> (PathBuf, PathBuf, PathBuf) {
    let (models, mut ini) = crate::model_detail::tests::build(root, case);
    if let Some(text) = case["ini_outside"]["text"].as_str() {
        ini = root.join("models.ini");
        std::fs::write(&ini, text).unwrap();
    }
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let statements: Vec<&str> = case["store"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    let db = Connection::open(data.join("model_loader.db")).unwrap();
    db.execute_batch(&statements.join("\n")).unwrap();
    (models, ini, data)
}

/// The case's `shutil.disk_usage` stand-in.
pub(crate) fn disk_of(case: &Value) -> DiskUsage {
    let numbers = match case["disk"].as_array() {
        Some(pair) => {
            let n = |v: &Value| -> u128 {
                v.as_u64().map(u128::from).unwrap_or_else(|| {
                    // Python's ints past u64 come through serde_json as floats; the fixture keeps
                    // such numbers exact as JSON integers, read back from their text.
                    v.to_string().parse().unwrap()
                })
            };
            (n(&pair[0]), n(&pair[1]))
        }
        None => DISK,
    };
    Arc::new(move |_: &Path| Ok(numbers))
}

/// The body with the case's models folder in place of `@MODELS@`.
pub(crate) fn expected(body: &str, models: &Path) -> String {
    let path = serde_json::to_string(models.to_str().unwrap()).unwrap();
    body.replace("@MODELS@", &path[1..path.len() - 1])
}

/// `Ok` or a panic naming the stop.
fn ok<T>(result: Result<T, Stop>) -> T {
    match result {
        Ok(value) => value,
        Err(Stop::Failed) => panic!("failed"),
        Err(Stop::Refused) => panic!("refused"),
    }
}

fn refuses(case: &Value, route: &str) -> bool {
    case["native_refuses"]
        .as_array()
        .is_some_and(|routes| routes.iter().any(|r| r == route))
}

/// (status, body) of the native list with the recorded child answer.
fn models_answer(case: &Value, models: &Path, ini: &Path, data: &Path) -> Option<(u16, String)> {
    let cache = Mutex::new(HashMap::new());
    let deadline = Instant::now() + DEADLINE;
    let listed = match list(
        models,
        ini,
        &disk_of(case),
        deadline,
        Root::synthetic,
        &cache,
    ) {
        Ok(listed) => listed,
        Err(Stop::Failed) => return Some((500, "Internal Server Error".into())),
        Err(Stop::Refused) => return None,
    };
    // The store is read after the child answered; a store failure is Python's 500, for which
    // the generator records no child answer.
    let badges = match badges(
        &data.join("model_loader.db"),
        &listed.keys,
        Instant::now() + STORE_DEADLINE,
    ) {
        Ok(badges) => badges,
        Err(Stop::Failed) => return Some((500, "Internal Server Error".into())),
        Err(Stop::Refused) => return None,
    };
    let runtime = &case["results"]["models_runtime"];
    let sizes: Vec<u128> = runtime["sizes"]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "{}: native answers, python {}",
                case["id"], case["results"]["models"]["status"]
            )
        })
        .iter()
        .map(|v| u128::from(v.as_u64().unwrap()))
        .collect();
    assert_eq!(listed.sizes(), sizes, "{}", case["id"]);
    match splice_models(
        &listed,
        runtime["body"].as_str().unwrap().as_bytes(),
        &badges,
    ) {
        Outcome::Json(body) => Some((200, String::from_utf8(body).unwrap())),
        other => panic!("{other:?} {}", case["id"]),
    }
}

/// (status, body) of the native overview with the recorded child answer.
fn overview_answer(case: &Value, models: &Path, ini: &Path) -> Option<(u16, String)> {
    let host = case["host_path"].as_str().unwrap_or_default();
    let counted = match count(models, ini, host, &disk_of(case), Root::synthetic) {
        Ok(counted) => counted,
        Err(Stop::Failed) => return Some((500, "Internal Server Error".into())),
        Err(Stop::Refused) => return None,
    };
    let runtime = case["results"]["overview_runtime"].as_str().unwrap();
    match splice_overview(&counted, runtime.as_bytes(), None) {
        Outcome::Json(body) => Some((200, String::from_utf8(body).unwrap())),
        other => panic!("{other:?} {}", case["id"]),
    }
}

#[test]
fn fixture_comes_from_the_pinned_services_sources() {
    let fixture = fixture();
    let oracle = &fixture["oracle"];
    for (name, sha) in ORACLE {
        assert_eq!(oracle[name], sha, "{name}");
    }
    assert_eq!(oracle.as_object().unwrap().len(), ORACLE.len());
    assert!(fixture["cases"].as_array().unwrap().len() >= 25);
}

#[test]
fn every_case_matches_python_byte_for_byte() {
    let fixture = fixture();
    for case in fixture["cases"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let (models, ini, data) = build(dir.path(), case);
        let results = &case["results"];
        for (route, answer) in [
            ("models", models_answer(case, &models, &ini, &data)),
            ("overview", overview_answer(case, &models, &ini)),
        ] {
            let python = &results[route];
            let Some((status, body)) = answer else {
                assert!(
                    refuses(case, route),
                    "unexpected refusal {} {route}",
                    case["id"]
                );
                assert_eq!(python["status"], 200, "{}", case["id"]);
                continue;
            };
            assert!(
                !refuses(case, route),
                "{} {route} should refuse",
                case["id"]
            );
            assert_eq!(python["status"], status, "{} {route}", case["id"]);
            let want = expected(python["body"].as_str().unwrap(), &models);
            assert_eq!(body, want, "{} {route}", case["id"]);
            let content_type = python["content_type"].as_str().unwrap();
            if status == 200 {
                assert_eq!(content_type, "application/json", "{}", case["id"]);
            } else {
                assert_eq!(content_type, "text/plain; charset=utf-8", "{}", case["id"]);
            }
        }
    }
}

#[test]
fn the_fixture_covers_what_it_claims() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap();
    let body = |id: &str, route: &str| -> String {
        let case = cases.iter().find(|c| c["id"] == id).unwrap();
        case["results"][route]["body"].as_str().unwrap().to_owned()
    };
    // Non-finite chips are null (pydantic's JSON mode), as the child renders them too.
    assert!(body("loaded-on-and-fit", "models").contains(r#""vram_gb":null"#));
    // Two rows named the same in different folders share their badges by display name.
    assert!(body("subfolders-shards-companions", "models").contains("subfolder copy"));
    assert!(body("disk-huge-numbers", "overview").contains(r#""usedPct":100.0"#));
    assert!(body("disk-rounding-ties", "overview").contains(r#""usedPct":0.1"#));
    assert!(body("no-backends-host-path-and-jobs", "overview").contains(r#""activeDownloads":3"#));
    for id in [
        "badge-mixed-types-500",
        "badge-zero-against-text-500",
        "no-badge-table-500",
    ] {
        let case = cases.iter().find(|c| c["id"] == id).unwrap();
        assert_eq!(case["results"]["models"]["status"], 500, "{id}");
        assert_eq!(case["results"]["overview"]["status"], 200, "{id}");
    }
}

#[test]
fn shapes_are_cached_by_path_mtime_and_size_and_errors_are_not() {
    let fixture = fixture();
    let case = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "flat-models-shapes-and-badges")
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (models, ini, _) = build(dir.path(), case);
    let cache = Mutex::new(HashMap::new());
    let first = ok(list(
        &models,
        &ini,
        &disk_of(case),
        Instant::now() + DEADLINE,
        Root::synthetic,
        &cache,
    ));
    // Parsed headers are cached; headers that do not parse are not (Python caches no error).
    let cached = |name: &str| {
        cache
            .lock()
            .unwrap()
            .keys()
            .any(|(path, _, _)| path.ends_with(name))
    };
    for name in ["alpha-Q8_0.gguf", "no-arch.gguf", "summary-raises.gguf"] {
        assert!(cached(name), "{name}");
    }
    for name in ["bad-magic.gguf", "empty.gguf"] {
        assert!(!cached(name), "{name}");
    }
    // A cached header is not read again: rewrite one with the same size and mtime.
    let path = models.join("moe-no-used.gguf");
    let meta = std::fs::metadata(&path).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    for b in bytes.iter_mut().skip(4) {
        *b = 0;
    }
    std::fs::write(&path, &bytes).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(meta.modified().unwrap()))
        .unwrap();
    let again = ok(list(
        &models,
        &ini,
        &disk_of(case),
        Instant::now() + DEADLINE,
        Root::synthetic,
        &cache,
    ));
    assert_eq!(first, again);
    // A new mtime is a new key: the header is read again (now without an architecture).
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))
        .unwrap();
    let fresh = ok(list(
        &models,
        &ini,
        &disk_of(case),
        Instant::now() + DEADLINE,
        Root::synthetic,
        &cache,
    ));
    let entry = fresh
        .entries
        .iter()
        .find(|e| e.display == "moe-no-used.gguf")
        .unwrap();
    assert!(entry.head.ends_with("\"shape\":null,"), "{}", entry.head);
}

#[test]
fn a_full_cache_starts_over() {
    let cache = Mutex::new(HashMap::new());
    for i in 0..SHAPE_CACHE {
        cache
            .lock()
            .unwrap()
            .insert((PathBuf::from(i.to_string()), 0, 0), String::new());
    }
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    // A GGUF v3 header with one key: general.architecture = "synth".
    let header = "474755460300000000000000000000000100000000000000140000000000000067656e6572616c2e61726368697465637475726508000000050000000000000073796e7468";
    let bytes: Vec<u8> = (0..header.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&header[i..i + 2], 16).unwrap())
        .collect();
    std::fs::write(models.join("a.gguf"), bytes).unwrap();
    let listed = ok(list(
        &models,
        &models.join("models.ini"),
        &statvfs_usage(),
        Instant::now() + DEADLINE,
        Root::synthetic,
        &cache,
    ));
    assert_eq!(listed.entries.len(), 1);
    assert_eq!(cache.lock().unwrap().len(), 1);
}

#[test]
fn a_past_deadline_refuses_instead_of_answering_partly() {
    let fixture = fixture();
    let case = &fixture["cases"][0];
    let dir = tempfile::tempdir().unwrap();
    let (models, ini, _) = build(dir.path(), case);
    let cache = Mutex::new(HashMap::new());
    assert!(matches!(
        list(
            &models,
            &ini,
            &disk_of(case),
            Instant::now(),
            Root::synthetic,
            &cache
        ),
        Err(Stop::Refused)
    ));
}

#[test]
fn a_disk_usage_error_or_impossible_numbers_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    let ini = models.join("models.ini");
    let failing: DiskUsage = Arc::new(|_: &Path| Err(()));
    assert!(matches!(
        count(&models, &ini, "", &failing, Root::synthetic),
        Err(Stop::Refused)
    ));
    let backwards: DiskUsage = Arc::new(|_: &Path| Ok((10, 11)));
    assert!(matches!(
        count(&models, &ini, "", &backwards, Root::synthetic),
        Err(Stop::Refused)
    ));
    let zero: DiskUsage = Arc::new(|_: &Path| Ok((0, 0)));
    assert!(matches!(
        count(&models, &ini, "", &zero, Root::synthetic),
        Err(Stop::Failed)
    ));
    // The list takes the disk usage too: an error is not shown there, a zero size raises.
    let cache = Mutex::new(HashMap::new());
    let deadline = Instant::now() + DEADLINE;
    assert!(list(&models, &ini, &failing, deadline, Root::synthetic, &cache).is_ok());
    assert!(list(&models, &ini, &backwards, deadline, Root::synthetic, &cache).is_ok());
    assert!(matches!(
        list(&models, &ini, &zero, deadline, Root::synthetic, &cache),
        Err(Stop::Failed)
    ));
    // A missing folder never asks for the disk.
    let missing = dir.path().join("missing");
    assert!(list(&missing, &ini, &zero, deadline, Root::synthetic, &cache).is_ok());
    let Ok(counted) = count(&missing, &ini, "/srv", &failing, Root::synthetic) else {
        panic!("a missing folder is answered");
    };
    assert!(counted.models_dir.contains(MISSING));
}

#[test]
fn integer_division_rounds_once_like_python() {
    // (numerator, denominator, bits of Python's numerator / denominator)
    let cases: [(u128, u128, u64); 17] = [
        (1, 3, 0x3fd5555555555555),
        (2, 3, 0x3fe5555555555555),
        (10, 3, 0x400aaaaaaaaaaaab),
        (276543210987700, 4000000000000, 0x405148b0fe012ffd),
        (
            1844674297419792383600,
            18446744073709551615,
            0x4058ffffe7000000,
        ),
        (
            170141183460469231731687303715884105729,
            3,
            0x47c5555555555555,
        ),
        (
            170141183460469231731687303715884105727,
            170141183460469231731687303715884105728,
            0x3ff0000000000000,
        ),
        (
            1,
            170141183460469231731687303715884105728,
            0x3800000000000000,
        ),
        (3, u128::MAX, 0x3808000000000000),
        (300000000000000600, 3000000000000007, 0x4058fffffffffffe),
        (18446744073709551617, 1, 0x43f0000000000000),
        (36893488147419103235, 2, 0x43f0000000000000),
        (99, 100, 0x3fefae147ae147ae),
        (1, 10, 0x3fb999999999999a),
        (7, 7, 0x3ff0000000000000),
        (9007199254740993, 1, 0x4340000000000000),
        (18014398509481987, 4, 0x4330000000000001),
    ];
    for (n, d, bits) in cases {
        assert_eq!(true_divide(n, d).to_bits(), bits, "{n} / {d}");
    }
    assert_eq!(true_divide(0, 5), 0.0);
}

#[test]
fn python_comparisons_of_stored_values() {
    use std::cmp::Ordering;
    assert_eq!(int_float(2, 2.5), Some(Ordering::Less));
    assert_eq!(int_float(3, 2.5), Some(Ordering::Greater));
    assert_eq!(int_float(-3, -2.5), Some(Ordering::Less));
    assert_eq!(int_float(2, 2.0), Some(Ordering::Equal));
    assert_eq!(
        int_float(i64::MAX, 9_223_372_036_854_775_808.0),
        Some(Ordering::Less)
    );
    assert_eq!(
        int_float(i64::MIN, -9_223_372_036_854_775_808.0),
        Some(Ordering::Equal)
    );
    assert_eq!(int_float(0, f64::NAN), None);
    assert_eq!(int_float(0, f64::INFINITY), Some(Ordering::Less));
    assert!(same_key(&Val::Int(1), &Val::Real(1.0)));
    assert!(!same_key(&Val::Text("1".into()), &Val::Int(1)));
    assert!(!same_key(&Val::Text("a".into()), &Val::Blob(b"a".to_vec())));
    let row = |v: Val| Row(vec![("created_at".to_owned(), v)]);
    let newer = |a: &Row, b: &Row| newer(a, b).ok();
    assert_eq!(newer(&row(Val::Real(2.5)), &row(Val::Int(2))), Some(true));
    assert_eq!(newer(&row(Val::Int(2)), &row(Val::Real(2.5))), Some(false));
    assert_eq!(newer(&row(Val::Null), &row(Val::Real(0.0))), Some(false));
    assert_eq!(
        newer(&row(Val::Text(String::new())), &row(Val::Int(0))),
        Some(false)
    );
    assert_eq!(
        newer(&row(Val::Text("b".into())), &row(Val::Text("a".into()))),
        Some(true)
    );
    assert_eq!(
        newer(
            &row(Val::Blob(b"b".to_vec())),
            &row(Val::Blob(b"a".to_vec()))
        ),
        Some(true)
    );
    assert_eq!(newer(&row(Val::Text("a".into())), &row(Val::Int(1))), None);
    assert_eq!(
        newer(&row(Val::Blob(b"a".to_vec())), &row(Val::Text("a".into()))),
        None
    );
    assert_eq!(newer(&Row(Vec::new()), &row(Val::Int(1))), None);
}

fn listed_two() -> Listed {
    Listed {
        entries: vec![
            Listing {
                head: "{\"key\":\"a\",".into(),
                model_id: "a".into(),
                display: "a.gguf".into(),
                bytes: 1,
            },
            Listing {
                head: "{\"key\":\"b\",".into(),
                model_id: "b".into(),
                display: "b.gguf".into(),
                bytes: 340282366920938463463374607431768211455,
            },
        ],
        keys: Vec::new(),
        unregistered: "[]".into(),
        revision: "r".into(),
    }
}

#[test]
fn the_runtime_request_names_every_size_in_list_order() {
    assert_eq!(
        runtime_request(&listed_two().sizes()),
        "{\"bytes\":[1,340282366920938463463374607431768211455]}"
    );
    assert_eq!(runtime_request(&[]), "{\"bytes\":[]}");
}

#[test]
fn a_runtime_answer_of_the_wrong_shape_is_never_guessed_at() {
    let listed = listed_two();
    let badges = HashMap::new();
    for body in [
        "",
        "[]",
        "{}",
        r#"{"loaded":{}}"#,
        r#"{"fit":[[],[]]}"#,
        r#"{"loaded":{},"fit":[[]]}"#,
        r#"{"loaded":{},"fit":[[],[],[]]}"#,
        r#"{"loaded":{},"fit":[[],{}]}"#,
        r#"{"loaded":{"a":"x"},"fit":[[],[]]}"#,
        r#"{"loaded":[],"fit":[[],[]]}"#,
    ] {
        assert_eq!(
            splice_models(&listed, body.as_bytes(), &badges),
            Outcome::Unavailable,
            "{body}"
        );
    }
    let ok = splice_models(
        &listed,
        br#"{"loaded":{"b":["x","y"]},"fit":[[{"vram_gb":1e+16}],[]]}"#,
        &HashMap::from([("a.gguf".to_owned(), "[{\"id\":1}]".to_owned())]),
    );
    assert_eq!(
        ok,
        Outcome::Json(
            br#"{"models":[{"key":"a","loadedOn":[],"fit":[{"vram_gb":1e+16}],"badges":[{"id":1}]},{"key":"b","loadedOn":["x","y"],"fit":[],"badges":[]}],"unregistered":[],"revision":"r"}"#
                .to_vec()
        )
    );
    let counted = Counted {
        models_dir: "{}".into(),
        models: 1,
        sections: 2,
        revision: "r".into(),
    };
    for body in [
        "",
        "{}",
        r#"{"backends":[]}"#,
        r#"{"activeDownloads":1}"#,
        r#"{"backends":{},"activeDownloads":1}"#,
        r#"{"backends":[],"activeDownloads":-1}"#,
        r#"{"backends":[],"activeDownloads":1.0}"#,
        r#"{"backends":[],"activeDownloads":"1"}"#,
    ] {
        assert_eq!(
            splice_overview(&counted, body.as_bytes(), None),
            Outcome::Unavailable,
            "{body}"
        );
    }
}

#[test]
fn the_native_download_count_replaces_the_childs() {
    let counted = Counted {
        models_dir: "{}".into(),
        models: 1,
        sections: 2,
        revision: "r".into(),
    };
    let body = br#"{"backends":[{"name":"x"}],"activeDownloads":0}"#;
    assert_eq!(
        splice_overview(&counted, body, Some(4)),
        Outcome::Json(
            br#"{"modelsDir":{},"models":1,"sections":2,"backends":[{"name":"x"}],"activeDownloads":4,"revision":"r"}"#.to_vec()
        )
    );
    assert_eq!(
        splice_overview(&counted, body, None),
        Outcome::Json(
            br#"{"modelsDir":{},"models":1,"sections":2,"backends":[{"name":"x"}],"activeDownloads":0,"revision":"r"}"#.to_vec()
        )
    );
}

#[test]
fn only_the_exact_get_paths_of_enabled_routes_are_native() {
    use axum::http::Method;
    let settings = |data: bool, host: bool| Settings {
        models: "/m".into(),
        ini: "/m/models.ini".into(),
        data: data.then(|| PathBuf::from("/d")),
        host_path: host.then(String::new),
    };
    let both = ModelList::new(settings(true, true), statvfs_usage());
    assert_eq!(
        both.route(&Method::GET, "/api/v1/models"),
        Some(Route::Models)
    );
    assert_eq!(
        both.route(&Method::GET, "/api/v1/overview"),
        Some(Route::Overview)
    );
    for (method, path) in [
        (Method::POST, "/api/v1/models"),
        (Method::HEAD, "/api/v1/models"),
        (Method::PUT, "/api/v1/overview"),
        (Method::GET, "/api/v1/models/"),
        (Method::GET, "/api/v1/overview/"),
        (Method::GET, "/api/v1/Models"),
        (Method::GET, "/api/v1/models/detail"),
        (Method::GET, "/api/v1/models%2F"),
    ] {
        assert_eq!(both.route(&method, path), None, "{method} {path}");
    }
    let list_only = ModelList::new(settings(true, false), statvfs_usage());
    assert_eq!(list_only.route(&Method::GET, "/api/v1/overview"), None);
    let overview_only = ModelList::new(settings(false, true), statvfs_usage());
    assert_eq!(overview_only.route(&Method::GET, "/api/v1/models"), None);
}

#[test]
fn statvfs_reads_the_folder_like_shutil() {
    let dir = tempfile::tempdir().unwrap();
    let (total, free) = statvfs_usage()(dir.path()).unwrap();
    assert!(total > 0 && free <= total);
    assert!(statvfs_usage()(&dir.path().join("missing")).is_err());
}

#[test]
fn the_revision_is_the_sha256_of_the_file_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("models.ini");
    assert_eq!(
        revision(&ini).ok().as_deref(),
        Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
    );
    std::fs::write(&ini, b"[a]\r\n").unwrap();
    // Raw bytes: the CRLF is hashed as written, not as the parser reads it.
    let raw: String = {
        use sha2::{Digest, Sha256};
        Sha256::digest(b"[a]\r\n")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    assert_eq!(revision(&ini).ok(), Some(raw));
    std::fs::remove_file(&ini).unwrap();
    std::fs::create_dir(&ini).unwrap();
    assert!(matches!(revision(&ini), Err(Stop::Failed)));
}

/// A list stuck on a hung read holds only the list's slot: the overview has its own.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_held_list_slot_never_holds_up_the_overview() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    std::fs::create_dir(&models).unwrap();
    let list = ModelList::new(
        Settings {
            models: models.clone(),
            ini: models.join("models.ini"),
            data: Some(dir.path().join("data")),
            host_path: Some(String::new()),
        },
        statvfs_usage(),
    );
    let _held = list.reads.clone().try_acquire_owned().unwrap();
    let client = reqwest::Client::new();
    // Nothing listens on port 9: the overview's file part runs, then its child call fails.
    let child = || Child {
        #[cfg(feature = "native-backends")]
        live: None,
        client: &client,
        base: "http://127.0.0.1:9",
        token: "",
    };
    let overview = tokio::time::timeout(Duration::from_secs(10), list.overview(child(), None))
        .await
        .unwrap();
    assert!(matches!(overview, Outcome::Unavailable));
    // The list itself still waits for its slot.
    let waiting = tokio::time::timeout(Duration::from_millis(300), list.models(child())).await;
    assert!(waiting.is_err());
}

#[test]
fn the_badge_rows_are_bounded_in_total_bytes() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE t(a TEXT, b BLOB); \
         INSERT INTO t VALUES('abcd', x'0102'); INSERT INTO t VALUES('ef', NULL);",
    )
    .unwrap();
    // 4 + 2 + 2 bytes of text and blob in all.
    assert_eq!(
        read_rows(&db, "SELECT * FROM t", 8).ok().map(|r| r.len()),
        Some(2)
    );
    assert!(matches!(
        read_rows(&db, "SELECT * FROM t", 7),
        Err(Stop::Failed)
    ));
}
