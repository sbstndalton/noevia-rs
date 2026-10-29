//! Native model-engine boundary and corpus-to-index orchestration. The engine computes
//! vectors only; parsing, validation, transaction ordering and search stay native.
use crate::{
    chat::FutureResult,
    diary::{Corpus, Hit},
    retrieval::{Applied, Index, Target},
    transport::{Endpoint, Transport},
    Control, Fault, Result,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};
pub trait Embeddings: Send + Sync {
    fn embed<'a>(
        &'a self,
        target: &'a Target,
        input: &'a [String],
        control: &'a Control,
    ) -> FutureResult<'a, Vec<Vec<f32>>>;
}
pub struct OpenAiEmbeddings {
    endpoint: Endpoint,
    transport: Transport,
}
impl OpenAiEmbeddings {
    pub fn new(endpoint: Endpoint) -> Result<Self> {
        Ok(Self {
            endpoint,
            transport: Transport::new()?,
        })
    }
}
impl Embeddings for OpenAiEmbeddings {
    fn embed<'a>(
        &'a self,
        target: &'a Target,
        input: &'a [String],
        control: &'a Control,
    ) -> FutureResult<'a, Vec<Vec<f32>>> {
        Box::pin(async move {
            target.id()?;
            if input.is_empty()
                || input.len() > 32
                || input.iter().map(String::len).sum::<usize>() > 1024 * 1024
            {
                return Err(Fault::Capacity);
            }
            let body = serde_json::to_vec(
                &json!({"model":target.model,"input":input,"encoding_format":"float"}),
            )?;
            let reply = self
                .transport
                .request(
                    &self.endpoint,
                    reqwest::Method::POST,
                    reqwest::header::HeaderMap::new(),
                    Some(body),
                    16 * 1024 * 1024,
                    control,
                )
                .await?;
            if !(200..300).contains(&reply.status) {
                return Err(Fault::Transport);
            }
            let value: Value = serde_json::from_slice(&reply.body)?;
            let data = value
                .get("data")
                .and_then(Value::as_array)
                .ok_or(Fault::Protocol)?;
            if data.len() != input.len() {
                return Err(Fault::Protocol);
            }
            let mut ordered = BTreeMap::new();
            let mut dim = None;
            for row in data {
                let index = row
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .ok_or(Fault::Protocol)?;
                if index >= input.len() {
                    return Err(Fault::Protocol);
                }
                let values = row
                    .get("embedding")
                    .and_then(Value::as_array)
                    .ok_or(Fault::Protocol)?;
                if values.is_empty()
                    || values.len() > 65536
                    || dim.is_some_and(|d| d != values.len())
                {
                    return Err(Fault::Protocol);
                }
                dim = Some(values.len());
                let vector = values
                    .iter()
                    .map(|v| {
                        let n = v.as_f64().ok_or(Fault::Protocol)? as f32;
                        if !n.is_finite() {
                            return Err(Fault::Protocol);
                        }
                        Ok(n)
                    })
                    .collect::<Result<Vec<_>>>()?;
                if ordered.insert(index, vector).is_some() {
                    return Err(Fault::Protocol);
                }
            }
            Ok(ordered.into_values().collect())
        })
    }
}
pub struct Indexer {
    pub corpus: Corpus,
    pub index: Arc<Index>,
    pub target: Target,
    pub embeddings: Arc<dyn Embeddings>,
}
impl Indexer {
    /// Reconciliation is safe to retry: a concurrent edit refuses publication and keeps
    /// the file dirty. A crash leaves it dirty until this complete operation succeeds.
    pub async fn reconcile(&self, file: &str, control: &Control) -> Result<Applied> {
        control.check()?;
        self.index.mark_dirty(file)?;
        let current = self.corpus.read(file, control)?;
        let mut plan = Index::plan(
            file,
            current.as_ref().map(|v| v.content.as_str()).unwrap_or(""),
            self.target.clone(),
        )?;
        let unique: BTreeMap<_, _> = plan
            .chunks
            .iter()
            .map(|c| (c.content_hash.clone(), c.text.clone()))
            .collect();
        let entries: Vec<_> = unique.into_iter().collect();
        for batch in entries.chunks(32) {
            control.check()?;
            let input: Vec<String> = batch.iter().map(|(_, text)| text.clone()).collect();
            let vectors = self.embeddings.embed(&self.target, &input, control).await?;
            if vectors.len() != batch.len() {
                return Err(Fault::Protocol);
            }
            for ((hash, _), vector) in batch.iter().zip(vectors) {
                if vector.is_empty() || vector.iter().any(|v| !v.is_finite()) {
                    return Err(Fault::Protocol);
                }
                plan.vectors.insert(hash.clone(), vector);
            }
        }
        self.corpus.publish_index(
            file,
            current.as_ref().map(|v| v.version.as_str()),
            control,
            || self.index.apply(&plan, control),
        )
    }
    pub async fn search(
        &self,
        query: &str,
        top_k: usize,
        min_score: f64,
        control: &Control,
    ) -> Result<Vec<Hit>> {
        if query.is_empty() || query.len() > 65536 {
            return Err(Fault::Invalid);
        }
        let input = vec![query.to_owned()];
        let vectors = self.embeddings.embed(&self.target, &input, control).await?;
        if vectors.len() != 1 {
            return Err(Fault::Protocol);
        }
        self.corpus.with_index_fence(control, |pending| {
            for file in pending {
                self.index.mark_dirty(file)?;
            }
            self.index.search(
                &self.target,
                vectors.first().ok_or(Fault::Protocol)?,
                top_k,
                min_score,
                control,
            )
        })
    }
}
