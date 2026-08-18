//! Read-only DVC inventory and resumable migration journal contracts.
//!
//! Inventory is separate from YAML conversion. It records source state that
//! must be accounted for before a migration can claim a safe cutover. DVC
//! hashes remain source identities and are never used as Crab content hashes.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use url::Url;

use crate::error::{Result, WorkflowError};
use crate::{ArtifactCatalog, ArtifactMetadata};

/// Current inventory schema.
pub const DVC_INVENTORY_SCHEMA_VERSION: u16 = 1;
/// Current migration journal schema.
pub const DVC_MIGRATION_JOURNAL_SCHEMA_VERSION: u16 = 1;

/// A DVC remote with credentials removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DvcRemoteDescriptor {
    /// Remote name.
    pub name: String,
    /// Credential-free URL.
    pub locator: String,
    /// URL scheme.
    pub scheme: String,
    /// Whether this is the configured default.
    pub default: bool,
    /// Config file that won after DVC precedence was applied.
    #[serde(default)]
    pub source_config: String,
    /// Credential source category, never the credential itself.
    #[serde(default)]
    pub credential_source: String,
    /// Read/write capability discovered during preflight.
    #[serde(default)]
    pub capability: String,
    /// Explicit Crab destination mapping, when supplied by the caller.
    #[serde(default)]
    pub destination: Option<String>,
}

/// Verification state for one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerificationState {
    /// Source is absent.
    Missing,
    /// Source bytes or manifest were verified.
    Verified,
    /// A non-payload inventory record was durably accounted for. This is
    /// intentionally distinct from `Verified`: remote declarations and
    /// metadata are recorded without pretending that a payload was read.
    Accounted,
    /// Source exists but needs a later protocol phase to verify.
    PresentUnverified,
    /// Source identity did not match.
    Mismatch {
        /// Expected DVC MD5.
        expected: Option<String>,
        /// Observed MD5.
        actual: Option<String>,
    },
    /// Source shape cannot be safely handled.
    Unsupported(String),
}

/// One output discovered in a DVC pointer or pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DvcOutputRecord {
    /// Pointer or pipeline declaration.
    pub declaration: String,
    /// Repository-relative output path.
    pub path: String,
    /// DVC MD5, if declared.
    pub dvc_md5: Option<String>,
    /// Declared size, if present.
    pub size: Option<u64>,
    /// Whether the output is represented by a directory manifest.
    pub directory: bool,
    /// Whether DVC marks the output executable.
    #[serde(default)]
    pub isexec: bool,
    /// Materialized working-tree verification.
    pub materialized: VerificationState,
    /// Local DVC cache verification.
    pub cache: VerificationState,
    /// Credential-free import provenance.
    pub provenance: Option<DvcImportProvenance>,
    /// Credential-free cache locator. The inventory stores the selected cache
    /// object's project-relative path when possible, or its absolute path for
    /// an external cache root, so resume cannot accidentally resolve it under
    /// the project root.
    #[serde(default)]
    pub cache_locator: Option<String>,
    /// Materialized byte count observed during verification.
    #[serde(default)]
    pub materialized_bytes: Option<u64>,
}

/// Credential-free provenance for an import-like pointer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DvcImportProvenance {
    /// Source kind, such as repo or url.
    pub kind: String,
    /// Canonical source locator.
    pub locator: String,
    /// Locked revision or query identity.
    pub revision: Option<String>,
    /// Source-side path when the DVC record preserves one.
    #[serde(default)]
    pub source_path: Option<String>,
}

/// Machine-readable inventory finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DvcFinding {
    /// Stable finding code.
    pub code: String,
    /// Repository-relative source, when known.
    pub source: Option<String>,
    /// Redacted diagnostic context.
    pub detail: String,
    /// Whether this finding blocks cutover.
    pub blocking: bool,
}

/// Read-only DVC state inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DvcInventory {
    /// Inventory schema.
    pub schema_version: u16,
    /// DVC metadata files relative to the project root.
    pub metadata_files: Vec<String>,
    /// Discovered outputs.
    pub outputs: Vec<DvcOutputRecord>,
    /// Configured remotes, with secrets removed.
    pub remotes: Vec<DvcRemoteDescriptor>,
    /// Stable lock records observed.
    pub lock_records: Vec<String>,
    /// Number of files beneath configured cache roots.
    pub cache_object_count: u64,
    /// Every regular cache object discovered beneath the configured roots.
    /// Paths are project-relative for in-project caches and absolute for
    /// external cache roots so a resume cannot silently change the source.
    #[serde(default)]
    pub cache_objects: Vec<String>,
    /// Credential-free cache roots selected by config precedence.
    #[serde(default)]
    pub cache_roots: Vec<String>,
    /// DVC run-cache files discovered under .dvc.
    #[serde(default)]
    pub run_cache_files: Vec<String>,
    /// Ignore files that affect the tracked working tree.
    #[serde(default)]
    pub ignore_files: Vec<String>,
    /// Fingerprint used to bind a resume journal to source state.
    pub fingerprint: String,
    /// Blocking and informational findings.
    pub findings: Vec<DvcFinding>,
    /// False until transfer and clean-clone verification complete.
    pub safe_to_remove_dvc: bool,
}

/// One durable, credential-free migration entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DvcJournalEntry {
    /// Stable inventory key.
    pub key: String,
    /// Selected source locator.
    pub source: String,
    /// Source DVC identity.
    pub dvc_md5: Option<String>,
    /// Canonical Crab identity after ingestion.
    pub crab_hash: Option<String>,
    /// Transfer/verification state.
    pub state: VerificationState,
    /// Stable error code, if blocked.
    pub error_code: Option<String>,
    /// Source class selected by the migration protocol.
    #[serde(default)]
    pub source_kind: String,
    /// Verified byte count.
    #[serde(default)]
    pub bytes: Option<u64>,
    /// Versioned import provenance, when the DVC output came from another
    /// repository, URL, or database source.
    #[serde(default)]
    pub provenance: Option<DvcImportProvenance>,
}

/// Resumable migration journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DvcMigrationJournal {
    /// Journal schema.
    pub schema_version: u16,
    /// Inventory fingerprint bound to this journal.
    pub inventory_fingerprint: String,
    /// Entries sorted by key.
    pub entries: Vec<DvcJournalEntry>,
    /// Blocking reason codes.
    pub blocking_reasons: Vec<String>,
    /// True only after all protocol verification gates pass.
    pub safe_to_remove_dvc: bool,
    /// Digest of the clean-clone verification evidence. A true cutover flag
    /// without this value is invalid and cannot be loaded.
    #[serde(default)]
    pub cutover_verification: Option<String>,
    /// Whether the canonical Crab pointer blobs were published to the
    /// repository's Git index. A sidecar pointer snapshot is not a cutover.
    #[serde(default)]
    pub git_index_published: bool,
    /// Whether migrated payloads were durably flushed to Crab's native
    /// staging area before the Git index was updated.
    #[serde(default)]
    pub staging_flushed: bool,
    /// Clean-clone evidence digests keyed by the explicitly mapped DVC
    /// remote name. A mapping is not considered verified merely because its
    /// destination parsed; the migration must push a temporary ref, clone it
    /// through Crab, and compare every migrated output before removing the
    /// destination blocking finding.
    #[serde(default)]
    pub remote_verifications: BTreeMap<String, String>,
}

impl DvcMigrationJournal {
    /// Create pending entries from an inventory.
    #[must_use]
    pub fn from_inventory(inventory: &DvcInventory) -> Self {
        let mut entries = inventory
            .outputs
            .iter()
            .map(|output| {
                let (source, source_kind, state) =
                    if output.materialized == VerificationState::Verified {
                        (
                            output.path.clone(),
                            "working-tree",
                            VerificationState::PresentUnverified,
                        )
                    } else if output.cache == VerificationState::Verified {
                        (
                            output
                                .cache_locator
                                .clone()
                                .unwrap_or_else(|| output.path.clone()),
                            "local-cache",
                            VerificationState::PresentUnverified,
                        )
                    } else {
                        (output.path.clone(), "remote", VerificationState::Missing)
                    };
                DvcJournalEntry {
                    key: output_key(output),
                    source,
                    dvc_md5: output.dvc_md5.clone(),
                    crab_hash: None,
                    state,
                    error_code: None,
                    source_kind: source_kind.to_owned(),
                    bytes: output.materialized_bytes.or(output.size),
                    provenance: output.provenance.clone(),
                }
            })
            .collect::<Vec<_>>();
        for relative in &inventory.metadata_files {
            entries.push(DvcJournalEntry {
                key: format!("metadata:{relative}"),
                source: relative.clone(),
                dvc_md5: None,
                crab_hash: None,
                state: VerificationState::PresentUnverified,
                error_code: None,
                source_kind: "metadata".to_owned(),
                bytes: None,
                provenance: None,
            });
        }
        for relative in &inventory.cache_objects {
            entries.push(DvcJournalEntry {
                key: format!("cache:{relative}"),
                source: relative.clone(),
                dvc_md5: None,
                crab_hash: None,
                state: VerificationState::PresentUnverified,
                error_code: None,
                source_kind: "cache-object".to_owned(),
                bytes: None,
                provenance: None,
            });
        }
        for relative in &inventory.run_cache_files {
            entries.push(DvcJournalEntry {
                key: format!("run-cache:{relative}"),
                source: relative.clone(),
                dvc_md5: None,
                crab_hash: None,
                state: VerificationState::PresentUnverified,
                error_code: None,
                source_kind: "run-cache".to_owned(),
                bytes: None,
                provenance: None,
            });
        }
        for remote in &inventory.remotes {
            entries.push(DvcJournalEntry {
                key: format!("remote:{}", remote.name),
                source: remote.locator.clone(),
                dvc_md5: None,
                crab_hash: None,
                state: VerificationState::PresentUnverified,
                error_code: None,
                source_kind: "remote-descriptor".to_owned(),
                bytes: None,
                provenance: None,
            });
        }
        entries.sort_by(|left, right| left.key.cmp(&right.key));
        let mut blocking_reasons = inventory
            .findings
            .iter()
            .filter(|finding| finding.blocking)
            .map(|finding| finding.code.clone())
            .collect::<Vec<_>>();
        blocking_reasons.push("transfer_pending".to_owned());
        blocking_reasons.sort();
        blocking_reasons.dedup();
        Self {
            schema_version: DVC_MIGRATION_JOURNAL_SCHEMA_VERSION,
            inventory_fingerprint: inventory.fingerprint.clone(),
            entries,
            blocking_reasons,
            safe_to_remove_dvc: false,
            cutover_verification: None,
            git_index_published: false,
            staging_flushed: false,
            remote_verifications: BTreeMap::new(),
        }
    }

