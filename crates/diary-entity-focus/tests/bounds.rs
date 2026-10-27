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
fn rejects_overlarge_text_and_scores() -> Result<(), Error> {
    assert!(matches!(
        analyse(&"a".repeat(MAX_QUERY_BYTES + 1), &[], None),
        Err(Error::Capacity)
    ));
    assert!(matches!(
        analyse("Alder", &vec!["a".repeat(MAX_QUERY_BYTES); 5], None),
        Err(Error::Capacity)
    ));
    let analysis = analyse("What did Alder Brannock say?", &[], None)?;
    let partition = TemporalPartition::identity(1)?;
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
    Ok(())
}
#[test]
fn rejects_bad_anchor_and_partitions() -> Result<(), Error> {
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
    let analysis = analyse("What did Alder Brannock say?", &[], None)?;
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
    Ok(())
}
#[test]
fn rejects_public_analysis_abuse() -> Result<(), Error> {
    let mut analysis = analyse("Alder", &[], None)?;
    analysis.entities = vec![
        Entity {
            name: "Alder".to_owned(),
            keys: vec!["alder".to_owned()]
        };
        5
    ];
    let partition = TemporalPartition::identity(1)?;
    assert!(matches!(
        focus_candidates(&analysis, &[row()], &partition, None, &NumericValue::Null),
        Err(Error::Capacity)
    ));
    Ok(())
}

#[test]
fn public_helpers_reject_raw_input_before_cleanup_or_early_return() -> Result<(), Error> {
    use diary_entity_focus::{
        asked_entities, carried_entities, classify, identified_people, near_word, refers_back,
        relational,
    };
    let normal = Entity {
        name: "Alder Brannock".into(),
        keys: vec!["alder".into(), "brannock".into()],
    };
    let empty = Entity {
        name: String::new(),
        keys: vec![],
    };
    // Curly apostrophes shrink on normalization; speaker labels shrink on clean.
    let curly = "’ ".repeat(MAX_QUERY_BYTES / 4 + 1);
    assert!(curly.len() > MAX_QUERY_BYTES);
    assert_eq!(asked_entities(&curly), Err(Error::Capacity));
    let roles = "**Me:** ".repeat(200_000);
    assert!(roles.len() > MAX_QUERY_BYTES);
    for entity in [&normal, &empty] {
        assert_eq!(classify(&roles, entity), Err(Error::Capacity));
    }
    assert_eq!(identified_people(&roles, None), Err(Error::Capacity));
    assert_eq!(refers_back(&roles), Err(Error::Capacity));
    assert_eq!(relational(&roles, &[]), Err(Error::Capacity));
    assert_eq!(near_word(&roles, "Alder"), Err(Error::Capacity));
    assert_eq!(near_word(&"a".repeat(257), "Alder"), Err(Error::Capacity));
    // Compatibility decomposition can expand a short raw word beyond the bound.
    assert_eq!(near_word(&"ﷺ".repeat(15), "Alder"), Err(Error::Capacity));
    let too_many = vec![String::new(); 65];
    let aggregate = vec![" ".repeat(MAX_QUERY_BYTES); 5];
    for message in [
        "What did Alder Brannock say?",
        "No referent",
        "And his email?",
    ] {
        assert_eq!(carried_entities(message, &too_many), Err(Error::Capacity));
        assert_eq!(carried_entities(message, &aggregate), Err(Error::Capacity));
        assert_eq!(
            carried_entities(message, std::slice::from_ref(&roles)),
            Err(Error::Capacity)
        );
    }
    assert_eq!(carried_entities(&roles, &[]), Err(Error::Capacity));
    // Ordinary negatives remain successful policy answers, distinct from refusal.
    assert_eq!(classify("Nothing here.", &empty)?, (None, None));
    assert!(!refers_back("No referent")?);
    assert!(!relational(
        "No relationship",
        std::slice::from_ref(&normal)
    )?);
    assert!(!near_word("Alder", "Alder")?);
    Ok(())
}

