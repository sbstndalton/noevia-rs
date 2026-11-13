//! A retrieved excerpt, as `searchProject` (`rag.cjs`) returns it and `filesContext` reads it.
//!
//! The search itself (the cosine SQL with its score floor, `ORDER BY`, `LIMIT`, the version filter
//! and the rounding) is not here: it is ported with the vector query (R4-2), against outputs
//! recorded from a real index, not a hand model of SQLite's ordering.

/// One excerpt. `score` is already rounded to three decimals by the search.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub file: String,
    pub body: String,
    pub score: f64,
}
