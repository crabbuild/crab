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

use bytes::Bytes;
use futures_util::StreamExt;
use object_store::path::Path as ObjectPath;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::error::{Result, WorkflowError};
use crate::store::WorkflowStore;
use crate::{ArtifactMetadata, hasher};

/// Current artifact contract schema.
pub const ARTIFACT_SCHEMA_VERSION: u16 = 1;
/// Immutable artifact ref prefix.
pub const ARTIFACT_REF_PREFIX: &str = "refs/crab/artifacts";

/// Current remote artifact envelope schema.
pub const ARTIFACT_REMOTE_SCHEMA_VERSION: u16 = 1;

/// Remote payload kind recorded beside an immutable manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteArtifactPayloadKind {
    /// One regular file is stored at the content-addressed payload path.
    File,
    /// A directory is represented by a tree manifest and content-addressed files.
    Directory,
}

/// One remote directory member, including the mode needed for a faithful get.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteArtifactTreeEntry {
    /// NFC-normalized path relative to the artifact root.
    pub path: String,
    /// `file` or `dir`.
    pub kind: String,
    /// Blake3 content hash for files; zero hash for directories.
    pub hash: String,
    /// File size in bytes; zero for directories.
    pub size: u64,
    /// Unix mode bits, or the stable non-Unix placeholder.
    pub mode: u32,
}

/// Immutable remote artifact record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteArtifactEnvelope {
    /// Envelope schema version.
    pub schema_version: u16,
    /// Immutable artifact manifest.
    pub manifest: ArtifactManifest,
    /// Payload representation.
    pub payload_kind: RemoteArtifactPayloadKind,
    /// Directory members, present only for directory payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<Vec<RemoteArtifactTreeEntry>>,
    /// Executable/read-only mode for a file payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_mode: Option<u32>,
}

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

/// Publish an immutable artifact, its content-addressed payload, and the
/// immutable version ref to the workflow remote.
///
/// The manifest is published last. A failed upload therefore leaves only
/// unreachable content-addressed objects; a successful manifest is a durable
/// proof that every payload object was verified locally before publication.
pub async fn publish_remote_artifact(
    store: &WorkflowStore,
    prefix: &str,
    manifest: &ArtifactManifest,
    payload: &Path,
) -> Result<()> {
    let envelope = build_remote_envelope(manifest, payload)?;
    validate_manifest(manifest)?;
    validate_remote_envelope(&envelope)?;
    let payload_root = remote_artifact_payload_root(prefix, &manifest.content_hash)?;
    let cancel = CancellationToken::new();
    match envelope.payload_kind {
        RemoteArtifactPayloadKind::File => {
            let size = fs::metadata(payload).map_err(WorkflowError::Io)?.len();
            upload_remote_file(
                store,
                &remote_artifact_file_path(&payload_root),
                payload,
                size,
                parse_digest(&manifest.content_hash)?,
                &cancel,
            )
            .await?;
        }
        RemoteArtifactPayloadKind::Directory => {
            let tree = envelope
                .tree
                .as_deref()
                .ok_or_else(|| invalid("artifact_remote_tree_missing", manifest.name.clone()))?;
            for entry in tree.iter().filter(|entry| entry.kind == "file") {
                let local_path = payload.join(Path::new(&entry.path));
                upload_remote_file(
                    store,
                    &remote_artifact_tree_file_path(&payload_root, &entry.path)?,
                    &local_path,
                    entry.size,
                    parse_digest(&entry.hash)?,
                    &cancel,
                )
                .await?;
            }
            let tree_path = remote_artifact_tree_path(&payload_root);
            let tree_bytes = serde_json::to_vec(tree)
                .map_err(|error| invalid_detail("artifact_remote_tree_serialize", error))?;
            store.put(&tree_path, Bytes::from(tree_bytes)).await?;
        }
    }

    let manifest_path =
        remote_artifact_manifest_path(prefix, &manifest.name, &manifest.version_id)?;
    let bytes = serde_json::to_vec(&envelope)
        .map_err(|error| invalid_detail("artifact_remote_manifest_serialize", error))?;
    store.put(&manifest_path, Bytes::from(bytes)).await?;

    let version_ref = ObjectPath::from(artifact_version_ref(&manifest.name, &manifest.version_id)?);
    store
        .put(
            &version_ref,
            Bytes::from(manifest_path.as_ref().as_bytes().to_vec()),
        )
        .await
}

