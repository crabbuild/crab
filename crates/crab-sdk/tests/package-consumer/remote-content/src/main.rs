#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), crab_sdk::Error> {
    let root = std::env::temp_dir().join("crab-sdk-package-consumer");
    let client = crab_sdk::Client::builder()
        .direct_store(crab_sdk::DirectStoreOptions::filesystem(&root)?)
        .build()?;
    let _request = client.open_remote(crab_sdk::RepositoryLocator::new("consumer/repository")?);
    client.close().await
}
