//! Distributed correlation identifier (`TraceId`) backed by time-sortable `UUIDv7`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Distributed trace identifier propagated across Extension, Bridge, Core, and UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
#[serde(transparent)]
#[cfg_attr(feature = "specta", specta(transparent))]
pub struct TraceId(#[cfg_attr(feature = "specta", specta(type = String))] Uuid);

impl TraceId {
    /// Generates a new time-sortable `UUIDv7` trace identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wraps an existing [`Uuid`].
    #[must_use]
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Returns the underlying [`Uuid`].
    #[must_use]
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for TraceId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for TraceId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(Self)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn trace_id_round_trips_string_and_json() {
        let id = TraceId::new();
        let text = id.to_string();
        let parsed: TraceId = text.parse().unwrap();
        assert_eq!(id, parsed);

        let json = serde_json::to_string(&id).unwrap();
        let decoded: TraceId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, decoded);
    }

    #[test]
    fn trace_id_uses_uuid_v7() {
        let id = TraceId::default();
        assert_eq!(id.as_uuid().get_version_num(), 7);
        assert_eq!(TraceId::from_uuid(id.as_uuid()), id);
    }
}
