//! Versioned artifact declarations and the local Git-native registry.
//!
//! The registry stores immutable manifests and compare-and-swap stage labels.
//! Payload snapshots are kept under Crab-owned per-worktree state; a later
//! remote adapter can publish the same manifest identity without changing the
//! declaration or promotion contract.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use crate::error::{Result, WorkflowError};
use crate::{ArtifactMetadata, hasher};

/// Current artifact contract schema.
pub const ARTIFACT_SCHEMA_VERSION: u16 = 1;
/// Immutable artifact ref prefix.
pub const ARTIFACT_REF_PREFIX: &str = "refs/crab/artifacts";

/// A validated artifact declaration from workflow metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDecl {
    /// Canonical artifact name.
    pub name: String,
    /// Repository-relative source path.
    pub path: String,
    /// Artifact kind, for example model or dataset.
    pub kind: String,
    /// Optional human description.
    pub description: Option<String>,
    /// Optional human labels.
    pub labels: Vec<String>,
    /// Bounded scalar metadata.
    pub metadata: BTreeMap<String, String>,
}

/// Validated declarations keyed by canonical name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactCatalog {
    /// Catalog schema.
    pub schema_version: u16,
    /// Declarations by name.
    pub declarations: BTreeMap<String, ArtifactDecl>,
}

/// Immutable artifact version manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactManifest {
    /// Manifest schema.
    pub schema_version: u16,
    /// Artifact name.
    pub name: String,
    /// Immutable version id derived from this manifest.
    pub version_id: String,
    /// Declared source path.
    pub path: String,
    /// Artifact kind.
    pub kind: String,
    /// Exact Crab content identity.
    pub content_hash: String,
    /// Byte size, including all files in a directory.
    pub size: u64,
    /// Source Git commit, when available.
    pub source_commit: Option<String>,
    /// Creation time, informational only.
    pub created_at_unix_ms: u64,
    /// Non-identity annotations.
    pub annotations: BTreeMap<String, String>,
}

/// One immutable promotion event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactPromotion {
    /// Artifact name.
    pub name: String,
    /// Target stage.
    pub stage: String,
    /// New immutable version.
    pub version_id: String,
    /// Prior stage value, if any.
    pub previous_version_id: Option<String>,
    /// Promotion timestamp.
    pub created_at_unix_ms: u64,
}

/// Local registry state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRegistry {
    /// Registry schema.
    pub schema_version: u16,
    /// Immutable manifests keyed by name then version id.
    pub versions: BTreeMap<String, BTreeMap<String, ArtifactManifest>>,
    /// Mutable stage labels.
    pub stages: BTreeMap<String, BTreeMap<String, String>>,
    /// Append-only local promotion history.
    pub history: Vec<ArtifactPromotion>,
}

impl Default for ArtifactRegistry {
    fn default() -> Self {
        Self {
            schema_version: ARTIFACT_SCHEMA_VERSION,
            versions: BTreeMap::new(),
            stages: BTreeMap::new(),
            history: Vec::new(),
        }
    }
}

impl ArtifactCatalog {
    /// Validate preserved top-level artifact metadata.
    pub fn from_metadata(metadata: &ArtifactMetadata) -> Result<Self> {
        let mut declarations = BTreeMap::new();
        let mut normalized_names = BTreeSet::new();
        let mut paths = BTreeSet::<String>::new();
        for (name, value) in &metadata.declarations {
            let declaration = ArtifactDecl::from_value(name, value)?;
            if !normalized_names.insert(name.to_ascii_lowercase()) {
                return Err(invalid("artifact_name_duplicate", name));
            }
            let normalized_path = normalize_artifact_path(&declaration.path);
            let normalized_prefix = format!("{normalized_path}/");
            if paths.iter().any(|existing| {
                normalized_path == *existing
                    || normalized_path.starts_with(&format!("{existing}/"))
                    || existing.starts_with(&normalized_prefix)
            }) {
                return Err(invalid("artifact_path_duplicate", declaration.path));
            }
            paths.insert(normalized_path);
            if declarations.insert(name.clone(), declaration).is_some() {
                return Err(invalid("artifact_name_duplicate", name));
            }
        }
        Ok(Self {
            schema_version: ARTIFACT_SCHEMA_VERSION,
            declarations,
        })
    }
}

