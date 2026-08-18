//! Immutable experiment checkpoint lineage contracts.
//!
//! The executor currently rejects checkpoint stages in ordinary run/repro.
//! This module owns the versioned record and transition invariants used by
//! the experiment control protocol, so later transport and IPC code cannot
//! silently collapse checkpoints into persisted outputs.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{Result, WorkflowError};

/// Current checkpoint record schema.
pub const CHECKPOINT_SCHEMA_VERSION: u16 = 1;
/// Version of the stage-to-supervisor checkpoint protocol included in stage
/// identity. A protocol change must not reuse cache entries created by an
/// older supervisor.
pub const CHECKPOINT_PROTOCOL_VERSION: u16 = 1;

/// Immutable acknowledged checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    /// Record schema.
    pub schema_version: u16,
    /// Stable checkpoint id.
    pub id: String,
    /// Experiment identity.
    pub experiment: String,
    /// Producing stage.
    pub stage: String,
    /// Monotonic sequence within the stage lineage.
    pub sequence: u64,
    /// Parent checkpoint id, if this is not the first point.
    pub parent: Option<String>,
    /// Authenticated stage request nonce, when created by the live control protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_nonce: Option<String>,
    /// Stage hash that produced this point.
    pub stage_hash: String,
    /// Creation time in milliseconds since the Unix epoch.
    pub created_at_unix_ms: u64,
    /// Immutable output content identities by path.
    pub outputs: BTreeMap<String, String>,
    /// Declared metrics associated with this point.
    pub metrics: BTreeMap<String, String>,
    /// Whether this point is terminal for the stage.
    pub terminal: bool,
    /// Whether a future run may resume from this point.
    pub resumable: bool,
}

/// In-memory lineage with append, apply, reset, and GC reachability rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointLineage {
    /// Lineage schema.
    pub schema_version: u16,
    /// Ordered immutable records.
    pub records: Vec<CheckpointRecord>,
}

impl Default for CheckpointLineage {
    fn default() -> Self {
        Self {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            records: Vec::new(),
        }
    }
}