    /// Mark this journal cutover-safe after clean-clone verification.
    pub fn mark_cutover_verified(&mut self, evidence_digest: String) -> Result<()> {
        if !is_evidence_digest(&evidence_digest) {
            return Err(invalid_detail(
                "dvc_migration_cutover_evidence_missing",
                "clean-clone evidence digest must be 64 hexadecimal characters",
            ));
        }
        if !self.entries.iter().all(|entry| {
            matches!(
                entry.state,
                VerificationState::Verified | VerificationState::Accounted
            )
        }) {
            return Err(invalid_detail(
                "dvc_migration_cutover_entries_unverified",
                "every migration entry must be verified or accounted",
            ));
        }
        if !self.blocking_reasons.is_empty() {
            return Err(invalid_detail(
                "dvc_migration_cutover_blocked",
                self.blocking_reasons.join(","),
            ));
        }
        if !self.git_index_published {
            return Err(invalid_detail(
                "dvc_migration_git_index_unpublished",
                "canonical Crab pointers were not published to the Git index",
            ));
        }
        if !self.staging_flushed {
            return Err(invalid_detail(
                "dvc_migration_staging_unflushed",
                "migrated payloads were not durably flushed to Crab staging",
            ));
        }
        self.cutover_verification = Some(evidence_digest);
        self.safe_to_remove_dvc = true;
        Ok(())
    }

    /// Persist using a synced temporary file and atomic rename.
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        if self.schema_version > DVC_MIGRATION_JOURNAL_SCHEMA_VERSION {
            return Err(invalid("dvc_migration_journal_schema_newer", path));
        }
        self.validate(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| invalid("dvc_migration_journal_path_no_parent", path))?;
        crate::atomic::ensure_parent_not_symlink(path)?;
        fs::create_dir_all(parent).map_err(WorkflowError::Io)?;
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| invalid_detail("dvc_migration_journal_serialize", error))?;
        let temporary = parent.join(format!(
            ".{}.tmp-{}-{}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("dvc"),
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

    /// Load and reject a journal bound to changed source state.
    pub fn load(path: &Path, expected_fingerprint: &str) -> Result<Self> {
        let metadata = fs::symlink_metadata(path).map_err(WorkflowError::Io)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(invalid("dvc_migration_journal_file_invalid", path));
        }
        let bytes = fs::read(path).map_err(WorkflowError::Io)?;
        let journal: Self = serde_json::from_slice(&bytes)
            .map_err(|error| invalid_detail("dvc_migration_journal_parse", error))?;
        if journal.schema_version > DVC_MIGRATION_JOURNAL_SCHEMA_VERSION {
            return Err(invalid("dvc_migration_journal_schema_newer", path));
        }
        if journal.inventory_fingerprint != expected_fingerprint {
            return Err(invalid("dvc_migration_source_changed", path));
        }
        journal.validate(path)?;
        Ok(journal)
    }

    fn validate(&self, path: &Path) -> Result<()> {
        if self.schema_version > DVC_MIGRATION_JOURNAL_SCHEMA_VERSION {
            return Err(invalid("dvc_migration_journal_schema_newer", path));
        }
        let mut keys = BTreeSet::new();
        for entry in &self.entries {
            if entry.key.trim().is_empty()
                || entry.source.trim().is_empty()
                || !keys.insert(entry.key.as_str())
            {
                return Err(invalid_detail(
                    "dvc_migration_journal_entry_invalid",
                    path.display(),
                ));
            }
            if let Some(hash) = entry.crab_hash.as_deref()
                && !is_crab_digest(hash)
            {
                return Err(invalid_detail(
                    "dvc_migration_journal_hash_invalid",
                    path.display(),
                ));
            }
            if matches!(
                entry.state,
                VerificationState::Verified | VerificationState::Accounted
            ) && entry.crab_hash.is_none()
            {
                return Err(invalid_detail(
                    "dvc_migration_journal_verified_hash_missing",
                    path.display(),
                ));
            }
        }
        if self
            .entries
            .windows(2)
            .any(|pair| pair[0].key >= pair[1].key)
        {
            return Err(invalid_detail(
                "dvc_migration_journal_entries_unsorted",
                path.display(),
            ));
        }
        if self
            .blocking_reasons
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err(invalid_detail(
                "dvc_migration_journal_reasons_unsorted",
                path.display(),
            ));
        }
        if self
            .remote_verifications
            .iter()
            .any(|(name, digest)| name.trim().is_empty() || !is_evidence_digest(digest))
        {
            return Err(invalid_detail(
                "dvc_migration_remote_verification_invalid",
                path.display(),
            ));
        }
        if self.safe_to_remove_dvc
            && (!self.entries.iter().all(|entry| {
                matches!(
                    entry.state,
                    VerificationState::Verified | VerificationState::Accounted
                )
            }) || self
                .cutover_verification
                .as_deref()
                .is_none_or(|digest| !is_evidence_digest(digest))
                || !self.git_index_published
                || !self.staging_flushed
                || !self.blocking_reasons.is_empty())
        {
            return Err(invalid_detail(
                "dvc_migration_journal_cutover_unverified",
                path.display(),
            ));
        }
        Ok(())
    }
}

/// Inventory DVC metadata, pointers, cache, lock records, and remotes.
pub fn inventory_project(root: &Path) -> Result<DvcInventory> {
    let root = fs::canonicalize(root).map_err(WorkflowError::Io)?;
    if !root.is_dir() {
        return Err(invalid("dvc_project_not_directory", &root));
    }
    let mut metadata_files = BTreeSet::new();
    let mut pointers = Vec::new();
    let mut findings = Vec::new();
    collect_files(
        &root,
        &root,
        &mut metadata_files,
        &mut pointers,
        &mut findings,
    )?;
    for relative in metadata_files
        .iter()
        .filter(|path| path.ends_with("dvc.yaml") && path.as_str() != "dvc.yaml")
    {
        findings.push(finding(
            "dvc_multiple_pipeline_files",
            Some(relative),
            "nested dvc.yaml is inventoried but the single-file converter cannot publish it safely",
            true,
        ));
    }
    let mut outputs = Vec::new();
    for pointer in pointers {
        let declaration = relative_path(&root, &pointer)?;
        let bytes = fs::read(&pointer).map_err(WorkflowError::Io)?;
        let value: Value = serde_yaml::from_slice(&bytes).map_err(|source| {
            WorkflowError::DvcMigrationInvalid {
                key: format!("invalid DVC pointer: {source}"),
                origin: declaration.clone(),
            }
        })?;
        parse_pointer(&root, &declaration, &value, &mut outputs, &mut findings)?;
    }
    for relative in metadata_files
        .iter()
        .filter(|path| path.ends_with("dvc.yaml"))
    {
        let pipeline = root.join(relative);
        let bytes = fs::read(&pipeline).map_err(WorkflowError::Io)?;
        let value: Value = serde_yaml::from_slice(&bytes).map_err(|source| {
            WorkflowError::DvcMigrationInvalid {
                key: format!("invalid dvc.yaml: {source}"),
                origin: relative.clone(),
            }
        })?;
        if let Some(artifacts) = value
            .as_mapping()
            .and_then(|map| map.get(Value::String("artifacts".to_owned())))
        {
            let mut declarations = BTreeMap::new();
            let keys_are_strings = if let Some(map) = artifacts.as_mapping() {
                let keys_are_strings = map.keys().all(Value::is_string);
                declarations.extend(map.iter().filter_map(|(key, value)| {
                    key.as_str().map(|key| (key.to_owned(), value.clone()))
                }));
                keys_are_strings
            } else {
                false
            };
            let metadata = ArtifactMetadata::from_declarations(declarations);
            let detail = if !keys_are_strings {
                "DVC artifact declaration names are not strings"
            } else {
                match ArtifactCatalog::from_metadata(&metadata) {
                    Ok(_) => {
                        "DVC artifact declarations are retained, but migration cutover requires the Crab artifact lifecycle and remote reachability proof"
                    }
                    Err(_) => {
                        "DVC artifact declarations are not representable by the Crab artifact lifecycle"
                    }
                }
            };
            findings.push(finding(
                "artifact_lifecycle_pending",
                Some(relative),
                detail,
                true,
            ));
        }
        parse_pipeline_outputs(&root, relative, &value, &mut outputs, &mut findings)?;
    }
    let lock_records = parse_lock_records(&root, &metadata_files, &mut findings)?;
    apply_lock_output_metadata(&root, &metadata_files, &mut outputs, &mut findings)?;
    let (remotes, cache_roots) = parse_config(&root, &mut findings)?;
    for cache_root in &cache_roots {
        match fs::symlink_metadata(cache_root) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                findings.push(finding(
                    "dvc_cache_root_unsupported",
                    Some(&relative_or_absolute(&root, cache_root)),
                    "configured DVC cache root is not a real directory",
                    true,
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => findings.push(finding(
                "dvc_cache_root_unreadable",
                Some(&relative_or_absolute(&root, cache_root)),
                error.to_string(),
                true,
            )),
        }
    }
    for output in &mut outputs {
        verify_output(&root, &cache_roots, output, &mut findings);
    }
    outputs.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.declaration.cmp(&right.declaration))
    });
    for pair in outputs.windows(2) {
        if pair[0].path == pair[1].path
            && (pair[0].declaration != pair[1].declaration || pair[0].dvc_md5 != pair[1].dvc_md5)
        {
            findings.push(finding(
                "dvc_output_path_conflict",
                Some(&pair[0].path),
                "multiple DVC declarations describe the same output path",
                true,
            ));
        }
    }
    outputs
        .dedup_by(|left, right| left.path == right.path && left.declaration == right.declaration);
    let mut cache_objects = Vec::new();
    for cache_root in &cache_roots {
        cache_objects.extend(collect_relative_files(cache_root, &root)?);
    }
    cache_objects.sort();
    cache_objects.dedup();
    let cache_object_count = u64::try_from(cache_objects.len())
        .map_err(|_| invalid_detail("dvc_cache_object_count_overflow", cache_objects.len()))?;
    let mut run_cache_files = collect_relative_files(&root.join(".dvc/run/cache"), &root)?;
    run_cache_files.extend(collect_relative_files(
        &root.join(".dvc/cache/runs"),
        &root,
    )?);
    run_cache_files.extend(collect_relative_files(&root.join(".dvc/tmp"), &root)?);
    run_cache_files.sort();
    run_cache_files.dedup();
    let ignore_files = metadata_files
        .iter()
        .filter(|path| path.ends_with(".dvcignore"))
        .cloned()
        .collect::<Vec<_>>();
    let mut fingerprint_input = Vec::new();
    for relative in &metadata_files {
        fingerprint_input.extend_from_slice(relative.as_bytes());
        fingerprint_input.push(0);
        fingerprint_input
            .extend_from_slice(&fs::read(root.join(relative)).map_err(WorkflowError::Io)?);
        fingerprint_input.push(0xff);
    }
    for output in &outputs {
        fingerprint_input.extend_from_slice(
            serde_json::to_string(output)
                .map_err(|error| invalid_detail("dvc_inventory_serialize", error))?
                .as_bytes(),
        );
    }
    for relative in &run_cache_files {
        fingerprint_input.extend_from_slice(relative.as_bytes());
        fingerprint_input.push(0);
        let path = root.join(relative);
        let metadata = fs::symlink_metadata(&path).map_err(WorkflowError::Io)?;
        if metadata.file_type().is_symlink() {
            // Keep the path bound to the source fingerprint without
            // following a DVC temporary/cache link outside the project.
            fingerprint_input.push(0);
        } else if metadata.is_file() {
            fingerprint_input.extend_from_slice(&fs::read(path).map_err(WorkflowError::Io)?);
        } else {
            fingerprint_input.push(0);
        }
        fingerprint_input.push(0xfe);
    }
    for cache_object in &cache_objects {
        fingerprint_input.extend_from_slice(cache_object.as_bytes());
        fingerprint_input.push(0xfd);
        let path = Path::new(cache_object);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        let metadata = fs::symlink_metadata(&path).map_err(WorkflowError::Io)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            fingerprint_input.push(0xfa);
            continue;
        }
        fingerprint_input.extend_from_slice(&metadata.len().to_le_bytes());
        fingerprint_input.push(0xfc);
        fingerprint_input.extend_from_slice(
            md5_file(&path)
                .map_err(|error| invalid_detail("dvc_cache_object_unreadable", error))?
                .as_bytes(),
        );
        fingerprint_input.push(0xfb);
    }
    let fingerprint = blake3::hash(&fingerprint_input).to_hex().to_string();
    findings.sort_by(|left, right| {
        left.code
            .cmp(&right.code)
            .then_with(|| left.source.cmp(&right.source))
    });
    Ok(DvcInventory {
        schema_version: DVC_INVENTORY_SCHEMA_VERSION,
        metadata_files: metadata_files.into_iter().collect(),
        outputs,
        remotes,
        lock_records,
        cache_object_count,
        cache_objects,
        cache_roots: cache_roots
            .iter()
            .map(|path| relative_or_absolute(&root, path))
            .collect(),
        run_cache_files,
        ignore_files,
        fingerprint,
        findings,
        safe_to_remove_dvc: false,
    })
}