impl ArtifactDecl {
    /// Parse and validate one artifact declaration.
    pub fn from_value(name: &str, value: &Value) -> Result<Self> {
        validate_artifact_name(name)?;
        let (path, kind, description, labels, metadata) = match value {
            Value::String(path) => (
                path.clone(),
                "file".to_owned(),
                None,
                Vec::new(),
                BTreeMap::new(),
            ),
            Value::Mapping(map) => {
                for key in map.keys() {
                    let Some(key) = key.as_str() else {
                        return Err(invalid("artifact_field_unsupported", name));
                    };
                    if !matches!(
                        key,
                        "path"
                            | "type"
                            | "kind"
                            | "description"
                            | "desc"
                            | "labels"
                            | "metadata"
                            | "meta"
                    ) {
                        return Err(invalid_detail(
                            "artifact_field_unsupported",
                            format!("{name}.{key}"),
                        ));
                    }
                }
                let path = yaml_string(map, "path")
                    .ok_or_else(|| invalid("artifact_path_missing", name))?;
                let kind = merge_alias(
                    yaml_string(map, "type"),
                    yaml_string(map, "kind"),
                    name,
                    "artifact_type_conflict",
                )?
                .unwrap_or_else(|| "file".to_owned());
                let description = merge_alias(
                    yaml_string(map, "description"),
                    yaml_string(map, "desc"),
                    name,
                    "artifact_description_conflict",
                )?;
                let labels = yaml_strings(map, "labels")?;
                let metadata = merge_metadata(
                    yaml_scalar_map(map, "metadata", name)?,
                    yaml_scalar_map(map, "meta", name)?,
                    name,
                )?;
                (path, kind, description, labels, metadata)
            }
            _ => return Err(invalid("artifact_declaration_shape", name)),
        };
        validate_artifact_path(&path, name)?;
        if kind.trim().is_empty() || kind.len() > 64 || kind.chars().any(char::is_control) {
            return Err(invalid("artifact_type_invalid", name));
        }
        if description
            .as_deref()
            .is_some_and(|value| value.len() > 1024 || value.chars().any(char::is_control))
        {
            return Err(invalid("artifact_description_invalid", name));
        }
        if labels.len() > 32 {
            return Err(invalid("artifact_labels_too_large", name));
        }
        let mut normalized_labels = BTreeSet::new();
        for label in &labels {
            if label.trim().is_empty() || label.len() > 64 || label.chars().any(char::is_control) {
                return Err(invalid("artifact_label_invalid", name));
            }
            if !normalized_labels.insert(label.to_ascii_lowercase()) {
                return Err(invalid("artifact_label_duplicate", name));
            }
        }
        Ok(Self {
            name: name.to_owned(),
            path: normalize_artifact_path(&path),
            kind,
            description,
            labels,
            metadata,
        })
    }
}

