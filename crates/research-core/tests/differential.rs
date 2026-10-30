//! Deep research's rules against the answers Node gave (tests/fixtures/record.cjs, at the Core head in
//! `cases.json`'s `core`).
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use research_core::{plan, report, sources as rs};
use serde_json::{json, Value};

fn cases() -> Value {
    serde_json::from_str(include_str!("fixtures/cases.json")).unwrap()
}

fn list<'a>(c: &'a Value, key: &str) -> &'a Vec<Value> {
    c[key].as_array().unwrap()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn sentences_words_html_and_chunks() {
    let c = cases();
    for k in list(&c, "sentences") {
        assert_eq!(
            json!(rs::split_sentences(k["t"].as_str().unwrap())),
            k["out"],
            "{}",
            k["t"]
        );
    }
    for k in list(&c, "normalise") {
        assert_eq!(
            rs::normalise(k["t"].as_str().unwrap()),
            k["out"].as_str().unwrap(),
            "{}",
            k["t"]
        );
    }
    for k in list(&c, "strip") {
        assert_eq!(
            rs::strip_boilerplate(k["t"].as_str().unwrap()),
            k["out"].as_str().unwrap(),
            "{}",
            k["t"]
        );
    }
    for k in list(&c, "chunk") {
        let got: Vec<Value> = rs::chunk(k["t"].as_str().unwrap(), 100)
            .iter()
            .map(|x| json!({"heading": x.heading, "text": x.text}))
            .collect();
        assert_eq!(json!(got), k["out"], "{}", k["t"]);
    }
    for k in list(&c, "score") {
        let got = rs::lexical_score(k["q"].as_str().unwrap(), k["t"].as_str().unwrap());
        assert_eq!(got, k["out"].as_f64().unwrap(), "{} / {}", k["q"], k["t"]);
    }
    for k in list(&c, "reduce") {
        let got = rs::reduce(
            k["page"].as_str().unwrap(),
            k["q"].as_str().unwrap(),
            k["cap"].as_u64().unwrap(),
        );
        assert_eq!(json!(got), k["out"], "{} {}", k["q"], k["cap"]);
    }
    for k in list(&c, "cap") {
        let input: Vec<(u64, Vec<String>)> = k["in"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (e["id"].as_u64().unwrap(), strings(&e["excerpts"])))
            .collect();
        let got: Vec<Value> = rs::cap_excerpts(input, k["max"].as_u64().unwrap())
            .into_iter()
            .map(|(id, excerpts)| json!({"id": id, "excerpts": excerpts}))
            .collect();
        assert_eq!(json!(got), k["out"]);
    }
}

#[test]
fn numbers_claims_instructions_and_the_pack() {
    let c = cases();
    for k in list(&c, "numbers") {
        let got: Vec<Value> = rs::numbers_in(k["t"].as_str().unwrap())
            .iter()
            .map(|n| json!({"value": n.value, "unit": n.unit}))
            .collect();
        assert_eq!(json!(got), k["out"], "{}", k["t"]);
    }
    for k in list(&c, "claims") {
        let got = rs::supports_claim(
            k["c"].as_str().unwrap(),
            &strings(&k["s"]),
            k["n"].as_bool().unwrap(),
        );
        assert_eq!(json!(got), k["out"], "{} / {}", k["c"], k["s"]);
    }
    for k in list(&c, "instructions") {
        assert_eq!(
            json!(rs::looks_like_instruction(k["t"].as_str().unwrap())),
            k["out"],
            "{}",
            k["t"]
        );
    }
    for k in list(&c, "pack") {
        let one = rs::Sentence {
            id: "S1".into(),
            text: k["t"].as_str().unwrap().into(),
            source: 1,
        };
        assert_eq!(
            rs::evidence_pack(&[vec![one]]),
            k["out"].as_str().unwrap(),
            "{}",
            k["t"]
        );
    }
}

fn registry() -> (rs::Registry, u64, u64, u64) {
    let mut r = rs::Registry::new();
    let a = r.register(
        rs::Kind::Web {
            url: "https://a.test/x",
        },
        "A page",
        1_700_000_000_000,
        &[
            "The tower is 50 metres tall. It opened in 1999.".to_owned(),
            "Bananas are yellow. The tower is 50 metres tall.".to_owned(),
        ],
    );
    let b = r.register(
        rs::Kind::Project { file: "notes.md" },
        "notes.md",
        1_700_000_000_001,
        &["Rivers flow. Five boats sailed.".to_owned()],
    );
    let again = r.register(
        rs::Kind::Web {
            url: "https://a.test/x",
        },
        "ignored",
        1_700_000_000_002,
        &[
            "Late excerpt here.".to_owned(),
            "Bananas are yellow. The tower is 50 metres tall.".to_owned(),
        ],
    );
    (r, a, b, again)
}