fn collect_files(
    root: &Path,
    directory: &Path,
    metadata: &mut BTreeSet<String>,
    pointers: &mut Vec<PathBuf>,
    findings: &mut Vec<DvcFinding>,
) -> Result<()> {
    for entry in fs::read_dir(directory).map_err(WorkflowError::Io)? {
        let entry = entry.map_err(WorkflowError::Io)?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(WorkflowError::Io)?;
        if file_type.is_symlink() {
            let is_dvc_metadata = path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| {
                    name == ".dvc"
                        || name == "dvc.yaml"
                        || name == "dvc.lock"
                        || name == ".dvcignore"
                        || name.ends_with(".dvc")
                });
            if is_dvc_metadata {
                let relative = relative_path(root, &path)?;
                findings.push(finding(
                    "dvc_metadata_symlink",
                    Some(&relative),
                    "DVC metadata symlinks are not followed during migration",
                    true,
                ));
            }
            continue;
        }
        let relative = relative_path(root, &path)?;
        if file_type.is_dir() {
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if matches!(name, ".git" | ".crab" | "target" | "node_modules") {
                continue;
            }
            if matches!(relative.as_str(), ".dvc/cache" | ".dvc/run" | ".dvc/tmp") {
                continue;
            }
            collect_files(root, &path, metadata, pointers, findings)?;
            continue;
        }
        let is_pointer = path.extension().and_then(|value| value.to_str()) == Some("dvc");
        let is_metadata = is_pointer
            || matches!(
                path.file_name().and_then(|value| value.to_str()),
                Some("dvc.yaml" | "dvc.lock" | ".dvcignore")
            )
            || relative == ".dvc/config"
            || relative == ".dvc/config.local";
        if is_metadata {
            metadata.insert(relative);
        }
        if is_pointer {
            pointers.push(path);
        }
    }
    Ok(())
}

fn parse_pointer(
    root: &Path,
    declaration: &str,
    value: &Value,
    outputs: &mut Vec<DvcOutputRecord>,
    findings: &mut Vec<DvcFinding>,
) -> Result<()> {
    let Some(map) = value.as_mapping() else {
        return Err(invalid_detail("dvc_pointer_not_mapping", declaration));
    };
    report_unknown_yaml_keys(
        map,
        &["outs"],
        declaration,
        "dvc_pointer_construct_unsupported",
        findings,
    );
    let Some(outs) = map.get(Value::String("outs".to_owned())) else {
        findings.push(finding(
            "dvc_pointer_missing_outs",
            Some(declaration),
            "pointer does not declare outs",
            true,
        ));
        return Ok(());
    };
    let Some(sequence) = outs.as_sequence() else {
        return Err(invalid_detail("dvc_pointer_outs_not_sequence", declaration));
    };
    let pointer_dir = Path::new(declaration)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    for output in sequence {
        let Some(output_map) = output.as_mapping() else {
            findings.push(finding(
                "dvc_output_shape_unsupported",
                Some(declaration),
                "output is not a mapping",
                true,
            ));
            continue;
        };
        report_unknown_yaml_keys(
            output_map,
            &[
                "path",
                "md5",
                "size",
                "isexec",
                "hash",
                "etag",
                "checksum",
                "version_id",
                "cloud_version_id",
                "remote",
                "files",
                "type",
                "cache",
                "persist",
                "checkpoint",
                "meta",
                "desc",
            ],
            declaration,
            "dvc_output_construct_unsupported",
            findings,
        );
        let raw_path = yaml_string(output_map, "path")
            .ok_or_else(|| invalid_detail("dvc_output_path_missing", declaration))?;
        let path = safe_output_path(
            root,
            &pointer_dir.join(&raw_path).to_string_lossy(),
            declaration,
            findings,
        )?;
        if let Some(checkpoint) = output_map.get(Value::String("checkpoint".to_owned())) {
            let detail = match checkpoint {
                Value::Bool(true) => {
                    format!("output '{path}' declares checkpoint semantics")
                }
                Value::Bool(false) => {
                    format!("output '{path}' declares an unsupported checkpoint field")
                }
                _ => format!("output '{path}' has a non-boolean checkpoint field"),
            };
            findings.push(finding(
                "dvc_checkpoint_unsupported",
                Some(declaration),
                detail,
                true,
            ));
        }
        let dvc_md5 = yaml_string(output_map, "md5");
        let directory = dvc_md5
            .as_deref()
            .is_some_and(|value| value.ends_with(".dir"));
        let provenance = provenance_from_map(output_map).or_else(|| provenance_from_map(map));
        if (has_provenance_fields(output_map) || has_provenance_fields(map)) && provenance.is_none()
        {
            findings.push(finding(
                "dvc_import_provenance_unsupported",
                Some(declaration),
                "import provenance has an unsupported or credential-bearing locator",
                true,
            ));
        }
        outputs.push(DvcOutputRecord {
            declaration: declaration.to_owned(),
            path,
            dvc_md5,
            size: yaml_u64(output_map, "size"),
            directory,
            isexec: yaml_bool(output_map, "isexec").unwrap_or(false),
            materialized: VerificationState::PresentUnverified,
            cache: VerificationState::Missing,
            provenance,
            cache_locator: None,
            materialized_bytes: None,
        });
    }
    Ok(())
}

fn parse_pipeline_outputs(
    root: &Path,
    pipeline_relative: &str,
    value: &Value,
    outputs: &mut Vec<DvcOutputRecord>,
    findings: &mut Vec<DvcFinding>,
) -> Result<()> {
    if let Some(map) = value.as_mapping() {
        report_unknown_yaml_keys(
            map,
            &[
                "stages",
                "vars",
                "params",
                "plots",
                "metrics",
                "artifacts",
                "phases",
                "defaults",
                "schema",
                "version",
            ],
            pipeline_relative,
            "dvc_pipeline_construct_unsupported",
            findings,
        );
    }
    let Some(stages) = value
        .as_mapping()
        .and_then(|map| map.get(Value::String("stages".to_owned())))
        .and_then(Value::as_mapping)
    else {
        return Ok(());
    };
    for (stage_name, stage_value) in stages {
        let stage_name = stage_name.as_str().unwrap_or("<unknown>");
        let Some(stage) = stage_value.as_mapping() else {
            findings.push(finding(
                "dvc_stage_shape_unsupported",
                Some(pipeline_relative),
                format!("stage {stage_name} is not a mapping"),
                true,
            ));
            continue;
        };
        report_unknown_yaml_keys(
            stage,
            &[
                "cmd",
                "deps",
                "outs",
                "params",
                "plots",
                "metrics",
                "wdir",
                "frozen",
                "always_changed",
                "desc",
                "meta",
                "vars",
                "do",
                "foreach",
                "matrix",
                "run",
                "needs",
                "executor",
                "repro",
                "live",
                "external",
            ],
            pipeline_relative,
            "dvc_stage_construct_unsupported",
            findings,
        );
        let pipeline_dir = Path::new(pipeline_relative)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let stage_wdir = match stage.get(Value::String("wdir".to_owned())) {
            None => PathBuf::new(),
            Some(value) => match value.as_str() {
                Some(value) => PathBuf::from(value),
                None => {
                    findings.push(finding(
                        "dvc_stage_wdir_invalid",
                        Some(pipeline_relative),
                        format!("stage {stage_name} wdir is not a string"),
                        true,
                    ));
                    continue;
                }
            },
        };
        let output_base = pipeline_dir.join(stage_wdir);
        let Some(outs) = stage
            .get(Value::String("outs".to_owned()))
            .and_then(Value::as_sequence)
        else {
            continue;
        };
        for output in outs {
            let (path, settings) = match output {
                Value::String(path) => (path.clone(), None),
                Value::Mapping(map) => {
                    if let Some(path) = yaml_string(map, "path") {
                        report_unknown_yaml_keys(
                            map,
                            &[
                                "path",
                                "md5",
                                "size",
                                "isexec",
                                "hash",
                                "etag",
                                "checksum",
                                "version_id",
                                "cloud_version_id",
                                "remote",
                                "files",
                                "type",
                                "cache",
                                "persist",
                                "checkpoint",
                                "meta",
                                "desc",
                                "metric",
                                "plot",
                            ],
                            pipeline_relative,
                            "dvc_output_construct_unsupported",
                            findings,
                        );
                        (path, Some(map))
                    } else if map.len() == 1 {
                        let Some((key, settings)) = map.iter().next() else {
                            continue;
                        };
                        let Some(path) = key.as_str() else { continue };
                        let settings = settings.as_mapping();
                        if let Some(settings) = settings {
                            report_unknown_yaml_keys(
                                settings,
                                &[
                                    "path",
                                    "md5",
                                    "size",
                                    "isexec",
                                    "hash",
                                    "etag",
                                    "checksum",
                                    "version_id",
                                    "cloud_version_id",
                                    "remote",
                                    "files",
                                    "type",
                                    "cache",
                                    "persist",
                                    "checkpoint",
                                    "meta",
                                    "desc",
                                    "metric",
                                    "plot",
                                ],
                                pipeline_relative,
                                "dvc_output_construct_unsupported",
                                findings,
                            );
                        }
                        (path.to_owned(), settings)
                    } else {
                        report_unknown_yaml_keys(
                            map,
                            &[
                                "path",
                                "md5",
                                "size",
                                "isexec",
                                "hash",
                                "etag",
                                "checksum",
                                "version_id",
                                "cloud_version_id",
                                "remote",
                                "files",
                                "type",
                                "cache",
                                "persist",
                                "checkpoint",
                                "meta",
                                "desc",
                                "metric",
                                "plot",
                            ],
                            pipeline_relative,
                            "dvc_output_construct_unsupported",
                            findings,
                        );
                        findings.push(finding(
                            "dvc_output_shape_unsupported",
                            Some(pipeline_relative),
                            format!("stage {stage_name} output has no path"),
                            true,
                        ));
                        continue;
                    }
                }
                _ => {
                    findings.push(finding(
                        "dvc_output_shape_unsupported",
                        Some(pipeline_relative),
                        format!("stage {stage_name} output has unsupported shape"),
                        true,
                    ));
                    continue;
                }
            };
            let path = output_base.join(path);
            let path =
                safe_output_path(root, &path.to_string_lossy(), pipeline_relative, findings)?;
            if let Some(settings) = settings
                && settings.contains_key(Value::String("checkpoint".to_owned()))
            {
                let Some(checkpoint) = settings.get(Value::String("checkpoint".to_owned())) else {
                    continue;
                };
                let detail = match checkpoint {
                    Value::Bool(true) => {
                        format!("stage {stage_name} output '{path}' declares checkpoint semantics")
                    }
                    Value::Bool(false) => format!(
                        "stage {stage_name} output '{path}' declares an unsupported checkpoint field"
                    ),
                    _ => format!(
                        "stage {stage_name} output '{path}' has a non-boolean checkpoint field"
                    ),
                };
                findings.push(finding(
                    "dvc_checkpoint_unsupported",
                    Some(pipeline_relative),
                    detail,
                    true,
                ));
            }
            let dvc_md5 = settings.and_then(|map| yaml_string(map, "md5"));
            let provenance = settings.and_then(provenance_from_map);
            if settings.is_some_and(has_provenance_fields) && provenance.is_none() {
                findings.push(finding(
                    "dvc_import_provenance_unsupported",
                    Some(pipeline_relative),
                    "import provenance has an unsupported or credential-bearing locator",
                    true,
                ));
            }
            outputs.push(DvcOutputRecord {
                declaration: format!("{pipeline_relative}#stages.{stage_name}"),
                path,
                dvc_md5: dvc_md5.clone(),
                size: settings.and_then(|map| yaml_u64(map, "size")),
                directory: dvc_md5
                    .as_deref()
                    .is_some_and(|value| value.ends_with(".dir")),
                isexec: settings
                    .and_then(|map| yaml_bool(map, "isexec"))
                    .unwrap_or(false),
                materialized: VerificationState::PresentUnverified,
                cache: VerificationState::Missing,
                provenance,
                cache_locator: None,
                materialized_bytes: None,
            });
        }
    }
    Ok(())
}

