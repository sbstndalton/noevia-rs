//! Replays `fixtures/node-engine-facts.v1.json` (written by `record_engine_facts.cjs` from
//! noevia-core's own modules and index.cjs's own lines) through the crate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use engine_facts::code::{self, Answer, CodeEnv, Deployment, Inputs};
use engine_facts::{provider, shared};
use js_json::{wtf16, JValue};

fn fixture() -> JValue {
    let text = include_str!("fixtures/node-engine-facts.v1.json");
    wtf16::parse(text).expect("the fixture parses")
}

fn list<'a>(f: &'a JValue, key: &str) -> &'a [JValue] {
    match f.get(key) {
        JValue::Arr(items) if !items.is_empty() => items,
        _ => panic!("the fixture has no {key} cases"),
    }
}

fn show(v: &JValue) -> String {
    wtf16::stringify(v).unwrap_or_else(|| "undefined".into())
}

/// The JSON a case recorded, as Node wrote it (`undefined` fields dropped).
fn json(v: &JValue) -> JValue {
    wtf16::parse(&show(v)).unwrap_or(JValue::Undefined)
}

#[test]
fn the_fixture_names_its_sources_and_the_cut_lines() {
    let f = fixture();
    for file in [
        "server/index.cjs",
        "server/providers.cjs",
        "server/shared-context.cjs",
        "server/rust-code-executor.cjs",
        "server/wasm/dav-parse.wasm",
    ] {
        assert_eq!(
            f.get("sources").get(file).as_str().map(str::len),
            Some(64),
            "{file}"
        );
    }
    for block in ["codeEngine", "codeRoleEndpoint", "bridge", "loadChats"] {
        assert!(f.get("blocks").get(block).is_object(), "{block}");
    }
    assert_eq!(f.get("differences"), &JValue::Arr(Vec::new()));
}

#[test]
fn engine_capabilities_match_node() {
    for c in list(&fixture(), "engineCaps") {
        let kind = match c.get("kind") {
            JValue::Str(s) => s.as_str(),
            _ => "undefined",
        };
        let want = c.get("out");
        let got = provider::engine_capabilities(kind).unwrap_or(JValue::Null);
        assert_eq!(&got, want, "kind {kind:?}");
    }
}

#[test]
fn effective_capabilities_match_node() {
    for c in list(&fixture(), "effective") {
        let got = provider::effective_capabilities(c.get("row"));
        assert_eq!(&got, c.get("out"), "row {}", show(c.get("row")));
    }
}

#[test]
fn the_lookup_matches_node() {
    for c in list(&fixture(), "lookup") {
        let JValue::Arr(view) = c.get("view") else {
            panic!("a view")
        };
        let default_id = c.get("defaultId").as_str().expect("a default id");
        let got = provider::get_provider(view, c.get("id"), default_id).unwrap_or(JValue::Null);
        assert_eq!(
            show(&got),
            show(c.get("out")),
            "id {} in {}",
            show(c.get("id")),
            show(c.get("view"))
        );
    }
}

#[test]
fn the_headers_match_node() {
    for c in list(&fixture(), "headers") {
        let got = provider::headers(c.get("provider"), c.get("extra")).expect("headers");
        assert_eq!(show(&got), show(c.get("out")), "{}", show(c));
    }
}

#[test]
fn the_external_verdict_matches_node() {
    for c in list(&fixture(), "external") {
        let row = c.get("row");
        let row = (!row.is_nullish()).then_some(row);
        assert_eq!(
            JValue::Bool(provider::is_external(row)),
            *c.get("out"),
            "row {}",
            show(c.get("row"))
        );
    }
}

#[test]
fn the_code_target_matches_node() {
    for c in list(&fixture(), "target") {
        let mut asked = Vec::new();
        let got = {
            let asked = std::cell::RefCell::new(&mut asked);
            code::code_target(
                |role: &str| {
                    asked
                        .borrow_mut()
                        .push(JValue::Str(format!("target:{role}")));
                    let t = c.get(role);
                    if t.as_str() == Some("throw") {
                        Err("machinesUnavailable")
                    } else {
                        Ok(t.clone())
                    }
                },
                || {
                    asked.borrow_mut().push(JValue::Str("autoRoles".into()));
                    let r = c.get("roles");
                    (r.as_str() != Some("throw")).then(|| r.clone())
                },
            )
        };
        match got {
            Ok(v) => assert_eq!(&v, c.get("out"), "{}", show(c)),
            Err(code) => assert_eq!(c.get("throws").as_str(), Some(code), "{}", show(c)),
        }
        assert_eq!(&JValue::Arr(asked), c.get("calls"), "{}", show(c));
    }
}

#[test]
fn for_code_matches_node() {
    for c in list(&fixture(), "forCode") {
        let project = c.get("project");
        let got = shared::load_chats(project).and_then(|chats| shared::for_code(project, &chats));
        match got {
            Ok(text) => assert_eq!(
                Some(text.as_str()),
                c.get("out").as_str(),
                "{}",
                show(project)
            ),
            Err(_) => assert!(c.get("throws").as_str().is_some(), "{}", show(project)),
        }
    }
}

