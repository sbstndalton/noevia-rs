use serde::{Deserialize, Serialize};
use std::ops::Deref;
use uuid::Uuid;

/// Macro to define a newtype ID.
///
/// noevia change: upstream wraps a `Uuid`; the protocol's ids are opaque strings and Noevia's
/// stored message ids are not UUIDs, so the newtype wraps a `String` (serialized as one, as before).
macro_rules! define_id_type {
    ($name:ident) => {
        #[doc = concat!(stringify!($name), ": A newtype used to prevent mixing it with other ID values.")]
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            /// Creates a new random ID.
            pub fn random() -> Self {
                Self(Uuid::new_v4().to_string())
            }

            /// noevia addition: an ID from any string.
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }
        }

        /// Allows creating an ID from a Uuid.
        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid.to_string())
            }
        }

        /// noevia addition: from a string.
        impl From<String> for $name {
            fn from(id: String) -> Self {
                Self(id)
            }
        }

        /// noevia addition: from a string slice.
        impl From<&str> for $name {
            fn from(id: &str) -> Self {
                Self(id.to_owned())
            }
        }

        /// Allows getting a reference to the inner string.
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        /// Allows printing the ID.
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        /// Allows parsing an ID from a string slice (any string is an ID).
        impl std::str::FromStr for $name {
            type Err = std::convert::Infallible;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(s.to_owned()))
            }
        }

        /// Allows comparing the ID with a string slice.
        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }
    };
}

define_id_type!(AgentId);
define_id_type!(ThreadId);
define_id_type!(RunId);
define_id_type!(MessageId);

/// A tool call ID.
/// Used by some providers to denote a specific ID for a tool call generation, where the result of the tool call must also use this ID.
#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct ToolCallId(String);

/// Tool Call ID
///
/// Does not follow UUID format, instead uses "call_xxxxxxxx"
impl ToolCallId {
    pub fn random() -> Self {
        let uuid: String = Uuid::new_v4().to_string().chars().take(8).collect();
        let id = format!("call_{uuid}");
        Self(id)
    }

    /// noevia addition: an ID from any string (a provider's own call id, or one derived from a
    /// stored message).
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// noevia addition: from a string.
impl From<String> for ToolCallId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// noevia addition: from a string slice.
impl From<&str> for ToolCallId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

impl std::fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Deref for ToolCallId {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    // Test whether tool call ID has same format as rest of AG-UI
    #[test]
    fn test_tool_call_random() {
        let id = super::ToolCallId::random();
        assert_eq!(id.0.len(), 5 + 8);
        assert!(id.0.starts_with("call_"));
        dbg!(id);
    }
}