fn parse_lock_records(
    root: &Path,
    metadata_files: &BTreeSet<String>,
    findings: &mut Vec<DvcFinding>,
) -> Result<Vec<String>> {
    let mut all_records = Vec::new();
    for relative in metadata_files
        .iter()
        .filter(|path| path.ends_with("dvc.lock"))
    {
        let path = root.join(relative);
        let bytes = fs::read(path).map_err(WorkflowError::Io)?;
        let value: Value = serde_yaml::from_slice(&bytes).map_err(|source| {
            WorkflowError::DvcMigrationInvalid {
                key: format!("invalid dvc.lock: {source}"),
                origin: relative.clone(),
            }
        })?;
        let Some(map) = value.as_mapping() else {
            findings.push(finding(
                "dvc_lock_shape_unsupported",
                Some(relative),
                "lockfile is not a mapping",
                true,
            ));
            continue;
        };
        report_unknown_yaml_keys(
            map,
            &[
                "schema", "version", "stages", "params", "plots", "metrics", "outs", "md5", "hash",
                "size",
            ],
            relative,
            "dvc_lock_construct_unsupported",
            findings,
        );
        if let Some(stages) = map
            .get(Value::String("stages".to_owned()))
            .and_then(Value::as_mapping)
        {
            for (stage_name, stage_value) in stages {
                let source = stage_name
                    .as_str()
                    .map_or_else(|| relative.clone(), |name| format!("{relative}#{name}"));
                if let Some(stage) = stage_value.as_mapping() {
                    report_unknown_yaml_keys(
                        stage,
                        &[
                            "cmd",
                            "deps",
                            "outs",
                            "params",
                            "plots",
                            "metrics",
                            "wdir",
                            "frozen",
                            "always_changed",
                            "desc",
                            "meta",
                            "vars",
                            "do",
                            "foreach",
                            "matrix",
                            "run",
                            "needs",
                            "executor",
                            "repro",
                            "live",
                            "external",
                        ],
                        &source,
                        "dvc_lock_stage_construct_unsupported",
                        findings,
                    );
                }
            }
        }
        if yaml_string(map, "schema").is_none() && yaml_string(map, "version").is_none() {
            findings.push(finding(
                "dvc_lock_version_unknown",
                Some(relative),
                "lockfile has no schema/version field",
                true,
            ));
        }
        let mut records = Vec::new();
        collect_lock_records(map, "", &mut records);
        all_records.extend(
            records
                .into_iter()
                .map(|record| format!("{relative}:{record}")),
        );
    }
    all_records.sort();
    all_records.dedup();
    Ok(all_records)
}

fn apply_lock_output_metadata(
    root: &Path,
    metadata_files: &BTreeSet<String>,
    outputs: &mut [DvcOutputRecord],
    findings: &mut Vec<DvcFinding>,
) -> Result<()> {
    let mut locked =
        BTreeMap::<(String, String, String), (String, Option<u64>, Option<bool>)>::new();
    for relative in metadata_files
        .iter()
        .filter(|path| path.ends_with("dvc.lock"))
    {
        let bytes = fs::read(root.join(relative)).map_err(WorkflowError::Io)?;
        let value: Value = serde_yaml::from_slice(&bytes).map_err(|source| {
            WorkflowError::DvcMigrationInvalid {
                key: format!("invalid dvc.lock: {source}"),
                origin: relative.clone(),
            }
        })?;
        let Some(stages) = value
            .as_mapping()
            .and_then(|map| map.get(Value::String("stages".to_owned())))
            .and_then(Value::as_mapping)
        else {
            continue;
        };
        for (stage_name, stage_value) in stages {
            let Some(stage_name) = stage_name.as_str() else {
                findings.push(finding(
                    "dvc_lock_stage_name_invalid",
                    Some(relative),
                    "lockfile stage name is not a string",
                    true,
                ));
                continue;
            };
            let Some(stage) = stage_value.as_mapping() else {
                findings.push(finding(
                    "dvc_lock_stage_shape_unsupported",
                    Some(relative),
                    format!("stage {stage_name} lock record is not a mapping"),
                    true,
                ));
                continue;
            };
            let lock_dir = Path::new(relative)
                .parent()
                .unwrap_or_else(|| Path::new(""));
            let stage_wdir = match stage.get(Value::String("wdir".to_owned())) {
                None => pipeline_stage_wdir(root, relative, stage_name)?.unwrap_or_default(),
                Some(value) => match value.as_str() {
                    Some(value) => PathBuf::from(value),
                    None => {
                        findings.push(finding(
                            "dvc_lock_stage_wdir_invalid",
                            Some(relative),
                            format!("stage {stage_name} lock wdir is not a string"),
                            true,
                        ));
                        continue;
                    }
                },
            };
            let lock_output_base = lock_dir.join(stage_wdir);
            let Some(outs) = stage
                .get(Value::String("outs".to_owned()))
                .and_then(Value::as_sequence)
            else {
                continue;
            };
            for output in outs {
                let Some(map) = output.as_mapping() else {
                    findings.push(finding(
                        "dvc_lock_output_shape_unsupported",
                        Some(relative),
                        format!("stage {stage_name} lock output is not a mapping"),
                        true,
                    ));
                    continue;
                };
                report_unknown_yaml_keys(
                    map,
                    &[
                        "path",
                        "md5",
                        "size",
                        "isexec",
                        "hash",
                        "etag",
                        "checksum",
                        "version_id",
                        "cloud_version_id",
                        "remote",
                        "files",
                        "type",
                        "cache",
                        "persist",
                        "checkpoint",
                        "meta",
                        "desc",
                        "metric",
                        "plot",
                    ],
                    relative,
                    "dvc_lock_output_construct_unsupported",
                    findings,
                );
                let Some(path) = yaml_string(map, "path") else {
                    findings.push(finding(
                        "dvc_lock_output_path_missing",
                        Some(relative),
                        format!("stage {stage_name} lock output has no path"),
                        true,
                    ));
                    continue;
                };
                let Some(md5) = yaml_string(map, "md5") else {
                    findings.push(finding(
                        "dvc_lock_output_checksum_missing",
                        Some(&path),
                        format!("stage {stage_name} lock output has no MD5"),
                        true,
                    ));
                    continue;
                };
                let lock_output_path = lock_output_base.join(&path);
                let key = (
                    relative.to_owned(),
                    stage_name.to_owned(),
                    normalize_dvc_path(&lock_output_path.to_string_lossy()),
                );
                let value = (md5, yaml_u64(map, "size"), yaml_bool(map, "isexec"));
                if let Some(previous) = locked.insert(key.clone(), value.clone())
                    && previous != value
                {
                    findings.push(finding(
                        "dvc_lock_mismatch",
                        Some(relative),
                        format!("lock output {} has conflicting identities", key.2),
                        true,
                    ));
                }
            }
        }
    }

    for output in outputs {
        let Some((pipeline, stage_name)) = output.declaration.split_once("#stages.") else {
            continue;
        };
        let lock_path = Path::new(pipeline)
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join("dvc.lock")
            .to_string_lossy()
            .replace('\\', "/");
        let Some((locked_md5, locked_size, locked_isexec)) = locked.get(&(
            lock_path,
            stage_name.to_owned(),
            normalize_dvc_path(&output.path),
        )) else {
            if output.dvc_md5.is_none() {
                findings.push(finding(
                    "dvc_lock_output_missing",
                    Some(&output.path),
                    "pipeline output has no matching dvc.lock identity",
                    true,
                ));
            }
            continue;
        };
        if let Some(declared) = output.dvc_md5.as_deref()
            && declared != locked_md5
        {
            findings.push(finding(
                "dvc_lock_mismatch",
                Some(&output.path),
                "pipeline output identity differs from dvc.lock",
                true,
            ));
            continue;
        }
        output.dvc_md5 = Some(locked_md5.clone());
        output.size = output.size.or(*locked_size);
        if let Some(isexec) = locked_isexec {
            output.isexec = *isexec;
        }
        output.directory = output
            .dvc_md5
            .as_deref()
            .is_some_and(|value| value.ends_with(".dir"));
    }
    Ok(())
}

