//! Strongly-typed identifiers.
//!
//! Each identifier is a distinct newtype so they cannot be transposed. Session,
//! turn, and message identifiers are UUIDs minted by LocalPilot. A tool-use
//! identifier instead wraps the opaque, provider-assigned string that correlates
//! a tool call with its result on the wire.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::CoreError;

macro_rules! uuid_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generate a new random identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Wrap an existing UUID.
            #[must_use]
            pub fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// The underlying UUID.
            #[must_use]
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = CoreError;

            /// # Errors
            /// Returns [`CoreError::InvalidId`] if `s` is not a valid UUID.
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }
    };
}

uuid_newtype! {
    /// Identifies a single session: one conversation and its persisted state.
    SessionId
}

uuid_newtype! {
    /// Identifies a single turn within a session.
    TurnId
}

uuid_newtype! {
    /// Identifies a single message within a session.
    MessageId
}

uuid_newtype! {
    /// Identifies one entry in a session's durable event log.
    EventId
}

/// Correlates a tool call with its result. This wraps the provider-assigned
/// string rather than a UUID, because that opaque token is what must match
/// between a tool call and the result returned to the provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolUseId(String);

impl ToolUseId {
    /// Wrap a provider-assigned correlation token.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The underlying token.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolUseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for ToolUseId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ToolUseId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_ids_are_distinct_and_roundtrip() {
        let s = SessionId::new();
        let parsed = SessionId::from_str(&s.to_string()).unwrap();
        assert_eq!(s, parsed);
        // Distinct types: the same UUID wrapped in two newtypes is not
        // interchangeable, which the compiler enforces; we assert value parity
        // only through the shared UUID.
        let raw = s.as_uuid();
        assert_eq!(TurnId::from_uuid(raw).as_uuid(), raw);
        assert_eq!(MessageId::from_uuid(raw).as_uuid(), raw);
    }

    #[test]
    fn invalid_uuid_is_a_typed_error() {
        assert!(SessionId::from_str("not-a-uuid").is_err());
    }

    #[test]
    fn tool_use_id_wraps_arbitrary_string() {
        let id = ToolUseId::from("call_abc123");
        assert_eq!(id.as_str(), "call_abc123");
        assert_eq!(id.to_string(), "call_abc123");
    }
}