/// Read the canonical remote artifact registry (manifests, stage labels, and
/// promotion history) for a repository prefix.
pub async fn read_remote_artifact_registry(
    store: &WorkflowStore,
    prefix: &str,
) -> Result<ArtifactRegistry> {
    let mut registry = ArtifactRegistry::default();
    let manifest_prefix = ObjectPath::from(remote_join(prefix, "workflow/artifacts/manifests"));
    for object in store.list_prefix(&manifest_prefix).await? {
        let key = object.location.as_ref();
        let Some((name, version)) = parse_manifest_key(prefix, key) else {
            continue;
        };
        let (bytes, _) = store.get_with_etag(&object.location).await?;
        let envelope: RemoteArtifactEnvelope = serde_json::from_slice(&bytes)
            .map_err(|error| invalid_detail("artifact_remote_manifest_parse", error))?;
        if envelope.schema_version > ARTIFACT_REMOTE_SCHEMA_VERSION
            || envelope.manifest.name != name
            || envelope.manifest.version_id != version
        {
            return Err(invalid("artifact_remote_manifest_invalid", key));
        }
        validate_remote_envelope(&envelope)?;
        registry.insert_version(envelope.manifest)?;
    }

    let stage_prefix = ObjectPath::from(remote_join(prefix, "refs/crab/artifacts"));
    for object in store.list_prefix(&stage_prefix).await? {
        let key = object.location.as_ref();
        let Some((name, stage)) = parse_stage_key(prefix, key) else {
            continue;
        };
        let (bytes, _) = store.get_with_etag(&object.location).await?;
        let version = String::from_utf8(bytes.to_vec())
            .map_err(|_| invalid("artifact_remote_stage_invalid", key))?;
        if registry
            .versions
            .get(&name)
            .is_none_or(|versions| !versions.contains_key(&version))
        {
            return Err(invalid("artifact_remote_stage_version_missing", version));
        }
        registry
            .stages
            .entry(name)
            .or_default()
            .insert(stage, version);
    }

    let history_prefix = ObjectPath::from(remote_join(prefix, "workflow/artifacts/history"));
    for object in store.list_prefix(&history_prefix).await? {
        let key = object.location.as_ref();
        if !key.ends_with(".json") {
            continue;
        }
        let (bytes, _) = store.get_with_etag(&object.location).await?;
        let event: ArtifactPromotion = serde_json::from_slice(&bytes)
            .map_err(|error| invalid_detail("artifact_remote_history_parse", error))?;
        if registry
            .versions
            .get(&event.name)
            .is_none_or(|versions| !versions.contains_key(&event.version_id))
        {
            return Err(invalid(
                "artifact_remote_history_version_missing",
                event.version_id,
            ));
        }
        registry.history.push(event);
    }
    registry
        .history
        .sort_by_key(|event| event.created_at_unix_ms);
    registry.validate()?;
    Ok(registry)
}

