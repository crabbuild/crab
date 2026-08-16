use std::error::Error as StdError;
use std::io;
use std::sync::Arc;

use bytes::Bytes;
use crab_remote_git::{
    EntryKind, GitPath, OperationKind, PageRequest, RemoteGitRepository, RemoteGitRuntime,
    RepositoryOptions, Revision,
};
use crab_storage::{StorageProviderKind, StoreLayout, build_static_env_store};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn StdError>> {
    let mut args = std::env::args().skip(1);
    let bucket = required_arg(&mut args, "bucket")?;
    let repo_prefix = required_arg(&mut args, "repository prefix")?;
    let paths = args.collect::<Vec<_>>();
    if paths.is_empty() {
        return Err(io::Error::other("at least one repository path is required").into());
    }

    let store = build_static_env_store(&bucket, StorageProviderKind::S3)?;
    let identity =
        crab_remote_git::RepositoryIdentity::new(format!("s3:{bucket}"), repo_prefix.clone(), 1)?;
    let layout = StoreLayout::new(store.clone(), repo_prefix);
    let cancellation = CancellationToken::new();
    let repository = RemoteGitRepository::open(
        store,
        layout,
        identity,
        Arc::new(RemoteGitRuntime::default()),
        RepositoryOptions::default(),
        &cancellation,
    )
    .await?;
    let head = repository
        .refs()
        .head
        .as_ref()
        .ok_or_else(|| io::Error::other("repository is empty"))?;
    let revision = Revision::Reference(head.name.clone());
    let operation = repository
        .operation(OperationKind::Repository, &cancellation)
        .await?;
    let result = async {
        let resolved = repository.resolve(&revision, &operation).await?;
        let snapshot = repository.snapshot(&revision, &operation).await?;
        let root = snapshot
            .list_directory(
                &GitPath::root(),
                &PageRequest::new(10_000, None)?,
                &operation,
            )
            .await?;

        println!("generation={}", repository.generation());
        println!("head_ref={}", head.name);
        println!("head_oid={}", resolved.commit);
        println!("root_tree_oid={}", snapshot.root_tree_oid());
        println!("root_entries={}", root.items.len());

        for selector in paths {
            let path = GitPath::new(Bytes::from(selector.clone().into_bytes()))?;
            let entry = snapshot
                .entry(&path, &operation)
                .await?
                .ok_or(crab_remote_git::Error::PathNotFound)?;
            if matches!(entry.kind, EntryKind::Blob | EntryKind::Symlink) {
                let blob = snapshot.read_blob(&path, &operation).await?;
                println!(
                    "path={selector} oid={} kind={:?} bytes={}",
                    blob.metadata.oid, entry.kind, blob.metadata.git_size
                );
            } else {
                println!(
                    "path={selector} oid={} kind={:?} bytes=metadata-only",
                    entry.oid, entry.kind
                );
            }
        }
        Ok(())
    }
    .await;
    operation.finish(result).await?;
    Ok(())
}

fn required_arg(args: &mut impl Iterator<Item = String>, name: &'static str) -> io::Result<String> {
    args.next()
        .ok_or_else(|| io::Error::other(format!("missing {name} argument")))
}
