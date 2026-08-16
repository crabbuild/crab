use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let generated = format!("{}\n", crab_auth::managed_openapi_json()?);
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("openapi/managed-v1.json");
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments == ["--check"] {
        let reviewed = std::fs::read_to_string(&path)?;
        if reviewed != generated {
            return Err(format!(
                "{} differs from the generated managed DTO contract",
                path.display()
            )
            .into());
        }
        return Ok(());
    }
    if arguments == ["--write"] {
        std::fs::write(path, generated)?;
        return Ok(());
    }
    if !arguments.is_empty() {
        return Err("usage: generate_managed_openapi [--check|--write]".into());
    }

    print!("{generated}");
    Ok(())
}