/// Return remote artifact objects that are reachable from immutable version
/// refs, mutable stage refs, and promotion history.
///
/// Artifact publication is intentionally two-phase: a failed upload may leave
/// an orphaned payload or manifest. Repo GC may reclaim those objects after
/// the grace period, but it must retain every object reachable from a ref or
/// history event. A malformed registry fails closed instead of widening the
/// deletion set.
pub async fn reachable_remote_artifact_objects(
    store: &WorkflowStore,
    prefix: &str,
) -> Result<BTreeSet<String>> {
    let registry = read_remote_artifact_registry(store, prefix).await?;
    let refs_prefix = ObjectPath::from(remote_join(prefix, "refs/crab/artifacts"));
    let mut reachable = BTreeSet::new();
    let mut manifest_paths = BTreeSet::new();
    let mut seen_manifests = BTreeSet::new();

    for object in store.list_prefix(&refs_prefix).await? {
        let key = object.location.as_ref().to_owned();
        reachable.insert(key.clone());
        if let Some((name, version)) = parse_version_ref_key(prefix, &key) {
            let (bytes, _) = store.get_with_etag(&object.location).await?;
            let manifest_path = String::from_utf8(bytes.to_vec())
                .map_err(|_| invalid("artifact_remote_version_ref_invalid", key.clone()))?;
            let expected = remote_artifact_manifest_path(prefix, &name, &version)?;
            if manifest_path != expected.to_string() {
                return Err(invalid("artifact_remote_version_ref_invalid", key));
            }
            manifest_paths.insert(manifest_path);
        } else if let Some((name, stage)) = parse_stage_key(prefix, &key) {
            let (bytes, _) = store.get_with_etag(&object.location).await?;
            let version = String::from_utf8(bytes.to_vec())
                .map_err(|_| invalid("artifact_remote_stage_invalid", key.clone()))?;
            let manifest_path = remote_artifact_manifest_path(prefix, &name, &version)?;
            if registry
                .versions
                .get(&name)
                .is_none_or(|versions| !versions.contains_key(&version))
            {
                return Err(invalid(
                    "artifact_remote_stage_version_missing",
                    format!("{stage}:{version}"),
                ));
            }
            manifest_paths.insert(manifest_path.to_string());
        }
    }

    let history_prefix = ObjectPath::from(remote_join(prefix, "workflow/artifacts/history"));
    for object in store.list_prefix(&history_prefix).await? {
        let key = object.location.as_ref().to_owned();
        if !key.ends_with(".json") {
            continue;
        }
        reachable.insert(key.clone());
        let (bytes, _) = store.get_with_etag(&object.location).await?;
        let event: ArtifactPromotion = serde_json::from_slice(&bytes)
            .map_err(|error| invalid_detail("artifact_remote_history_parse", error))?;
        let versions = registry.versions.get(&event.name).ok_or_else(|| {
            invalid(
                "artifact_remote_history_version_missing",
                event.version_id.clone(),
            )
        })?;
        if !versions.contains_key(&event.version_id) {
            return Err(invalid(
                "artifact_remote_history_version_missing",
                event.version_id,
            ));
        }
        manifest_paths.insert(
            remote_artifact_manifest_path(prefix, &event.name, &event.version_id)?.to_string(),
        );
    }

    let manifest_prefix = ObjectPath::from(remote_join(prefix, "workflow/artifacts/manifests"));
    let mut payload_hashes = BTreeSet::new();
    for object in store.list_prefix(&manifest_prefix).await? {
        let key = object.location.as_ref();
        if !manifest_paths.contains(key) {
            continue;
        }
        let (bytes, _) = store.get_with_etag(&object.location).await?;
        let envelope: RemoteArtifactEnvelope = serde_json::from_slice(&bytes)
            .map_err(|error| invalid_detail("artifact_remote_manifest_parse", error))?;
        validate_remote_envelope(&envelope)?;
        reachable.insert(key.to_owned());
        seen_manifests.insert(key.to_owned());
        payload_hashes.insert(envelope.manifest.content_hash);
    }

    if let Some(missing) = manifest_paths.difference(&seen_manifests).next() {
        return Err(invalid(
            "artifact_remote_manifest_missing",
            missing.to_owned(),
        ));
    }

    let payload_prefix = ObjectPath::from(remote_join(prefix, "workflow/artifacts/payloads"));
    for object in store.list_prefix(&payload_prefix).await? {
        let key = object.location.as_ref();
        if payload_hashes.iter().any(|hash| {
            let Ok(root) = remote_artifact_payload_root(prefix, hash) else {
                return false;
            };
            key.starts_with(&format!("{}/", root.as_ref()))
        }) {
            reachable.insert(key.to_owned());
        }
    }

    Ok(reachable)
}

/// Promote a remote immutable version using a compare-and-swap stage label.
pub async fn promote_remote_artifact(
    store: &WorkflowStore,
    prefix: &str,
    name: &str,
    version_id: &str,
    stage: &str,
    expected: Option<&str>,
) -> Result<ArtifactPromotion> {
    validate_artifact_name(name)?;
    validate_artifact_stage(stage)?;
    let envelope = read_remote_artifact(store, prefix, name, Some(version_id), None).await?;
    let stage = stage.to_ascii_lowercase();
    let stage_path = ObjectPath::from(remote_stage_path(prefix, name, &stage)?);
    let previous = match store.get_with_etag(&stage_path).await {
        Ok((bytes, etag)) => {
            let current = String::from_utf8(bytes.to_vec())
                .map_err(|_| invalid("artifact_remote_stage_invalid", stage.clone()))?;
            if expected.is_some_and(|wanted| wanted != current) {
                return Err(WorkflowError::CasConflict {
                    path: stage_path.to_string(),
                    expected_etag: expected.map(ToOwned::to_owned),
                });
            }
            if current != version_id {
                store
                    .as_storage()
                    .update(
                        &stage_path,
                        Bytes::from(version_id.as_bytes().to_vec()),
                        etag,
                    )
                    .await
                    .map_err(WorkflowError::StorageDomain)?;
            }
            Some(current)
        }
        Err(WorkflowError::NotFound { .. })
        | Err(WorkflowError::StorageDomain(crab_storage::StorageError::NotFound { .. })) => {
            if expected.is_some() {
                return Err(WorkflowError::CasConflict {
                    path: stage_path.to_string(),
                    expected_etag: expected.map(ToOwned::to_owned),
                });
            }
            store
                .put(&stage_path, Bytes::from(version_id.as_bytes().to_vec()))
                .await?;
            None
        }
        Err(error) => return Err(error),
    };

    let event = ArtifactPromotion {
        name: name.to_owned(),
        stage,
        version_id: envelope.manifest.version_id,
        previous_version_id: previous,
        created_at_unix_ms: now_unix_ms(),
    };
    let history_path = ObjectPath::from(remote_history_path(prefix, &event)?);
    let bytes = serde_json::to_vec(&event)
        .map_err(|error| invalid_detail("artifact_remote_history_serialize", error))?;
    store.put(&history_path, Bytes::from(bytes)).await?;
    Ok(event)
}

