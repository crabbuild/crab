//! Shared pointer inspection for mirror checks and collaboration publication.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crab_cache::lifecycle::CacheUseGuard;
use crab_types::pointer::Pointer;
use tokio_util::sync::CancellationToken;

use super::{CommandRunner, ProcessCommand, check_cancelled, run_required};
use crate::core::error::{CrabError, Result};
use crate::core::output::OutputMode;

const POINTER_SCAN_LIMITS: crab_git::walk::PointerScanLimits = crab_git::walk::PointerScanLimits {
    objects: 2_000_000,
    lookups: 8_000_000,
    allocation_bytes: 64 * 1024 * 1024,
};

pub(super) enum Source {
    Cache(Arc<CacheUseGuard>),
    // The pre-push source belongs to the user, not mirror cache cleanup.
    // Do not create cache ownership markers in their Git directory.
    Repository(PathBuf),
}

impl Source {
    fn path(&self) -> &Path {
        match self {
            Self::Cache(cache) => cache.path(),
            Self::Repository(path) => path,
        }
    }
}

pub(super) async fn collect(
    source: Source,
    refs: &BTreeMap<String, String>,
    cancel: &CancellationToken,
    runner: &mut dyn CommandRunner,
) -> Result<Vec<Pointer>> {
    check_cancelled(cancel)?;
    let refs = refs
        .iter()
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect::<Vec<_>>();
    let scan_cancel = cancel.child_token();
    let _cancel_on_drop = scan_cancel.clone().drop_guard();
    // Dropping a blocking task's awaiter does not stop that worker. Retain
    // cache ownership while queued/running, then through raw-byte verification;
    // caller drop cancels only this scan, never the caller's parent operation.
    let (source, reachable) = tokio::task::spawn_blocking(move || {
        let reachable =
            crab_git::walk::scan_pointers(source.path(), &refs, POINTER_SCAN_LIMITS, &|| {
                scan_cancel.is_cancelled()
            });
        (source, reachable)
    })
    .await
    .map_err(|source| CrabError::Io(std::io::Error::other(source)))?;
    let reachable = reachable?;
    check_cancelled(cancel)?;
    if !reachable.unchecked_blobs.is_empty() {
        // A plausible oversized header is not evidence that an object is not
        // a pointer. Stream its exact raw bytes and bind kind/size/hash before
        // allowing the candidate inventory to authorize a check or publication.
        let command = ProcessCommand::new(
            "git",
            vec![
                "--no-replace-objects".to_owned(),
                "--git-dir=.".to_owned(),
                "cat-file".to_owned(),
                "--batch".to_owned(),
            ],
        )
        .current_dir(Some(source.path()))
        .env_remove(super::GIT_ENV_REMOVALS)
        .env("GIT_NO_LAZY_FETCH", "1".into())
        // Old Git clients ignore NO_LAZY_FETCH; inspection permits no transport.
        .env("GIT_ALLOW_PROTOCOL", "".into())
        .verify_blobs(reachable.unchecked_blobs);
        run_required(runner, command, OutputMode::Json)?;
    }
    let mut pointers = BTreeMap::<[u8; 32], u64>::new();
    for pointer in reachable.pointers {
        check_cancelled(cancel)?;
        match pointers.insert(pointer.file_hash, pointer.size) {
            Some(size) if size != pointer.size => {
                return Err(CrabError::CorruptObject {
                    path: crab_types::pointer::hex_encode(&pointer.file_hash),
                    reason: format!(
                        "the same file hash is declared with sizes {size} and {}",
                        pointer.size
                    ),
                });
            }
            _ => {}
        }
    }
    Ok(pointers
        .into_iter()
        .map(|(file_hash, size)| Pointer {
            file_hash,
            size,
            shard_hint: None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::super::SystemCommandRunner;
    use super::*;
    use gix_object::Write as _;

    struct CacheCheckingRunner {
        path: PathBuf,
        reject: bool,
    }

    impl CommandRunner for CacheCheckingRunner {
        fn run(
            &mut self,
            command: &ProcessCommand,
            mode: OutputMode,
        ) -> Result<super::super::ProcessOutput> {
            assert!(!command.verify_blobs.is_empty());
            assert!(CacheUseGuard::acquire(&self.path, &CancellationToken::new()).is_err());
            let output = SystemCommandRunner::default().run(command, mode)?;
            if self.reject {
                return Err(CrabError::Protocol("verification rejected".to_owned()));
            }
            Ok(output)
        }
    }

    #[tokio::test]
    async fn raw_blob_verification_retains_cache_on_success_and_error() {
        for reject in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("scan-cache.git");
            let cancel = CancellationToken::new();
            let cache = Arc::new(CacheUseGuard::acquire(&path, &cancel).unwrap());
            let git = std::process::Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&path)
                .output()
                .unwrap();
            assert!(git.status.success());
            let odb = gix_odb::at(path.join("objects")).unwrap();
            let bytes = vec![b'x'; crab_types::pointer::MAX_POINTER_SIZE + 1];
            let oid = odb.write_buf(gix_object::Kind::Blob, &bytes).unwrap();
            let refs = BTreeMap::from([("refs/tags/blob".to_owned(), oid.to_string())]);
            let mut runner = CacheCheckingRunner {
                path: path.clone(),
                reject,
            };
            let result = collect(Source::Cache(cache), &refs, &cancel, &mut runner).await;
            assert!(
                result.is_err() == reject
                    && !cancel.is_cancelled()
                    && CacheUseGuard::acquire(&path, &cancel).is_ok(),
                "verification did not release cache correctly: reject={reject}, result={result:?}"
            );
        }
    }

    #[test]
    fn dropping_pointer_scan_retains_cache_until_queued_worker_exits() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (entered, started) = tokio::sync::oneshot::channel();
            let (release, blocked) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                entered.send(()).unwrap();
                let _ = blocked.recv();
            });
            started.await.unwrap();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("scan-cache.git");
            let cancel = CancellationToken::new();
            let cache = Arc::new(CacheUseGuard::acquire(&path, &cancel).unwrap());
            let worker_cache = Arc::downgrade(&cache);
            let caller_cancel = cancel.clone();
            let mut caller = Box::pin(async move {
                let mut runner = SystemCommandRunner::new(caller_cancel.clone());
                collect(
                    Source::Cache(cache),
                    &BTreeMap::new(),
                    &caller_cancel,
                    &mut runner,
                )
                .await
            });
            assert!(futures_util::poll!(&mut caller).is_pending());
            drop(caller);
            let held = CacheUseGuard::acquire(&path, &CancellationToken::new()).is_err();
            release.send(()).unwrap();
            blocker.await.unwrap();
            let drained = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while worker_cache.strong_count() != 0
                    || CacheUseGuard::acquire(&path, &CancellationToken::new()).is_err()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_ok();
            assert!(
                held && drained
                    && !cancel.is_cancelled()
                    && CacheUseGuard::acquire(&path, &CancellationToken::new()).is_ok(),
                "queued pointer worker lost cache ownership: held={held}, drained={drained}"
            );
        });
    }
}
