use tokio_util::sync::CancellationToken;

use super::{Directory, DirectoryStream, Path, component_name, io};
use crate::clean::{EntryKind, entry_kind};
use crate::{CacheCleanReport, CacheError, Result};

pub(in crate::private_fs) fn clean(
    directory: &Directory,
    dry_run: bool,
    cancel: &CancellationToken,
    remove: &mut crate::private_fs::RemovePayload<'_>,
) -> Result<CacheCleanReport> {
    let mut report = CacheCleanReport {
        dry_run,
        ..Default::default()
    };
    check_cancelled(cancel)?;
    clean_directory(directory, Path::new(""), &mut report, cancel, remove)?;
    Ok(report)
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(CacheError::Cancelled)
    } else {
        Ok(())
    }
}

fn clean_directory(
    directory: &Directory,
    relative: &Path,
    report: &mut CacheCleanReport,
    cancel: &CancellationToken,
    remove: &mut crate::private_fs::RemovePayload<'_>,
) -> Result<()> {
    // Fixed layout depth bounds both descriptors and memory. Unknown subtrees
    // are never traversed, so a mirror/workspace cannot become a cleanup target.
    let mut stream = DirectoryStream::new(directory)?;
    while let Some(name) = stream.next_name()? {
        check_cancelled(cancel)?;
        let relative = relative.join(&name);
        let result = match entry_kind(&relative) {
            EntryKind::Retain => {
                report.retained_entries += 1;
                continue;
            }
            EntryKind::Directory => directory
                .child(&name, false)
                .and_then(|child| clean_directory(&child, &relative, report, cancel, remove)),
            EntryKind::Payload | EntryKind::Temporary => {
                let name = component_name(&name)?;
                // Cancellation is checked again immediately before mutation.
                check_cancelled(cancel)?;
                remove(&relative, &mut |identity: Option<
                    &crate::private_fs::FileIdentity,
                >| {
                    check_cancelled(cancel)?;
                    // A temporary name is a candidate, never deletion authority.
                    // Its catalog must supply the exact captured file identity.
                    if identity.is_none() && matches!(entry_kind(&relative), EntryKind::Temporary) {
                        return Ok(None);
                    }
                    directory.remove_payload_if(&name, report.dry_run, &mut |file| {
                        identity.map_or(Ok(true), |identity| identity.matches(file))
                    })
                })
                .map(|removed| {
                    if let Some(bytes) = removed {
                        report.files_removed += 1;
                        report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(bytes);
                    } else {
                        report.retained_entries += 1;
                    }
                })
            }
        };
        match result {
            Ok(()) => {}
            Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                report.busy_entries += 1
            }
            Err(CacheError::UnsafeRoot { .. }) => report.unsafe_entries += 1,
            Err(error) => return Err(error),
        }
    }
    check_cancelled(cancel)
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::TemporaryFile;
    use super::*;

    #[test]
    fn swapped_root_cannot_redirect_cleanup_of_pinned_tree() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let relative = format!("chunks/ab/{}", "ab".repeat(32));
        TemporaryFile::new(&root, &root.join(&relative))
            .unwrap()
            .commit()
            .unwrap();
        let pinned = Directory::root(&root, false).unwrap();
        let moved = temp.path().join("moved");
        std::fs::rename(&root, &moved).unwrap();
        TemporaryFile::new(&root, &root.join(&relative))
            .unwrap()
            .commit()
            .unwrap();
        let mut report = CacheCleanReport::default();
        clean_directory(
            &pinned,
            Path::new(""),
            &mut report,
            &CancellationToken::new(),
            &mut |_, operation| operation(None),
        )
        .unwrap();
        assert_eq!(report.files_removed, 1);
        assert!(root.join(&relative).exists());
        assert!(!moved.join(relative).exists());
    }

    #[test]
    fn swapped_namespace_parent_cannot_redirect_payload_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let relative = format!(
            "namespaces/{}/chunks/ab/{}",
            "cd".repeat(32),
            "ab".repeat(32)
        );
        TemporaryFile::new(&root, &root.join(&relative))
            .unwrap()
            .commit()
            .unwrap();
        let pinned = Directory::root(&root, false).unwrap();
        let moved = temp.path().join("moved");
        std::fs::rename(&root, &moved).unwrap();
        TemporaryFile::new(&root, &root.join(&relative))
            .unwrap()
            .commit()
            .unwrap();
        let report = clean(
            &pinned,
            false,
            &CancellationToken::new(),
            &mut |_, operation| operation(None),
        )
        .unwrap();
        assert_eq!(report.files_removed, 1);
        assert!(root.join(&relative).exists());
        assert!(!moved.join(relative).exists());
    }
}