/// Resolve an immutable version or stage label from the remote registry.
pub async fn read_remote_artifact(
    store: &WorkflowStore,
    prefix: &str,
    name: &str,
    version: Option<&str>,
    stage: Option<&str>,
) -> Result<RemoteArtifactEnvelope> {
    validate_artifact_name(name)?;
    if version.is_some() == stage.is_some() {
        return Err(invalid("artifact_selector_requires_exactly_one", name));
    }
    let version_id = if let Some(version) = version {
        version.to_owned()
    } else {
        let stage = stage.unwrap_or_default();
        validate_artifact_stage(stage)?;
        let stage_path = ObjectPath::from(remote_stage_path(prefix, name, stage)?);
        let (bytes, _) = store.get_with_etag(&stage_path).await?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| invalid("artifact_remote_stage_invalid", stage.to_owned()))?
    };
    let manifest_path = remote_artifact_manifest_path(prefix, name, &version_id)?;
    let (bytes, _) = store.get_with_etag(&manifest_path).await?;
    let envelope: RemoteArtifactEnvelope = serde_json::from_slice(&bytes)
        .map_err(|error| invalid_detail("artifact_remote_manifest_parse", error))?;
    if envelope.schema_version > ARTIFACT_REMOTE_SCHEMA_VERSION
        || envelope.manifest.name != name
        || envelope.manifest.version_id != version_id
    {
        return Err(invalid(
            "artifact_remote_manifest_invalid",
            manifest_path.to_string(),
        ));
    }
    ArtifactRegistry::default().insert_version(envelope.manifest.clone())?;
    validate_remote_envelope(&envelope)?;
    Ok(envelope)
}

/// Download and verify a remote artifact payload into a new local path.
pub async fn download_remote_artifact(
    store: &WorkflowStore,
    prefix: &str,
    envelope: &RemoteArtifactEnvelope,
    destination: &Path,
) -> Result<()> {
    validate_remote_envelope(envelope)?;
    if fs::symlink_metadata(destination).is_ok() {
        return Err(invalid(
            "artifact_destination_exists",
            destination.display().to_string(),
        ));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(WorkflowError::Io)?;
    }
    let payload_root = remote_artifact_payload_root(prefix, &envelope.manifest.content_hash)?;
    let result = match envelope.payload_kind {
        RemoteArtifactPayloadKind::File => {
            let temporary = temporary_snapshot_path(destination);
            let outcome =
                download_remote_file(store, &remote_artifact_file_path(&payload_root), &temporary)
                    .await
                    .and_then(|()| {
                        verify_payload(
                            &temporary,
                            &envelope.manifest.content_hash,
                            envelope.manifest.size,
                        )
                    })
                    .and_then(|()| {
                        set_file_mode(&temporary, envelope.file_mode.unwrap_or(0o644))?;
                        fs::rename(&temporary, destination).map_err(WorkflowError::Io)
                    });
            if outcome.is_err() {
                let _ = fs::remove_file(&temporary);
            }
            outcome
        }
        RemoteArtifactPayloadKind::Directory => {
            fs::create_dir(destination).map_err(WorkflowError::Io)?;
            let tree = envelope.tree.as_deref().ok_or_else(|| {
                invalid(
                    "artifact_remote_tree_missing",
                    envelope.manifest.name.clone(),
                )
            })?;
            let outcome = download_remote_tree(store, &payload_root, destination, tree).await;
            if outcome.is_ok() {
                verify_payload(
                    destination,
                    &envelope.manifest.content_hash,
                    envelope.manifest.size,
                )
            } else {
                outcome
            }
        }
    };
    if result.is_err() {
        let _ = remove_download_destination(destination);
    }
    result
}

