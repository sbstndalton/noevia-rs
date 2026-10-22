//! The router's model listing (`GET /models`), reduced to what admission needs: each row's id,
//! whether its process holds the engine, and whether it is a small exempt model.

use serde::Deserialize;

/// A row's state as the router reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowStatus {
    Unloaded,
    Loading,
    Loaded,
    /// The process runs but has freed its weights; the next request wakes it.
    Sleeping,
    /// Anything else (downloading, an unknown future state): not resident.
    Other,
}

impl RowStatus {
    /// Whether the row's process occupies the router's model slot.
    pub fn resident(self) -> bool {
        matches!(self, Self::Loading | Self::Loaded | Self::Sleeping)
    }
}

/// One listed model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRow {
    pub id: String,
    pub status: RowStatus,
    /// The router starts it as an embedding or reranking server.
    pub small: bool,
}

/// The parsed listing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub rows: Vec<ModelRow>,
}

impl Listing {
    pub fn status_of(&self, model: &str) -> Option<RowStatus> {
        self.rows.iter().find(|r| r.id == model).map(|r| r.status)
    }
}

/// The most rows read from one listing.
pub const MAX_ROWS: usize = 4096;

#[derive(Deserialize)]
struct RawListing {
    data: Vec<serde_json::Value>,
}

/// Parses a `GET /models` body. `None` when it is not a listing at all (the caller then treats
/// the router as not answering). Rows without a usable id are skipped.
pub fn parse_listing(body: &[u8]) -> Option<Listing> {
    let raw: RawListing = serde_json::from_slice(body).ok()?;
    let mut rows = Vec::new();
    for value in raw.data.iter().take(MAX_ROWS) {
        let Some(id) = value.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        if !crate::config::valid_model_id(id) {
            continue;
        }
        let status_value = value.get("status").and_then(|s| s.get("value"));
        let status = match status_value.and_then(|v| v.as_str()) {
            Some("unloaded") => RowStatus::Unloaded,
            Some("loading") => RowStatus::Loading,
            Some("loaded") => RowStatus::Loaded,
            Some("sleeping") => RowStatus::Sleeping,
            _ => RowStatus::Other,
        };
        let small = value
            .get("status")
            .and_then(|s| s.get("args"))
            .and_then(|a| a.as_array())
            .is_some_and(|args| {
                args.iter().any(|arg| {
                    matches!(
                        arg.as_str(),
                        Some("--embedding" | "--embeddings" | "--rerank" | "--reranking")
                    )
                })
            });
        rows.push(ModelRow {
            id: id.to_owned(),
            status,
            small,
        });
    }
    Some(Listing { rows })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn reads_rows_states_and_small_models() {
        let body = br#"{"data":[
            {"id":"route-code-model","status":{"value":"loaded","args":["--ctx-size","4096"]}},
            {"id":"route-smart-model","status":{"value":"sleeping"}},
            {"id":"embed-model","status":{"value":"unloaded","args":["--embedding"]}},
            {"id":"odd","status":{"value":"downloading"}},
            {"status":{"value":"loaded"}},
            {"id":"","status":{"value":"loaded"}},
            {"id":"bad\u0007id","status":{"value":"loaded"}}
        ]}"#;
        let listing = parse_listing(body).unwrap();
        assert_eq!(listing.rows.len(), 4);
        assert_eq!(listing.rows[0].status, RowStatus::Loaded);
        assert!(!listing.rows[0].small);
        assert_eq!(listing.rows[1].status, RowStatus::Sleeping);
        assert!(listing.rows[1].status.resident());
        assert!(listing.rows[2].small);
        assert_eq!(listing.rows[3].status, RowStatus::Other);
        assert!(!RowStatus::Other.resident());
    }

    #[test]
    fn refuses_what_is_not_a_listing() {
        assert!(parse_listing(b"not json").is_none());
        assert!(parse_listing(br#"{"data":{}}"#).is_none());
        assert!(parse_listing(br#"[]"#).is_none());
        assert_eq!(parse_listing(br#"{"data":[]}"#).unwrap().rows.len(), 0);
    }
}
