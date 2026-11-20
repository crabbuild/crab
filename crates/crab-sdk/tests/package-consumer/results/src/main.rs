use crab_sdk::operation::Options;
use crab_sdk::results::{ResultsRead, RunQuery};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), crab_sdk::Error> {
    let root = std::env::temp_dir().join("crab-sdk-package-consumer");
    let client = crab_sdk::Client::builder()
        .direct_store(crab_sdk::storage::DirectStoreOptions::filesystem(&root)?)
        .build()?;
    let repository = client
        .open(crab_sdk::OpenOptions::remote(
            crab_sdk::RepositoryLocator::new("consumer/repository")?,
        ))
        .await?;
    let results = repository.remote()?.results();
    let _read = results
        .read(ResultsRead::List(RunQuery::default()))
        .with_options(Options::default());
    client.close().await
}