/// Return the remote object path for an immutable artifact manifest.
pub fn remote_artifact_manifest_path(
    prefix: &str,
    name: &str,
    version_id: &str,
) -> Result<ObjectPath> {
    validate_artifact_name(name)?;
    let digest = version_id.strip_prefix("b3:").unwrap_or_default();
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("artifact_version_id_invalid", version_id));
    }
    Ok(ObjectPath::from(remote_join(
        prefix,
        &format!(
            "workflow/artifacts/manifests/{}/{}.json",
            percent_encode_name(name),
            digest
        ),
    )))
}

fn build_remote_envelope(
    manifest: &ArtifactManifest,
    payload: &Path,
) -> Result<RemoteArtifactEnvelope> {
    let metadata = fs::symlink_metadata(payload).map_err(WorkflowError::Io)?;
    let (payload_kind, tree, file_mode) = if metadata.is_file() {
        (
            RemoteArtifactPayloadKind::File,
            None,
            Some(file_mode(payload)?),
        )
    } else if metadata.is_dir() {
        let directory = hasher::hash_directory(payload, false)?;
        let actual_hash = format!("b3:{}", hex_digest(&directory.hash));
        if actual_hash != manifest.content_hash {
            return Err(invalid(
                "artifact_remote_payload_integrity",
                payload.display().to_string(),
            ));
        }
        let entries = directory
            .manifest
            .into_iter()
            .map(|entry| RemoteArtifactTreeEntry {
                path: entry.path.to_string_lossy().replace('\\', "/"),
                kind: match entry.kind {
                    hasher::TreeEntryKind::File => "file".to_owned(),
                    hasher::TreeEntryKind::Directory => "dir".to_owned(),
                },
                hash: format!("b3:{}", hex_digest(&entry.file_hash)),
                size: entry.size,
                mode: entry.mode,
            })
            .collect();
        (RemoteArtifactPayloadKind::Directory, Some(entries), None)
    } else {
        return Err(invalid(
            "artifact_remote_payload_type_unsupported",
            payload.display().to_string(),
        ));
    };
    Ok(RemoteArtifactEnvelope {
        schema_version: ARTIFACT_REMOTE_SCHEMA_VERSION,
        manifest: manifest.clone(),
        payload_kind,
        tree,
        file_mode,
    })
}

async fn upload_remote_file(
    store: &WorkflowStore,
    remote_path: &ObjectPath,
    local_path: &Path,
    size: u64,
    expected_hash: [u8; 32],
    cancel: &CancellationToken,
) -> Result<()> {
    match store.head(remote_path).await {
        Ok(_) => return Ok(()),
        Err(WorkflowError::NotFound { .. })
        | Err(WorkflowError::StorageDomain(crab_storage::StorageError::NotFound { .. })) => {}
        Err(error) => return Err(error),
    }
    if size <= 8 * 1024 * 1024 {
        let bytes = fs::read(local_path).map_err(WorkflowError::Io)?;
        if blake3::hash(&bytes).as_bytes() != &expected_hash {
            return Err(invalid(
                "artifact_remote_payload_integrity",
                local_path.display().to_string(),
            ));
        }
        return store.put(remote_path, Bytes::from(bytes)).await;
    }
    store
        .as_storage()
        .put_multipart_file_retry(
            remote_path,
            local_path,
            size,
            expected_hash,
            8 * 1024 * 1024,
            cancel,
            None,
        )
        .await
        .map_err(WorkflowError::StorageDomain)
}

async fn download_remote_file(
    store: &WorkflowStore,
    remote_path: &ObjectPath,
    destination: &Path,
) -> Result<()> {
    let (_, _, mut stream) = store
        .as_storage()
        .get_stream(remote_path, None)
        .await
        .map_err(WorkflowError::StorageDomain)?;
    let mut file = tokio::fs::File::create(destination)
        .await
        .map_err(WorkflowError::Io)?;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(WorkflowError::StorageDomain)?;
        file.write_all(&chunk).await.map_err(WorkflowError::Io)?;
    }
    file.sync_all().await.map_err(WorkflowError::Io)
}

async fn download_remote_tree(
    store: &WorkflowStore,
    payload_root: &ObjectPath,
    destination: &Path,
    tree: &[RemoteArtifactTreeEntry],
) -> Result<()> {
    for entry in tree {
        let relative = validate_remote_tree_path(&entry.path)?;
        let target = destination.join(&relative);
        match entry.kind.as_str() {
            "dir" => fs::create_dir_all(&target).map_err(WorkflowError::Io)?,
            "file" => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(WorkflowError::Io)?;
                }
                download_remote_file(
                    store,
                    &remote_artifact_tree_file_path(payload_root, &entry.path)?,
                    &target,
                )
                .await?;
                verify_payload(&target, &entry.hash, entry.size)?;
                set_file_mode(&target, entry.mode)?;
            }
            _ => return Err(invalid("artifact_remote_tree_kind_invalid", &entry.kind)),
        }
    }
    Ok(())
}

