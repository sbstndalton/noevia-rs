use diary_entity_focus::{
    analyse, focus_candidates, Anchor, Entity, Error, NumericValue, Row, TemporalPartition,
    MAX_QUERY_BYTES, MAX_ROW_BYTES,
};
fn row() -> Row {
    Row {
        header: String::new(),
        text: "Alder Brannock wrote.".to_owned(),
        day: Some("2030-01-01".to_owned()),
        score: NumericValue::Number(0.9),
    }
}
#[test]
fn rejects_overlarge_text_and_scores() {
    assert!(matches!(
        analyse(&"a".repeat(MAX_QUERY_BYTES + 1), &[], None),
        Err(Error::Capacity)
    ));
    assert!(matches!(
        analyse("Alder", &vec!["a".repeat(MAX_QUERY_BYTES); 5], None),
        Err(Error::Capacity)
    ));
    let analysis = analyse("What did Alder Brannock say?", &[], None).ok();
    let Some(analysis) = analysis else {
        return;
    };
    let partition = TemporalPartition::identity(1).ok();
    let Some(partition) = partition else {
        return;
    };
    let mut r = row();
    r.text = "a".repeat(MAX_ROW_BYTES + 1);
    assert!(matches!(
        focus_candidates(&analysis, &[r], &partition, None, &NumericValue::Null),
        Err(Error::Capacity)
    ));
    let mut r = row();
    r.score = NumericValue::Text("9".repeat(129));
    assert!(matches!(
        focus_candidates(&analysis, &[r], &partition, None, &NumericValue::Null),
        Err(Error::Capacity)
    ));
    assert!(matches!(
        focus_candidates(
            &analysis,
            &[row()],
            &partition,
            None,
            &NumericValue::Text("0".repeat(129))
        ),
        Err(Error::Capacity)
    ));
    assert!(matches!(
        focus_candidates(
            &analysis,
            &[row()],
            &partition,
            Some(10_001),
            &NumericValue::Null
        ),
        Err(Error::Capacity)
    ));
}
#[test]
fn rejects_bad_anchor_and_partitions() {
    assert!(matches!(
        analyse(
            "What did Alder Brannock say?",
            &[],
            Some(Anchor {
                day: "2030-02-30".to_owned(),
                inclusive: true
            })
        ),
        Err(Error::InvalidAnchor)
    ));
    let Some(analysis) = analyse("What did Alder Brannock say?", &[], None).ok() else {
        return;
    };
    let rows = [row(), row()];
    for partition in [
        TemporalPartition {
            survivors: vec![1, 1],
            removed: vec![],
        },
        TemporalPartition {
            survivors: vec![2, 1],
            removed: vec![],
        },
        TemporalPartition {
            survivors: vec![1],
            removed: vec![1],
        },
        TemporalPartition {
            survivors: vec![1],
            removed: vec![],
        },
        TemporalPartition {
            survivors: vec![1, 3],
            removed: vec![],
        },
        TemporalPartition {
            survivors: vec![1],
            removed: vec![2],
        },
    ] {
        assert!(matches!(
            focus_candidates(&analysis, &rows, &partition, None, &NumericValue::Null),
            Err(Error::InvalidPartition)
        ));
    }
}
#[test]
fn rejects_public_analysis_abuse() {
    let Some(mut analysis) = analyse("Alder", &[], None).ok() else {
        return;
    };
    analysis.entities = vec![
        Entity {
            name: "Alder".to_owned(),
            keys: vec!["alder".to_owned()]
        };
        5
    ];
    let Some(partition) = TemporalPartition::identity(1).ok() else {
        return;
    };
    assert!(matches!(
        focus_candidates(&analysis, &[row()], &partition, None, &NumericValue::Null),
        Err(Error::Capacity)
    ));
}
