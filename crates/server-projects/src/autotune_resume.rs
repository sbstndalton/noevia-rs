//! Public resume decisions; the live actor retains metadata, restoration and state-write authority.
use js_json::JValue;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    Confirm,
    Resumable,
    Unsafe,
    Revision,
    Router,
    Configured,
    Identity,
    Changed,
    State,
}
impl Fault {
    pub fn status(self) -> u16 {
        if self == Self::Confirm {
            400
        } else if self == Self::State {
            503
        } else {
            409
        }
    }
    pub fn message(self) -> &'static str {
        match self {
        Self::Confirm => "Confirm that chat can pause before resuming auto-tune.",
        Self::Resumable => "No resumable auto-tune is available.",
        Self::Unsafe => "Settings could not safely be restored; inspect models.ini before starting a new tune.",
        Self::Revision => "Settings changed outside auto-tune. Start a new tune after reviewing them.",
        Self::Router => "The model server is not responding.",
        Self::Configured => "A queued model is no longer configured.",
        Self::Identity => "A queued model identity could not be read.",
        Self::Changed => "A queued model changed since tuning began.",
        Self::State => "Auto-tune state is unavailable. Try again shortly.",
    }
    }
}
fn array(v: &JValue) -> Result<&[JValue], Fault> {
    match v {
        JValue::Arr(a) => Ok(a),
        _ => Err(Fault::State),
    }
}
fn n(i: usize) -> JValue {
    JValue::Num(i as f64)
}
pub fn validate(confirm: &JValue, facts: &JValue) -> Result<(), Fault> {
    if confirm.as_bool() != Some(true) {
        return Err(Fault::Confirm);
    }
    let j = facts.get("job");
    if matches!(j, JValue::Undefined) {
        return Err(Fault::State);
    }
    if facts.get("starting").as_bool().ok_or(Fault::State)?
        || !matches!(
            j.get("status").as_str(),
            Some("cancelled" | "interrupted" | "failed")
        )
        || !matches!(j.get("models"), JValue::Arr(_))
    {
        return Err(Fault::Resumable);
    }
    if j.get("unsafe").as_bool().ok_or(Fault::State)? {
        return Err(Fault::Unsafe);
    }
    for item in array(j.get("models"))? {
        for phase in array(item.get("phases"))? {
            if phase.get("restored").as_bool() == Some(false) {
                return Err(Fault::Unsafe);
            }
        }
    }
    let revision = facts.get("revision").as_str().ok_or(Fault::State)?;
    let pending = j.get("pendingRevision").as_str();
    let effective = if pending == Some(revision) {
        pending
    } else {
        j.get("revision").as_str()
    };
    if effective != Some(revision) {
        return Err(Fault::Revision);
    }
    Ok(())
}
pub fn filter(facts: &JValue, listing: &JValue) -> Result<Option<JValue>, Fault> {
    let queue = array(facts.get("job").get("models"))?;
    let rows = match listing {
        JValue::Arr(a) if a.len() == queue.len() => a,
        JValue::Null => return Err(Fault::Router),
        _ => return Err(Fault::State),
    };
    let mut keep = Vec::new();
    let mut skipped = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if row.get("index") != &n(i) {
            return Err(Fault::State);
        }
        if row.get("system").as_bool().ok_or(Fault::State)? {
            skipped.push(JValue::obj([
                ("index", n(i)),
                ("reason", JValue::from("System routing model — not tuned")),
            ]));
        } else {
            keep.push(n(i));
        }
    }
    if skipped.is_empty() {
        return Ok(None);
    }
    let current = facts.get("job").get("model");
    let removed_current = rows
        .iter()
        .enumerate()
        .any(|(i, r)| r.get("system").as_bool() == Some(true) && queue[i].get("model") == current);
    let model = if removed_current {
        keep.first().cloned().unwrap_or(JValue::Null)
    } else {
        JValue::Null
    };
    Ok(Some(JValue::obj([
        ("kind", JValue::from("filter")),
        ("complete", JValue::Bool(keep.is_empty())),
        ("keep", JValue::Arr(keep)),
        ("skipped", JValue::Arr(skipped)),
        ("model", model),
    ])))
}
pub fn selectors(facts: &JValue, listing: &[JValue]) -> Result<JValue, Fault> {
    let queue = array(facts.get("job").get("models"))?;
    if queue.len() != listing.len() {
        return Err(Fault::State);
    }
    let mut out = Vec::new();
    for (i, item) in queue.iter().enumerate() {
        if item.get("status").as_str() == Some("passed") {
            continue;
        }
        let model_configured = item.get("configured").as_bool().ok_or(Fault::State)?;
        let base = item.get("mode").as_str() == Some("long")
            && (!model_configured || item.get("sectionCreated").truthy());
        let (field, configured, listed) = if base {
            (
                "base",
                item.get("baseConfigured"),
                listing[i].get("baseListed"),
            )
        } else {
            (
                "model",
                item.get("configured"),
                listing[i].get("modelListed"),
            )
        };
        if !item.get(field).truthy()
            || configured.as_bool() != Some(true)
            || listed.as_bool() != Some(true)
        {
            return Err(Fault::Configured);
        }
        out.push(JValue::obj([
            ("index", n(i)),
            ("field", JValue::from(field)),
        ]));
    }
    Ok(JValue::Arr(out))
}
pub fn identities(facts: &JValue, selectors: &JValue, identities: &JValue) -> Result<(), Fault> {
    let queue = array(facts.get("job").get("models"))?;
    let asks = array(selectors)?;
    let replies = array(identities)?;
    if asks.len() != replies.len() {
        return Err(Fault::State);
    }
    for (ask, reply) in asks.iter().zip(replies) {
        if ask.get("index") != reply.get("index") || ask.get("field") != reply.get("field") {
            return Err(Fault::State);
        }
        let index = match ask.get("index") {
            JValue::Num(n) if *n >= 0.0 && n.fract() == 0.0 => *n as usize,
            _ => return Err(Fault::State),
        };
        let row = queue.get(index).ok_or(Fault::State)?;
        let current = reply.get("identity").as_str().ok_or(Fault::Identity)?;
        if row.get("identitySet").as_bool().ok_or(Fault::State)?
            && row.get("identity").as_str() != Some(current)
        {
            return Err(Fault::Changed);
        }
    }
    Ok(())
}
pub fn launch(facts: &JValue) -> Result<JValue, Fault> {
    let mut reset = Vec::new();
    for (i, item) in array(facts.get("job").get("models"))?.iter().enumerate() {
        if item.get("status").as_str() == Some("passed") {
            continue;
        }
        let mut phases = Vec::new();
        for (p, phase) in array(item.get("phases"))?.iter().enumerate() {
            if phase.get("status").as_str() == Some("passed") {
                continue;
            }
            let steps = array(phase.get("steps"))?;
            let kept = if item.get("planned").truthy()
                && matches!(phase.get("id").as_str(), Some("context" | "verify"))
            {
                JValue::Arr(
                    steps
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| {
                            matches!(s.get("status").as_str(), Some("passed" | "failed"))
                        })
                        .map(|(s, _)| n(s))
                        .collect(),
                )
            } else {
                JValue::Null
            };
            phases.push(JValue::obj([("index", n(p)), ("keepSteps", kept)]));
        }
        reset.push(JValue::obj([
            ("index", n(i)),
            ("phases", JValue::Arr(phases)),
        ]));
    }
    Ok(JValue::obj([
        ("kind", JValue::from("launch")),
        ("reset", JValue::Arr(reset)),
    ]))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use js_json::wtf16;
    fn facts(case: &JValue) -> JValue {
        let old = case.get("job");
        let models = if let JValue::Arr(items) = old.get("models") {
            JValue::Arr(
                items
                    .iter()
                    .map(|m| {
                        JValue::obj([
                            ("model", m.get("model").clone()),
                            ("base", m.get("base").clone()),
                            ("mode", m.get("mode").clone()),
                            ("status", m.get("status").clone()),
                            (
                                "sectionCreated",
                                JValue::Bool(m.get("sectionCreated").truthy()),
                            ),
                            ("identitySet", JValue::Bool(m.get("_identity").truthy())),
                            ("identity", m.get("_identity").clone()),
                            ("planned", JValue::Bool(m.get("plan").truthy())),
                            ("configured", JValue::Bool(true)),
                            ("baseConfigured", JValue::Bool(true)),
                            ("phases", m.get("phases").clone()),
                        ])
                    })
                    .collect(),
            )
        } else {
            JValue::Null
        };
        JValue::obj([
            ("starting", JValue::Bool(false)),
            ("revision", JValue::from("r1")),
            (
                "job",
                if matches!(old, JValue::Null) {
                    JValue::Null
                } else {
                    JValue::obj([
                        ("status", old.get("status").clone()),
                        ("model", old.get("model").clone()),
                        ("unsafe", JValue::Bool(old.get("_unsafe").truthy())),
                        ("revision", old.get("_revision").clone()),
                        ("pendingRevision", old.get("_pendingRevision").clone()),
                        ("models", models),
                    ])
                },
            ),
        ])
    }
    #[test]
    fn actual_old_node_oracle_preserves_refusals_and_successful_queue_decisions() {
        let JValue::Arr(cases) =
            wtf16::parse(include_str!("../tests/fixtures/autotune-resume-node.json")).unwrap()
        else {
            panic!("oracle array")
        };
        assert_eq!(cases.len(), 15);
        for case in cases {
            let f = facts(&case);
            let confirm = case.get("input").get("confirmPause");
            let result = (|| {
                validate(confirm, &f)?;
                let listing = if let JValue::Arr(rows) = case.get("rows") {
                    let queue = array(f.get("job").get("models"))?;
                    JValue::Arr(
                        queue
                            .iter()
                            .enumerate()
                            .map(|(i, item)| {
                                let row = rows.iter().find(|r| r.get("id") == item.get("model"));
                                JValue::obj([
                                    ("index", n(i)),
                                    (
                                        "system",
                                        JValue::Bool(row.is_some_and(|r| {
                                            r.get("system").as_bool() == Some(true)
                                        })),
                                    ),
                                    ("modelListed", JValue::Bool(row.is_some())),
                                    ("baseListed", JValue::Bool(false)),
                                ])
                            })
                            .collect(),
                    )
                } else {
                    JValue::Null
                };
                let filtering = filter(&f, &listing)?;
                if let Some(p) = filtering {
                    assert_eq!(p.get("complete").as_bool(), Some(true));
                    return Ok(200);
                }
                let JValue::Arr(rows) = listing else {
                    return Err(Fault::State);
                };
                let selects = selectors(&f, &rows)?;
                let JValue::Arr(asks) = &selects else {
                    return Err(Fault::State);
                };
                let identities = JValue::Arr(
                    asks.iter()
                        .map(|ask| {
                            JValue::obj([
                                ("index", ask.get("index").clone()),
                                ("field", ask.get("field").clone()),
                                (
                                    "identity",
                                    if case.get("name").as_str() == Some("missing-identity") {
                                        JValue::Null
                                    } else {
                                        JValue::from("e".repeat(64))
                                    },
                                ),
                            ])
                        })
                        .collect(),
                );
                self::identities(&f, &selects, &identities)?;
                let patch = launch(&f)?;
                assert_eq!(patch.get("kind").as_str(), Some("launch"));
                Ok(202)
            })();
            match result {
                Ok(status) => assert_eq!(
                    case.get("status"),
                    &JValue::Num(status as f64),
                    "{:?}",
                    case.get("name")
                ),
                Err(fault) => {
                    assert_eq!(
                        case.get("status"),
                        &JValue::Num(fault.status() as f64),
                        "{:?}",
                        case.get("name")
                    );
                    assert_eq!(
                        case.get("body").get("error").as_str(),
                        Some(fault.message()),
                        "{:?}",
                        case.get("name")
                    );
                }
            }
        }
    }
    #[test]
    fn planned_reset_retains_only_measured_context_steps_and_unplanned_resets_all() {
        let f=wtf16::parse(r#"{"job":{"models":[{"status":"interrupted","planned":true,"phases":[{"id":"context","status":"interrupted","steps":[{"status":"passed"},{"status":"failed"},{"status":"pending"}]},{"id":"sampling","status":"failed","steps":[{"status":"passed"}]}]},{"status":"passed","phases":[]}]}}"#).unwrap();
        let expected=wtf16::parse(r#"{"kind":"launch","reset":[{"index":0,"phases":[{"index":0,"keepSteps":[0,1]},{"index":1,"keepSteps":null}]}]}"#).unwrap();
        assert_eq!(launch(&f).unwrap(), expected);
    }
    #[test]
    fn missing_or_misaligned_identity_facts_fail_closed() {
        let f =
            wtf16::parse(r#"{"job":{"models":[{"identitySet":true,"identity":"old"}]}}"#).unwrap();
        let asks = wtf16::parse(r#"[{"index":0,"field":"model"}]"#).unwrap();
        assert_eq!(
            identities(&f, &asks, &JValue::Arr(vec![])),
            Err(Fault::State)
        );
        let different = wtf16::parse(r#"[{"index":0,"field":"model","identity":"new"}]"#).unwrap();
        assert_eq!(identities(&f, &asks, &different), Err(Fault::Changed));
    }
}