impl ArtifactRegistry {
    /// Load registry state, rejecting newer schemas.
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
            return Err(invalid(
                "artifact_registry_file_invalid",
                path.display().to_string(),
            ));
        }
        let registry: Self = serde_json::from_slice(&fs::read(path).map_err(WorkflowError::Io)?)
            .map_err(|error| invalid_detail("artifact_registry_parse", error))?;
        if registry.schema_version > ARTIFACT_SCHEMA_VERSION {
            return Err(invalid(
                "artifact_registry_schema_newer",
                path.display().to_string(),
            ));
        }
        registry.validate()?;
        Ok(registry)
    }

    /// Persist registry state atomically.
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        if self.schema_version > ARTIFACT_SCHEMA_VERSION {
            return Err(invalid(
                "artifact_registry_schema_newer",
                path.display().to_string(),
            ));
        }
        self.validate()?;
        let parent = path.parent().ok_or_else(|| {
            invalid(
                "artifact_registry_path_no_parent",
                path.display().to_string(),
            )
        })?;
        crate::atomic::ensure_parent_not_symlink(path)?;
        fs::create_dir_all(parent).map_err(WorkflowError::Io)?;
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| invalid_detail("artifact_registry_serialize", error))?;
        let temporary = parent.join(format!(
            ".{}.tmp-{}-{}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("registry"),
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

    /// Add an immutable version, rejecting identity reuse with different data.
    pub fn insert_version(&mut self, manifest: ArtifactManifest) -> Result<()> {
        validate_manifest(&manifest)?;
        let versions = self.versions.entry(manifest.name.clone()).or_default();
        if let Some(existing) = versions.get(&manifest.version_id) {
            if existing != &manifest {
                return Err(invalid("artifact_version_collision", manifest.version_id));
            }
            return Ok(());
        }
        versions.insert(manifest.version_id.clone(), manifest);
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version > ARTIFACT_SCHEMA_VERSION {
            return Err(invalid(
                "artifact_registry_schema_newer",
                self.schema_version.to_string(),
            ));
        }
        for (name, versions) in &self.versions {
            validate_artifact_name(name)?;
            for (version_id, manifest) in versions {
                if version_id != &manifest.version_id || manifest.name != *name {
                    return Err(invalid("artifact_registry_key_mismatch", name));
                }
                validate_manifest(manifest)?;
            }
        }
        for (name, labels) in &self.stages {
            validate_artifact_name(name)?;
            let versions = self
                .versions
                .get(name)
                .ok_or_else(|| invalid("artifact_stage_artifact_missing", name))?;
            for (stage, version_id) in labels {
                validate_artifact_stage(stage)?;
                if !versions.contains_key(version_id) {
                    return Err(invalid("artifact_stage_version_missing", version_id));
                }
            }
        }
        for event in &self.history {
            validate_artifact_name(&event.name)?;
            validate_artifact_stage(&event.stage)?;
            let Some(versions) = self.versions.get(&event.name) else {
                return Err(invalid("artifact_history_artifact_missing", &event.name));
            };
            if !versions.contains_key(&event.version_id)
                || event
                    .previous_version_id
                    .as_ref()
                    .is_some_and(|version| !versions.contains_key(version))
            {
                return Err(invalid(
                    "artifact_history_version_missing",
                    &event.version_id,
                ));
            }
        }
        Ok(())
    }

    /// Promote a version with an optional expected current value.
    pub fn promote(
        &mut self,
        name: &str,
        version_id: &str,
        stage: &str,
        expected: Option<&str>,
    ) -> Result<()> {
        validate_artifact_name(name)?;
        validate_artifact_stage(stage)?;
        let Some(versions) = self.versions.get(name) else {
            return Err(invalid("artifact_version_not_found", version_id));
        };
        if !versions.contains_key(version_id) {
            return Err(invalid("artifact_version_not_found", version_id));
        }
        let stage = stage.to_ascii_lowercase();
        let labels = self.stages.entry(name.to_owned()).or_default();
        let current = labels.get(&stage).map(String::as_str);
        if expected.is_some() && expected != current {
            return Err(invalid("artifact_promotion_conflict", stage));
        }
        let previous = labels.insert(stage.clone(), version_id.to_owned());
        self.history.push(ArtifactPromotion {
            name: name.to_owned(),
            stage,
            version_id: version_id.to_owned(),
            previous_version_id: previous,
            created_at_unix_ms: now_unix_ms(),
        });
        Ok(())
    }

    /// Resolve an immutable version or stage label.
    pub fn resolve(
        &self,
        name: &str,
        version: Option<&str>,
        stage: Option<&str>,
    ) -> Result<&ArtifactManifest> {
        validate_artifact_name(name)?;
        if version.is_some() == stage.is_some() {
            return Err(invalid("artifact_selector_requires_exactly_one", name));
        }
        let version_id = if let Some(version) = version {
            version.to_owned()
        } else {
            let stage = stage.unwrap_or_default().to_ascii_lowercase();
            self.stages
                .get(name)
                .and_then(|labels| labels.get(&stage))
                .cloned()
                .ok_or_else(|| invalid("artifact_stage_not_found", stage))?
        };
        self.versions
            .get(name)
            .and_then(|versions| versions.get(&version_id))
            .ok_or_else(|| invalid("artifact_version_not_found", version_id))
    }
}

/// Build an immutable manifest from a declared path.
pub fn manifest_from_path(
    root: &Path,
    declaration: &ArtifactDecl,
    source_commit: Option<String>,
) -> Result<(ArtifactManifest, PathBuf)> {
    validate_artifact_path(&declaration.path, &declaration.name)?;
    let path = root.join(&declaration.path);
    let metadata = fs::symlink_metadata(&path).map_err(WorkflowError::Io)?;
    if metadata.file_type().is_symlink() {
        return Err(invalid("artifact_symlink_unsupported", &declaration.path));
    }
    let (content_hash, size) = if metadata.is_file() {
        hash_file(&path)?
    } else if metadata.is_dir() {
        let tree = hasher::hash_directory(&path, false)?;
        (
            format!(
                "b3:{}",
                tree.hash
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ),
            tree.manifest.iter().map(|entry| entry.size).sum(),
        )
    } else {
        return Err(invalid("artifact_path_type_unsupported", &declaration.path));
    };
    let created_at_unix_ms = now_unix_ms();
    let mut manifest = ArtifactManifest {
        schema_version: ARTIFACT_SCHEMA_VERSION,
        name: declaration.name.clone(),
        version_id: String::new(),
        path: declaration.path.clone(),
        kind: declaration.kind.clone(),
        content_hash,
        size,
        source_commit,
        created_at_unix_ms,
        annotations: declaration.metadata.clone(),
    };
    manifest.version_id = manifest_identity(&manifest)?;
    Ok((manifest, path))
}

/// Return the Git ref namespace for an immutable artifact version.
pub fn artifact_version_ref(name: &str, version_id: &str) -> Result<String> {
    validate_artifact_name(name)?;
    let digest = version_id.strip_prefix("b3:").unwrap_or_default();
    if digest.len() != 64 || !digest.chars().all(|value| value.is_ascii_hexdigit()) {
        return Err(invalid("artifact_version_id_invalid", version_id));
    }
    Ok(format!(
        "{ARTIFACT_REF_PREFIX}/{}/versions/{}",
        percent_encode_name(name),
        version_id.trim_start_matches("b3:")
    ))
}

/// Return the Git ref namespace for a mutable artifact stage.
pub fn artifact_stage_ref(name: &str, stage: &str) -> Result<String> {
    validate_artifact_name(name)?;
    validate_artifact_stage(stage)?;
    Ok(format!(
        "{ARTIFACT_REF_PREFIX}/{}/stages/{}",
        percent_encode_name(name),
        stage.to_ascii_lowercase()
    ))
}

/// Copy an immutable payload snapshot into the registry version directory.
pub fn snapshot_payload(source: &Path, destination: &Path) -> Result<()> {
    let source_identity = payload_identity(source)?;
    if fs::symlink_metadata(destination).is_ok() {
        let destination_identity = payload_identity(destination)?;
        if destination_identity == source_identity {
            return Ok(());
        }
        return Err(invalid(
            "artifact_snapshot_collision",
            destination.display().to_string(),
        ));
    }
    crate::atomic::ensure_parent_not_symlink(destination)?;
    if source_identity.0 {
        let parent = destination.parent().ok_or_else(|| {
            invalid(
                "artifact_snapshot_path_no_parent",
                destination.display().to_string(),
            )
        })?;
        fs::create_dir_all(parent).map_err(WorkflowError::Io)?;
        let temporary = temporary_snapshot_path(destination);
        if let Err(error) = fs::copy(source, &temporary)
            .and_then(|_| fs::metadata(source))
            .and_then(|metadata| fs::set_permissions(&temporary, metadata.permissions()))
            .and_then(|_| File::open(&temporary).and_then(|file| file.sync_all()))
            .and_then(|_| fs::rename(&temporary, destination))
        {
            let _ = fs::remove_file(&temporary);
            return Err(WorkflowError::Io(error));
        }
        return Ok(());
    }
    let parent = destination.parent().ok_or_else(|| {
        invalid(
            "artifact_snapshot_path_no_parent",
            destination.display().to_string(),
        )
    })?;
    fs::create_dir_all(parent).map_err(WorkflowError::Io)?;
    let temporary = temporary_snapshot_path(destination);
    if let Err(error) = copy_directory(source, &temporary)
        .and_then(|_| fs::rename(&temporary, destination).map_err(WorkflowError::Io))
    {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    Ok(())
}

/// Verify a stored artifact payload against its immutable manifest identity.
pub fn verify_payload(path: &Path, expected_hash: &str, expected_size: u64) -> Result<()> {
    let (_, actual_hash, actual_size) = payload_identity(path)?;
    if actual_hash != expected_hash || actual_size != expected_size {
        return Err(invalid(
            "artifact_payload_integrity",
            path.display().to_string(),
        ));
    }
    Ok(())
}

fn payload_identity(path: &Path) -> Result<(bool, String, u64)> {
    let metadata = fs::symlink_metadata(path).map_err(WorkflowError::Io)?;
    if metadata.file_type().is_symlink() {
        return Err(invalid(
            "artifact_snapshot_symlink_unsupported",
            path.display().to_string(),
        ));
    }
    if metadata.is_file() {
        let (hash, size) = hash_file(path)?;
        return Ok((true, hash, size));
    }
    if metadata.is_dir() {
        let tree = hasher::hash_directory(path, false)?;
        let size = tree.manifest.iter().map(|entry| entry.size).sum();
        return Ok((
            false,
            format!(
                "b3:{}",
                tree.hash
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            ),
            size,
        ));
    }
    Err(invalid(
        "artifact_snapshot_source_missing",
        path.display().to_string(),
    ))
}

/// Validate a canonical artifact name before encoding it into a Git ref.
pub fn validate_artifact_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || name == "."
        || name == ".."
        || name.starts_with('.')
        || name.ends_with('.')
        || name.ends_with(".lock")
        || name.contains("..")
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return Err(invalid("artifact_name_invalid", name));
    }
    Ok(())
}

