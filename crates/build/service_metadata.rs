use std::env;

pub fn emit(metadata_path: &str) -> Result<(), env::VarError> {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed={metadata_path}");
    println!("cargo::rerun-if-env-changed=GIT_SHA");
    let revision = env::var("GIT_SHA").unwrap_or_default();
    let source = if matches!(revision.len(), 40 | 64)
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        revision.as_str()
    } else {
        "unknown"
    };
    println!("cargo::rustc-env=CRAB_SERVICE_SOURCE_SHA={source}");
    println!(
        "cargo::rustc-env=CRAB_SERVICE_TARGET={}",
        env::var("TARGET")?
    );
    println!(
        "cargo::rustc-env=CRAB_SERVICE_PROFILE={}",
        env::var("PROFILE")?
    );
    let configured = env::var("CARGO_CFG_FEATURE").unwrap_or_default();
    let mut features: Vec<_> = configured.split(',').collect();
    features.sort_unstable();
    println!(
        "cargo::rustc-env=CRAB_SERVICE_FEATURES={}",
        features.join(",")
    );
    Ok(())
}