fn pipeline_stage_wdir(
    root: &Path,
    lock_relative: &str,
    stage_name: &str,
) -> Result<Option<PathBuf>> {
    let pipeline_relative = Path::new(lock_relative).with_file_name("dvc.yaml");
    let pipeline_path = root.join(&pipeline_relative);
    if !pipeline_path.is_file() {
        return Ok(None);
    }
    let bytes = fs::read(&pipeline_path).map_err(WorkflowError::Io)?;
    let value: Value =
        serde_yaml::from_slice(&bytes).map_err(|source| WorkflowError::DvcMigrationInvalid {
            key: format!("invalid dvc.yaml: {source}"),
            origin: pipeline_relative.to_string_lossy().into_owned(),
        })?;
    let Some(stage) = value
        .as_mapping()
        .and_then(|map| map.get(Value::String("stages".to_owned())))
        .and_then(Value::as_mapping)
        .and_then(|stages| stages.get(Value::String(stage_name.to_owned())))
        .and_then(Value::as_mapping)
    else {
        return Ok(None);
    };
    let Some(value) = stage.get(Value::String("wdir".to_owned())) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| invalid_detail("dvc_lock_stage_wdir_invalid", stage_name))
        .map(Some)
}

fn collect_lock_records(map: &serde_yaml::Mapping, prefix: &str, records: &mut Vec<String>) {
    for (key, value) in map {
        let Some(key) = key.as_str() else { continue };
        let path = if prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{prefix}.{key}")
        };
        if matches!(key, "md5" | "hash" | "size" | "path") {
            if let Some(value) = value.as_str() {
                records.push(format!("{path}={value}"));
            } else if let Some(value) = value.as_i64() {
                records.push(format!("{path}={value}"));
            }
        }
        if let Some(child) = value.as_mapping() {
            collect_lock_records(child, &path, records);
        }
    }
}

fn parse_config(
    root: &Path,
    findings: &mut Vec<DvcFinding>,
) -> Result<(Vec<DvcRemoteDescriptor>, Vec<PathBuf>)> {
    let mut core_remote = None;
    let mut cache_dir = None;
    let mut remote_values = BTreeMap::<String, String>::new();
    let mut remote_sources = BTreeMap::<String, String>::new();
    for relative in [".dvc/config", ".dvc/config.local"] {
        let path = root.join(relative);
        if !path.is_file() {
            continue;
        }
        let text = fs::read_to_string(path).map_err(WorkflowError::Io)?;
        let mut section = String::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                section = line[1..line.len() - 1].trim().to_owned();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim().trim_matches('"').trim_matches('\'').to_owned();
            if section == "core" && key == "remote" {
                core_remote = Some(value);
            } else if section == "cache" && key == "dir" {
                cache_dir = Some(value);
            } else if section.starts_with("remote ") && key == "url" {
                let name = section.trim_start_matches("remote ").trim_matches('"');
                remote_values.insert(name.to_owned(), value);
                remote_sources.insert(name.to_owned(), relative.to_owned());
            }
        }
    }
    let mut remotes = Vec::new();
    for (name, raw) in remote_values {
        let Some((scheme, locator)) = redact_locator(&raw) else {
            findings.push(finding(
                "dvc_remote_url_invalid",
                Some(".dvc/config"),
                format!("remote {name} has an invalid URL"),
                true,
            ));
            continue;
        };
        let source_config = remote_sources
            .get(&name)
            .cloned()
            .unwrap_or_else(|| ".dvc/config".to_owned());
        remotes.push(DvcRemoteDescriptor {
            default: core_remote.as_deref() == Some(name.as_str()),
            name,
            locator,
            scheme,
            source_config,
            credential_source: credential_source(&raw),
            capability: "unmapped".to_owned(),
            destination: None,
        });
    }
    remotes.sort_by(|left, right| left.name.cmp(&right.name));
    if let Some(default_remote) = core_remote.as_deref()
        && !remotes.iter().any(|remote| remote.name == default_remote)
    {
        findings.push(finding(
            "dvc_remote_default_missing",
            Some(default_remote),
            "DVC core.remote names a remote section that is not configured",
            true,
        ));
    }
    for remote in &remotes {
        findings.push(finding(
            "dvc_remote_unmapped",
            Some(&remote.name),
            format!(
                "DVC remote '{}' requires an explicit --remote-map destination",
                remote.name
            ),
            true,
        ));
    }
    let mut cache_roots = vec![root.join(".dvc/cache/files/md5")];
    if let Some(path) = cache_dir {
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
        let path = if path.ends_with(Path::new("files/md5")) {
            path
        } else {
            path.join("files/md5")
        };
        cache_roots.insert(0, path);
    }
    let mut unique_roots = Vec::with_capacity(cache_roots.len());
    for root in cache_roots {
        if !unique_roots.contains(&root) {
            unique_roots.push(root);
        }
    }
    Ok((remotes, unique_roots))
}

/// Reconstruct a directory output from a verified DVC `.dir` cache object.
///
/// DVC stores the manifest separately from its member objects. The manifest
/// itself is never treated as the directory payload; every member is copied
/// and re-verified from the same cache root before the caller hashes it with
/// Crab's canonical tree hasher.
pub fn materialize_cached_directory(manifest_path: &Path, destination: &Path) -> Result<u64> {
    let metadata = fs::symlink_metadata(manifest_path).map_err(WorkflowError::Io)?;
    if !metadata.is_file() {
        return Err(invalid_detail(
            "dvc_cache_manifest_invalid",
            manifest_path.display(),
        ));
    }
    let file_name = manifest_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| invalid_detail("dvc_cache_manifest_invalid", manifest_path.display()))?;
    let prefix = manifest_path
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .ok_or_else(|| invalid_detail("dvc_cache_manifest_invalid", manifest_path.display()))?;
    let rest = file_name
        .strip_suffix(".dir")
        .ok_or_else(|| invalid_detail("dvc_cache_manifest_invalid", manifest_path.display()))?;
    let expected_md5 = format!("{prefix}{rest}");
    verify_dir_manifest(manifest_path, &expected_md5)
        .map_err(|error| invalid_detail("dvc_cache_manifest_invalid", error))?;
    if fs::symlink_metadata(destination).is_ok() {
        return Err(invalid_detail(
            "dvc_cache_destination_exists",
            destination.display(),
        ));
    }
    let cache_root = manifest_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| invalid_detail("dvc_cache_manifest_invalid", manifest_path.display()))?;
    let entries = read_dir_manifest(manifest_path)
        .map_err(|error| invalid_detail("dvc_cache_manifest_invalid", error))?;
    fs::create_dir_all(destination).map_err(WorkflowError::Io)?;
    let mut total = 0_u64;
    for entry in entries {
        let target = destination.join(&entry.relpath);
        if !is_safe_relative(Path::new(&entry.relpath)) {
            return Err(invalid_detail(
                "dvc_cache_manifest_invalid",
                format!("unsafe member path {}", entry.relpath),
            ));
        }
        let source = cache_object_path(cache_root, &entry.md5, "");
        let source_meta = fs::symlink_metadata(&source).map_err(|error| {
            invalid_detail(
                "dvc_cache_member_missing",
                format!("{}: {error}", source.display()),
            )
        })?;
        if source_meta.file_type().is_symlink() || !source_meta.is_file() {
            return Err(invalid_detail("dvc_cache_member_invalid", source.display()));
        }
        let actual = md5_file(&source)
            .map_err(|error| invalid_detail("dvc_cache_member_unreadable", error.to_string()))?;
        if actual != entry.md5 {
            return Err(invalid_detail(
                "dvc_cache_member_mismatch",
                format!("{} != {}", actual, entry.md5),
            ));
        }
        if let Some(size) = entry.size
            && source_meta.len() != size
        {
            return Err(invalid_detail(
                "dvc_cache_member_size_mismatch",
                entry.relpath,
            ));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(WorkflowError::Io)?;
        }
        fs::copy(&source, &target).map_err(WorkflowError::Io)?;
        set_executable(&target, entry.isexec).map_err(WorkflowError::Io)?;
        total = total.saturating_add(source_meta.len());
    }
    Ok(total)
}

