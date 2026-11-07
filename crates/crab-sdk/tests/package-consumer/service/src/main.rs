fn main() -> Result<(), crab_sdk::Error> {
    let options =
        crab_sdk::managed::Options::new(std::env::temp_dir())?.with_authority("crab.build")?;
    let client = crab_sdk::Client::builder().managed(options).build()?;
    let _managed = client.managed();
    let _repository = crab_sdk::RepositoryLocator::managed("crab.build", "acme", "models")?;
    Ok(())
}
