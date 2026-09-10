use crab_sdk::storage::DirectStoreOptions;
use crab_sdk::{Client, GitPath, RepositoryLocator, Revision};
use std::io::Write;

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
    let [bucket, repository, branch, path, mode, cache @ ..] = args.as_slice() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage: remote_read BUCKET REPOSITORY BRANCH PATH git|hydrated [CACHE_DIRECTORY [CACHE_BYTES]]",
        )
        .into());
    };
    if !matches!(mode.as_str(), "git" | "hydrated")
        || cache.len() > 2
        || (mode == "git" && !cache.is_empty())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid content mode or cache arguments",
        )
        .into());
    }
    let locator = RepositoryLocator::new(repository)?;
    let revision = Revision::branch(branch)?;
    let path = GitPath::new(path.as_bytes().to_vec())?;
    let builder = Client::builder().direct_store(DirectStoreOptions::s3_from_env(bucket)?);
    #[cfg(feature = "content")]
    let builder = match cache {
        [directory, rest @ ..] => builder.content_cache(crab_sdk::storage::ContentCache::new(
            std::path::Path::new(directory),
            rest.first()
                .map(|value| value.parse::<u64>())
                .transpose()?
                .unwrap_or(512 * 1024 * 1024),
        )?),
        _ => builder,
    };
    #[cfg(not(feature = "content"))]
    if !cache.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cache configuration requires the content feature",
        )
        .into());
    }
    let client = builder.build()?;
    let result = async {
        let repository = client.open(crab_sdk::OpenOptions::remote(locator)).await?;
        let snapshot = repository.remote()?.snapshot(revision).await?;
        let (size, hash) = if mode == "git" {
            let bytes = snapshot.read_blob(path).await?;
            (bytes.len() as u64, blake3::hash(&bytes))
        } else {
            let options = crab_sdk::operation::ReadOptions::default()
                .with_limits(crab_sdk::operation::ReadLimits {
                    max_fetched_bytes: 4 * 1024 * 1024 * 1024,
                    ..Default::default()
                })?
                .with_timeout(std::time::Duration::from_secs(120))?;
            let mut stream = snapshot.open_file(path).with_options(options).await?;
            let mut size = 0u64;
            let mut hash = blake3::Hasher::new();
            while let Some(bytes) = stream.next().await? {
                size += bytes.len() as u64;
                hash.update(&bytes);
            }
            stream.close().await?;
            (size, hash.finalize())
        };
        writeln!(
            std::io::stdout(),
            "commit={} bytes={} blake3={}",
            snapshot.commit_id()?,
            size,
            hash
        )?;
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    let close = client.close().await;
    if let (Err(_), Err(cleanup)) = (&result, &close) {
        let _ = writeln!(
            std::io::stderr(),
            "additional client cleanup failure: {cleanup:?}"
        );
    }
    result?;
    close?;
    Ok(())
}