fn validate_artifact_path(path: &str, name: &str) -> Result<()> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(invalid("artifact_path_invalid", name));
    }
    Ok(())
}

fn normalize_artifact_path(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").to_owned()
}

fn temporary_snapshot_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("payload");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    destination
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{name}.tmp-{}-{nonce}", std::process::id()))
}

fn validate_artifact_stage(stage: &str) -> Result<()> {
    if stage.is_empty()
        || stage.len() > 64
        || !stage.chars().enumerate().all(|(index, c)| {
            if index == 0 {
                c.is_ascii_lowercase() || c.is_ascii_digit()
            } else {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
            }
        })
    {
        return Err(invalid("artifact_stage_invalid", stage));
    }
    Ok(())
}

fn validate_manifest(manifest: &ArtifactManifest) -> Result<()> {
    if manifest.schema_version > ARTIFACT_SCHEMA_VERSION {
        return Err(invalid(
            "artifact_manifest_schema_newer",
            manifest.name.clone(),
        ));
    }
    validate_artifact_name(&manifest.name)?;
    validate_artifact_path(&manifest.path, &manifest.name)?;
    validate_annotations(&manifest.annotations, &manifest.name)?;
    validate_digest("artifact_content_hash_invalid", &manifest.content_hash)?;
    artifact_version_ref(&manifest.name, &manifest.version_id)?;
    let expected = manifest_identity(manifest)?;
    if manifest.version_id != expected {
        return Err(invalid(
            "artifact_manifest_identity_mismatch",
            manifest.version_id.clone(),
        ));
    }
    Ok(())
}

