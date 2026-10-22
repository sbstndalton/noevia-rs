//! The router's model listing (`GET /models`), reduced to what admission needs: each row's id and
//! aliases, whether its process holds the engine, and whether it is a small exempt model.
//!
//! The router accepts a model's aliases wherever it accepts its id and resolves them to the id, so
//! admission keys everything by the id ([`Listing::canonical`]).

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
    /// Other names the router accepts for this model.
    pub aliases: Vec<String>,
}

/// The parsed listing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub rows: Vec<ModelRow>,
}

impl Listing {
    pub fn status_of(&self, model: &str) -> Option<RowStatus> {
        self.row(model).map(|r| r.status)
    }

    /// The row a name refers to: the row with that id, else the row with that alias (as the
    /// router resolves names).
    pub fn row(&self, name: &str) -> Option<&ModelRow> {
        self.rows.iter().find(|r| r.id == name).or_else(|| {
            self.rows
                .iter()
                .find(|r| r.aliases.iter().any(|a| a == name))
        })
    }

    /// The id a name refers to, or `None` when the listing knows no such model or alias.
    pub fn canonical(&self, name: &str) -> Option<&str> {
        self.row(name).map(|r| r.id.as_str())
    }
}

/// The most aliases read for one row.
pub const MAX_ALIASES: usize = 64;

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
        let aliases = value
            .get("aliases")
            .and_then(|a| a.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|a| a.as_str())
                    .filter(|a| crate::config::valid_model_id(a) && *a != id)
                    .take(MAX_ALIASES)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        rows.push(ModelRow {
            id: id.to_owned(),
            status,
            small,
            aliases,
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
            {"id":"route-smart-model","status":{"value":"sleeping"},"aliases":["smart-alias","",7,"route-smart-model"]},
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
        assert_eq!(listing.rows[1].aliases, vec!["smart-alias".to_owned()]);
        assert_eq!(listing.canonical("smart-alias"), Some("route-smart-model"));
        assert_eq!(
            listing.canonical("route-smart-model"),
            Some("route-smart-model")
        );
        assert_eq!(listing.canonical("unknown"), None);
        assert_eq!(listing.status_of("smart-alias"), Some(RowStatus::Sleeping));
    }

    #[test]
    fn refuses_what_is_not_a_listing() {
        assert!(parse_listing(b"not json").is_none());
        assert!(parse_listing(br#"{"data":{}}"#).is_none());
        assert!(parse_listing(br#"[]"#).is_none());
        assert_eq!(parse_listing(br#"{"data":[]}"#).unwrap().rows.len(), 0);
    }
}
