#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use diary_entity_focus::{
    analyse, asked_entities, classify, focus_candidates, identified_people, Anchor, Entity,
    NumericValue, Row, TemporalPartition,
};
use serde_json::{json, Value};
const DATA: &str = include_str!("entity-fixtures.jsonl");
fn entity(v: &Value) -> Entity {
    Entity {
        name: v["name"].as_str().unwrap().to_owned(),
        keys: v["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_owned())
            .collect(),
    }
}
fn entity_value(e: &Entity) -> Value {
    json!({"name":e.name,"keys":e.keys})
}
fn numeric(v: &Value) -> NumericValue {
    match v {
        Value::Null => NumericValue::Null,
        Value::Bool(b) => NumericValue::Bool(*b),
        Value::Number(n) => NumericValue::Number(n.as_f64().unwrap()),
        Value::String(s) => NumericValue::Text(s.clone()),
        _ => panic!("bad fixture numeric"),
    }
}
fn anchor(v: &Value) -> Option<Anchor> {
    if v.is_null() {
        None
    } else {
        Some(Anchor {
            day: v["day"].as_str().unwrap().to_owned(),
            inclusive: v["inclusive"].as_bool().unwrap(),
        })
    }
}
fn rows(v: &Value) -> Vec<Row> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|r| Row {
            day: r["day"].as_str().map(str::to_owned),
            header: r["header"].as_str().unwrap().to_owned(),
            text: r["text"].as_str().unwrap().to_owned(),
            score: numeric(&r["score"]),
        })
        .collect()
}
fn partition(v: &Value) -> TemporalPartition {
    TemporalPartition {
        survivors: v["survivors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_u64().unwrap() as usize)
            .collect(),
        removed: v["removed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_u64().unwrap() as usize)
            .collect(),
    }
}
#[test]
fn python_source_differential_focus() {
    let mut n = 0;
    for line in DATA.lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        if v["kind"] != "focus" {
            continue;
        }
        let earlier = v["earlier"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let a = analyse(
            v["message"].as_str().unwrap(),
            &earlier,
            anchor(&v["expected"]["anchor"]),
        )
        .unwrap();
        let out = focus_candidates(
            &a,
            &rows(&v["rows"]),
            &partition(&v["partition"]),
            v["limit"].as_u64().map(|n| n as usize),
            &numeric(&v["margin"]),
        )
        .unwrap();
        let actual = json!({
          "entities":out.analysis.entities.iter().map(entity_value).collect::<Vec<_>>(),
          "carried":out.analysis.carried,
          "anchor":out.analysis.anchor.as_ref().map(|a|json!({"day":a.day,"inclusive":a.inclusive})),
          "relational":out.analysis.relational,
          "margin":out.margin,
          "matched":out.matched.iter().map(entity_value).collect::<Vec<_>>(),
          "kept_ranks":out.kept_ranks,
          "removed":out.removed.iter().map(|(rank,reason)|json!({"rank":rank,"reason":reason.as_str()})).collect::<Vec<_>>(),
          "similar":out.similar.iter().map(|(rank,similar)|json!({"rank":rank,"kind":similar.kind.as_str(),"shown":similar.shown,"asked":similar.asked})).collect::<Vec<_>>(),
          "corrected_by":out.corrected_by.iter().map(|(rank,day)|json!({"rank":rank,"day":day})).collect::<Vec<_>>(),
          "absent":out.absent.iter().map(|(e,spellings)|json!({"entity":entity_value(e),"spellings":spellings})).collect::<Vec<_>>(),
          "exact":out.exact.iter().map(|(rank,names)|json!({"rank":rank,"names":names})).collect::<Vec<_>>()
        });
        assert_eq!(actual, v["expected"], "focus case {}", v["id"]);
        n += 1;
    }
    assert_eq!(n, 784);
}
#[test]
fn python_source_differential_helpers() {
    let mut n = 0;
    for line in DATA.lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        if v["kind"] != "classify" {
            continue;
        }
        let e = entity(&v["entity"]);
        let text = v["text"].as_str().unwrap();
        let (kind, shown) = classify(text, &e).unwrap();
        assert_eq!(
            json!([kind.map(|k| k.as_str()), shown]),
            v["found"],
            "classify case {n}: {text:?}"
        );
        let people = identified_people(text, Some(std::slice::from_ref(&e))).unwrap();
        assert_eq!(json!(people), v["people"], "people case {n}: {text:?}");
        let asked = asked_entities(text).unwrap();
        assert_eq!(
            json!(asked.iter().map(entity_value).collect::<Vec<_>>()),
            v["asked"],
            "asked case {n}: {text:?}"
        );
        n += 1;
    }
    assert_eq!(n, 420);
}
#[test]
fn python_source_margin_text() {
    let mut n = 0;
    for line in DATA.lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        if v["kind"] != "margin" {
            continue;
        }
        let actual = diary_entity_focus::parse_margin(&numeric(&v["input"]));
        assert_eq!(
            json!(actual),
            v["expected"],
            "margin case {n}: {:?}",
            v["input"]
        );
        n += 1;
    }
    assert_eq!(n, 13);
}