fn manifest_identity(manifest: &ArtifactManifest) -> Result<String> {
    let mut identity = manifest.clone();
    identity.version_id.clear();
    identity.created_at_unix_ms = 0;
    let bytes = serde_json::to_vec(&identity)
        .map_err(|error| invalid_detail("artifact_manifest_serialize", error))?;
    Ok(format!("b3:{}", blake3::hash(&bytes).to_hex()))
}

fn validate_digest(code: &str, digest: &str) -> Result<()> {
    let value = digest.strip_prefix("b3:").unwrap_or_default();
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(code, digest));
    }
    Ok(())
}

fn percent_encode_name(name: &str) -> String {
    name.as_bytes()
        .iter()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                char::from(*byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut file = File::open(path).map_err(WorkflowError::Io)?;
    let size = file.metadata().map_err(WorkflowError::Io)?.len();
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(WorkflowError::Io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok((format!("b3:{}", hasher.finalize().to_hex()), size))
}

fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination).map_err(WorkflowError::Io)?;
    for entry in fs::read_dir(source).map_err(WorkflowError::Io)? {
        let entry = entry.map_err(WorkflowError::Io)?;
        let source_path = entry.path();
        let target_path = destination.join(entry.file_name());
        let file_type = entry.file_type().map_err(WorkflowError::Io)?;
        if file_type.is_symlink() {
            return Err(invalid(
                "artifact_snapshot_symlink_unsupported",
                source_path.display().to_string(),
            ));
        }
        if file_type.is_dir() {
            copy_directory(&source_path, &target_path)?;
            let permissions = fs::metadata(&source_path)
                .map_err(WorkflowError::Io)?
                .permissions();
            fs::set_permissions(&target_path, permissions).map_err(WorkflowError::Io)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &target_path).map_err(WorkflowError::Io)?;
            let permissions = fs::metadata(&source_path)
                .map_err(WorkflowError::Io)?
                .permissions();
            fs::set_permissions(&target_path, permissions).map_err(WorkflowError::Io)?;
            File::open(&target_path)
                .and_then(|file| file.sync_all())
                .map_err(WorkflowError::Io)?;
        } else {
            return Err(invalid(
                "artifact_snapshot_type_unsupported",
                source_path.display().to_string(),
            ));
        }
    }
    Ok(())
}

