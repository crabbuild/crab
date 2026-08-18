//! Credential-free source descriptors used by data import/update commands.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Component;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{Result, WorkflowError};

/// Current source descriptor schema.
pub const SOURCE_DESCRIPTOR_SCHEMA_VERSION: u16 = 1;

/// One imported data source bound to a repository-relative target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDescriptor {
    /// Descriptor schema.
    pub schema_version: u16,
    /// Stable descriptor id.
    pub id: String,
    /// Source kind: `repo`, `url`, or a connector name.
    pub kind: String,
    /// Credential-free canonical locator.
    pub locator: String,
    /// Locked revision or query identity.
    pub revision: Option<String>,
    /// Provider validator observed at import time.
    pub validator: Option<String>,
    /// Verified Crab content hash.
    pub content_hash: String,
    /// Verified byte count.
    pub size: u64,
    /// Target path relative to the worktree.
    pub target: String,
    /// Non-secret connector metadata.
    pub metadata: BTreeMap<String, String>,
}

/// Load a descriptor, rejecting newer schemas.
pub fn load_source_descriptor(path: &Path) -> Result<SourceDescriptor> {
    crate::atomic::ensure_parent_not_symlink(path)?;
    let metadata = fs::symlink_metadata(path).map_err(WorkflowError::Io)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid(
            "source_descriptor_file_invalid",
            path.display().to_string(),
        ));
    }
    let descriptor: SourceDescriptor =
        serde_json::from_slice(&fs::read(path).map_err(WorkflowError::Io)?)
            .map_err(|error| invalid("source_descriptor_parse", error.to_string()))?;
    validate_descriptor(&descriptor, path)?;
    Ok(descriptor)
}

