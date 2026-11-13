//! A retrieved excerpt, as `searchProject` (`rag.cjs`) returns it and `filesContext` reads it.
//!
//! The search that produces it is [`crate::search`].

/// One excerpt. `score` is already rounded to three decimals by the search.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub file: String,
    pub body: String,
    pub score: f64,
}
