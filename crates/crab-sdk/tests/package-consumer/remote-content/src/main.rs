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
    let _refs = repository.remote()?.refs();
    client.close().await
}