fn verify_output(
    root: &Path,
    cache_roots: &[PathBuf],
    output: &mut DvcOutputRecord,
    findings: &mut Vec<DvcFinding>,
) {
    let path = root.join(&output.path);
    let path_metadata = fs::symlink_metadata(&path);
    if path_metadata
        .as_ref()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        output.materialized =
            VerificationState::Unsupported("symlink output is unsupported".to_owned());
        findings.push(finding(
            "dvc_output_type_unsupported",
            Some(&output.path),
            "output is a symlink; migration will not follow it",
            true,
        ));
    } else if path_metadata
        .as_ref()
        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    {
        output.materialized = VerificationState::Missing;
    } else if let Err(error) = path_metadata {
        output.materialized = VerificationState::Unsupported(error.to_string());
        findings.push(finding(
            "dvc_output_materialized_unreadable",
            Some(&output.path),
            error.to_string(),
            true,
        ));
    } else if path.is_file() {
        match md5_file(&path) {
            Ok(actual) if output.dvc_md5.as_deref() == Some(actual.as_str()) => {
                output.materialized = VerificationState::Verified;
                output.materialized_bytes = fs::metadata(&path).ok().map(|meta| meta.len());
                if let Some(expected_size) = output.size
                    && output.materialized_bytes != Some(expected_size)
                {
                    output.materialized = VerificationState::Mismatch {
                        expected: output.dvc_md5.clone(),
                        actual: Some(actual.clone()),
                    };
                    findings.push(finding(
                        "dvc_output_size_mismatch",
                        Some(&output.path),
                        format!(
                            "materialized output size differs from declared size {expected_size}"
                        ),
                        true,
                    ));
                }
                if is_executable(&path).ok() != Some(output.isexec) {
                    output.materialized = VerificationState::Mismatch {
                        expected: output.dvc_md5.clone(),
                        actual: Some(actual),
                    };
                    findings.push(finding(
                        "dvc_output_mode_mismatch",
                        Some(&output.path),
                        "materialized output executable mode differs from DVC metadata",
                        true,
                    ));
                }
            }
            Ok(actual) => {
                output.materialized = VerificationState::Mismatch {
                    expected: output.dvc_md5.clone(),
                    actual: Some(actual),
                };
                findings.push(finding(
                    "dvc_output_materialized_mismatch",
                    Some(&output.path),
                    "materialized output does not match DVC MD5",
                    true,
                ));
            }
            Err(error) => {
                output.materialized = VerificationState::Unsupported(error.to_string());
                findings.push(finding(
                    "dvc_output_materialized_unreadable",
                    Some(&output.path),
                    error.to_string(),
                    true,
                ));
            }
        }
    } else if path.is_dir() {
        let Some(md5) = output.dvc_md5.as_deref() else {
            output.materialized = VerificationState::PresentUnverified;
            findings.push(finding(
                "dvc_output_checksum_missing",
                Some(&output.path),
                "directory output has no DVC MD5",
                true,
            ));
            return;
        };
        let cache_name = md5.strip_suffix(".dir").unwrap_or(md5);
        let mut manifest = None;
        for cache_root in cache_roots {
            let cache_path = cache_object_path(cache_root, cache_name, ".dir");
            let cache_metadata = fs::symlink_metadata(&cache_path).ok();
            if cache_metadata
                .as_ref()
                .is_some_and(|metadata| metadata.file_type().is_symlink())
            {
                findings.push(finding(
                    "dvc_cache_object_unsupported",
                    Some(&output.path),
                    "DVC directory manifest is a symlink",
                    true,
                ));
                continue;
            }
            if cache_metadata.as_ref().is_some_and(fs::Metadata::is_file) {
                match read_dir_manifest(&cache_path) {
                    Ok(entries) => {
                        output.cache_locator =
                            Some(cache_path.to_string_lossy().replace('\\', "/"));
                        manifest = Some(entries);
                        break;
                    }
                    Err(error) => findings.push(finding(
                        "dvc_cache_manifest_invalid",
                        Some(&output.path),
                        error,
                        true,
                    )),
                }
            }
        }
        if let Some(entries) = manifest {
            let mut total = 0_u64;
            let mut valid = true;
            for entry in &entries {
                let member = path.join(&entry.relpath);
                match (
                    member.is_file(),
                    md5_file(&member),
                    entry.size,
                    is_executable(&member).ok(),
                ) {
                    (true, Ok(actual), declared_size, mode)
                        if actual == entry.md5
                            && declared_size.is_none_or(|size| {
                                fs::metadata(&member).map(|meta| meta.len()).ok() == Some(size)
                            })
                            && mode == Some(entry.isexec) =>
                    {
                        total = total.saturating_add(
                            fs::metadata(&member).map(|meta| meta.len()).unwrap_or(0),
                        );
                    }
                    _ => {
                        valid = false;
                        findings.push(finding(
                            "dvc_directory_materialized_mismatch",
                            Some(&output.path),
                            format!("directory member {} is missing or differs", entry.relpath),
                            true,
                        ));
                    }
                }
            }
            if valid {
                match directory_matches_manifest(&path, &entries) {
                    Ok(true) => {}
                    Ok(false) => {
                        valid = false;
                        findings.push(finding(
                            "dvc_directory_materialized_mismatch",
                            Some(&output.path),
                            "directory contains files not represented by its DVC manifest",
                            true,
                        ));
                    }
                    Err(error) => {
                        valid = false;
                        findings.push(finding(
                            "dvc_directory_materialized_unreadable",
                            Some(&output.path),
                            error,
                            true,
                        ));
                    }
                }
            }
            output.materialized_bytes = Some(total);
            if valid && output.size.is_none_or(|size| size == total) {
                output.materialized = VerificationState::Verified;
            } else {
                output.materialized = VerificationState::Mismatch {
                    expected: Some(md5.to_owned()),
                    actual: None,
                };
                if let Some(expected_size) = output.size
                    && expected_size != total
                {
                    findings.push(finding(
                        "dvc_output_size_mismatch",
                        Some(&output.path),
                        format!(
                            "directory size {total} differs from declared size {expected_size}"
                        ),
                        true,
                    ));
                }
            }
        } else {
            output.materialized = VerificationState::PresentUnverified;
            findings.push(finding(
                "dvc_directory_manifest_missing",
                Some(&output.path),
                "directory identity requires its DVC .dir manifest",
                true,
            ));
        }
    } else {
        output.materialized = VerificationState::Unsupported("unsupported file type".to_owned());
        findings.push(finding(
            "dvc_output_type_unsupported",
            Some(&output.path),
            "output is neither a regular file nor directory",
            true,
        ));
    }
    let Some(md5) = output.dvc_md5.as_deref() else {
        findings.push(finding(
            "dvc_output_checksum_missing",
            Some(&output.path),
            "output has no DVC MD5",
            true,
        ));
        return;
    };
    let md5_name = md5.strip_suffix(".dir").unwrap_or(md5);
    if !is_dvc_md5(md5_name) {
        output.cache = VerificationState::Unsupported("invalid DVC MD5 identity".to_owned());
        findings.push(finding(
            "dvc_checksum_invalid",
            Some(&output.path),
            "DVC output checksum is not a 32-character hexadecimal MD5",
            true,
        ));
        return;
    }
    let directory = output.directory || md5.ends_with(".dir");
    let cache_name = md5_name;
    let suffix = if directory { ".dir" } else { "" };
    for cache_root in cache_roots {
        let cache_path = cache_object_path(cache_root, cache_name, suffix);
        let cache_metadata = match fs::symlink_metadata(&cache_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                findings.push(finding(
                    "dvc_cache_object_unreadable",
                    Some(&output.path),
                    error.to_string(),
                    true,
                ));
                continue;
            }
        };
        if cache_metadata.file_type().is_symlink() || !cache_metadata.is_file() {
            findings.push(finding(
                "dvc_cache_object_unsupported",
                Some(&output.path),
                "DVC cache object is not a regular file",
                true,
            ));
            continue;
        }
        output.cache = if directory {
            match verify_dir_manifest(&cache_path, cache_name) {
                Ok(()) => VerificationState::Verified,
                Err(error) => {
                    findings.push(finding(
                        "dvc_cache_manifest_invalid",
                        Some(&output.path),
                        error,
                        true,
                    ));
                    VerificationState::Mismatch {
                        expected: Some(md5.to_owned()),
                        actual: None,
                    }
                }
            }
        } else {
            match md5_file(&cache_path) {
                Ok(actual) if actual == cache_name => VerificationState::Verified,
                Ok(actual) => {
                    findings.push(finding(
                        "dvc_cache_object_mismatch",
                        Some(&output.path),
                        format!("cache object MD5 {actual} differs from declared {cache_name}"),
                        true,
                    ));
                    VerificationState::Mismatch {
                        expected: Some(cache_name.to_owned()),
                        actual: Some(actual),
                    }
                }
                Err(error) => VerificationState::Unsupported(error.to_string()),
            }
        };
        output.cache_locator = Some(cache_path.to_string_lossy().replace('\\', "/"));
        return;
    }
    output.cache = VerificationState::Missing;
    findings.push(finding(
        "dvc_cache_object_missing",
        Some(&output.path),
        "DVC cache object is missing from configured cache roots",
        true,
    ));
    if output.materialized == VerificationState::Missing {
        findings.push(finding(
            "dvc_output_materialized_missing",
            Some(&output.path),
            "materialized output is missing and no verified cache object is available",
            true,
        ));
    }
}

#[derive(Debug)]
struct DvcManifestEntry {
    relpath: String,
    md5: String,
    size: Option<u64>,
    isexec: bool,
}

fn directory_matches_manifest(
    directory: &Path,
    entries: &[DvcManifestEntry],
) -> std::result::Result<bool, String> {
    let expected = entries
        .iter()
        .map(|entry| entry.relpath.clone())
        .collect::<BTreeSet<_>>();
    let mut actual = BTreeSet::new();
    collect_regular_files(directory, directory, &mut actual)?;
    Ok(actual == expected)
}

fn collect_regular_files(
    root: &Path,
    directory: &Path,
    files: &mut BTreeSet<String>,
) -> std::result::Result<(), String> {
    let entries = fs::read_dir(directory).map_err(|error| error.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            return Err(format!("directory contains symlink {}", path.display()));
        }
        if metadata.is_dir() {
            collect_regular_files(root, &path, files)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            files.insert(relative);
        } else {
            return Err(format!(
                "directory contains unsupported entry {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn read_dir_manifest(path: &Path) -> std::result::Result<Vec<DvcManifestEntry>, String> {
    let value: serde_json::Value =
        serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    let Some(entries) = value.as_array() else {
        return Err("DVC directory manifest is not an array".to_owned());
    };
    let mut seen = HashSet::new();
    let mut result = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(map) = entry.as_object() else {
            return Err("DVC directory entry is not an object".to_owned());
        };
        let Some(relative) = map.get("relpath").and_then(|value| value.as_str()) else {
            return Err("DVC directory entry has no relpath".to_owned());
        };
        let relative_path = Path::new(relative);
        if !is_safe_relative(relative_path) || !seen.insert(relative.to_owned()) {
            return Err("DVC directory manifest has traversal or duplicate paths".to_owned());
        }
        if result.iter().any(|entry: &DvcManifestEntry| {
            relative_path.starts_with(Path::new(&entry.relpath))
                || Path::new(&entry.relpath).starts_with(relative_path)
        }) {
            return Err("DVC directory manifest has colliding paths".to_owned());
        }
        let md5 = map
            .get("md5")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "DVC directory entry has no md5".to_owned())?;
        if !is_dvc_md5(md5) {
            return Err("DVC directory entry has an invalid md5".to_owned());
        }
        let size =
            match map.get("size") {
                None => None,
                Some(value) => Some(value.as_u64().ok_or_else(|| {
                    "DVC directory entry size is not an unsigned integer".to_owned()
                })?),
            };
        let isexec = match map.get("isexec") {
            None => false,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| "DVC directory entry isexec is not a boolean".to_owned())?,
        };
        result.push(DvcManifestEntry {
            relpath: relative.to_owned(),
            md5: md5.to_owned(),
            size,
            isexec,
        });
    }
    Ok(result)
}

fn is_dvc_md5(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_crab_digest(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("b3:") else {
        return false;
    };
    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_evidence_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn verify_dir_manifest(path: &Path, expected_md5: &str) -> std::result::Result<(), String> {
    let actual = md5_file(path).map_err(|error| error.to_string())?;
    if actual != expected_md5 {
        return Err(format!(
            "DVC directory manifest MD5 {actual} differs from declared {expected_md5}"
        ));
    }
    read_dir_manifest(path).map(|_| ())
}

fn cache_object_path(root: &Path, md5: &str, suffix: &str) -> PathBuf {
    let (prefix, rest) = md5.split_at(md5.len().min(2));
    root.join(prefix).join(format!("{rest}{suffix}"))
}

fn is_executable(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(fs::metadata(path)?.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = fs::metadata(path)?;
        Ok(false)
    }
}

fn set_executable(path: &Path, executable: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        let mode = permissions.mode();
        permissions.set_mode(if executable {
            mode | 0o111
        } else {
            mode & !0o111
        });
        fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, executable);
    }
    Ok(())
}

fn md5_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Md5::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex_bytes(&digest.finalize()))
}

