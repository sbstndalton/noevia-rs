//! One blocking publication owner, including predicate evaluation and known refusals.
//! A scoped writer never reacquires its SQLite transaction and never crosses an await.
use super::{current, detail, Outcome, SectionWrites};
use model_publication::Guard;
use std::sync::{Arc, Mutex};

pub(super) type Predicate = dyn Fn(&Guard) -> Result<(), Outcome> + Send + Sync;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity {
    #[serde(alias = "path", alias = "relativePath")]
    pub relative: String,
    #[serde(alias = "dev")]
    pub device: u64,
    #[serde(alias = "ino")]
    pub inode: u64,
    pub size: u64,
    #[serde(
        rename = "mtimeNs",
        alias = "mtime_ns",
        deserialize_with = "timestamp",
        serialize_with = "serialize_timestamp"
    )]
    pub mtime_ns: i128,
}
fn serialize_timestamp<S: serde::Serializer>(value: &i128, output: S) -> Result<S::Ok, S::Error> {
    output.serialize_str(&value.to_string())
}
fn timestamp<'de, D: serde::Deserializer<'de>>(input: D) -> Result<i128, D::Error> {
    let value = <serde_json::Value as serde::Deserialize>::deserialize(input)?;
    value
        .as_str()
        .and_then(|v| v.parse().ok())
        .or_else(|| value.as_i64().map(i128::from))
        .or_else(|| value.as_u64().map(i128::from))
        .ok_or_else(|| serde::de::Error::custom("invalid file identity timestamp"))
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationCondition {
    pub file_absent: Option<String>,
    pub file_identity: Option<FileIdentity>,
}

#[derive(Debug)]
pub(super) struct ConditionChanged;
impl std::fmt::Display for ConditionChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("publication condition changed")
    }
}
impl std::error::Error for ConditionChanged {}