fn yaml_string(map: &serde_yaml::Mapping, key: &str) -> Option<String> {
    map.get(Value::String(key.to_owned()))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn merge_alias(
    canonical: Option<String>,
    legacy: Option<String>,
    name: &str,
    conflict_code: &str,
) -> Result<Option<String>> {
    match (canonical, legacy) {
        (Some(canonical), Some(legacy)) if canonical != legacy => Err(invalid(conflict_code, name)),
        (Some(value), _) | (_, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

fn merge_metadata(
    canonical: BTreeMap<String, String>,
    legacy: BTreeMap<String, String>,
    name: &str,
) -> Result<BTreeMap<String, String>> {
    let mut merged = canonical;
    for (key, value) in legacy {
        if let Some(existing) = merged.get(&key)
            && existing != &value
        {
            return Err(invalid("artifact_metadata_conflict", name));
        }
        merged.insert(key, value);
    }
    Ok(merged)
}

fn yaml_strings(map: &serde_yaml::Mapping, key: &str) -> Result<Vec<String>> {
    let Some(value) = map.get(Value::String(key.to_owned())) else {
        return Ok(Vec::new());
    };
    let Some(values) = value.as_sequence() else {
        return Err(invalid_detail("artifact_labels_invalid", key));
    };
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| invalid_detail("artifact_label_invalid", key))
        })
        .collect()
}

fn yaml_scalar_map(
    map: &serde_yaml::Mapping,
    key: &str,
    name: &str,
) -> Result<BTreeMap<String, String>> {
    let Some(value) = map.get(Value::String(key.to_owned())) else {
        return Ok(BTreeMap::new());
    };
    let Some(values) = value.as_mapping() else {
        return Err(invalid("artifact_metadata_invalid", name));
    };
    if values.len() > 64 {
        return Err(invalid("artifact_metadata_too_large", name));
    }
    values
        .iter()
        .map(|(key, value)| {
            let key = key
                .as_str()
                .ok_or_else(|| invalid("artifact_metadata_key_invalid", name))?;
            if key.trim().is_empty() || key.len() > 64 || key.chars().any(char::is_control) {
                return Err(invalid("artifact_metadata_key_invalid", name));
            }
            let value = match value {
                Value::String(value) => value.clone(),
                Value::Number(value) => value.to_string(),
                Value::Bool(value) => value.to_string(),
                Value::Null => "null".to_owned(),
                _ => return Err(invalid("artifact_metadata_value_invalid", name)),
            };
            if value.len() > 1024 || value.chars().any(char::is_control) {
                return Err(invalid("artifact_metadata_value_invalid", name));
            }
            let lower = key.to_ascii_lowercase();
            if [
                "token",
                "secret",
                "password",
                "credential",
                "private_key",
                "access_key",
            ]
            .iter()
            .any(|marker| lower.contains(marker))
            {
                return Err(invalid("artifact_metadata_secret", name));
            }
            Ok((key.to_owned(), value))
        })
        .collect()
}

fn validate_annotations(annotations: &BTreeMap<String, String>, name: &str) -> Result<()> {
    if annotations.len() > 64 {
        return Err(invalid("artifact_metadata_too_large", name));
    }
    for (key, value) in annotations {
        if key.trim().is_empty()
            || key.len() > 64
            || key.chars().any(char::is_control)
            || value.len() > 1024
            || value.chars().any(char::is_control)
        {
            return Err(invalid("artifact_metadata_invalid", name));
        }
        let lower = key.to_ascii_lowercase();
        if [
            "token",
            "secret",
            "password",
            "credential",
            "private_key",
            "access_key",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        {
            return Err(invalid("artifact_metadata_secret", name));
        }
    }
    Ok(())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn invalid(code: impl Into<String>, detail: impl Into<String>) -> WorkflowError {
    WorkflowError::DvcMigrationInvalid {
        key: code.into(),
        origin: detail.into(),
    }
}

fn invalid_detail(code: &str, detail: impl std::fmt::Display) -> WorkflowError {
    invalid(code, detail.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn refs_encode_names_and_normalize_stages() {
        let version = format!("b3:{}", "ab".repeat(32));
        assert_eq!(
            artifact_version_ref("model/v1", &version).unwrap(),
            format!(
                "{ARTIFACT_REF_PREFIX}/model%2Fv1/versions/{}",
                "ab".repeat(32)
            )
        );
        assert_eq!(
            artifact_stage_ref("model", "Production")
                .unwrap_err()
                .to_string(),
            "DVC migration input invalid in Production: artifact_stage_invalid"
        );
    }

    #[test]
    fn registry_promotion_is_compare_and_swap() {
        let mut registry = ArtifactRegistry::default();
        let mut manifest = ArtifactManifest {
            schema_version: ARTIFACT_SCHEMA_VERSION,
            name: "model".to_owned(),
            version_id: String::new(),
            path: "model.bin".to_owned(),
            kind: "model".to_owned(),
            content_hash: format!("b3:{}", "cd".repeat(32)),
            size: 3,
            source_commit: None,
            created_at_unix_ms: 0,
            annotations: BTreeMap::new(),
        };
        manifest.version_id = manifest_identity(&manifest).unwrap();
        registry.insert_version(manifest.clone()).unwrap();
        let version = manifest.version_id;
        registry
            .promote("model", &version, "production", None)
            .unwrap();
        assert!(
            registry
                .promote("model", &version, "production", Some("b3:00"))
                .is_err()
        );
    }

    #[test]
    fn manifest_snapshot_is_immutable_local_copy() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("snapshot/file");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("file"), b"one").unwrap();
        snapshot_payload(&source, &destination).unwrap();
        fs::write(source.join("file"), b"two").unwrap();
        assert_eq!(fs::read(destination.join("file")).unwrap(), b"one");
    }

    #[test]
    fn payload_verification_rejects_corruption() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source.bin");
        fs::write(&source, b"immutable").unwrap();
        let (expected_hash, expected_size) = hash_file(&source).unwrap();
        verify_payload(&source, &expected_hash, expected_size).unwrap();
        fs::write(&source, b"corrupted").unwrap();
        assert!(verify_payload(&source, &expected_hash, expected_size).is_err());
    }

    #[test]
    fn dvc_artifact_aliases_are_retained_by_the_catalog() {
        let value: Value = serde_yaml::from_str(
            r#"
path: models/resnet.pt
type: model
desc: CV classification
meta:
  framework: pytorch
"#,
        )
        .unwrap();
        let declaration = ArtifactDecl::from_value("cv-classification", &value).unwrap();
        assert_eq!(
            declaration.description.as_deref(),
            Some("CV classification")
        );
        assert_eq!(
            declaration.metadata.get("framework").map(String::as_str),
            Some("pytorch")
        );
    }

    #[test]
    fn artifact_catalog_rejects_unrepresented_fields() {
        let value: Value = serde_yaml::from_str(
            r#"
path: models/resnet.pt
remote: models
"#,
        )
        .unwrap();
        assert!(ArtifactDecl::from_value("model", &value).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_symlinked_parent_directories() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source.bin");
        fs::write(&source, b"payload").unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, temp.path().join("owned")).unwrap();
        let destination = temp.path().join("owned/payload");
        assert!(snapshot_payload(&source, &destination).is_err());
        assert!(!outside.join("payload").exists());
    }
}