impl CheckpointLineage {
    /// Load a lineage journal, treating a missing file as an empty lineage.
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(WorkflowError::Io(error)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(invalid(
                "checkpoint_lineage_file_invalid",
                &path.display().to_string(),
            ));
        }
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => return Err(WorkflowError::Io(error)),
        };
        let lineage: Self = serde_json::from_slice(&bytes)
            .map_err(|error| invalid("checkpoint_lineage_parse", &error.to_string()))?;
        lineage.validate_schema(path)?;
        lineage.validate_records()?;
        Ok(lineage)
    }

    /// Persist the lineage through a synced temporary file and atomic rename.
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        self.validate_schema(path)?;
        self.validate_records()?;
        let parent = path.parent().ok_or_else(|| {
            invalid(
                "checkpoint_lineage_path_no_parent",
                &path.display().to_string(),
            )
        })?;
        crate::atomic::ensure_parent_not_symlink(path)?;
        fs::create_dir_all(parent).map_err(WorkflowError::Io)?;
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| invalid("checkpoint_lineage_serialize", &error.to_string()))?;
        let temporary = parent.join(format!(
            ".{}.tmp-{}-{}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("lineage"),
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(WorkflowError::Io)?;
        if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&temporary);
            return Err(WorkflowError::Io(error));
        }
        if let Err(error) = crate::atomic::replace_file(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        Ok(())
    }

    /// Append one acknowledged record and atomically persist the result.
    pub fn append_atomic(path: &Path, record: CheckpointRecord) -> Result<Self> {
        let mut lineage = Self::load(path)?;
        lineage.append(record)?;
        lineage.save_atomic(path)?;
        Ok(lineage)
    }

    /// Append an acknowledged point after checking sequence and parent links.
    pub fn append(&mut self, record: CheckpointRecord) -> Result<()> {
        if self.schema_version > CHECKPOINT_SCHEMA_VERSION
            || record.schema_version > CHECKPOINT_SCHEMA_VERSION
        {
            return Err(invalid("checkpoint_schema_newer", &record.id));
        }
        if record.sequence != self.records.len() as u64 {
            return Err(invalid("checkpoint_sequence_invalid", &record.id));
        }
        if record.sequence == 0 {
            if record.parent.is_some() {
                return Err(invalid("checkpoint_parent_invalid", &record.id));
            }
        } else {
            let expected = self.records.last().map(|value| value.id.as_str());
            if record.parent.as_deref() != expected {
                return Err(invalid("checkpoint_parent_mismatch", &record.id));
            }
        }
        if self.records.iter().any(|value| value.id == record.id) {
            return Err(invalid("checkpoint_id_duplicate", &record.id));
        }
        if record.id.trim().is_empty()
            || record.id.len() > 128
            || record.id.chars().any(char::is_control)
            || record.experiment.trim().is_empty()
            || record.experiment.len() > 128
            || record.experiment.chars().any(char::is_control)
            || record.stage.trim().is_empty()
            || record.stage.len() > 128
            || record.stage.chars().any(char::is_control)
            || record.parent.as_deref().is_some_and(|value| {
                value.is_empty() || value.len() > 128 || value.chars().any(char::is_control)
            })
            || record.request_nonce.as_deref().is_some_and(|value| {
                value.is_empty() || value.len() > 256 || value.chars().any(char::is_control)
            })
        {
            return Err(invalid("checkpoint_identity_invalid", &record.id));
        }
        if let Some(first) = self.records.first()
            && (record.experiment != first.experiment || record.stage != first.stage)
        {
            return Err(invalid("checkpoint_lineage_identity_mismatch", &record.id));
        }
        if !valid_checkpoint_digest(&record.stage_hash) {
            return Err(invalid("checkpoint_stage_hash_invalid", &record.id));
        }
        let mut paths = std::collections::BTreeSet::new();
        for (path, hash) in record.outputs.iter().chain(record.metrics.iter()) {
            if !is_safe_relative_path(path) || !paths.insert(path) || !valid_checkpoint_digest(hash)
            {
                return Err(invalid("checkpoint_payload_invalid", &record.id));
            }
        }
        if let Some(nonce) = record.request_nonce.as_deref()
            && self
                .records
                .iter()
                .any(|value| value.request_nonce.as_deref() == Some(nonce))
        {
            return Err(invalid("checkpoint_request_replayed", nonce));
        }
        self.records.push(record);
        Ok(())
    }

    /// Select one checkpoint by id or sequence, rejecting ambiguous selectors.
    pub fn select(&self, selector: &str) -> Result<&CheckpointRecord> {
        let by_id = self.records.iter().filter(|record| record.id == selector);
        let by_sequence = selector.parse::<u64>().ok().and_then(|sequence| {
            self.records
                .iter()
                .find(|record| record.sequence == sequence)
        });
        match (by_id.count(), by_sequence) {
            (1, None) => self
                .records
                .iter()
                .find(|record| record.id == selector)
                .ok_or_else(|| invalid("checkpoint_not_found", selector)),
            (0, Some(record)) => Ok(record),
            _ => Err(invalid("checkpoint_selector_ambiguous", selector)),
        }
    }

    /// Return the latest acknowledged resumable point.
    pub fn latest_resumable(&self) -> Option<&CheckpointRecord> {
        self.records.iter().rev().find(|record| record.resumable)
    }

    /// Return records reachable from a selected checkpoint, for GC protection.
    pub fn reachable_to(&self, selector: &str) -> Result<Vec<&CheckpointRecord>> {
        let selected = self.select(selector)?;
        let mut result = Vec::new();
        let mut current = Some(selected.id.as_str());
        while let Some(id) = current {
            let record = self
                .records
                .iter()
                .find(|record| record.id == id)
                .ok_or_else(|| invalid("checkpoint_parent_missing", id))?;
            current = record.parent.as_deref();
            result.push(record);
        }
        result.reverse();
        Ok(result)
    }

    /// Reset lineage to a base point, preserving the selected record as history.
    pub fn reset(&self, selector: Option<&str>) -> Result<Option<&CheckpointRecord>> {
        selector.map(|value| self.select(value)).transpose()
    }

    /// Return a lineage truncated to the selected checkpoint.
    ///
    /// The original journal remains untouched; callers persist this returned
    /// lineage only after recording their reset decision in experiment state.
    pub fn reset_to(&self, selector: &str) -> Result<Self> {
        let selected = self.select(selector)?;
        let records = self
            .records
            .iter()
            .take(selected.sequence as usize + 1)
            .cloned()
            .collect();
        Ok(Self {
            schema_version: self.schema_version,
            records,
        })
    }

    fn validate_schema(&self, path: &Path) -> Result<()> {
        if self.schema_version > CHECKPOINT_SCHEMA_VERSION {
            return Err(invalid(
                "checkpoint_lineage_schema_newer",
                &path.display().to_string(),
            ));
        }
        Ok(())
    }

    fn validate_records(&self) -> Result<()> {
        let mut checked = Self {
            schema_version: self.schema_version,
            records: Vec::new(),
        };
        for record in &self.records {
            checked.append(record.clone())?;
        }
        Ok(())
    }
}

