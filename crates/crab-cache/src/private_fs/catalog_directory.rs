use std::path::{Component, Path, PathBuf};

use super::PinnedRoot;
use crate::{CacheError, Result};

pub(crate) struct CatalogDirectory {
    pub(crate) directory: PinnedRoot,
    pub(crate) owner: Option<PinnedRoot>,
    pub(crate) owner_path: PathBuf,
    pub(crate) prefix: PathBuf,
}

impl CatalogDirectory {
    pub(crate) fn open(owner_path: &Path, directory: &Path) -> Result<Self> {
        let owner_path = std::path::absolute(owner_path)?;
        let directory = std::path::absolute(directory)?;
        let prefix = directory
            .strip_prefix(&owner_path)
            .map_err(|_| CacheError::UnsafeRoot {
                path: directory.display().to_string(),
                reason: "cache directory is outside its catalog owner".into(),
            })?
            .to_owned();
        if prefix.as_os_str().is_empty()
            || prefix
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(CacheError::UnsafeRoot {
                path: directory.display().to_string(),
                reason: "cache directory is not a safe catalog descendant".into(),
            });
        }
        let (directory, owner) = if prefix.components().count() == 1 {
            // Tagged standalone range maintenance permits a private leaf with
            // a nonprivate parent. Only the captured private parent owns SQL.
            PinnedRoot::open_with_private_parent(&directory)?
        } else {
            let owner = PinnedRoot::open(&owner_path)?;
            let directory = owner.subdirectory(&prefix, false)?;
            (directory, Some(owner))
        };
        Ok(Self {
            directory,
            owner,
            owner_path,
            prefix,
        })
    }
}