fn validate_remote_envelope(envelope: &RemoteArtifactEnvelope) -> Result<()> {
    if envelope.schema_version > ARTIFACT_REMOTE_SCHEMA_VERSION {
        return Err(invalid(
            "artifact_remote_manifest_schema_newer",
            envelope.manifest.name.clone(),
        ));
    }
    validate_manifest(&envelope.manifest)?;
    match envelope.payload_kind {
        RemoteArtifactPayloadKind::File => {
            if envelope.tree.is_some() {
                return Err(invalid(
                    "artifact_remote_tree_unexpected",
                    &envelope.manifest.name,
                ));
            }
            let Some(mode) = envelope.file_mode else {
                return Err(invalid(
                    "artifact_remote_file_mode_missing",
                    &envelope.manifest.name,
                ));
            };
            if mode > 0o7777 {
                return Err(invalid(
                    "artifact_remote_file_mode_invalid",
                    envelope.manifest.name.clone(),
                ));
            }
            Ok(())
        }
        RemoteArtifactPayloadKind::Directory => {
            if envelope.file_mode.is_some() {
                return Err(invalid(
                    "artifact_remote_file_mode_unexpected",
                    &envelope.manifest.name,
                ));
            }
            let tree = envelope
                .tree
                .as_deref()
                .ok_or_else(|| invalid("artifact_remote_tree_missing", &envelope.manifest.name))?;
            let mut paths = BTreeSet::new();
            let mut entries = Vec::with_capacity(tree.len());
            for entry in tree {
                let path = validate_remote_tree_path(&entry.path)?;
                if !paths.insert(path.to_string_lossy().into_owned()) {
                    return Err(invalid("artifact_remote_tree_duplicate", &entry.path));
                }
                if entry.mode > 0o7777 {
                    return Err(invalid("artifact_remote_tree_mode_invalid", &entry.path));
                }
                let (kind, file_hash) = match entry.kind.as_str() {
                    "file" => (
                        hasher::TreeEntryKind::File,
                        parse_digest(&entry.hash).map_err(|_| {
                            invalid("artifact_remote_tree_hash_invalid", &entry.hash)
                        })?,
                    ),
                    "dir" => {
                        let hash = parse_digest(&entry.hash).map_err(|_| {
                            invalid("artifact_remote_tree_hash_invalid", &entry.hash)
                        })?;
                        if entry.size != 0 || hash != [0; 32] {
                            return Err(invalid(
                                "artifact_remote_tree_directory_metadata_invalid",
                                &entry.path,
                            ));
                        }
                        (hasher::TreeEntryKind::Directory, hash)
                    }
                    _ => {
                        return Err(invalid("artifact_remote_tree_kind_invalid", &entry.kind));
                    }
                };
                entries.push(hasher::TreeEntry {
                    path,
                    kind,
                    file_hash,
                    size: entry.size,
                    mode: entry.mode,
                });
            }
            let size = entries.iter().try_fold(0_u64, |total, entry| {
                total.checked_add(entry.size).ok_or_else(|| {
                    invalid(
                        "artifact_remote_tree_size_overflow",
                        &envelope.manifest.name,
                    )
                })
            })?;
            if size != envelope.manifest.size
                || format!("b3:{}", hex_digest(&hasher::hash_tree_entries(&entries)))
                    != envelope.manifest.content_hash
            {
                return Err(invalid(
                    "artifact_remote_tree_integrity",
                    &envelope.manifest.name,
                ));
            }
            Ok(())
        }
    }
}

fn remote_artifact_payload_root(prefix: &str, hash: &str) -> Result<ObjectPath> {
    validate_digest("artifact_content_hash_invalid", hash)?;
    Ok(ObjectPath::from(remote_join(
        prefix,
        &format!(
            "workflow/artifacts/payloads/{}",
            hash.trim_start_matches("b3:")
        ),
    )))
}

fn remote_artifact_file_path(root: &ObjectPath) -> ObjectPath {
    ObjectPath::from(format!("{}/file", root.as_ref()))
}

fn remote_artifact_tree_path(root: &ObjectPath) -> ObjectPath {
    ObjectPath::from(format!("{}/tree.json", root.as_ref()))
}