#[test]
fn the_registry_and_the_verifier() {
    let c = cases();
    let (r, a, b, again) = registry();
    let want = &c["registry"];
    assert_eq!(
        (json!(a), json!(b), json!(again)),
        (want["a"].clone(), want["b"].clone(), want["again"].clone())
    );
    assert_eq!(json!(r.list()), want["list"]);
    assert_eq!(r.footer(), want["footer"].as_str().unwrap());
    let of = r.sentences_of(
        a,
        &[
            "Bananas are yellow. The tower is 50 metres tall.".to_owned(),
            "Unknown thing here.".to_owned(),
        ],
    );
    let of: Vec<Value> = of
        .iter()
        .map(|x| json!({"id": x.id, "text": x.text, "sourceId": x.source}))
        .collect();
    assert_eq!(json!(of), want["of"]);
    let g1 = r.sentences_of(
        a,
        &["The tower is 50 metres tall. It opened in 1999.".to_owned()],
    );
    let g2 = r.sentences_of(b, &["Rivers flow. Five boats sailed.".to_owned()]);
    assert_eq!(
        rs::evidence_pack(&[g1, Vec::new(), g2]),
        want["pack"].as_str().unwrap()
    );
    for k in list(&c, "verify") {
        let got = rs::verify_claims(
            k["d"].as_str().unwrap(),
            &r,
            &strings(&k["allowed"]),
            k["mode"] == "flag",
            2,
        );
        let noted = |n: &Vec<rs::Noted>| -> Vec<Value> {
            n.iter()
                .map(|x| match &x.cited {
                    Some(cited) => json!({"text": x.text, "reason": x.reason, "cited": cited}),
                    None => json!({"text": x.text, "reason": x.reason}),
                })
                .collect()
        };
        let want = &k["out"];
        let what = format!("{} ({})", k["d"], k["mode"]);
        assert_eq!(got.markdown, want["markdown"].as_str().unwrap(), "{what}");
        assert_eq!(json!(got.total), want["total"], "{what}");
        assert_eq!(json!(got.valid), want["valid"], "{what}");
        assert_eq!(
            json!({"total": got.claims.total, "supported": got.claims.supported, "flagged": got.claims.flagged, "dropped": got.claims.dropped, "uncited": got.claims.uncited}),
            want["claims"],
            "{what}"
        );
        assert_eq!(json!(noted(&got.flagged)), want["flagged"], "{what}");
        assert_eq!(json!(noted(&got.dropped)), want["dropped"], "{what}");
    }
}

fn plan_json(r: Result<Vec<String>, plan::Readable>) -> Value {
    match r {
        Ok(v) => json!({"ok": v}),
        Err(e) => json!({"error": e.message, "status": e.status}),
    }
}

#[test]
fn the_plan() {
    let c = cases();
    for k in list(&c, "plans") {
        let l = research_core::report::to_j(&k["l"]);
        assert_eq!(
            plan_json(plan::sanitize_plan(&l, k["min"].as_u64().unwrap() as usize)),
            k["out"],
            "{}",
            k["l"]
        );
    }
    for k in list(&c, "replies") {
        assert_eq!(
            plan_json(plan::parse_plan(k["t"].as_str().unwrap())),
            k["out"],
            "{}",
            k["t"]
        );
    }
}

#[test]
fn the_report_files_and_the_view() {
    let c = cases();
    for k in list(&c, "slugs") {
        assert_eq!(
            report::slug(k["t"].as_str().unwrap()),
            k["out"].as_str().unwrap(),
            "{}",
            k["t"]
        );
    }
    let want = &c["report"];
    let [md, sources] = report::report_files(&want["result"], 1_700_000_000_000, &|_| false);
    assert_eq!(md.name, want["pair"][0]["name"].as_str().unwrap());
    assert_eq!(md.text, want["pair"][0]["text"].as_str().unwrap());
    assert_eq!(sources.name, want["pair"][1]["name"].as_str().unwrap());
    // Byte for byte, though this value came through a sorted map: the hash of the first write decides a retry.
    assert_eq!(sources.text, want["pair"][1]["text"].as_str().unwrap());
    let taken = |n: &str| n.ends_with("tower.md") || n.ends_with("(2).sources.json");
    let [md, sources] = report::report_files(&want["result"], 1_700_000_000_000, &taken);
    assert_eq!(md.name, want["taken"][0]["name"].as_str().unwrap());
    assert_eq!(sources.name, want["taken"][1]["name"].as_str().unwrap());
    let [md, sources] = report::report_files(&want["partialOld"], 86_400_000 * 366, &|_| false);
    assert_eq!(md.name, want["old"][0]["name"].as_str().unwrap());
    assert_eq!(sources.text, want["old"][1]["text"].as_str().unwrap());
    for k in list(&c, "view") {
        let files = k["files"]
            .as_array()
            .map(|f| f.iter().map(report::to_j).collect::<Vec<_>>());
        let got = report::view(&k["job"], files.as_deref());
        let got: Value = serde_json::from_str(&js_json::stringify(&got).unwrap()).unwrap();
        assert_eq!(got, k["out"], "{}", k["job"]);
    }
}

#[test]
fn a_pending_file_is_retried_only_with_its_exact_bytes() {
    let c = cases();
    let result = &c["report"]["result"];
    let [md, sources] = report::report_files(result, 0, &|_| false);
    for file in [&md, &sources] {
        let pending = json!({"name": file.name, "hash": file.hash(), "bytes": file.bytes()});
        assert_eq!(
            report::pending_report_file(result, &pending).unwrap().text,
            file.text
        );
        let wrong = json!({"name": file.name, "hash": "00", "bytes": file.bytes()});
        assert!(report::pending_report_file(result, &wrong).is_none());
    }
    let other = json!({"name": "x.txt", "hash": md.hash(), "bytes": md.bytes()});
    assert!(report::pending_report_file(result, &other).is_none());
    assert!(
        report::pending_report_file(&json!({"sections": 1}), &json!({"name": "a.md"})).is_none()
    );
}
