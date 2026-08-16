//! Experiment identifier contract.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::{Result, WorkflowError};

/// Content-addressed identifier for an experiment.
///
/// Wraps a UUIDv7 so the canonical string form sorts chronologically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExperimentId(pub Uuid);

impl ExperimentId {
    /// Generate a fresh UUIDv7 identifier.
    #[must_use]
    pub fn new_v7() -> Self {
        Self(Uuid::now_v7())
    }

    /// Borrow the underlying UUID.
    #[must_use]
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl fmt::Display for ExperimentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.hyphenated(), f)
    }
}

impl FromStr for ExperimentId {
    type Err = WorkflowError;

    fn from_str(s: &str) -> Result<Self> {
        if s.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(WorkflowError::ExperimentIdInvalid {
                raw: s.to_owned(),
                reason: "experiment id must be lowercase hyphenated UUID",
            });
        }

        let uuid = Uuid::parse_str(s).map_err(|_| WorkflowError::ExperimentIdInvalid {
            raw: s.to_owned(),
            reason: "experiment id is not a valid UUID",
        })?;

        if uuid.get_version_num() != 7 {
            return Err(WorkflowError::ExperimentIdInvalid {
                raw: s.to_owned(),
                reason: "experiment id must be UUIDv7",
            });
        }

        Ok(Self(uuid))
    }
}

impl Serialize for ExperimentId {
    fn serialize<S>(&self, ser: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ser.collect_str(&self.0.hyphenated())
    }
}

impl<'de> Deserialize<'de> for ExperimentId {
    fn deserialize<D>(de: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(de)?;
        Self::from_str(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use std::collections::HashSet;
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn new_v7_produces_version_seven() {
        let id = ExperimentId::new_v7();
        assert_eq!(id.as_uuid().get_version_num(), 7);
    }

    #[test]
    fn new_v7_ids_sort_in_chronological_order_as_strings() {
        let mut ids = Vec::with_capacity(10);
        for _ in 0..10 {
            ids.push(ExperimentId::new_v7());
            thread::sleep(Duration::from_millis(2));
        }

        let as_strings: Vec<String> = ids.iter().map(ToString::to_string).collect();
        let mut sorted = as_strings.clone();
        sorted.sort();

        assert_eq!(as_strings, sorted);
        let distinct: HashSet<&String> = as_strings.iter().collect();
        assert_eq!(distinct.len(), 10);
    }

    #[test]
    fn display_and_from_str_round_trip() {
        let id = ExperimentId::new_v7();
        let s = id.to_string();
        let parsed: ExperimentId = s.parse().expect("canonical form must round-trip");
        assert_eq!(id, parsed);
        assert_eq!(s, parsed.to_string());
    }

    #[test]
    fn from_str_rejects_too_short() {
        let err = ExperimentId::from_str("abc").unwrap_err();
        assert!(matches!(err, WorkflowError::ExperimentIdInvalid { .. }));
    }

    #[test]
    fn from_str_rejects_non_hex() {
        let err = ExperimentId::from_str("01931b9e-4b3c-7b2a-b9f0-zzzzzzzzzzzz").unwrap_err();
        assert!(matches!(err, WorkflowError::ExperimentIdInvalid { .. }));
    }

    #[test]
    fn from_str_rejects_uppercase() {
        let id = ExperimentId::new_v7();
        let upper = id.to_string().to_ascii_uppercase();
        let err = ExperimentId::from_str(&upper).unwrap_err();
        assert!(matches!(err, WorkflowError::ExperimentIdInvalid { .. }));
    }

    #[test]
    fn from_str_rejects_non_v7_uuid() {
        let v4 = "f47ac10b-58cc-4372-a567-0e02b2c3d479";
        let err = ExperimentId::from_str(v4).unwrap_err();
        match err {
            WorkflowError::ExperimentIdInvalid { raw, reason } => {
                assert_eq!(raw, v4);
                assert!(reason.contains("UUIDv7"), "reason was: {reason}");
            }
            other => panic!("wrong variant: {other}"),
        }
    }

    #[test]
    fn serde_round_trip_uses_canonical_string() {
        let id = ExperimentId::new_v7();
        let json = serde_json::to_string(&id).expect("serialize must succeed");
        assert_eq!(json, format!("\"{id}\""));

        let back: ExperimentId = serde_json::from_str(&json).expect("deserialize must succeed");
        assert_eq!(back, id);
    }

    #[test]
    fn serde_rejects_non_v7_uuid() {
        let v4_json = "\"f47ac10b-58cc-4372-a567-0e02b2c3d479\"";
        let res: std::result::Result<ExperimentId, _> = serde_json::from_str(v4_json);
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("UUIDv7"));
    }
}
