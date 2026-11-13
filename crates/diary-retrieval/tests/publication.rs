//! The publication guard is taken only for writes: opening an index whose schema is in place,
//! and searching it, never takes it (Python `tenant_writer.ensure_schema` checks first).

use std::cell::Cell;

use diary_retrieval::index::{Embedder, Kind, Options, Retriever};
use diary_retrieval::{Publication, Result, Target};

struct Counting<'a>(&'a Cell<usize>);

impl Publication for Counting<'_> {
    fn run(&self, write: &mut dyn FnMut() -> Result<()>) -> Result<()> {
        self.0.set(self.0.get() + 1);
        write()
    }
}

struct Fixed;

impl Embedder for Fixed {
    fn active(&self) -> Target {
        Target::new(None, "qa-embed-a", "embeddinggemma-2")
    }
    fn embed(
        &self,
        _: &Target,
        texts: &[&str],
        _: Kind,
    ) -> std::result::Result<Vec<Vec<f64>>, String> {
        Ok(texts.iter().map(|_| vec![1.0, 0.5, 0.25]).collect())
    }
}

#[test]
fn opening_an_existing_index_takes_no_guard() -> std::result::Result<(), Box<dyn std::error::Error>>
{
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("index.db");
    let calls = Cell::new(0);
    let mut first = Retriever::open(&path, Fixed, Counting(&calls), Options::default())?;
    assert_eq!(calls.get(), 1, "creating the schema is a guarded write");
    first.reindex_file(
        "2031-03.md",
        Some("## Monday, March 3, 2031\n\n### 08:10\n\n**Me:** QA line.\n"),
    )?;
    assert_eq!(calls.get(), 2, "a re-index is a guarded write");
    drop(first);
    calls.set(0);
    let mut again = Retriever::open(&path, Fixed, Counting(&calls), Options::default())?;
    let (hits, status) = again.search("QA line", 8, 0.0, false)?;
    assert_eq!((hits.len(), status), (1, None));
    assert_eq!(
        calls.get(),
        0,
        "open and search of an existing index write nothing"
    );
    Ok(())
}
