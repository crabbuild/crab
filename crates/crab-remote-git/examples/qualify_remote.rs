use std::error::Error as StdError;
use std::future::Future;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use crab_remote_git::{
    GitPath, HistoryTraversal, ObjectLimits, OperationKind, OperationLimits, PageRequest,
    RemoteGitRepository, RemoteGitRuntime, RepositoryOptions, Revision,
};
use crab_storage::{StorageProviderKind, StorageReadKind, StoreLayout, build_static_env_store};
use futures_util::TryStreamExt as _;
use tokio_util::sync::CancellationToken;

type QualificationResult<T> = Result<T, Box<dyn StdError>>;

#[derive(Default)]
struct ReadCounters {
    get: AtomicU64,
    get_version: AtomicU64,
    stream: AtomicU64,
    head: AtomicU64,
    range: AtomicU64,
    bytes: AtomicU64,
}

#[derive(Clone, Copy)]
struct ReadSnapshot {
    get: u64,
    get_version: u64,
    stream: u64,
    head: u64,
    range: u64,
    bytes: u64,
}

impl ReadCounters {
    fn record_request(&self, kind: StorageReadKind) {
        let counter = match kind {
            StorageReadKind::Get => &self.get,
            StorageReadKind::GetVersion => &self.get_version,
            StorageReadKind::Stream => &self.stream,
            StorageReadKind::Head => &self.head,
            StorageReadKind::Range => &self.range,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn record_bytes(&self, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn snapshot(&self) -> ReadSnapshot {
        ReadSnapshot {
            get: self.get.load(Ordering::Relaxed),
            get_version: self.get_version.load(Ordering::Relaxed),
            stream: self.stream.load(Ordering::Relaxed),
            head: self.head.load(Ordering::Relaxed),
            range: self.range.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }
}

impl ReadSnapshot {
    fn since(self, earlier: Self) -> Self {
        Self {
            get: self.get.saturating_sub(earlier.get),
            get_version: self.get_version.saturating_sub(earlier.get_version),
            stream: self.stream.saturating_sub(earlier.stream),
            head: self.head.saturating_sub(earlier.head),
            range: self.range.saturating_sub(earlier.range),
            bytes: self.bytes.saturating_sub(earlier.bytes),
        }
    }

    fn requests(self) -> u64 {
        self.get
            .saturating_add(self.get_version)
            .saturating_add(self.stream)
            .saturating_add(self.head)
            .saturating_add(self.range)
    }
}

#[tokio::main]
async fn main() -> QualificationResult<()> {
    let mut args = std::env::args().skip(1);
    let bucket = required_arg(&mut args, "bucket")?;
    let repo_prefix = required_arg(&mut args, "repository prefix")?;
    let selected_path = GitPath::new(Bytes::from(
        required_arg(&mut args, "path changed by HEAD")?.into_bytes(),
    ))?;
    if args.next().is_some() {
        return Err(io::Error::other("unexpected extra argument").into());
    }

    let counters = Arc::new(ReadCounters::default());
    let request_counters = Arc::clone(&counters);
    let byte_counters = Arc::clone(&counters);
    let store = build_static_env_store(&bucket, StorageProviderKind::S3)?
        .with_read_request_observer(Arc::new(move |kind| {
            request_counters.record_request(kind);
        }))
        .with_read_byte_observer(Arc::new(move |bytes| {
            byte_counters.record_bytes(bytes);
        }));
    let layout = StoreLayout::new(store.clone(), repo_prefix.clone());
    let identity =
        crab_remote_git::RepositoryIdentity::new(format!("s3:{bucket}"), repo_prefix, 1)?;
    let runtime = Arc::new(RemoteGitRuntime::default());
    let cancellation = CancellationToken::new();
    let repository_options = RepositoryOptions::new(
        ObjectLimits::default(),
        OperationLimits {
            max_logical_objects: 50_000,
            max_storage_requests: 100_000,
            ..OperationLimits::default()
        },
    )?;

    let repository = measured("open", &counters, async {
        RemoteGitRepository::open(
            store,
            layout,
            identity,
            Arc::clone(&runtime),
            repository_options,
            &cancellation,
        )
        .await
    })
    .await?;
    let head = repository
        .refs()
        .head
        .as_ref()
        .ok_or_else(|| io::Error::other("repository is empty"))?;
    let revision = Revision::Reference(head.name.clone());

    let snapshot = measured("snapshot", &counters, async {
        let operation = repository
            .operation(OperationKind::Snapshot, &cancellation)
            .await?;
        let result = repository.snapshot(&revision, &operation).await;
        operation.finish(result).await
    })
    .await?;
    let commit = measured("commit", &counters, async {
        let operation = repository
            .operation(OperationKind::Commit, &cancellation)
            .await?;
        let result = snapshot.commit(&operation).await;
        operation.finish(result).await
    })
    .await?;
    let root = measured("root-cold", &counters, async {
        let operation = repository
            .operation(OperationKind::Tree, &cancellation)
            .await?;
        let result = snapshot
            .list_directory(
                &GitPath::root(),
                &PageRequest::new(1_000, None)?,
                &operation,
            )
            .await;
        operation.finish(result).await
    })
    .await?;
    let warm_root = measured("root-warm", &counters, async {
        let operation = repository
            .operation(OperationKind::Tree, &cancellation)
            .await?;
        let result = snapshot
            .list_directory(
                &GitPath::root(),
                &PageRequest::new(1_000, None)?,
                &operation,
            )
            .await;
        operation.finish(result).await
    })
    .await?;
    let blob = measured("blob", &counters, async {
        let operation = repository
            .operation(OperationKind::Content, &cancellation)
            .await?;
        let result = snapshot.read_blob(&selected_path, &operation).await;
        operation.finish(result).await
    })
    .await?;
    let warm_blob = measured("blob-warm", &counters, async {
        let operation = repository
            .operation(OperationKind::Content, &cancellation)
            .await?;
        let result = snapshot.read_blob(&selected_path, &operation).await;
        operation.finish(result).await
    })
    .await?;
    let history = measured("history", &counters, async {
        let operation = repository
            .operation(OperationKind::History, &cancellation)
            .await?;
        let result = snapshot
            .history(
                HistoryTraversal::FirstParent,
                &PageRequest::new(20, None)?,
                &operation,
            )
            .await;
        operation.finish(result).await
    })
    .await?;
    let path_history = measured("path-history", &counters, async {
        let operation = repository
            .operation(OperationKind::PathHistory, &cancellation)
            .await?;
        let result = snapshot
            .path_history(
                &selected_path,
                HistoryTraversal::FirstParent,
                &PageRequest::new(1, None)?,
                &operation,
            )
            .await;
        operation.finish(result).await
    })
    .await?;

    let parent = commit
        .parents
        .first()
        .copied()
        .ok_or_else(|| io::Error::other("HEAD has no parent for comparison"))?;
    let base = measured("base-snapshot", &counters, async {
        let operation = repository
            .operation(OperationKind::Snapshot, &cancellation)
            .await?;
        let result = repository
            .snapshot(&Revision::Commit(parent), &operation)
            .await;
        operation.finish(result).await
    })
    .await?;
    let comparison = measured("compare", &counters, async {
        let operation = repository
            .operation(OperationKind::Compare, &cancellation)
            .await?;
        let result = snapshot.compare(&base, &operation).await;
        operation.finish(result).await
    })
    .await?;
    let diff = measured("diff", &counters, async {
        let operation = repository
            .operation(OperationKind::Diff, &cancellation)
            .await?;
        let result = snapshot.diff(&base, &selected_path, &operation).await;
        operation.finish(result).await
    })
    .await?;
    let blame = measured("blame", &counters, async {
        let operation = repository
            .operation(OperationKind::Blame, &cancellation)
            .await?;
        let result = snapshot.blame(&selected_path, &operation).await;
        operation.finish(result).await
    })
    .await?;
    let (archive_entries, archive_bytes) = measured("archive", &counters, async {
        let operation = repository
            .operation(OperationKind::Archive, &cancellation)
            .await?;
        let mut stream = snapshot.archive_stream(operation)?;
        let mut entries = 0u64;
        let mut bytes = 0u64;
        while let Some(entry) = stream.try_next().await? {
            entries = entries.saturating_add(1);
            bytes = bytes.saturating_add(entry.bytes.map_or(0, |value| value.len() as u64));
        }
        Ok((entries, bytes))
    })
    .await?;

    let runtime_snapshot = runtime.snapshot().await;
    println!("generation={}", repository.generation());
    println!("pack_count={}", repository.pack_count());
    println!("head_ref={}", head.name);
    println!("head_oid={}", snapshot.commit_oid());
    println!("parent_oid={parent}");
    println!("root_entries={}", root.items.len());
    println!("warm_root_entries={}", warm_root.items.len());
    println!("blob_oid={}", blob.metadata.oid);
    println!("blob_bytes={}", blob.bytes.len());
    println!("warm_blob_bytes={}", warm_blob.bytes.len());
    println!("history_entries={}", history.items.len());
    println!("path_history_entries={}", path_history.items.len());
    println!("comparison_changes={}", comparison.changes.len());
    println!("diff_classification={:?}", diff.classification);
    println!("diff_hunks={}", diff.hunks.len());
    println!("blame_ranges={}", blame.ranges.len());
    println!("archive_entries={archive_entries}");
    println!("archive_blob_bytes={archive_bytes}");
    println!("runtime_object_entries={}", runtime_snapshot.object_entries);
    println!("runtime_object_bytes={}", runtime_snapshot.object_bytes);
    println!(
        "runtime_pack_index_bytes={}",
        runtime_snapshot.pack_index_bytes
    );
    println!("runtime_parsed_bytes={}", runtime_snapshot.parsed_bytes);
    runtime.shutdown().await;
    Ok(())
}

async fn measured<T, F>(
    name: &str,
    counters: &ReadCounters,
    operation: F,
) -> crab_remote_git::Result<T>
where
    F: Future<Output = crab_remote_git::Result<T>>,
{
    let before = counters.snapshot();
    let started = Instant::now();
    let result = operation.await;
    let elapsed = started.elapsed();
    let reads = counters.snapshot().since(before);
    println!(
        "operation={name} outcome={} elapsed_ms={} store_requests={} get={} get_version={} stream={} head={} range={} store_bytes={}",
        if result.is_ok() { "ok" } else { "error" },
        duration_millis(elapsed),
        reads.requests(),
        reads.get,
        reads.get_version,
        reads.stream,
        reads.head,
        reads.range,
        reads.bytes,
    );
    result
}

fn duration_millis(duration: Duration) -> u128 {
    duration.as_micros().div_ceil(1_000)
}

fn required_arg(args: &mut impl Iterator<Item = String>, name: &'static str) -> io::Result<String> {
    args.next()
        .ok_or_else(|| io::Error::other(format!("missing {name} argument")))
}
