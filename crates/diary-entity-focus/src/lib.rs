//! Pure Diary entity focus and post-search candidate decisions. Temporal
//! filtering is supplied as a validated original-rank partition by the caller.
mod generated_rules;
mod lexical;
mod people;
mod policy;

pub use lexical::{asked_entities, carried_entities, classify, near_word, refers_back, relational};
pub use people::identified_people;
pub use policy::{analyse, focus_candidates, parse_margin};

pub const PYTHON_SOURCE_SHA256: &str = generated_rules::SOURCE_SHA256;
pub const PYTHON_VERSION: &str = generated_rules::PYTHON_VERSION;
pub const PYTHON_UNICODE_VERSION: &str = generated_rules::UNICODE_VERSION;
pub const MAX_QUERY_BYTES: usize = 1024 * 1024;
pub const MAX_EARLIER: usize = 64;
pub const MAX_EARLIER_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_ROW_BYTES: usize = 256 * 1024;
pub const MAX_CANDIDATES: usize = 10_000;
pub const MAX_TOTAL_ROW_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_WORDS: usize = 8_192;
pub const MAX_WORD_CHARS: usize = 256;
pub const MAX_ENTITIES: usize = 4;
pub const MAX_NEAR_SPELLINGS: usize = 3;
pub const DEFAULT_MARGIN: f64 = 0.05;
pub const EPSILON: f64 = 1e-9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Capacity,
    InvalidPartition,
    InvalidAnchor,
    Internal,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Capacity => f.write_str("entity focus input capacity exceeded"),
            Self::InvalidPartition => f.write_str("invalid temporal rank partition"),
            Self::InvalidAnchor => f.write_str("invalid canonical temporal anchor"),
            Self::Internal => f.write_str("entity focus internal pattern failure"),
        }
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Entity {
    pub name: String,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Shared,
    Variant,
    Near,
    Partial,
    Extended,
    Match,
}
impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Variant => "variant",
            Self::Near => "near",
            Self::Partial => "partial",
            Self::Extended => "extended",
            Self::Match => "match",
        }
    }
    pub(crate) fn about(self) -> bool {
        matches!(self, Self::Match | Self::Extended | Self::Partial)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub day: String,
    pub inclusive: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    pub entities: Vec<Entity>,
    pub carried: bool,
    pub anchor: Option<Anchor>,
    pub relational: bool,
}
impl Analysis {
    pub fn active(&self) -> bool {
        !self.entities.is_empty() || self.anchor.is_some()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum NumericValue {
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
}
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub header: String,
    pub text: String,
    pub day: Option<String>,
    pub score: NumericValue,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalPartition {
    pub survivors: Vec<usize>,
    pub removed: Vec<usize>,
}
impl TemporalPartition {
    pub fn identity(n: usize) -> Result<Self, Error> {
        if n > MAX_CANDIDATES {
            return Err(Error::Capacity);
        }
        Ok(Self {
            survivors: (1..=n).collect(),
            removed: Vec::new(),
        })
    }
    pub fn validate(&self, n: usize) -> Result<(), Error> {
        if n > MAX_CANDIDATES {
            return Err(Error::Capacity);
        }
        if self.survivors.len().saturating_add(self.removed.len()) != n {
            return Err(Error::InvalidPartition);
        }
        let mut seen = vec![false; n];
        for ranks in [&self.survivors, &self.removed] {
            if ranks
                .windows(2)
                .any(|w| w.first().zip(w.get(1)).is_some_and(|(a, b)| a >= b))
            {
                return Err(Error::InvalidPartition);
            }
            for rank in ranks {
                let Some(slot) = rank.checked_sub(1).and_then(|i| seen.get_mut(i)) else {
                    return Err(Error::InvalidPartition);
                };
                if *slot {
                    return Err(Error::InvalidPartition);
                }
                *slot = true;
            }
        }
        if seen.iter().all(|b| *b) {
            Ok(())
        } else {
            Err(Error::InvalidPartition)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    AfterTimeAnchor,
    NearName,
    NearNameUnmatched,
    OtherPerson,
    BelowMargin,
    CandidateLimit,
}
impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AfterTimeAnchor => "after_time_anchor",
            Self::NearName => "near_name",
            Self::NearNameUnmatched => "near_name_unmatched",
            Self::OtherPerson => "other_person",
            Self::BelowMargin => "below_margin",
            Self::CandidateLimit => "candidate_limit",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Similar {
    pub kind: Kind,
    pub shown: String,
    pub asked: String,
}
#[derive(Debug, Clone, PartialEq)]
pub struct FocusResult {
    pub analysis: Analysis,
    pub margin: f64,
    pub matched: Vec<Entity>,
    pub kept_ranks: Vec<usize>,
    pub removed: Vec<(usize, Reason)>,
    pub similar: Vec<(usize, Similar)>,
    pub corrected_by: Vec<(usize, String)>,
    pub absent: Vec<(Entity, Vec<String>)>,
    pub exact: Vec<(usize, Vec<String>)>,
}
impl FocusResult {
    pub fn applied(&self) -> bool {
        !self.matched.is_empty()
    }
}
