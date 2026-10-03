use std::path::{Path, PathBuf};

use crab_sdk::operation::{Options as OperationOptions, ReadLimits};
use crab_sdk::remote::EntryMode;
use crab_sdk::remote::write::{CommitIdentity, CommitOptions, FileEdit, MutationOutcome};
use crab_sdk::storage::{ContentCache, DirectStoreOptions};
use crab_sdk::{Client, GitPath, RepositoryLocator, Revision};

const LISTING_OBJECTS: usize = 10_000;

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
    let [bucket, repository, scratch, large, pointer, listing] = args.as_slice() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage: s3_gateway_fixture BUCKET REPOSITORY SCRATCH LARGE_FILE POINTER_FILE LISTING_DIRECTORY",
        )
        .into());
    };
    let scratch = absolute_directory(scratch, "SCRATCH")?;
    let large = PathBuf::from(large);
    let pointer = PathBuf::from(pointer);
    let listing = PathBuf::from(listing);
    if !large.is_absolute() || !large.is_file() || !pointer.is_absolute() || !listing.is_absolute()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "fixture input and output paths must be absolute",
        )
        .into());
    }

    let cache = scratch.join("content-cache");
    std::fs::create_dir_all(&cache)?;
    let client = Client::builder()
        .direct_store(DirectStoreOptions::s3_from_env(bucket)?)
        .content_cache(ContentCache::new(&cache, 512 * 1024 * 1024)?)
        .build()?;
    let result = publish(
        &client,
        RepositoryLocator::new(repository)?,
        &scratch,
        &large,
        &pointer,
        &listing,
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

async fn publish(
    client: &Client,
    locator: RepositoryLocator,
    scratch: &Path,
    large: &Path,
    pointer: &Path,
    listing: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let operation = OperationOptions::default().with_limits(ReadLimits {
        max_logical_objects: 100_000,
        max_storage_requests: 100_000,
        ..ReadLimits::default()
    })?;
    client
        .initialize_remote(locator.clone(), "refs/heads/main")
        .await?;
    let identity = CommitIdentity::new(
        "S3 gateway qualification",
        "qualification@example.invalid",
        1_700_000_000,
        0,
    )?;
    let large_size = tokio::fs::metadata(large).await?.len();
    let repository = client
        .open(crab_sdk::OpenOptions::remote(locator.clone()))
        .await?;
    if !repository.remote()?.refs().await?.entries().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "repository must have no refs",
        )
        .into());
    }
    let initial = repository
        .remote()?
        .prepare_commit(
            CommitOptions::initial(
                "refs/heads/main",
                identity.clone(),
                identity.clone(),
                b"add Xet range qualification fixture\n".to_vec(),
            )?,
            vec![FileEdit::hydrated(
                GitPath::new("qualification/xet-large.bin")?,
                EntryMode::Regular,
                large_size,
                tokio::fs::File::open(large).await?,
            )?],
            scratch.to_owned(),
        )
        .with_options(operation.clone())
        .await?;
    let first = initial
        .commit_id()
        .ok_or("prepared commit has no identity")?;
    committed(initial.execute().with_options(operation.clone()).await?)?;

    let repository = client
        .open(crab_sdk::OpenOptions::remote(locator.clone()))
        .await?;
    let snapshot = repository
        .remote()?
        .snapshot(Revision::commit(first))
        .await?;
    let pointer_bytes = snapshot
        .read_blob(GitPath::new("qualification/xet-large.bin")?)
        .await?;
    if !pointer_bytes.starts_with(b"version https://crab.build/spec/v1\n") {
        return Err("SDK did not commit a Crab pointer for the large fixture".into());
    }
    if let Some(parent) = pointer.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(pointer, &pointer_bytes)?;
    write_listing_files(listing, &pointer_bytes)?;

    let mut edits = Vec::with_capacity(LISTING_OBJECTS + 33);
    edits.push(FileEdit::git(
        GitPath::new("qualification/xet-duplicate.bin")?,
        EntryMode::Regular,
        pointer_bytes.len() as u64,
        std::io::Cursor::new(pointer_bytes.clone()),
    )?);
    for index in 0..LISTING_OBJECTS {
        edits.push(pointer_edit(
            format!("qualification/listing/flat-{index:05}.pointer"),
            &pointer_bytes,
        )?);
    }
    for group in ["group-a", "group-b"] {
        for index in 0..16 {
            edits.push(pointer_edit(
                format!("qualification/listing/{group}/item-{index:02}.pointer"),
                &pointer_bytes,
            )?);
        }
    }
    let final_commit = client
        .open(crab_sdk::OpenOptions::remote(locator))
        .await?
        .remote()?
        .prepare_commit(
            CommitOptions::new(
                first,
                "refs/heads/main",
                Some(first),
                identity.clone(),
                identity,
                b"add projected Xet listing fixtures\n".to_vec(),
            )?,
            edits,
            scratch.to_owned(),
        )
        .with_options(operation.clone())
        .await?;
    committed(final_commit.execute().with_options(operation).await?)?;
    Ok(())
}

fn pointer_edit(path: String, pointer: &[u8]) -> Result<FileEdit, crab_sdk::Error> {
    FileEdit::git(
        GitPath::new(path)?,
        EntryMode::Regular,
        pointer.len() as u64,
        std::io::Cursor::new(pointer.to_vec()),
    )
}

fn write_listing_files(root: &Path, pointer: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    for index in 0..LISTING_OBJECTS {
        std::fs::write(root.join(format!("flat-{index:05}.pointer")), pointer)?;
    }
    for group in ["group-a", "group-b"] {
        let directory = root.join(group);
        std::fs::create_dir_all(&directory)?;
        for index in 0..16 {
            std::fs::write(directory.join(format!("item-{index:02}.pointer")), pointer)?;
        }
    }
    Ok(())
}

fn absolute_directory(value: &str, label: &str) -> Result<PathBuf, std::io::Error> {
    let path = PathBuf::from(value);
    if path.is_absolute() && path.is_dir() {
        return Ok(path);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("{label} must be an existing absolute directory"),
    ))
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
