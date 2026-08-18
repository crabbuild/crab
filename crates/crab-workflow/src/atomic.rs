use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::{Result, WorkflowError};

/// Replace a file from a sibling temporary path without following links.
pub(crate) fn replace_file(temporary: &Path, destination: &Path) -> Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(temporary, destination).map_err(WorkflowError::Io)?;
        Ok(())
    }

    #[cfg(windows)]
    {
        let parent = destination.parent().ok_or_else(|| {
            WorkflowError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "destination has no parent",
            ))
        })?;
        let exists = match fs::symlink_metadata(destination) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(WorkflowError::Io(error)),
        };
        let backup = exists.then(|| {
            parent.join(format!(
                ".{}.backup-{}",
                destination
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("workflow"),
                uuid::Uuid::now_v7()
            ))
        });
        if let Some(backup) = &backup {
            if let Err(error) = fs::rename(destination, backup) {
                let _ = fs::remove_file(temporary);
                return Err(WorkflowError::Io(error));
            }
        }
        if let Err(error) = fs::rename(temporary, destination) {
            if let Some(backup) = &backup {
                let _ = fs::rename(backup, destination);
            }
            let _ = fs::remove_file(temporary);
            return Err(WorkflowError::Io(error));
        }
        if let Some(backup) = backup {
            if let Err(error) = fs::remove_file(&backup) {
                tracing::warn!(path = %backup.display(), error = %error, "atomic replacement backup cleanup deferred");
            }
        }
        Ok(())
    }
}

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
