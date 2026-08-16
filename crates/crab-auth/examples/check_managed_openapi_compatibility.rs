use std::error::Error;
use std::path::Path;

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let baseline = arguments
        .next()
        .ok_or("usage: check_managed_openapi_compatibility BASELINE CANDIDATE")?;
    let candidate = arguments
        .next()
        .ok_or("usage: check_managed_openapi_compatibility BASELINE CANDIDATE")?;
    if arguments.next().is_some() {
        return Err("usage: check_managed_openapi_compatibility BASELINE CANDIDATE".into());
    }

    let baseline = read_json(Path::new(&baseline))?;
    let candidate = read_json(Path::new(&candidate))?;
    let changes = crab_auth::managed_openapi_breaking_changes(&baseline, &candidate);
    if changes.is_empty() {
        return Ok(());
    }
    Err(format!(
        "breaking managed OpenAPI changes:\n- {}",
        changes.join("\n- ")
    )
    .into())
}

fn read_json(path: &Path) -> Result<serde_json::Value, Box<dyn Error>> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}
