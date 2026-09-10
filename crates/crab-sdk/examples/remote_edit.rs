use std::path::{Path, PathBuf};

use crab_sdk::remote::EntryMode;
use crab_sdk::remote::write::{
    CommitIdentity, CommitOptions, FileEdit, MutationOutcome, RefBatch, RefUpdate,
};
use crab_sdk::storage::{ContentCache, DirectStoreOptions};
use crab_sdk::{Client, GitPath, RepositoryLocator, Revision};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args_os()
        .skip(1)
        .map(|value| {
            value.into_string().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "arguments must be UTF-8")
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let [bucket, repository, scratch, text, large] = args.as_slice() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage: remote_edit BUCKET REPOSITORY SCRATCH TEXT_FILE LARGE_FILE",
        )
        .into());
    };
    let scratch = PathBuf::from(scratch);
    if !scratch.is_absolute() || !scratch.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "SCRATCH must be an existing absolute directory",
        )
        .into());
    }
    let cache = scratch.join("content-cache");
    std::fs::create_dir_all(&cache)?;
    let client = Client::builder()
        .direct_store(DirectStoreOptions::s3_from_env(bucket)?)
        .content_cache(ContentCache::new(&cache, 512 * 1024 * 1024)?)
        .build()?;
    let result = run(
        &client,
        RepositoryLocator::new(repository)?,
        scratch,
        Path::new(text),
        Path::new(large),
    )
    .await;
    let close = client.close().await;
    match (result, close) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Err(close)) => Err(format!("{error}; client close failed: {close}").into()),
    }
}

async fn run(
    client: &Client,
    locator: RepositoryLocator,
    scratch: PathBuf,
    text: &Path,
    large: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    client
        .initialize_remote(locator.clone(), "refs/heads/main")
        .await?;
    let repository = client
        .open(crab_sdk::OpenOptions::remote(locator.clone()))
        .await?;
    let remote = repository.remote()?;
    if !remote.refs().await?.entries().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "repository must have no refs",
        )
        .into());
    }
    let identity =
        CommitIdentity::new("Crab SDK example", "sdk@example.invalid", 1_700_000_000, 0)?;
    let text_size = tokio::fs::metadata(text).await?.len();
    let large_size = tokio::fs::metadata(large).await?.len();
    let large_digest = file_digest(large).await?;
    let initial = remote
        .prepare_commit(
            CommitOptions::initial(
                "refs/heads/main",
                identity.clone(),
                identity.clone(),
                b"initial remote commit\n".to_vec(),
            )?,
            vec![
                FileEdit::git(
                    GitPath::new("README.md")?,
                    EntryMode::Regular,
                    text_size,
                    tokio::fs::File::open(text).await?,
                )?,
                FileEdit::hydrated(
                    GitPath::new("large.bin")?,
                    EntryMode::Executable,
                    large_size,
                    tokio::fs::File::open(large).await?,
                )?,
                FileEdit::git(
                    GitPath::new("obsolete.txt")?,
                    EntryMode::Regular,
                    9,
                    std::io::Cursor::new(b"obsolete\n".to_vec()),
                )?,
            ],
            scratch.clone(),
        )
        .await?;
    let first = initial
        .commit_id()
        .ok_or("prepared commit has no identity")?;
    committed(initial.execute().await?)?;

    let repository = client
        .open(crab_sdk::OpenOptions::remote(locator.clone()))
        .await?;
    let tag = repository
        .remote()?
        .prepare_ref_update(
            RefBatch::new(vec![RefUpdate::create("refs/tags/v1", first)?])?,
            scratch.clone(),
        )
        .await?;
    committed(tag.execute().await?)?;

    let updated = b"updated remotely\n".to_vec();
    let repository = client
        .open(crab_sdk::OpenOptions::remote(locator.clone()))
        .await?;
    let second = repository
        .remote()?
        .prepare_commit(
            CommitOptions::new(
                first,
                "refs/heads/main",
                Some(first),
                identity.clone(),
                identity,
                b"update and delete\n".to_vec(),
            )?,
            vec![
                FileEdit::git(
                    GitPath::new("README.md")?,
                    EntryMode::Regular,
                    updated.len() as u64,
                    std::io::Cursor::new(updated),
                )?,
                FileEdit::delete(GitPath::new("obsolete.txt")?)?,
            ],
            scratch.clone(),
        )
        .await?;
    let second_id = second
        .commit_id()
        .ok_or("prepared commit has no identity")?;
    committed(second.execute().await?)?;

    let repository = client
        .open(crab_sdk::OpenOptions::remote(locator.clone()))
        .await?;
    let delete_tag = repository
        .remote()?
        .prepare_ref_update(
            RefBatch::new(vec![RefUpdate::delete("refs/tags/v1", first)?])?,
            scratch,
        )
        .await?;
    committed(delete_tag.execute().await?)?;

    let repository = client.open(crab_sdk::OpenOptions::remote(locator)).await?;
    let snapshot = repository
        .remote()?
        .snapshot(Revision::commit(second_id))
        .await?;
    if snapshot
        .read_blob(GitPath::new("README.md")?)
        .await?
        .as_ref()
        != b"updated remotely\n"
    {
        return Err("remote read did not return the committed text".into());
    }
    let mut content = snapshot.open_file(GitPath::new("large.bin")?).await?;
    let mut hydrated_digest = Sha256::new();
    let mut hydrated_size = 0u64;
    while let Some(bytes) = content.next().await? {
        hydrated_size += bytes.len() as u64;
        hydrated_digest.update(&bytes);
    }
    let hydrated_digest: [u8; 32] = hydrated_digest.finalize().into();
    if hydrated_size != large_size || hydrated_digest != large_digest {
        return Err("hydrated read did not match the committed large file".into());
    }
    println!("initial={first} updated={second_id} large_bytes={large_size}");
    Ok(())
}

async fn file_digest(path: &Path) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().into())
}

fn committed(outcome: MutationOutcome) -> Result<(), Box<dyn std::error::Error>> {
    match outcome {
        MutationOutcome::Committed { .. } => Ok(()),
        MutationOutcome::Rejected { reasons } => Err(format!(
            "mutation rejected: {}",
            reasons
                .first()
                .map(|reason| reason.error().to_string())
                .unwrap_or_else(|| "no reason".to_owned())
        )
        .into()),
        MutationOutcome::Indeterminate { recovery } => Err(format!(
            "mutation outcome is indeterminate; retain plan {}",
            recovery.plan_id()
        )
        .into()),
        _ => Err("SDK returned an unsupported mutation outcome".into()),
    }
}
