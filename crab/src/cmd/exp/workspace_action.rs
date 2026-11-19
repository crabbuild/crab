use super::*;
use crab_workflow::results::{AttachmentKind, Availability, LocalResults, SealedResult};

pub(super) struct PreparedSnapshot {
    pub(super) directory: tempfile::TempDir,
    pub(super) manifest: ExpWorkspaceManifest,
}

pub(super) fn prepare(
    repo_root: &Path,
    id: &ExperimentId,
    digest: Option<&str>,
    command: &str,
) -> Result<PreparedSnapshot> {
    let source = workspace_dir_path(repo_root, id);
    let metadata = fs::symlink_metadata(&source).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CrabError::Configuration {
                key: command.into(),
                origin: format!(
                    "experiment {id} has no workspace snapshot; rerun it with a Crab version that records apply snapshots"
                ),
            }
        } else {
            CrabError::Io(error)
        }
    })?;
    if !metadata.file_type().is_dir() {
        return Err(CrabError::CorruptObject {
            path: source.display().to_string(),
            reason: "experiment workspace must be a directory".into(),
        });
    }
    let manifest = read_workspace_manifest_for_command(repo_root, id, command)?;
    for path in &manifest.deleted {
        ensure_safe_workspace_relpath(path)?;
    }
    let directory = tempfile::Builder::new()
        .prefix("prepared-exp-")
        .tempdir_in(repo_root.join(".crab"))
        .map_err(CrabError::Io)?;
    // Workspace actions consume this owned copy. Verifying the mutable cache
    // then reading it again during Apply could restore different artifact bytes.
    copy_workspace_tree(&source, &source, directory.path())?;
    if let Some(digest) = digest {
        let selected = LocalResults::new(repo_root)?.run_at(&id.to_string(), digest)?;
        verify_outputs(directory.path(), &selected)?;
        let metadata = read_local_metadata(repo_root, id)?;
        validate_result_source(repo_root, &metadata, Some(digest))?;
    }
    Ok(PreparedSnapshot {
        directory,
        manifest,
    })
}

pub(super) fn validate_target(prepared: &PreparedSnapshot, target: &Path) -> Result<()> {
    for relative in &prepared.manifest.deleted {
        let path = target.join(relative);
        if let Some(parent) = path.parent() {
            ensure_parent_not_symlink(parent, target, "experiment workspace")?;
        }
    }
    validate_target_tree(prepared.directory.path(), target, target)
}

fn validate_target_tree(source: &Path, target: &Path, trusted_root: &Path) -> Result<()> {
    for entry in fs::read_dir(source).map_err(CrabError::Io)? {
        let entry = entry.map_err(CrabError::Io)?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path).map_err(CrabError::Io)?;
        if metadata.is_dir() {
            ensure_parent_not_symlink(&target_path, trusted_root, "experiment workspace")?;
            validate_target_tree(&source_path, &target_path, trusted_root)?;
        } else if let Some(parent) = target_path.parent() {
            ensure_parent_not_symlink(parent, trusted_root, "experiment workspace")?;
        }
    }
    Ok(())
}

fn verify_outputs(root: &Path, selected: &SealedResult) -> Result<()> {
    let mut witnesses = BTreeMap::new();
    for stage in &selected.snapshot.stages {
        for output in &stage.outputs {
            witnesses.insert(output.path.as_str(), (output.hash.as_str(), output.size));
        }
    }
    // Retained observations describe the final workspace. A later stage can
    // change an earlier output; unretained outputs still use their stage witness.
    for attachment in &selected.snapshot.attachments {
        if attachment.state == Availability::Present
            && (matches!(
                attachment.kind,
                AttachmentKind::Metric
                    | AttachmentKind::Plot
                    | AttachmentKind::Image
                    | AttachmentKind::Output
            ) || selected.snapshot.status == "saved"
                && matches!(
                    attachment.kind,
                    AttachmentKind::Input | AttachmentKind::Workflow
                ))
            && let Some(hash) = attachment.hash.as_deref()
        {
            witnesses.insert(&attachment.path, (hash, attachment.size));
        }
    }
    let root = root.canonicalize().map_err(CrabError::Io)?;
    for (relative, (expected_hash, expected_size)) in witnesses {
        ensure_safe_workspace_relpath(Path::new(relative))?;
        let path = root.join(relative);
        let metadata = fs::symlink_metadata(&path).map_err(CrabError::Io)?;
        if !path
            .canonicalize()
            .map_err(CrabError::Io)?
            .starts_with(&root)
            || metadata.file_type().is_symlink()
        {
            return Err(CrabError::CorruptObject {
                path: relative.into(),
                reason: "selected result output escapes the prepared workspace".into(),
            });
        }
        let (actual_hash, actual_size) = if metadata.is_dir() {
            let tree = crab_workflow::hasher::hash_directory(&path, true)?;
            (
                blake3::Hash::from(tree.hash).to_hex().to_string(),
                tree.manifest.iter().map(|entry| entry.size).sum(),
            )
        } else if metadata.is_file() {
            let file = fs::File::open(&path).map_err(CrabError::Io)?;
            let mut hash = blake3::Hasher::new();
            hash.update_reader(file).map_err(CrabError::Io)?;
            (hash.finalize().to_hex().to_string(), metadata.len())
        } else {
            return Err(CrabError::CorruptObject {
                path: relative.into(),
                reason: "selected result output is not a regular file or directory".into(),
            });
        };
        if actual_hash != expected_hash || actual_size != expected_size {
            return Err(CrabError::CorruptObject {
                path: relative.into(),
                reason: "experiment workspace bytes do not match the selected result".into(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_restore_rejects_symlink_parents_without_changing_external_files() {
        let target = PathBuf::from(std::env::var_os("CARGO_TARGET_DIR").unwrap())
            .canonicalize()
            .unwrap();
        let fixture = tempfile::tempdir_in(target).unwrap();
        let workspace = fixture.path().join("workspace");
        let outside = fixture.path().join("outside");
        let state = fixture.path().join("state");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&outside).unwrap();
        let bytes = b"checkpoint model";
        let hash = blake3::hash(bytes).to_hex().to_string();
        let object = state.join("objects").join(&hash);
        fs::create_dir_all(&object).unwrap();
        fs::write(object.join("payload"), bytes).unwrap();
        fs::write(outside.join("model.bin"), b"keep outside model").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, workspace.join("data")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&outside, workspace.join("data")).unwrap();
        let record = CheckpointRecord {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            id: "checkpoint".into(),
            experiment: "exp".into(),
            stage: "train".into(),
            sequence: 0,
            parent: None,
            request_nonce: None,
            stage_hash: format!("b3:{}", "ab".repeat(32)),
            created_at_unix_ms: 0,
            outputs: BTreeMap::from([("data/model.bin".into(), format!("b3:{hash}"))]),
            metrics: BTreeMap::new(),
            terminal: false,
            resumable: true,
        };
        CheckpointLineage::default().append(record.clone()).unwrap();
        let ordinary = fixture.path().join("ordinary");
        fs::create_dir(&ordinary).unwrap();
        apply_checkpoint_record_to(&state, &ordinary, &record).unwrap();
        assert_eq!(fs::read(ordinary.join("data/model.bin")).unwrap(), bytes);
        let restored = apply_checkpoint_record_to(&state, &workspace, &record);
        assert_eq!(
            (
                restored.is_err(),
                fs::read(outside.join("model.bin")).unwrap()
            ),
            (true, b"keep outside model".to_vec()),
        );
    }
}
