//! Versioned validator-aware hashes for large external dependencies.
//!
//! The index is deliberately a local observation cache, not a trust store.
//! Callers may reuse a digest only when the provider supplies a matching
//! strong validator and credential scope; otherwise they must stream and
//! re-hash the body.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{Result, WorkflowError};

/// Current external hash index schema.
pub const EXTERNAL_HASH_INDEX_SCHEMA_VERSION: u16 = 1;

/// One validator-bound observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalHashRecord {
    /// Canonical provider name.
    pub provider: String,
    /// Credential-free resource identity.
    pub locator: String,
    /// Non-secret credential or tenant scope.
    pub credential_scope: String,
    /// Provider-reported size.
    pub size: u64,
    /// Strong provider validator or version identity.
    pub validator: Option<String>,
    /// Provider last-modified timestamp, when exposed independently of the
    /// validator.
    #[serde(default)]
    pub last_modified: Option<String>,
    /// Crab content digest after streamed verification.
    pub crab_hash: String,
    /// Observation timestamp in milliseconds.
    pub observed_at_unix_ms: u64,
}

/// Crab-owned external hash index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalHashIndex {
    /// Index schema.
    pub schema_version: u16,
    /// Records keyed by canonical provider/resource/scope identity.
    pub records: BTreeMap<String, ExternalHashRecord>,
}

impl Default for ExternalHashIndex {
    fn default() -> Self {
        Self {
            schema_version: EXTERNAL_HASH_INDEX_SCHEMA_VERSION,
            records: BTreeMap::new(),
        }
    }
}

