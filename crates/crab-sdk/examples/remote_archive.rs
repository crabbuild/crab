use crab_sdk::remote::{ArchiveEvent, ContentMode};
use crab_sdk::storage::DirectStoreOptions;
use crab_sdk::{Client, RepositoryLocator, Revision};
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
    let [bucket, repository, branch] = args.as_slice() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage: remote_archive BUCKET REPOSITORY BRANCH",
        )
        .into());
    };
    let locator = RepositoryLocator::new(repository)?;
    let revision = Revision::branch(branch)?;
    let client = Client::builder()
        .direct_store(DirectStoreOptions::s3_from_env(bucket)?)
        .build()?;
    let result = async {
        let repository = client.open(crab_sdk::OpenOptions::remote(locator)).await?;
        let snapshot = repository.remote()?.snapshot(revision).await?;
        let mut archive = snapshot.archive(ContentMode::Git).await?;
        // EOF includes verification and finalization. A dropped/erroring stream
        // is still drained by the client close below.
        let mut pending = None;
        while let Some(event) = archive.next().await? {
            match event {
                ArchiveEvent::Entry { entry, size } => pending = Some((entry, size)),
                ArchiveEvent::Data(_) => {}
                ArchiveEvent::EndEntry => {
                    if let Some((entry, size)) = pending.take() {
                        writeln!(
                            std::io::stdout(),
                            "{:?} {}",
                            entry.path.as_bytes(),
                            size.unwrap_or(0)
                        )?;
                    }
                }
                _ => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "unsupported archive event",
                    )
                    .into());
                }
            }
        }
        archive.close().await?;
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
