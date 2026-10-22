//! Request classification, including hostile input (sbstndalton/noevia#2115).
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use inference_admission::{classify, Class, Refusal};

fn model(m: &str, explicit_load: bool) -> Class {
    Class::Model {
        model: m.into(),
        explicit_load,
    }
}

#[test]
fn reads_that_cannot_load_pass_through() {
    for (path, query) in [
        ("/models", None),
        ("/v1/models", None),
        ("/health", None),
        ("/props", Some("model=route-code&autoload=false")),
        ("/metrics", Some("model=route-code&autoload=false")),
        ("/slots", Some("model=route-code&autoload=0")),
        ("/props", Some("model=route-code&autoload=no")),
        ("/v1/stream", Some("conv_id=abc&from=3")),
        ("/props", None),
    ] {
        assert_eq!(
            classify("GET", path, query, None),
            Class::Passthrough,
            "{path}?{query:?}"
        );
    }
    assert_eq!(
        classify("DELETE", "/v1/stream", Some("conv_id=abc"), None),
        Class::Passthrough
    );
    assert_eq!(
        classify("OPTIONS", "/v1/chat/completions", None, None),
        Class::Passthrough
    );
}

#[test]
fn reads_that_may_load_are_admitted() {
    for query in [
        "model=route-code",
        "model=route-code&autoload=true",
        "model=route-code&autoload=1",
        // The router reads the first value of a repeated key, so does the gate.
        "model=route-code&autoload=1&autoload=false",
        "autoload=true&model=route-code&model=route-smart",
    ] {
        assert_eq!(
            classify("GET", "/props", Some(query), None),
            model("route-code", false),
            "{query}"
        );
    }
    // Escapes are decoded as the router decodes them.
    assert_eq!(
        classify(
            "GET",
            "/props",
            Some("model=route%2Dcode&autoload=%74rue"),
            None
        ),
        model("route-code", false)
    );
}

#[test]
fn generations_and_embeddings_always_need_admission() {
    let body = br#"{"model":"route-code","messages":[],"stream":true}"#;
    assert_eq!(
        classify("POST", "/v1/chat/completions", None, Some(body)),
        model("route-code", false)
    );
    // Even when they ask the router not to load: the gate must count them in flight.
    assert_eq!(
        classify(
            "POST",
            "/v1/chat/completions",
            Some("autoload=false"),
            Some(body)
        ),
        model("route-code", false)
    );
    assert_eq!(
        classify(
            "POST",
            "/v1/embeddings",
            None,
            Some(br#"{"model":"embed","input":"x"}"#)
        ),
        model("embed", false)
    );
    // An unknown route that names a model is admitted too (no bypass through new routes).
    assert_eq!(
        classify(
            "POST",
            "/v9/new-thing",
            None,
            Some(br#"{"model":"route-code"}"#)
        ),
        model("route-code", false)
    );
    assert_eq!(
        classify(
            "POST",
            "/models/load",
            None,
            Some(br#"{"model":"route-code"}"#)
        ),
        model("route-code", true)
    );
}

#[test]
fn posts_without_a_model_pass_through() {
    for body in [
        &b""[..],
        b"  ",
        br#"{"content":"x"}"#,
        br#"[1,2]"#,
        br#""text""#,
    ] {
        assert_eq!(
            classify("POST", "/tokenize", None, Some(body)),
            Class::Passthrough,
            "{:?}",
            std::str::from_utf8(body)
        );
    }
}

#[test]
fn unloads_deletes_and_reloads_are_recognised() {
    assert_eq!(
        classify(
            "POST",
            "/models/unload",
            None,
            Some(br#"{"model":"route-code"}"#)
        ),
        Class::Unload {
            model: "route-code".into(),
            force: false,
            delete: false
        }
    );
    assert_eq!(
        classify(
            "POST",
            "/models/unload",
            None,
            Some(br#"{"model":"route-code","force":true}"#)
        ),
        Class::Unload {
            model: "route-code".into(),
            force: true,
            delete: false
        }
    );
    assert_eq!(
        classify("DELETE", "/models", Some("model=route-code"), None),
        Class::Unload {
            model: "route-code".into(),
            force: false,
            delete: true
        }
    );
    assert_eq!(
        classify("GET", "/models", Some("reload=1"), None),
        Class::Reload
    );
    assert_eq!(
        classify("GET", "/models", Some("reload=0"), None),
        Class::Reload
    );
}

#[test]
fn hostile_bodies_are_refused() {
    let cases: &[(&str, &[u8], Refusal)] = &[
        ("/v1/chat/completions", b"{not json", Refusal::InvalidJson),
        (
            "/v1/chat/completions",
            br#"{"model":1}"#,
            Refusal::InvalidModel,
        ),
        (
            "/v1/chat/completions",
            br#"{"model":["a"]}"#,
            Refusal::InvalidModel,
        ),
        (
            "/v1/chat/completions",
            br#"{"model":null}"#,
            Refusal::InvalidModel,
        ),
        (
            "/v1/chat/completions",
            br#"{"model":""}"#,
            Refusal::InvalidModel,
        ),
        (
            "/v1/chat/completions",
            br#"{"model":"a\u0000b"}"#,
            Refusal::InvalidModel,
        ),
        (
            "/v1/chat/completions",
            br#"{"model":"a\nb"}"#,
            Refusal::InvalidModel,
        ),
        // A repeated model key could be read differently by the router: refused.
        (
            "/v1/chat/completions",
            br#"{"model":"route-code","model":"route-smart"}"#,
            Refusal::InvalidJson,
        ),
        (
            "/v1/chat/completions",
            br#"{"model":"route-code""#,
            Refusal::InvalidJson,
        ),
        ("/v1/chat/completions", b"[1,", Refusal::InvalidJson),
        ("/models/unload", br#"{}"#, Refusal::InvalidModel),
        ("/models/unload", b"", Refusal::InvalidModel),
        (
            "/models/unload",
            br#"{"model":"x","force":"yes"}"#,
            Refusal::InvalidJson,
        ),
        ("/models/load", br#"[]"#, Refusal::InvalidModel),
    ];
    for (path, body, refusal) in cases {
        assert_eq!(
            classify("POST", path, None, Some(body)),
            Class::Invalid(*refusal),
            "{path} {:?}",
            String::from_utf8_lossy(body)
        );
    }
    let long = format!(r#"{{"model":"{}"}}"#, "m".repeat(257));
    assert_eq!(
        classify("POST", "/v1/chat/completions", None, Some(long.as_bytes())),
        Class::Invalid(Refusal::InvalidModel)
    );
    assert_eq!(
        classify("DELETE", "/models", None, None),
        Class::Invalid(Refusal::InvalidModel)
    );
}

#[test]
fn hostile_paths_are_refused() {
    for path in [
        "/models/%75nload",
        "/v1/../models/unload",
        "/models/./unload",
        "//models/unload",
        "/models//unload",
        "/models/unload/",
        "\\models\\unload",
        "models/unload",
        "/models/unl oad",
        "/mödels",
        "/..",
        "/.",
    ] {
        assert_eq!(
            classify("POST", path, None, Some(br#"{"model":"route-code"}"#)),
            Class::Invalid(Refusal::InvalidPath),
            "{path}"
        );
    }
    let long = format!("/{}", "a".repeat(3000));
    assert_eq!(
        classify("GET", &long, None, None),
        Class::Invalid(Refusal::InvalidPath)
    );
    assert_eq!(classify("GET", "/", None, None), Class::Passthrough);
}