/// Save a descriptor with an atomic replace.
pub fn save_source_descriptor(path: &Path, descriptor: &SourceDescriptor) -> Result<()> {
    validate_descriptor(descriptor, path)?;
    let parent = path.parent().ok_or_else(|| {
        invalid(
            "source_descriptor_path_no_parent",
            path.display().to_string(),
        )
    })?;
    crate::atomic::ensure_parent_not_symlink(path)?;
    fs::create_dir_all(parent).map_err(WorkflowError::Io)?;
    let bytes = serde_json::to_vec_pretty(descriptor)
        .map_err(|error| invalid("source_descriptor_serialize", error.to_string()))?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("source"),
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

fn validate_descriptor(descriptor: &SourceDescriptor, path: &Path) -> Result<()> {
    if descriptor.schema_version > SOURCE_DESCRIPTOR_SCHEMA_VERSION {
        return Err(invalid(
            "source_descriptor_schema_newer",
            path.display().to_string(),
        ));
    }
    if descriptor.id.len() != 64
        || !descriptor.id.bytes().all(|byte| byte.is_ascii_hexdigit())
        || descriptor.id.chars().any(char::is_control)
        || descriptor.kind.trim().is_empty()
        || descriptor.kind.len() > 64
        || descriptor.kind.chars().any(char::is_control)
        || descriptor.locator.trim().is_empty()
        || descriptor.locator.len() > 4096
        || descriptor.locator.chars().any(char::is_control)
        || descriptor.target.trim().is_empty()
    {
        return Err(invalid(
            "source_descriptor_identity_invalid",
            path.display().to_string(),
        ));
    }
    if descriptor
        .revision
        .as_deref()
        .is_some_and(|value| value.len() > 2048 || value.chars().any(char::is_control))
        || descriptor
            .validator
            .as_deref()
            .is_some_and(|value| value.len() > 1024 || value.chars().any(char::is_control))
    {
        return Err(invalid(
            "source_descriptor_identity_invalid",
            path.display().to_string(),
        ));
    }
    validate_locator(&descriptor.locator, path)?;
    let target = Path::new(&descriptor.target);
    let target_text = descriptor.target.replace('\\', "/");
    if target.as_os_str().is_empty()
        || target == Path::new(".")
        || target.is_absolute()
        || target_text.starts_with('/')
        || target_text.starts_with("../")
        || target_text == ".."
        || target_text.split('/').any(|component| component == "..")
        || target_text.as_bytes().get(1) == Some(&b':')
        || target.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(invalid(
            "source_descriptor_target_invalid",
            descriptor.target.clone(),
        ));
    }
    let Some(hash) = descriptor.content_hash.strip_prefix("b3:") else {
        return Err(invalid(
            "source_descriptor_hash_invalid",
            descriptor.content_hash.clone(),
        ));
    };
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(
            "source_descriptor_hash_invalid",
            descriptor.content_hash.clone(),
        ));
    }
    if descriptor.metadata.len() > 64 {
        return Err(invalid(
            "source_descriptor_metadata_too_large",
            path.display().to_string(),
        ));
    }
    for (key, value) in &descriptor.metadata {
        if key.trim().is_empty()
            || key.len() > 64
            || key.chars().any(char::is_control)
            || value.len() > 1024
            || value.chars().any(char::is_control)
        {
            return Err(invalid(
                "source_descriptor_metadata_invalid",
                path.display().to_string(),
            ));
        }
        let lower = key.to_ascii_lowercase();
        if [
            "token",
            "secret",
            "password",
            "credential",
            "access_key",
            "private_key",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
            || value.contains("@") && descriptor.kind == "url"
        {
            return Err(invalid(
                "source_descriptor_secret_metadata",
                path.display().to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_locator(locator: &str, path: &Path) -> Result<()> {
    let Ok(url) = url::Url::parse(locator) else {
        return Ok(());
    };
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(invalid(
            "source_descriptor_locator_secret",
            path.display().to_string(),
        ));
    }
    for (key, _) in url.query_pairs() {
        let lower = key.to_ascii_lowercase();
        if [
            "token",
            "secret",
            "password",
            "credential",
            "access_key",
            "signature",
            "auth",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        {
            return Err(invalid(
                "source_descriptor_locator_secret",
                path.display().to_string(),
            ));
        }
    }
    Ok(())
}

fn invalid(code: &str, detail: impl Into<String>) -> WorkflowError {
    WorkflowError::DvcMigrationInvalid {
        key: code.to_owned(),
        origin: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_round_trips_without_secret_fields() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sources/model.json");
        let descriptor = SourceDescriptor {
            schema_version: SOURCE_DESCRIPTOR_SCHEMA_VERSION,
            id: "ab".repeat(32),
            kind: "url".to_owned(),
            locator: "https://example.test/model".to_owned(),
            revision: Some("etag-1".to_owned()),
            validator: Some("etag-1".to_owned()),
            content_hash: format!("b3:{}", "ab".repeat(32)),
            size: 3,
            target: "model.bin".to_owned(),
            metadata: BTreeMap::new(),
        };
        save_source_descriptor(&path, &descriptor).unwrap();
        assert_eq!(load_source_descriptor(&path).unwrap(), descriptor);
        assert!(!fs::read_to_string(path).unwrap().contains("secret"));
    }

    #[test]
    fn descriptor_rejects_credentials_in_locator() {
        let descriptor = SourceDescriptor {
            schema_version: SOURCE_DESCRIPTOR_SCHEMA_VERSION,
            id: "ab".repeat(32),
            kind: "url".to_owned(),
            locator: "https://user:secret@example.test/model".to_owned(),
            revision: None,
            validator: None,
            content_hash: format!("b3:{}", "ab".repeat(32)),
            size: 3,
            target: "model.bin".to_owned(),
            metadata: BTreeMap::new(),
        };
        let error = validate_descriptor(&descriptor, Path::new("descriptor.json"))
            .expect_err("credential-bearing locator");
        assert!(
            error
                .to_string()
                .contains("source_descriptor_locator_secret")
        );
    }
}
