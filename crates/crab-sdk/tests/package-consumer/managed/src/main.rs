fn main() -> Result<(), crab_sdk::Error> {
    let _options = crab_sdk::ManagedOptions::new(std::env::temp_dir())?
        .with_authority("crab.build")?;
    let _repository = crab_sdk::RepositoryLocator::managed("crab.build", "acme", "models")?;
    Ok(())
}
