use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::{Result, WorkflowError};

/// Reject an existing symlink in a destination's parent path.
pub(crate) fn ensure_parent_not_symlink(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let mut current = PathBuf::new();
    let allow_system_ancestor_link = parent.is_absolute();
    let mut normal_components = 0_usize;
    for component in parent.components() {
        if matches!(component, Component::CurDir) {
            continue;
        }
        if matches!(component, Component::Normal(_)) {
            normal_components += 1;
        }
        current.push(component.as_os_str());
        // Windows places the root separator after the drive/UNC prefix.
        // A bare verbatim drive prefix addresses a volume and fails metadata lookup.
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if allow_system_ancestor_link && normal_components == 1 {
                    continue;
                }
                return Err(WorkflowError::YamlInvalid {
                    key: "destination_parent_symlink".to_owned(),
                    origin: current.display().to_string(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(WorkflowError::Io(error)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_roots_validate_existing_and_missing_parents() {
        let temporary = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        for parent in [root.clone(), root.join("missing").join("nested")] {
            ensure_parent_not_symlink(&parent.join("entry")).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn canonical_roots_still_reject_nested_symlinks() {
        let temporary = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temporary.path()).unwrap();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, root.join("redirect")).unwrap();
        assert!(matches!(
            ensure_parent_not_symlink(&root.join("redirect").join("entry")),
            Err(WorkflowError::YamlInvalid { key, .. }) if key == "destination_parent_symlink"
        ));
    }
}