#[test]
fn the_bridge_answer_matches_node() {
    let f = fixture();
    let cases = list(&f, "bridge");
    let mut answered = 0;
    for c in cases {
        let env = c.get("env");
        let code_env = c.get("codeEnv");
        let lookup = |src: &JValue| {
            let src = src.clone();
            move |k: &str| src.get(k).as_str().map(str::to_string)
        };
        let deployment = Deployment::from_lookup(&lookup(env));
        let code_env = CodeEnv::from_lookup(&lookup(code_env));
        let provider = deployment.provider();
        let JValue::Arr(features) = c.get("features") else {
            panic!("features")
        };
        let flag = |name: &str| features.iter().any(|f| f.as_str() == Some(name));
        let inputs = Inputs {
            env: &code_env,
            provider: &provider,
            external: provider::is_external(Some(&provider)),
            roles: c.get("roles"),
            last_loaded: c.get("lastLoaded"),
            role_wanted: flag("codePipeline") || flag("plannerReview"),
        };
        let project = c.get("project");
        let project = project.is_object().then_some(project);
        let got = code::answer(
            project,
            &inputs,
            |role: &str| {
                let t = c.get(role);
                if t.as_str() == Some("throw") {
                    Err(())
                } else {
                    Ok(t.clone())
                }
            },
            || c.get("paused").clone(),
        );
        assert_eq!(c.get("handled"), &JValue::Bool(true));
        let (status, body) = match &got {
            Answer::NotFound => (
                404,
                JValue::obj([("error", JValue::from("project not found"))]),
            ),
            Answer::Unavailable(_) => (
                503,
                JValue::obj([("error", JValue::from("Code engine facts are unavailable."))]),
            ),
            Answer::Facts(facts) => {
                answered += 1;
                (200, json(facts))
            }
        };
        assert_eq!(
            c.get("status").as_num(),
            Some(f64::from(status)),
            "{}",
            show(c)
        );
        assert_eq!(show(&body), show(c.get("answer")), "{}", show(c));
    }
    assert!(answered > cases.len() / 2);
}

#[test]
fn the_default_row_is_the_deployment_s() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| (*v).to_string())
        }
    };
    let d = Deployment::from_lookup(&env(&[]));
    assert_eq!(
        show(&d.provider()),
        r#"{"id":"default","label":"Local inference","baseUrl":"http://host.docker.internal:11434","apiKey":"","shared":true,"capabilities":{}}"#
    );
    let d = Deployment::from_lookup(&env(&[
        ("LEMONADE_BASE_URL", "http://qa-lemonade.invalid"),
        ("INFERENCE_API_KEY", ""),
    ]));
    assert_eq!(d.kind, "lemonade");
    let d = Deployment::from_lookup(&env(&[("MODEL_MANAGER_KIND", "llamacpp")]));
    assert_eq!(
        show(d.provider().get("capabilities")),
        r#"{"jsonSchemaParam":true}"#
    );
    assert!(format!("{d:?}").contains("<redacted>"));
}

#[test]
fn no_key_reaches_debug_output() {
    let env = |k: &str| match k {
        "INFERENCE_API_KEY" => Some("qa-secret-key".to_string()),
        "INFERENCE_BASE_URL" => Some("http://qa:qa-secret-pass@qa-engine.invalid/v1".to_string()),
        "CODE_ENGINE_API_KEY" => Some("qa-secret-code".to_string()),
        _ => None,
    };
    let deployment = Deployment::from_lookup(&env);
    let code_env = CodeEnv::from_lookup(&env);
    let provider = deployment.provider();
    let inputs = Inputs {
        env: &code_env,
        provider: &provider,
        external: false,
        roles: &JValue::Null,
        last_loaded: &JValue::Null,
        role_wanted: true,
    };
    let project = wtf16::parse(r#"{"id":"qa-project"}"#).unwrap();
    let answer = code::answer(
        Some(&project),
        &inputs,
        |_| Ok::<_, ()>(JValue::Null),
        || JValue::Bool(false),
    );
    let Answer::Facts(facts) = &answer else {
        panic!("{answer:?}")
    };
    assert_eq!(
        facts.get("engine").get("apiKey").as_str(),
        Some("qa-secret-code")
    );
    assert_eq!(
        facts.get("role").get("apiKey").as_str(),
        Some("qa-secret-key")
    );
    for shown in [
        format!("{answer:?}"),
        format!("{deployment:?}"),
        format!("{code_env:?}"),
    ] {
        assert!(!shown.contains("qa-secret"), "{shown}");
    }
}

#[test]
fn a_projection_too_large_for_the_port_is_external() {
    let row = |n: usize| {
        JValue::obj([
            ("baseUrl", JValue::from("http://qa-engine.invalid/v1")),
            ("label", JValue::Str("\u{1}".repeat(n))),
        ])
    };
    // `\u0001` is six bytes of JSON: about 1.4 million of them pass the 8 MiB limit.
    assert!(!provider::is_external(Some(&row(1_000_000))));
    assert!(provider::is_external(Some(&row(1_500_000))));
    assert_eq!(
        provider::headers(&JValue::Null, &JValue::Undefined),
        Err(engine_facts::Thrown)
    );
}
