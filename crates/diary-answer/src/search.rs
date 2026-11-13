//! The chunk index as the context assembler's vector search: `diary_retrieval`'s `Retriever`
//! behind `diary_core::assembler::Search`. A failed search is the assembler's `search_failed`
//! (the direct-file fallback then runs), as Python's context treats any exception from
//! `Retriever.search`.

use std::sync::{Arc, Mutex};

use diary_core::assembler::{Hit, Search, SearchFailed, SearchOutcome};
use diary_retrieval::index::{Embedder, Kind, Retriever};
use diary_retrieval::{Publication, Target};

/// An embedder shared by the index and the migration.
pub struct Shared<E>(pub Arc<E>);

impl<E: Embedder> Embedder for Shared<E> {
    fn active(&self) -> Target {
        self.0.active()
    }
    fn embed(&self, target: &Target, texts: &[&str], kind: Kind) -> Result<Vec<Vec<f64>>, String> {
        self.0.embed(target, texts, kind)
    }
}

pub struct IndexSearch<E: Embedder, P: Publication> {
    pub retriever: Mutex<Retriever<E, P>>,
}

impl<E: Embedder, P: Publication> Search for IndexSearch<E, P> {
    fn search(
        &self,
        query: &str,
        top_k: i64,
        min_score: f64,
        with_ids: bool,
    ) -> Result<SearchOutcome, SearchFailed> {
        let mut r = self
            .retriever
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match r.search(query, top_k, min_score, with_ids) {
            Ok((hits, status)) => Ok(SearchOutcome {
                hits: hits
                    .into_iter()
                    .map(|h| Hit {
                        id: h.id,
                        day: h.day,
                        header: h.header,
                        text: h.text,
                        score: h.score,
                    })
                    .collect(),
                status: status.map(|s| s.as_str().to_owned()),
            }),
            Err(_) => Err(SearchFailed),
        }
    }
}