fn invalid(code: &str, detail: &str) -> WorkflowError {
    WorkflowError::YamlInvalid {
        key: code.to_owned(),
        origin: detail.to_owned(),
    }
}

fn valid_checkpoint_digest(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("b3:") else {
        return false;
    };
    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_safe_relative_path(value: &str) -> bool {
    let path = Path::new(value);
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path.components().all(|component| {
            !matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(sequence: u64, id: &str, parent: Option<&str>) -> CheckpointRecord {
        CheckpointRecord {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            id: id.to_owned(),
            experiment: "exp".to_owned(),
            stage: "train".to_owned(),
            sequence,
            parent: parent.map(ToOwned::to_owned),
            request_nonce: None,
            stage_hash: format!("b3:{}", "cd".repeat(32)),
            created_at_unix_ms: 0,
            outputs: BTreeMap::from([("model.bin".to_owned(), format!("b3:{}", "ab".repeat(32)))]),
            metrics: BTreeMap::new(),
            terminal: false,
            resumable: true,
        }
    }

    #[test]
    fn lineage_requires_parent_and_sequence() {
        let mut lineage = CheckpointLineage::default();
        lineage.append(record(0, "one", None)).unwrap();
        lineage.append(record(1, "two", Some("one"))).unwrap();
        assert_eq!(
            lineage.latest_resumable().map(|value| value.id.as_str()),
            Some("two")
        );
        assert_eq!(lineage.reachable_to("two").unwrap().len(), 2);
        assert!(lineage.append(record(3, "bad", Some("two"))).is_err());
    }

    #[test]
    fn lineage_rejects_replayed_control_nonce() {
        let mut lineage = CheckpointLineage::default();
        let mut first = record(0, "one", None);
        first.request_nonce = Some("nonce".to_owned());
        lineage.append(first).unwrap();
        let mut replay = record(1, "two", Some("one"));
        replay.request_nonce = Some("nonce".to_owned());
        assert!(lineage.append(replay).is_err());
    }

    #[test]
    fn numeric_selector_and_reset_are_explicit() {
        let mut lineage = CheckpointLineage::default();
        lineage.append(record(0, "zero", None)).unwrap();
        assert_eq!(lineage.select("0").unwrap().id, "zero");
        assert_eq!(lineage.reset(Some("zero")).unwrap().unwrap().id, "zero");
        assert_eq!(lineage.reset_to("zero").unwrap().records.len(), 1);
    }

    #[test]
    fn lineage_round_trips_atomically_and_rejects_newer_schema() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lineage.json");
        let mut lineage = CheckpointLineage::default();
        lineage.append(record(0, "zero", None)).unwrap();
        lineage.save_atomic(&path).unwrap();
        assert_eq!(CheckpointLineage::load(&path).unwrap(), lineage);

        let mut newer = lineage;
        newer.schema_version = CHECKPOINT_SCHEMA_VERSION + 1;
        assert!(newer.save_atomic(&path).is_err());
    }
}