fn remote_artifact_tree_file_path(root: &ObjectPath, path: &str) -> Result<ObjectPath> {
    let path = validate_remote_tree_path(path)?;
    let encoded = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(percent_encode_name)
        .collect::<Vec<_>>()
        .join("/");
    Ok(ObjectPath::from(format!(
        "{}/files/{}",
        root.as_ref(),
        encoded
    )))
}

fn remote_stage_path(prefix: &str, name: &str, stage: &str) -> Result<String> {
    validate_artifact_name(name)?;
    validate_artifact_stage(stage)?;
    Ok(remote_join(
        prefix,
        &format!(
            "refs/crab/artifacts/{}/stages/{}",
            percent_encode_name(name),
            stage.to_ascii_lowercase()
        ),
    ))
}

fn remote_history_path(prefix: &str, event: &ArtifactPromotion) -> Result<String> {
    validate_artifact_name(&event.name)?;
    validate_artifact_stage(&event.stage)?;
    Ok(remote_join(
        prefix,
        &format!(
            "workflow/artifacts/history/{}/{}-{}.json",
            percent_encode_name(&event.name),
            event.created_at_unix_ms,
            Uuid::now_v7()
        ),
    ))
}

fn parse_manifest_key(prefix: &str, key: &str) -> Option<(String, String)> {
    let root = remote_join(prefix, "workflow/artifacts/manifests");
    let relative = key.strip_prefix(&format!("{root}/"))?;
    let (encoded_name, version) = relative.split_once('/')?;
    let version = version.strip_suffix(".json")?;
    Some((percent_decode_name(encoded_name)?, format!("b3:{version}")))
}

fn parse_stage_key(prefix: &str, key: &str) -> Option<(String, String)> {
    let root = remote_join(prefix, "refs/crab/artifacts");
    let relative = key.strip_prefix(&format!("{root}/"))?;
    let (encoded_name, rest) = relative.split_once('/')?;
    let stage = rest.strip_prefix("stages/")?;
    Some((percent_decode_name(encoded_name)?, stage.to_owned()))
}

fn parse_version_ref_key(prefix: &str, key: &str) -> Option<(String, String)> {
    let root = remote_join(prefix, "refs/crab/artifacts");
    let relative = key.strip_prefix(&format!("{root}/"))?;
    let (encoded_name, version) = relative.split_once('/')?;
    let version = version.strip_prefix("versions/")?;
    let version = version.strip_prefix("b3:").unwrap_or(version);
    if version.len() != 64 || !version.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some((percent_decode_name(encoded_name)?, format!("b3:{version}")))
}

fn remote_join(prefix: &str, suffix: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        suffix.to_owned()
    } else {
        format!("{prefix}/{suffix}")
    }
}

fn validate_remote_tree_path(path: &str) -> Result<PathBuf> {
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
        return Err(invalid(
            "artifact_remote_tree_path_invalid",
            path.display().to_string(),
        ));
    }
    Ok(path.to_path_buf())
}

fn percent_decode_name(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut chars = value.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let high = hex_nibble(chars.next()?)?;
        let low = hex_nibble(chars.next()?)?;
        bytes.push((high * 16 + low) as u8);
    }
    String::from_utf8(bytes).ok()
}

fn parse_digest(value: &str) -> Result<[u8; 32]> {
    validate_digest("artifact_content_hash_invalid", value)?;
    let raw = value.strip_prefix("b3:").unwrap_or_default();
    let mut digest = [0_u8; 32];
    for (index, pair) in raw.as_bytes().chunks_exact(2).enumerate() {
        let high =
            hex_nibble(pair[0]).ok_or_else(|| invalid("artifact_content_hash_invalid", value))?;
        let low =
            hex_nibble(pair[1]).ok_or_else(|| invalid("artifact_content_hash_invalid", value))?;
        digest[index] = ((high << 4) | low) as u8;
    }
    Ok(digest)
}

fn hex_digest(value: &[u8; 32]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(unix)]
fn file_mode(path: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(path)
        .map_err(WorkflowError::Io)?
        .permissions()
        .mode()
        & 0o7777)
}

#[cfg(not(unix))]
fn file_mode(path: &Path) -> Result<u32> {
    let readonly = fs::metadata(path)
        .map_err(WorkflowError::Io)?
        .permissions()
        .readonly();
    Ok(u32::from(!readonly))
}

fn hex_nibble(byte: u8) -> Option<u32> {
    match byte {
        b'0'..=b'9' => Some(u32::from(byte - b'0')),
        b'a'..=b'f' => Some(u32::from(byte - b'a' + 10)),
        b'A'..=b'F' => Some(u32::from(byte - b'A' + 10)),
        _ => None,
    }
}

