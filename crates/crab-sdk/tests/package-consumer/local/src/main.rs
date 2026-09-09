fn main() -> Result<(), crab_sdk::Error> {
    let _clone = crab_sdk::CloneOptions::default().with_depth(1)?;
    let _fetch = crab_sdk::FetchOptions::default().with_depth(crab_sdk::FetchDepth::Depth(1))?;
    let _push = crab_sdk::PushOptions::current_branch().dry_run(true);
    Ok(())
}
