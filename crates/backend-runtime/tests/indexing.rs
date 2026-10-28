use backend_runtime::{
    chat::FutureResult,
    diary::{Corpus, WriteCondition},
    indexing::{Embeddings, Indexer},
    retrieval::{Index, Target},
    Control, Fault,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
struct Engine {
    corpus: Corpus,
    mutate: AtomicBool,
}
impl Embeddings for Engine {
    fn embed<'a>(
        &'a self,
        _: &'a Target,
        input: &'a [String],
        control: &'a Control,
    ) -> FutureResult<'a, Vec<Vec<f32>>> {
        Box::pin(async move {
            if self.mutate.swap(false, Ordering::SeqCst) {
                self.corpus.put(
                    "diary.md",
                    "## October 28, 2026\n### Changed\n**Me:** new\n**Claude:** newer",
                    WriteCondition {
                        if_match: None,
                        if_none_match: false,
                    },
                    1.0,
                    control,
                    || Ok(()),
                )?;
            }
            Ok(input.iter().map(|_| vec![1.0, 0.0]).collect())
        })
    }
}
#[tokio::test]
async fn concurrent_edit_refuses_publication_and_pending_outbox_hides_stale_text(
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("managed.db");
    let db = rusqlite::Connection::open(&path)?;
    db.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);CREATE TABLE files(path TEXT PRIMARY KEY,data BLOB NOT NULL,version TEXT NOT NULL,updated REAL NOT NULL);CREATE TABLE directories(path TEXT PRIMARY KEY);CREATE TABLE workspace_index_outbox(path TEXT PRIMARY KEY);")?;
    let user = "01234567-89ab-4cde-8fab-0123456789ab";
    for (k, v) in [("tenant", user), ("active", "1"), ("generation", "0")] {
        db.execute("INSERT INTO meta VALUES(?,?)", [k, v])?;
    }
    let corpus = Corpus::open(&path, user, 0.0)?;
    let control = Control::new(Duration::from_secs(10))?;
    corpus.put(
        "diary.md",
        "## October 28, 2026\n### Original\n**Me:** old\n**Claude:** answer",
        WriteCondition {
            if_match: None,
            if_none_match: false,
        },
        0.0,
        &control,
        || Ok(()),
    )?;
    let engine = Arc::new(Engine {
        corpus: corpus.clone(),
        mutate: AtomicBool::new(true),
    });
    let indexer = Indexer {
        corpus: corpus.clone(),
        index: Arc::new(Index::create(&root.path().join("index.db"))?),
        target: Target {
            model: "synthetic".into(),
            profile: "test".into(),
        },
        embeddings: engine,
    };
    assert_eq!(
        indexer.reconcile("diary.md", &control).await,
        Err(Fault::Conflict)
    );
    assert!(indexer
        .search("query", 10, -1.0, &control)
        .await?
        .is_empty());
    assert!(!indexer.reconcile("diary.md", &control).await?.pending);
    let hits = indexer.search("query", 10, -1.0, &control).await?;
    assert!(!hits.is_empty());
    assert!(hits.iter().any(|h| h.text.contains("newer")));
    corpus.put(
        "diary.md",
        "",
        WriteCondition {
            if_match: None,
            if_none_match: false,
        },
        2.0,
        &control,
        || Ok(()),
    )?;
    assert!(indexer
        .search("query", 10, -1.0, &control)
        .await?
        .is_empty());
    indexer.reconcile("diary.md", &control).await?;
    assert!(indexer
        .search("query", 10, -1.0, &control)
        .await?
        .is_empty());
    Ok(())
}