impl PublicationCondition {
    fn check(&self, reader: &dyn crate::sections::Read) -> Result<(), Outcome> {
        if let Some(identity) = &self.file_identity {
            if !reader
                .file_identity(identity)
                .map_err(|_| detail(503, "File identity is unavailable."))?
            {
                return Err(Outcome::Json(409,br#"{"detail":"Validated model file changed.","code":"maintenance_file_changed"}"#.to_vec()));
            }
        }
        if let Some(name) = &self.file_absent {
            if !super::valid_section_name(name) {
                return Err(detail(400, "invalid section name"));
            }
            let bytes = reader
                .response(None, false)
                .map_err(|_| detail(503, "Model file presence is unavailable."))?;
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|_| detail(503, "Model file presence is unavailable."))?;
            let row = value
                .get("sections")
                .and_then(serde_json::Value::as_array)
                .and_then(|rows| rows.iter().find(|row| row["name"].as_str() == Some(name)))
                .ok_or_else(|| detail(404, "section not found"))?;
            match row["hasFile"].as_bool() {
                Some(false) => {}
                Some(true) => {
                    return Err(Outcome::Json(
                        409,
                        br#"{"detail":"Model file is present.","code":"maintenance_file_present"}"#
                            .to_vec(),
                    ))
                }
                None => return Err(detail(503, "Model file presence is unavailable.")),
            }
        }
        Ok(())
    }
}

pub(super) fn stale() -> Outcome {
    Outcome::Json(
        409,
        br#"{"detail":"Stale maintenance operation.","code":"maintenance_operation_stale"}"#
            .to_vec(),
    )
}

fn status(outcome: &Outcome) -> u16 {
    match outcome {
        Outcome::Json(status, _) => *status,
        Outcome::Failed => 500,
    }
}

impl SectionWrites {
    pub fn operation_fence(&self) -> Option<&model_publication::Fence> {
        self.fence.as_ref()
    }
    pub fn with_condition(
        &self,
        condition: PublicationCondition,
        reader: Arc<dyn crate::sections::Read>,
    ) -> Self {
        let mut writer = self.clone();
        writer.predicate = Some(Arc::new(move |_| condition.check(reader.as_ref())));
        writer
    }
    pub fn delete_missing(
        &self,
        name: &str,
        base: &str,
        reader: &dyn crate::sections::Read,
    ) -> Outcome {
        self.conditional(
            |_| {
                PublicationCondition {
                    file_absent: Some(name.to_owned()),
                    ..Default::default()
                }
                .check(reader)
            },
            |writer| writer.delete(name, base),
        )
    }
    pub(super) fn needs_transaction(&self) -> bool {
        (self.fence.is_some() || self.predicate.is_some()) && self.scope.is_none()
    }
    pub(crate) fn note_publication(&self) {
        if let Some(published) = &self.published {
            published.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    /// Run a synchronous predicate and its effect inside the SAME publication transaction.
    /// The callback must not escape the scoped writer or spawn asynchronous work.
    pub fn conditional(
        &self,
        predicate: impl FnOnce(&Guard) -> Result<(), Outcome>,
        effect: impl FnOnce(&Self) -> Outcome,
    ) -> Outcome {
        self.transaction(|scoped| {
            let verdict = {
                let Some(scope) = scoped.scope.as_ref() else {
                    return stale();
                };
                let guard = scope
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match guard.as_ref() {
                    Some(guard) => predicate(guard),
                    None => Err(stale()),
                }
            };
            match verdict {
                Ok(()) => effect(scoped),
                Err(outcome) => outcome,
            }
        })
    }

    /// Terminalize known no-effect refusals before returning them to the client.
    pub fn refusal(&self, outcome: Outcome) -> Outcome {
        if self.fence.is_none() {
            return outcome;
        }
        self.transaction(|_| outcome)
    }

    pub(crate) fn transaction(&self, effect: impl FnOnce(&Self) -> Outcome) -> Outcome {
        if self.scope.is_some() {
            return effect(self);
        }
        let guard = match Guard::at(&self.publication) {
            Ok(guard) => guard,
            Err(_) => return detail(503, "Preset publication is busy or unavailable."),
        };
        let mut refused = None;
        let original = self
            .fence
            .as_ref()
            .and_then(|f| guard.status(f).ok().flatten())
            .map(|r| r.state);
        if let Some(fence) = &self.fence {
            if original.as_deref() != Some("issued") || guard.validate(fence).is_err() {
                refused = Some(stale());
            } else if let Ok((_, revision)) = current(&self.ini) {
                if fence.expected_revision.is_none() || guard.expected(fence, &revision).is_err() {
                    refused = Some(stale());
                } else if guard.no_other_publishers(fence).is_err() {
                    refused = Some(detail(409, "Another publisher has not settled."));
                } else if guard.running(fence).is_err() {
                    refused = Some(stale());
                }
            } else {
                refused = Some(detail(503, "Native models.ini writes are unavailable"));
            }
        }
        if refused.is_none() {
            if let Some(predicate) = &self.predicate {
                if let Err(outcome) = predicate(&guard) {
                    refused = Some(outcome);
                }
            }
        }
        let scope = Arc::new(Mutex::new(Some(guard)));
        let mut scoped = self.clone();
        scoped.scope = Some(scope.clone());
        scoped.published = Some(Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let outcome = match refused {
            Some(outcome) => outcome,
            None => effect(&scoped),
        };
        let Some(guard) = scope
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return detail(503, "Maintenance settlement unavailable.");
        };
        let revision = if scoped
            .published
            .as_ref()
            .is_some_and(|p| p.load(std::sync::atomic::Ordering::Acquire))
        {
            current(&self.ini).ok().map(|(_, revision)| revision)
        } else {
            None
        };
        if let Some(revision) = &revision {
            if guard.observe_revision(revision).is_err() {
                return detail(503, "Maintenance settlement unavailable.");
            }
        }
        if let Some(fence) = &self.fence {
            if status(&outcome) < 500 && original.as_deref() == Some("issued") {
                // finish only changes a pending exact receipt; a committed revision is preserved.
                if guard.finish(fence, "settled", revision.as_deref()).is_err() {
                    return detail(503, "Maintenance settlement unavailable.");
                }
            } else if status(&outcome) >= 500
                && original.as_deref() == Some("issued")
                && guard.finish(fence, "recovery", None).is_err()
            {
                return detail(503, "Maintenance settlement unavailable.");
            }
        }
        if guard.commit().is_err() {
            return detail(503, "Maintenance settlement unavailable.");
        }
        outcome
    }

    pub(crate) fn with_publication<T>(
        &self,
        effect: impl FnOnce(&Guard) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        if let Some(scope) = &self.scope {
            let guard = scope
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            return effect(
                guard
                    .as_ref()
                    .ok_or_else(|| std::io::Error::other("publication already settled"))?,
            );
        }
        let guard = Guard::at(&self.publication)?;
        let result = effect(&guard)?;
        guard.commit()?;
        Ok(result)
    }
}