#[test]
fn public_entity_arguments_and_numeric_capacity_are_checked() -> Result<(), Error> {
    use diary_entity_focus::{
        classify, identified_people, parse_margin, relational, Analysis, MAX_ENTITIES, MAX_WORDS,
    };
    let normal = Entity {
        name: "Alder".into(),
        keys: vec!["alder".into()],
    };
    let oversized = [
        Entity {
            name: " ".repeat(MAX_QUERY_BYTES + 1),
            keys: vec![],
        },
        Entity {
            name: "Alder".into(),
            keys: vec!["x".repeat(MAX_QUERY_BYTES + 1)],
        },
        Entity {
            name: "Alder".into(),
            keys: vec!["x".into(); MAX_WORDS + 1],
        },
        Entity {
            name: "Alder".into(),
            keys: vec!["x".repeat(MAX_QUERY_BYTES / 2 + 1); 2],
        },
    ];
    let malformed = [
        Entity {
            name: String::new(),
            keys: vec!["alder".into()],
        },
        Entity {
            name: "Alder".into(),
            keys: vec![String::new()],
        },
    ];
    for (entities, expected) in [
        (&oversized[..], Error::Capacity),
        (&malformed[..], Error::InvalidEntity),
    ] {
        for entity in entities {
            assert_eq!(classify("Nothing here.", entity), Err(expected));
            assert_eq!(
                identified_people("Nothing here.", Some(std::slice::from_ref(entity))),
                Err(expected)
            );
            assert_eq!(
                relational("Nothing here.", std::slice::from_ref(entity)),
                Err(expected)
            );
            let analysis = Analysis {
                entities: vec![entity.clone()],
                carried: false,
                anchor: None,
                relational: false,
            };
            assert_eq!(
                focus_candidates(
                    &analysis,
                    &[],
                    &TemporalPartition::identity(0)?,
                    None,
                    &NumericValue::Null
                ),
                Err(expected)
            );
        }
    }
    let many = vec![normal; MAX_ENTITIES + 1];
    assert_eq!(
        identified_people("Nothing here.", Some(&many)),
        Err(Error::Capacity)
    );
    assert_eq!(relational("Nothing here.", &many), Err(Error::Capacity));
    assert_eq!(
        parse_margin(&NumericValue::Text("9".repeat(129))),
        Err(Error::Capacity)
    );
    assert_eq!(parse_margin(&NumericValue::Text("bad".into()))?, 0.05);
    assert_eq!(parse_margin(&NumericValue::Text(" ".repeat(128)))?, 0.05);
    Ok(())
}

#[test]
fn extraction_retains_four_entities_instead_of_refusing_five() -> Result<(), Error> {
    use diary_entity_focus::{asked_entities, MAX_ENTITIES};
    let entities = asked_entities(
        "Did Alder Brannock, Bryony Castellow, Caspian Juxon, William Hart, Janet Bell speak?",
    )?;
    assert_eq!(entities.len(), MAX_ENTITIES);
    Ok(())
}

fn expanded_names() -> (String, String) {
    (
        format!("F{}a{}", "ﬃ".repeat(90), "ﬃ".repeat(10)),
        format!("F{}b{}", "ﬃ".repeat(90), "ﬃ".repeat(10)),
    )
}
#[test]
fn normalized_fuzzy_classification_never_becomes_absence() -> Result<(), Error> {
    use diary_entity_focus::{asked_entities, classify, Kind};
    let (a, b) = expanded_names();
    let entities = asked_entities(&format!("What about {a}?"))?;
    let entity = entities.first().ok_or(Error::Internal)?;
    assert_eq!(
        entity.keys.first().ok_or(Error::Internal)?.chars().count(),
        302
    );
    // Frozen Python says near; this exceeds our documented fuzzy capacity.
    // Refuse the operation rather than producing successful absence.
    assert_eq!(classify(&b, entity), Err(Error::Capacity));
    assert_eq!(classify(&a, entity)?.0, Some(Kind::Match));
    Ok(())
}
#[test]
fn normalized_fuzzy_focus_never_becomes_absence() -> Result<(), Error> {
    let (a, b) = expanded_names();
    let analysis = analyse(&format!("What about {a}?"), &[], None)?;
    let mut candidate = row();
    candidate.text = b;
    assert_eq!(
        focus_candidates(
            &analysis,
            &[candidate],
            &TemporalPartition::identity(1)?,
            None,
            &NumericValue::Null
        ),
        Err(Error::Capacity)
    );
    let mut exact = row();
    exact.text = a;
    let out = focus_candidates(
        &analysis,
        &[exact],
        &TemporalPartition::identity(1)?,
        None,
        &NumericValue::Null,
    )?;
    assert_eq!(out.kept_ranks, vec![1]);
    assert_eq!(out.exact.len(), 1);
    Ok(())
}
