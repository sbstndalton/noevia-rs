//! Hash-bound missing-chunk fill, with Core's batch-to-single refusal isolation.
//! Concrete SQLite/HTTP adapters are deliberately absent until their compatible
//! schema, tenant and network policies can be qualified together.
use crate::{Batch, Control, Index, Target, Task};
use prompt_framing::embedding_input::{self, Kind, Profile};
use std::collections::BTreeSet;
#[derive(Clone, Debug)]
pub struct Row {
    pub id: i64,
    pub hash: String,
    pub text: Vec<u16>,
}
pub trait Store: Sync {
    /// None means an unavailable/removed index. Never open with CREATE here.
    fn missing(
        &self,
        index: &Index,
        target: &Target,
        limit: usize,
        exclude: &BTreeSet<i64>,
    ) -> Result<Option<Vec<Row>>, String>;
    fn remaining(&self, index: &Index, target: &Target) -> Result<usize, String>;
    /// Recheck config version, cancellation, tenant/project lifecycle and each
    /// (id, hash) inside the write transaction. Ensure/re-read the model/profile
    /// slot and skip width mismatches. Count only actual published vectors.
    fn publish(
        &self,
        index: &Index,
        target: &Target,
        rows: &[Row],
        vectors: &[Vec<f32>],
        version: u64,
        control: &Control,
    ) -> Result<usize, String>;
    fn current(&self, version: u64, control: &Control) -> bool;
}
pub trait Embed: Sync {
    /// Already framed UTF-16 input. HTTP adapter must preserve JS JSON surrogate
    /// semantics, send only to the target endpoint, and never forward inference
    /// credentials to an explicit base URL. It owns ordered/empty-batch fallback
    /// from rag.cjs embedFramed and request cancellation/bounds.
    fn embed<'a>(
        &'a self,
        target: &'a Target,
        input: &'a [Vec<u16>],
        control: &'a Control,
    ) -> Task<'a, Result<Vec<Vec<f32>>, String>>;
}
pub struct Filler<S, E> {
    pub store: S,
    pub embedder: E,
}
impl<S: Store, E: Embed> Filler<S, E> {
    async fn embed_rows(
        &self,
        index: &Index,
        target: &Target,
        rows: &[Row],
        version: u64,
        control: &Control,
    ) -> Result<usize, String> {
        if control.stopped() || !self.store.current(version, control) {
            return Ok(0);
        }
        let profile =
            Profile::try_from(target.profile.as_str()).map_err(|_| "unknownProfile".to_owned())?;
        let input: Vec<_> = rows
            .iter()
            .map(|row| embedding_input::frame(profile, Kind::Document, &row.text, None))
            .collect();
        let vectors = self.embedder.embed(target, &input, control).await?;
        if control.stopped() || !self.store.current(version, control) {
            return Ok(0);
        }
        if vectors.len() != rows.len()
            || vectors
                .iter()
                .any(|v| v.is_empty() || v.iter().any(|n| !n.is_finite()))
        {
            return Err("invalidVectors".into());
        }
        self.store
            .publish(index, target, rows, &vectors, version, control)
    }
    pub async fn fill(
        &self,
        index: &Index,
        target: &Target,
        limit: usize,
        exclude: &BTreeSet<i64>,
        version: u64,
        control: &Control,
    ) -> Result<Option<Batch>, String> {
        let Some(rows) = self.store.missing(index, target, limit, exclude)? else {
            return Ok(None);
        };
        let mut embedded = 0;
        let mut refused = vec![];
        if !rows.is_empty() {
            match self
                .embed_rows(index, target, &rows, version, control)
                .await
            {
                Ok(n) => embedded = n,
                Err(error) => {
                    if control.stopped() {
                        return Err(error);
                    }
                    for row in &rows {
                        match self
                            .embed_rows(index, target, std::slice::from_ref(row), version, control)
                            .await
                        {
                            Ok(n) => embedded += n,
                            Err(_) => refused.push(row.id),
                        }
                    }
                }
            }
        }
        Ok(Some(Batch {
            embedded,
            attempted: rows.len(),
            refused,
            remaining: self.store.remaining(index, target)?,
        }))
    }
}