fn remove_download_destination(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(WorkflowError::Io(error)),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path).map_err(WorkflowError::Io)
    } else {
        fs::remove_file(path).map_err(WorkflowError::Io)
    }
}

#[cfg(unix)]
fn set_file_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777)).map_err(WorkflowError::Io)
}

#[cfg(not(unix))]
fn set_file_mode(path: &Path, mode: u32) -> Result<()> {
    let mut permissions = fs::metadata(path).map_err(WorkflowError::Io)?.permissions();
    permissions.set_readonly(mode & 0o200 == 0);
    fs::set_permissions(path, permissions).map_err(WorkflowError::Io)
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

    #[tokio::test]
    async fn remote_artifact_round_trip_publishes_stage_and_history() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("model.bin");
        fs::write(&source, b"remote-model").unwrap();
        let declaration = ArtifactDecl {
            name: "model".to_owned(),
            path: "model.bin".to_owned(),
            kind: "model".to_owned(),
            description: None,
            labels: Vec::new(),
            metadata: BTreeMap::new(),
        };
        let (manifest, source_path) = manifest_from_path(temp.path(), &declaration, None).unwrap();
        let store = WorkflowStore::new(std::sync::Arc::new(object_store::memory::InMemory::new()));
        publish_remote_artifact(&store, "repo", &manifest, &source_path)
            .await
            .unwrap();
        let event = promote_remote_artifact(
            &store,
            "repo",
            "model",
            &manifest.version_id,
            "production",
            None,
        )
        .await
        .unwrap();
        assert_eq!(event.version_id, manifest.version_id);

        let registry = read_remote_artifact_registry(&store, "repo").await.unwrap();
        assert_eq!(
            registry
                .stages
                .get("model")
                .and_then(|stages| stages.get("production")),
            Some(&manifest.version_id)
        );
        assert_eq!(registry.history.len(), 1);

        let envelope = read_remote_artifact(&store, "repo", "model", None, Some("production"))
            .await
            .unwrap();
        let destination = temp.path().join("downloaded.bin");
        download_remote_artifact(&store, "repo", &envelope, &destination)
            .await
            .unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"remote-model");

        fs::write(&source, b"remote-model-v2").unwrap();
        let (next_manifest, next_source_path) =
            manifest_from_path(temp.path(), &declaration, None).unwrap();
        publish_remote_artifact(&store, "repo", &next_manifest, &next_source_path)
            .await
            .unwrap();
        let next_event = promote_remote_artifact(
            &store,
            "repo",
            "model",
            &next_manifest.version_id,
            "production",
            Some(&manifest.version_id),
        )
        .await
        .unwrap();
        assert_eq!(next_event.previous_version_id, Some(manifest.version_id));
        let registry = read_remote_artifact_registry(&store, "repo").await.unwrap();
        assert_eq!(registry.history.len(), 2);
    }

    #[tokio::test]
    async fn remote_directory_artifact_round_trip_preserves_tree_and_modes() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("model");
        fs::create_dir_all(source.join("nested/empty")).unwrap();
        fs::write(source.join("nested/weights.bin"), b"weights").unwrap();
        let declaration = ArtifactDecl {
            name: "model".to_owned(),
            path: "model".to_owned(),
            kind: "model".to_owned(),
            description: None,
            labels: Vec::new(),
            metadata: BTreeMap::new(),
        };
        let (manifest, source_path) = manifest_from_path(temp.path(), &declaration, None).unwrap();
        let store = WorkflowStore::new(std::sync::Arc::new(object_store::memory::InMemory::new()));
        publish_remote_artifact(&store, "repo", &manifest, &source_path)
            .await
            .unwrap();
        promote_remote_artifact(
            &store,
            "repo",
            "model",
            &manifest.version_id,
            "production",
            None,
        )
        .await
        .unwrap();

        let envelope = read_remote_artifact(&store, "repo", "model", None, Some("production"))
            .await
            .unwrap();
        assert_eq!(envelope.payload_kind, RemoteArtifactPayloadKind::Directory);
        let destination = temp.path().join("downloaded-model");
        download_remote_artifact(&store, "repo", &envelope, &destination)
            .await
            .unwrap();
        verify_payload(&destination, &manifest.content_hash, manifest.size).unwrap();
        assert_eq!(
            fs::read(destination.join("nested/weights.bin")).unwrap(),
            b"weights"
        );
        assert!(destination.join("nested/empty").is_dir());
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
