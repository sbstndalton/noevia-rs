#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use embedding_worker::{fill::*, *};
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};
struct Fixture {
    current: AtomicBool,
    writes: Mutex<Vec<i64>>,
    replies: Mutex<VecDeque<Result<Vec<Vec<f32>>, String>>>,
    inputs: Mutex<Vec<Vec<Vec<u16>>>>,
    invalidate: bool,
}
impl Fixture {
    fn new(replies: Vec<Result<Vec<Vec<f32>>, String>>, invalidate: bool) -> Self {
        Self {
            current: AtomicBool::new(true),
            writes: Mutex::new(vec![]),
            replies: Mutex::new(replies.into()),
            inputs: Mutex::new(vec![]),
            invalidate,
        }
    }
}
impl Store for &Fixture {
    fn missing(
        &self,
        _: &Index,
        _: &Target,
        _: usize,
        _: &BTreeSet<i64>,
    ) -> Result<Option<Vec<Row>>, String> {
        Ok(Some(
            (1..=2)
                .map(|id| Row {
                    id,
                    hash: format!("hash-{id}"),
                    text: format!("chunk-{id}").encode_utf16().collect(),
                })
                .collect(),
        ))
    }
    fn remaining(&self, _: &Index, _: &Target) -> Result<usize, String> {
        Ok(2 - self.writes.lock().unwrap().len())
    }
    fn current(&self, _: u64, control: &Control) -> bool {
        self.current.load(Ordering::Relaxed) && !control.stopped()
    }
    fn publish(
        &self,
        _: &Index,
        _: &Target,
        rows: &[Row],
        _: &[Vec<f32>],
        version: u64,
        control: &Control,
    ) -> Result<usize, String> {
        if !self.current(version, control) {
            return Ok(0);
        }
        for row in rows {
            assert_eq!(row.hash, format!("hash-{}", row.id));
            self.writes.lock().unwrap().push(row.id);
        }
        Ok(rows.len())
    }
}
impl Embed for &Fixture {
    fn embed<'a>(
        &'a self,
        _: &'a Target,
        input: &'a [Vec<u16>],
        _: &'a Control,
    ) -> Task<'a, Result<Vec<Vec<f32>>, String>> {
        Box::pin(async move {
            self.inputs.lock().unwrap().push(input.to_vec());
            if self.invalidate {
                self.current.store(false, Ordering::Relaxed);
            }
            self.replies.lock().unwrap().pop_front().unwrap()
        })
    }
}
async fn fill(f: &Fixture) -> Batch {
    Filler {
        store: f,
        embedder: f,
    }
    .fill(
        &Index {
            user: Some("synthetic".into()),
            project: "p".into(),
        },
        &Target {
            model: "test".into(),
            profile: "embeddinggemma-2".into(),
            base_url: None,
        },
        4,
        &BTreeSet::new(),
        7,
        &Control::default(),
    )
    .await
    .unwrap()
    .unwrap()
}
#[tokio::test]
async fn batch_failure_retries_single_chunks_and_records_only_refused_ids() {
    let f = Fixture::new(
        vec![
            Err("batch refused".into()),
            Ok(vec![vec![1.0, 0.0]]),
            Err("chunk refused".into()),
        ],
        false,
    );
    let result = fill(&f).await;
    assert_eq!(result.embedded, 1);
    assert_eq!(result.refused, vec![2]);
    assert_eq!(result.remaining, 1);
    assert_eq!(*f.writes.lock().unwrap(), vec![1]);
    assert_eq!(
        String::from_utf16(&f.inputs.lock().unwrap()[0][0]).unwrap(),
        "title: none | text: chunk-1"
    );
}
#[tokio::test]
async fn config_changed_while_embedding_prevents_publication() {
    let f = Fixture::new(vec![Ok(vec![vec![1.0], vec![0.0]])], true);
    assert_eq!(fill(&f).await.embedded, 0);
    assert!(f.writes.lock().unwrap().is_empty());
}
#[tokio::test]
async fn malformed_vectors_do_not_reach_transaction() {
    let f = Fixture::new(
        vec![
            Ok(vec![vec![f32::NAN], vec![1.0]]),
            Ok(vec![]),
            Ok(vec![vec![]]),
        ],
        false,
    );
    let r = fill(&f).await;
    assert_eq!(r.refused, vec![1, 2]);
    assert_eq!(r.embedded, 0);
    assert!(f.writes.lock().unwrap().is_empty());
}
