//! Stable identifiers for the entities routes and configuration refer to.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Declares a newtype over `Uuid` with the shared constructor, display and serde behaviour.
macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a new identifier that has never been used before.
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// Returns the underlying UUID, for callers that need the raw value.
            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }

            /// Parses an identifier from its canonical hyphenated text form.
            pub fn parse(text: &str) -> Result<Self, uuid::Error> {
                Uuid::parse_str(text).map(Self)
            }

            /// Wraps an existing UUID, for identifiers derived rather than generated.
            pub fn from_uuid(value: Uuid) -> Self {
                Self(value)
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

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", $prefix, self.0)
            }
        }
    };
}

uuid_id!(
    /// Identifies an endpoint for the lifetime of the configuration that contains it.
    ///
    /// Assigned once at creation and never reused. Renaming an endpoint does not change it, which
    /// is what allows routes to survive renames, reboots and hardware being replugged.
    EndpointId,
    "EndpointId"
);

impl EndpointId {
    /// Derives the identifier of something belonging to this endpoint, the same on every run.
    pub fn derived(&self, label: &str) -> Self {
        Self(Uuid::new_v5(&self.0, label.as_bytes()))
    }
}

uuid_id!(
    /// Identifies a route between two endpoints.
    RouteId,
    "RouteId"
);

uuid_id!(
    /// Identifies a discovered or manually added network peer.
    PeerId,
    "PeerId"
);

/// Identifies an entry in the daemon's bounded event history.
///
/// Monotonic within a single daemon run. The history does not survive a restart, so these are not
/// persisted and carry no meaning across runs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(u64);

impl EventId {
    /// Creates an identifier from a raw sequence number.
    pub fn from_raw(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw sequence number.
    pub fn get(&self) -> u64 {
        self.0
    }

    /// Returns the next identifier in sequence, saturating rather than wrapping.
    pub fn next(&self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