impl ExternalHashIndex {
    /// Load an index, treating a missing file as empty.
    pub fn load(path: &Path) -> Result<Self> {
        crate::atomic::ensure_parent_not_symlink(path)?;
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(WorkflowError::Io(error)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(WorkflowError::DvcMigrationInvalid {
                key: "external_hash_index_file_invalid".to_owned(),
                origin: path.display().to_string(),
            });
        }
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => return Err(WorkflowError::Io(error)),
        };
        let index: Self =
            serde_json::from_slice(&bytes).map_err(|error| WorkflowError::DvcMigrationInvalid {
                key: "external_hash_index_parse".to_owned(),
                origin: error.to_string(),
            })?;
        index.validate(path)?;
        Ok(index)
    }

    /// Save the index atomically.
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        self.validate(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| WorkflowError::DvcMigrationInvalid {
                key: "external_hash_index_path_no_parent".to_owned(),
                origin: path.display().to_string(),
            })?;
        crate::atomic::ensure_parent_not_symlink(path)?;
        fs::create_dir_all(parent).map_err(WorkflowError::Io)?;
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| {
            WorkflowError::DvcMigrationInvalid {
                key: "external_hash_index_serialize".to_owned(),
                origin: error.to_string(),
            }
        })?;
        let temporary = parent.join(format!(
            ".{}.tmp-{}-{}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("external-hashes"),
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

    /// Return a hash only when the validator and credential scope match.
    #[must_use]
    pub fn reusable(
        &self,
        provider: &str,
        locator: &str,
        credential_scope: &str,
        size: u64,
        validator: Option<&str>,
    ) -> Option<&str> {
        // A default SDK credential chain can resolve to a different tenant or
        // role between runs.  Treat records written without an explicit,
        // stable scope as observations that must be revalidated by streaming.
        // `default@...` is rejected as well because older indexes used that
        // ambiguous scope before the explicit-profile contract was added.
        if credential_scope == "unscoped" || credential_scope.starts_with("default@") {
            return None;
        }
        let key = record_key(provider, locator, credential_scope);
        let record = self.records.get(&key)?;
        if record.size != size
            || record.validator.as_deref() != validator
            || validator.is_some_and(|value| value.trim_start().starts_with("W/"))
        {
            return None;
        }
        validator.filter(|value| !value.is_empty())?;
        let hash = record.crab_hash.strip_prefix("b3:")?;
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        Some(record.crab_hash.as_str())
    }

    /// Record a digest observed after a complete streamed verification.
    pub fn insert(&mut self, record: ExternalHashRecord) -> Result<()> {
        if self.schema_version > EXTERNAL_HASH_INDEX_SCHEMA_VERSION
            || !valid_text(&record.provider, 64, true)
            || !valid_text(&record.locator, 4096, true)
            || !valid_text(&record.credential_scope, 256, true)
            || !valid_hash(&record.crab_hash)
            || record.validator.as_deref().is_some_and(|value| {
                !valid_text(value, 1024, true) || value.trim_start().starts_with("W/")
            })
            || record
                .last_modified
                .as_deref()
                .is_some_and(|value| !valid_text(value, 128, true))
        {
            return Err(WorkflowError::DvcMigrationInvalid {
                key: "external_hash_record_invalid".to_owned(),
                origin: record.locator,
            });
        }
        let key = record_key(&record.provider, &record.locator, &record.credential_scope);
        self.records.insert(key, record);
        Ok(())
    }

    fn validate(&self, path: &Path) -> Result<()> {
        if self.schema_version > EXTERNAL_HASH_INDEX_SCHEMA_VERSION {
            return Err(WorkflowError::DvcMigrationInvalid {
                key: "external_hash_index_schema_newer".to_owned(),
                origin: path.display().to_string(),
            });
        }
        for (key, record) in &self.records {
            if key != &record_key(&record.provider, &record.locator, &record.credential_scope)
                || !valid_text(&record.provider, 64, true)
                || !valid_text(&record.locator, 4096, true)
                || !valid_text(&record.credential_scope, 256, true)
                || !valid_hash(&record.crab_hash)
                || record.validator.as_deref().is_some_and(|value| {
                    !valid_text(value, 1024, true) || value.trim_start().starts_with("W/")
                })
                || record
                    .last_modified
                    .as_deref()
                    .is_some_and(|value| !valid_text(value, 128, true))
            {
                return Err(WorkflowError::DvcMigrationInvalid {
                    key: "external_hash_record_invalid".to_owned(),
                    origin: path.display().to_string(),
                });
            }
        }
        Ok(())
    }
}

/// Construct the stable non-secret index key.
#[must_use]
pub fn record_key(provider: &str, locator: &str, credential_scope: &str) -> String {
    format!("{provider}\0{locator}\0{credential_scope}")
}

fn valid_hash(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("b3:") else {
        return false;
    };
    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_text(value: &str, max_len: usize, nonempty: bool) -> bool {
    (!nonempty || !value.trim().is_empty())
        && value.len() <= max_len
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuse_requires_validator_and_scope() {
        let mut index = ExternalHashIndex::default();
        let expected_hash = format!("b3:{}", "ab".repeat(32));
        index
            .insert(ExternalHashRecord {
                provider: "http".to_owned(),
                locator: "https://example.test/object".to_owned(),
                credential_scope: "public".to_owned(),
                size: 7,
                validator: Some("etag-1".to_owned()),
                last_modified: None,
                crab_hash: expected_hash.clone(),
                observed_at_unix_ms: 1,
            })
            .unwrap();
        assert_eq!(
            index.reusable(
                "http",
                "https://example.test/object",
                "public",
                7,
                Some("etag-1")
            ),
            Some(expected_hash.as_str())
        );
        assert!(
            index
                .reusable(
                    "http",
                    "https://example.test/object",
                    "other",
                    7,
                    Some("etag-1")
                )
                .is_none()
        );
        assert!(
            index
                .reusable("http", "https://example.test/object", "public", 7, None)
                .is_none()
        );
        assert!(
            index
                .reusable("s3", "s3://bucket/object", "unscoped", 7, Some("etag-1"))
                .is_none()
        );
        index
            .insert(ExternalHashRecord {
                provider: "s3".to_owned(),
                locator: "s3://bucket/object".to_owned(),
                credential_scope: "unscoped".to_owned(),
                size: 7,
                validator: Some("etag-1".to_owned()),
                last_modified: None,
                crab_hash: expected_hash.clone(),
                observed_at_unix_ms: 1,
            })
            .unwrap();
        assert!(
            index
                .reusable(
                    "s3",
                    "s3://bucket/object",
                    "default@bucket",
                    7,
                    Some("etag-1")
                )
                .is_none()
        );
    }
}
