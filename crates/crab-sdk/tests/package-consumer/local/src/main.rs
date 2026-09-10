fn main() -> Result<(), crab_sdk::Error> {
    let _clone = crab_sdk::local::CloneOptions::default().with_depth(1)?;
    let _fetch = crab_sdk::local::FetchOptions::default()
        .with_depth(crab_sdk::local::FetchDepth::Depth(1))?;
    let _push = crab_sdk::local::PushOptions::current_branch().dry_run(true);
    let _open = crab_sdk::OpenOptions::local(std::env::temp_dir());
    Ok(())
}

fn inspect(repository: &crab_sdk::Repository) -> crab_sdk::Result<()> {
    let local = repository.local()?;
    let _status = local.status();
    Ok(())
}
