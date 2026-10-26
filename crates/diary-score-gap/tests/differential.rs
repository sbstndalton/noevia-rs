use diary_score_gap::cut;

fn number(s: &str) -> Result<Option<f64>, std::num::ParseFloatError> {
    if s == "missing" {
        Ok(None)
    } else {
        s.parse().map(Some)
    }
}
fn positions(s: &str) -> Result<Vec<usize>, std::num::ParseIntError> {
    if s.is_empty() {
        Ok(Vec::new())
    } else {
        s.split(",").map(str::parse).collect()
    }
}

#[test]
fn actual_python_cut_vectors() -> Result<(), Box<dyn std::error::Error>> {
    let mut count = 0;
    for line in include_str!("python-cut.tsv")
        .lines()
        .filter(|l| !l.starts_with("#"))
    {
        let fields: Vec<_> = line.split("\t").collect();
        let [gap, scores, kept, removed, best] = fields.as_slice() else {
            return Err("bad synthetic fixture".into());
        };
        let scores: Vec<_> = if scores.is_empty() {
            Vec::new()
        } else {
            scores
                .split(",")
                .map(number)
                .collect::<Result<Vec<_>, _>>()?
        };
        let actual = cut(&scores, number(gap)?);
        assert_eq!(actual.kept, positions(kept)?, "{line}");
        assert_eq!(actual.removed, positions(removed)?, "{line}");
        assert_eq!(
            actual.best.map(f64::to_bits),
            number(best)?.map(f64::to_bits),
            "{line}"
        );
        count += 1;
    }
    assert_eq!(count, 352);
    Ok(())
}

#[test]
fn focus_policy_stays_with_host() {
    // Time focus already removed a later 0.90 row; best must now be 0.78.
    let scores = [Some(0.78), Some(0.72), Some(0.65)];
    let time_only = cut(&scores, Some(0.08));
    assert_eq!(time_only.best, Some(0.78));
    assert_eq!(time_only.kept, vec![0, 1]);
    // Host suppresses the cut when the actual focus result contains entities.
    let named_entity = cut(&scores, None);
    assert_eq!(named_entity.kept, vec![0, 1, 2]);
    assert_eq!(named_entity.best, Some(0.78));
}
