use tokio_util::sync::CancellationToken;

use super::{Directory, DirectoryStream, Path, component_name, io};
use crate::clean::{EntryKind, entry_kind};
use crate::{CacheCleanReport, CacheError, Result};

pub(in crate::private_fs) fn clean(
    root: &Path,
    dry_run: bool,
    cancel: &CancellationToken,
) -> Result<CacheCleanReport> {
    let mut report = CacheCleanReport {
        dry_run,
        ..Default::default()
    };
    check_cancelled(cancel)?;
    let directory = match Directory::root(root, false) {
        Ok(directory) => directory,
        Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(error) => return Err(error),
    };
    clean_directory(&directory, Path::new(""), &mut report, cancel)?;
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
                .and_then(|child| clean_directory(&child, &relative, report, cancel)),
            EntryKind::Payload => {
                let name = component_name(&name)?;
                // Cancellation is checked again immediately before mutation.
                check_cancelled(cancel)?;
                directory
                    .remove_payload(&name, report.dry_run)
                    .map(|bytes| {
                        report.files_removed += 1;
                        report.bytes_reclaimed = report.bytes_reclaimed.saturating_add(bytes);
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

#[cfg(test)]
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
        )
        .unwrap();
        assert_eq!(report.files_removed, 1);
        assert!(root.join(&relative).exists());
        assert!(!moved.join(relative).exists());
    }
}