fn collect_relative_files(directory: &Path, root: &Path) -> Result<Vec<String>> {
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    collect_relative_files_inner(directory, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_relative_files_inner(
    directory: &Path,
    root: &Path,
    files: &mut Vec<String>,
) -> Result<()> {
    for entry in fs::read_dir(directory).map_err(WorkflowError::Io)? {
        let entry = entry.map_err(WorkflowError::Io)?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(WorkflowError::Io)?;
        if file_type.is_symlink() {
            // Keep the path in the inventory so a symlinked cache/run-cache
            // record is accounted for and blocks cutover instead of silently
            // disappearing from the journal.
            files.push(relative_or_absolute(root, &path));
            continue;
        }
        if file_type.is_dir() {
            collect_relative_files_inner(&path, root, files)?;
        } else if file_type.is_file() {
            files.push(relative_or_absolute(root, &path));
        }
    }
    Ok(())
}

fn relative_or_absolute(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|value| value.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

fn provenance_from_map(map: &serde_yaml::Mapping) -> Option<DvcImportProvenance> {
    let raw_locator = yaml_string(map, "repo").or_else(|| yaml_string(map, "url"))?;
    let (scheme, locator) = redact_locator(&raw_locator)?;
    let kind = if matches!(scheme.as_str(), "http" | "https" | "file") {
        "url"
    } else {
        "repo"
    };
    Some(DvcImportProvenance {
        kind: kind.to_owned(),
        locator,
        revision: yaml_string(map, "rev").or_else(|| yaml_string(map, "version")),
        source_path: yaml_string(map, "path"),
    })
}

fn has_provenance_fields(map: &serde_yaml::Mapping) -> bool {
    yaml_string(map, "repo").is_some() || yaml_string(map, "url").is_some()
}

fn redact_locator(raw: &str) -> Option<(String, String)> {
    let mut url = match Url::parse(raw) {
        Ok(url) => url,
        Err(_) if !raw.trim().is_empty() && !raw.contains('@') && !raw.contains('?') => {
            return Some(("file".to_owned(), raw.trim().to_owned()));
        }
        Err(_) => return None,
    };
    let scheme = url.scheme().to_owned();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    Some((scheme, url.to_string()))
}

fn credential_source(raw: &str) -> String {
    if raw.contains('@') || raw.contains("token") || raw.contains("key") {
        "url-or-config".to_owned()
    } else if std::env::var_os("AWS_ACCESS_KEY_ID").is_some()
        || std::env::var_os("AWS_PROFILE").is_some()
    {
        "environment-or-profile".to_owned()
    } else {
        "none-observed".to_owned()
    }
}

fn safe_output_path(
    root: &Path,
    raw: &str,
    declaration: &str,
    findings: &mut Vec<DvcFinding>,
) -> Result<String> {
    let path = Path::new(raw);
    if !is_safe_relative(path) {
        findings.push(finding(
            "dvc_output_path_unsafe",
            Some(declaration),
            format!("output path {raw:?} escapes the project root"),
            true,
        ));
        return Err(invalid_detail("dvc_output_path_unsafe", declaration));
    }
    if root.join(path).starts_with(root) {
        Ok(path.to_string_lossy().replace('\\', "/"))
    } else {
        Err(invalid_detail("dvc_output_path_unsafe", declaration))
    }
}

fn is_safe_relative(path: &Path) -> bool {
    let raw = path.to_string_lossy();
    if raw.is_empty()
        || raw == "."
        || raw.starts_with('/')
        || raw.starts_with('\\')
        || raw.as_bytes().get(1) == Some(&b':')
        || raw.split(['/', '\\']).any(|component| component == "..")
    {
        return false;
    }
    let portable = raw.replace('\\', "/");
    !Path::new(&portable).is_absolute()
        && Path::new(&portable).components().all(|component| {
            !matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

fn relative_path(root: &Path, path: &Path) -> Result<String> {
    path.strip_prefix(root)
        .map(|value| value.to_string_lossy().replace('\\', "/"))
        .map_err(|_| invalid_detail("dvc_path_outside_project", path.display()))
}

fn yaml_string(map: &serde_yaml::Mapping, key: &str) -> Option<String> {
    map.get(Value::String(key.to_owned()))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn yaml_u64(map: &serde_yaml::Mapping, key: &str) -> Option<u64> {
    map.get(Value::String(key.to_owned()))
        .and_then(Value::as_u64)
}

fn yaml_bool(map: &serde_yaml::Mapping, key: &str) -> Option<bool> {
    map.get(Value::String(key.to_owned()))
        .and_then(Value::as_bool)
}

fn report_unknown_yaml_keys(
    map: &serde_yaml::Mapping,
    allowed: &[&str],
    source: &str,
    code: &str,
    findings: &mut Vec<DvcFinding>,
) {
    for key in map.keys() {
        let Some(key) = key.as_str() else {
            findings.push(finding(
                code,
                Some(source),
                "mapping contains a non-string key",
                true,
            ));
            continue;
        };
        if !allowed.contains(&key) {
            findings.push(finding(
                code,
                Some(source),
                format!("unsupported DVC construct '{key}'"),
                true,
            ));
        }
    }
}

fn normalize_dvc_path(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").to_owned()
}

fn output_key(output: &DvcOutputRecord) -> String {
    format!(
        "{}:{}:{}",
        output.declaration,
        output.path,
        output.dvc_md5.as_deref().unwrap_or("<missing>")
    )
}

fn finding(
    code: &str,
    source: Option<&str>,
    detail: impl Into<String>,
    blocking: bool,
) -> DvcFinding {
    DvcFinding {
        code: code.to_owned(),
        source: source.map(ToOwned::to_owned),
        detail: detail.into(),
        blocking,
    }
}

fn invalid(code: &str, path: &Path) -> WorkflowError {
    WorkflowError::DvcMigrationInvalid {
        key: code.to_owned(),
        origin: path.display().to_string(),
    }
}

fn invalid_detail(code: &str, detail: impl std::fmt::Display) -> WorkflowError {
    WorkflowError::DvcMigrationInvalid {
        key: code.to_owned(),
        origin: detail.to_string(),
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn inventory_redacts_credentials_and_verifies_file_cache() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("data.bin"), b"payload").expect("data");
        let md5 = hex_bytes(&Md5::digest(b"payload"));
        fs::create_dir_all(
            temp.path()
                .join(format!(".dvc/cache/files/md5/{}", &md5[..2])),
        )
        .expect("cache");
        fs::write(
            temp.path().join("data.bin.dvc"),
            format!("outs:\n  - md5: {md5}\n    size: 7\n    path: data.bin\n"),
        )
        .expect("pointer");
        fs::write(
            temp.path().join(".dvc/config"),
            "[core]\n    remote = origin\n[remote \"origin\"]\n    url = https://user:secret@example.test/path?token=secret\n",
        )
        .expect("config");
        fs::write(
            temp.path()
                .join(format!(".dvc/cache/files/md5/{}/{}", &md5[..2], &md5[2..])),
            b"payload",
        )
        .expect("cache object");
        let inventory = inventory_project(temp.path()).expect("inventory");
        assert_eq!(inventory.outputs.len(), 1);
        assert_eq!(inventory.cache_object_count, 1);
        assert_eq!(inventory.cache_objects.len(), 1);
        assert_eq!(
            inventory.outputs[0].materialized,
            VerificationState::Verified
        );
        assert_eq!(inventory.outputs[0].cache, VerificationState::Verified);
        assert_eq!(inventory.remotes[0].locator, "https://example.test/path");
        assert!(!inventory.remotes[0].locator.contains("secret"));
        assert!(!inventory.safe_to_remove_dvc);
    }

    #[test]
    fn pipeline_outputs_use_dvc_lock_identities() {
        let temp = TempDir::new().expect("tempdir");
        let md5 = hex_bytes(&Md5::digest(b"payload"));
        fs::write(
            temp.path().join("dvc.yaml"),
            "stages:\n  train:\n    cmd: train\n    outs:\n      - model.bin\n",
        )
        .expect("pipeline");
        fs::write(
            temp.path().join("dvc.lock"),
            format!(
                "schema: '2.0'\nstages:\n  train:\n    outs:\n      - md5: {md5}\n        size: 7\n        path: model.bin\n"
            ),
        )
        .expect("lock");
        fs::write(temp.path().join("model.bin"), b"payload").expect("output");
        let cache = temp
            .path()
            .join(format!(".dvc/cache/files/md5/{}/{}", &md5[..2], &md5[2..]));
        fs::create_dir_all(cache.parent().expect("cache parent")).expect("cache dir");
        fs::write(cache, b"payload").expect("cache object");

        let inventory = inventory_project(temp.path()).expect("inventory");
        let output = inventory
            .outputs
            .iter()
            .find(|output| output.path == "model.bin")
            .expect("pipeline output");
        assert_eq!(output.dvc_md5.as_deref(), Some(md5.as_str()));
        assert_eq!(output.materialized, VerificationState::Verified);
        assert_eq!(output.cache, VerificationState::Verified);
        assert!(
            !inventory
                .findings
                .iter()
                .any(|finding| finding.code == "dvc_output_checksum_missing")
        );
    }

    #[test]
    fn nested_pipeline_files_block_single_file_cutover() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("dvc.yaml"), "stages: {}\n").expect("root pipeline");
        fs::create_dir_all(temp.path().join("nested")).expect("nested directory");
        fs::write(
            temp.path().join("nested/dvc.yaml"),
            "stages:\n  train:\n    cmd: train\n",
        )
        .expect("nested pipeline");

        let inventory = inventory_project(temp.path()).expect("inventory");
        assert!(inventory.findings.iter().any(|finding| {
            finding.code == "dvc_multiple_pipeline_files"
                && finding.source.as_deref() == Some("nested/dvc.yaml")
                && finding.blocking
        }));
        assert!(!inventory.safe_to_remove_dvc);
    }

    #[test]
    fn pipeline_outputs_resolve_stage_wdir_for_lock_and_materialized_data() {
        let temp = TempDir::new().expect("tempdir");
        let md5 = hex_bytes(&Md5::digest(b"payload"));
        fs::create_dir_all(temp.path().join("sub/training")).expect("working directory");
        fs::write(
            temp.path().join("sub/dvc.yaml"),
            "stages:\n  train:\n    wdir: training\n    cmd: train\n    outs:\n      - model.bin\n",
        )
        .expect("pipeline");
        fs::write(
            temp.path().join("sub/dvc.lock"),
            format!(
                "schema: '2.0'\nstages:\n  train:\n    outs:\n      - md5: {md5}\n        size: 7\n        path: model.bin\n"
            ),
        )
        .expect("lock");
        fs::write(temp.path().join("sub/training/model.bin"), b"payload").expect("output");
        let cache = temp
            .path()
            .join(format!(".dvc/cache/files/md5/{}/{}", &md5[..2], &md5[2..]));
        fs::create_dir_all(cache.parent().expect("cache parent")).expect("cache dir");
        fs::write(cache, b"payload").expect("cache object");

        let inventory = inventory_project(temp.path()).expect("inventory");
        let output = inventory
            .outputs
            .iter()
            .find(|output| output.path == "sub/training/model.bin")
            .expect("pipeline output");
        assert_eq!(output.dvc_md5.as_deref(), Some(md5.as_str()));
        assert_eq!(output.materialized, VerificationState::Verified);
        assert_eq!(output.cache, VerificationState::Verified);
        assert!(
            !inventory
                .findings
                .iter()
                .any(|finding| finding.code == "dvc_lock_output_missing")
        );
    }

    #[test]
    fn inventory_rejects_checkpoint_outputs_before_cutover() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(
            temp.path().join("dvc.yaml"),
            "stages:\n  train:\n    cmd: train\n    outs:\n      - model.bin:\n          checkpoint: true\n",
        )
        .expect("pipeline");
        fs::write(temp.path().join("model.bin"), b"payload").expect("output");

        let inventory = inventory_project(temp.path()).expect("inventory");
        assert!(
            inventory
                .findings
                .iter()
                .any(|finding| finding.code == "dvc_checkpoint_unsupported" && finding.blocking)
        );
        assert!(!inventory.safe_to_remove_dvc);
    }

    #[test]
    fn standalone_pointer_checkpoint_is_blocking() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(
            temp.path().join("model.bin.dvc"),
            "outs:\n  - path: model.bin\n    checkpoint: true\n",
        )
        .expect("pointer");
        fs::write(temp.path().join("model.bin"), b"payload").expect("output");

        let inventory = inventory_project(temp.path()).expect("inventory");
        assert!(
            inventory
                .findings
                .iter()
                .any(|finding| finding.code == "dvc_checkpoint_unsupported" && finding.blocking)
        );
        assert!(!inventory.safe_to_remove_dvc);
    }

    #[test]
    fn inventory_blocks_unrepresentable_import_provenance() {
        let temp = TempDir::new().expect("tempdir");
        let md5 = hex_bytes(&Md5::digest(b"payload"));
        fs::write(
            temp.path().join("model.bin.dvc"),
            format!(
                "outs:\n  - path: model.bin\n    md5: {md5}\n    repo: git@github.com:example/model.git\n"
            ),
        )
        .expect("pointer");
        fs::write(temp.path().join("model.bin"), b"payload").expect("output");

        let inventory = inventory_project(temp.path()).expect("inventory");
        assert!(inventory.findings.iter().any(|finding| {
            finding.code == "dvc_import_provenance_unsupported" && finding.blocking
        }));
        assert!(inventory.outputs[0].provenance.is_none());
    }

    #[test]
    fn inventory_blocks_unknown_pointer_and_stage_constructs() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(
            temp.path().join("data.bin.dvc"),
            "outs:\n  - path: data.bin\n    md5: 0123456789abcdef0123456789abcdef\n    future_output_flag: true\nunknown_pointer_field: true\n",
        )
        .expect("pointer");
        fs::write(temp.path().join("data.bin"), b"payload").expect("output");
        fs::write(
            temp.path().join("dvc.yaml"),
            "stages:\n  train:\n    cmd: train\n    future_stage_flag: true\n    outs:\n      - path: model.bin\n        future_output_flag: true\n",
        )
        .expect("pipeline");

        let inventory = inventory_project(temp.path()).expect("inventory");
        let codes = inventory
            .findings
            .iter()
            .filter(|finding| finding.blocking)
            .map(|finding| finding.code.as_str())
            .collect::<BTreeSet<_>>();
        assert!(codes.contains("dvc_pointer_construct_unsupported"));
        assert!(codes.contains("dvc_output_construct_unsupported"));
        assert!(codes.contains("dvc_stage_construct_unsupported"));
        assert!(!inventory.safe_to_remove_dvc);
    }

    #[test]
    fn inventory_blocks_unknown_lock_constructs() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(
            temp.path().join("dvc.lock"),
            "schema: '2.0'\nstages:\n  train:\n    outs:\n      - path: model.bin\n        md5: 0123456789abcdef0123456789abcdef\n        future_output_flag: true\n    future_stage_flag: true\nfuture_lock_flag: true\n",
        )
        .expect("lock");
        let inventory = inventory_project(temp.path()).expect("inventory");
        let codes = inventory
            .findings
            .iter()
            .filter(|finding| finding.blocking)
            .map(|finding| finding.code.as_str())
            .collect::<BTreeSet<_>>();
        assert!(codes.contains("dvc_lock_construct_unsupported"));
        assert!(codes.contains("dvc_lock_stage_construct_unsupported"));
        assert!(codes.contains("dvc_lock_output_construct_unsupported"));
        assert!(!inventory.safe_to_remove_dvc);
    }

    #[test]
    fn inventory_keeps_artifact_cutover_unsafe_until_lifecycle_proof() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(
            temp.path().join("dvc.yaml"),
            "artifacts:\n  model:\n    path: model.bin\nstages: {}\n",
        )
        .expect("pipeline");
        let inventory = inventory_project(temp.path()).expect("inventory");
        assert!(
            inventory.findings.iter().any(|finding| {
                finding.code == "artifact_lifecycle_pending" && finding.blocking
            })
        );
    }

    #[test]
    fn nested_pointer_paths_resolve_relative_to_the_pointer_file() {
        let temp = TempDir::new().expect("tempdir");
        let md5 = hex_bytes(&Md5::digest(b"payload"));
        fs::create_dir_all(temp.path().join("nested")).expect("nested");
        fs::write(
            temp.path().join("nested/data.bin.dvc"),
            format!("outs:\n  - md5: {md5}\n    path: data.bin\n"),
        )
        .expect("pointer");
        fs::write(temp.path().join("nested/data.bin"), b"payload").expect("output");
        let cache = temp
            .path()
            .join(format!(".dvc/cache/files/md5/{}/{}", &md5[..2], &md5[2..]));
        fs::create_dir_all(cache.parent().expect("cache parent")).expect("cache dir");
        fs::write(cache, b"payload").expect("cache object");

        let inventory = inventory_project(temp.path()).expect("inventory");
        assert_eq!(inventory.outputs[0].path, "nested/data.bin");
    }

    #[test]
    fn journal_rejects_changed_source() {
        let inventory = DvcInventory {
            schema_version: DVC_INVENTORY_SCHEMA_VERSION,
            metadata_files: vec!["dvc.yaml".to_owned()],
            outputs: Vec::new(),
            remotes: Vec::new(),
            lock_records: Vec::new(),
            cache_object_count: 0,
            cache_objects: Vec::new(),
            cache_roots: Vec::new(),
            run_cache_files: Vec::new(),
            ignore_files: Vec::new(),
            fingerprint: "one".to_owned(),
            findings: Vec::new(),
            safe_to_remove_dvc: false,
        };
        let journal = DvcMigrationJournal::from_inventory(&inventory);
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("journal.json");
        journal.save_atomic(&path).expect("save");
        let error = DvcMigrationJournal::load(&path, "two").expect_err("stale source");
        assert!(error.to_string().contains("dvc_migration_source_changed"));
    }

    #[test]
    fn journal_requires_accounting_before_cutover() {
        let inventory = DvcInventory {
            schema_version: DVC_INVENTORY_SCHEMA_VERSION,
            metadata_files: Vec::new(),
            outputs: vec![DvcOutputRecord {
                declaration: "model.dvc".to_owned(),
                path: "model.bin".to_owned(),
                dvc_md5: None,
                size: None,
                directory: false,
                isexec: false,
                materialized: VerificationState::Missing,
                cache: VerificationState::Missing,
                provenance: None,
                cache_locator: None,
                materialized_bytes: None,
            }],
            remotes: Vec::new(),
            lock_records: Vec::new(),
            cache_object_count: 0,
            cache_objects: Vec::new(),
            cache_roots: Vec::new(),
            run_cache_files: Vec::new(),
            ignore_files: Vec::new(),
            fingerprint: "fingerprint".to_owned(),
            findings: Vec::new(),
            safe_to_remove_dvc: false,
        };
        let mut journal = DvcMigrationJournal::from_inventory(&inventory);
        let evidence = "cd".repeat(32);
        assert!(journal.mark_cutover_verified(evidence.clone()).is_err());
        journal.entries[0].state = VerificationState::Verified;
        journal.entries[0].crab_hash = Some(format!("b3:{}", "ab".repeat(32)));
        journal.git_index_published = true;
        journal.staging_flushed = true;
        journal.blocking_reasons.clear();
        journal
            .mark_cutover_verified(evidence.clone())
            .expect("accounted entry can be cut over");
        assert!(journal.safe_to_remove_dvc);
        assert_eq!(
            journal.cutover_verification.as_deref(),
            Some(evidence.as_str())
        );
    }

    #[test]
    fn directory_manifest_rejects_traversal() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("object.dir");
        fs::write(&path, r#"[{"relpath":"../escape","md5":"abc"}]"#).expect("manifest");
        assert!(verify_dir_manifest(&path, "abc").is_err());
    }

    #[test]
    fn directory_manifest_rejects_cross_platform_paths_and_invalid_metadata() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("object.dir");
        fs::write(
            &path,
            r#"[{"relpath":"..\\escape","md5":"00000000000000000000000000000000","size":"7"}]"#,
        )
        .expect("manifest");
        assert!(read_dir_manifest(&path).is_err());

        fs::write(
            &path,
            r#"[{"relpath":"member","md5":"00000000000000000000000000000000","size":7,"isexec":"yes"}]"#,
        )
        .expect("manifest");
        assert!(read_dir_manifest(&path).is_err());
    }

    #[test]
    fn cache_directory_reconstructs_members_not_manifest_bytes() {
        let temp = TempDir::new().expect("tempdir");
        let cache_root = temp.path().join("cache/files/md5");
        let member = b"payload";
        let member_md5 = hex_bytes(&Md5::digest(member));
        let manifest =
            format!(r#"[{{"relpath":"nested/file.txt","md5":"{member_md5}","size":7}}]"#);
        let manifest_md5 = hex_bytes(&Md5::digest(manifest.as_bytes()));
        let manifest_path = cache_root
            .join(&manifest_md5[..2])
            .join(format!("{}.dir", &manifest_md5[2..]));
        fs::create_dir_all(manifest_path.parent().unwrap()).expect("manifest parent");
        fs::write(&manifest_path, manifest).expect("manifest");
        let member_path = cache_root.join(&member_md5[..2]).join(&member_md5[2..]);
        fs::create_dir_all(member_path.parent().unwrap()).expect("member parent");
        fs::write(member_path, member).expect("member");

        let destination = temp.path().join("reconstructed");
        assert_eq!(
            materialize_cached_directory(&manifest_path, &destination).unwrap(),
            7
        );
        assert_eq!(
            fs::read(destination.join("nested/file.txt")).unwrap(),
            member
        );
        assert!(!destination.join("manifest").exists());
    }
}
