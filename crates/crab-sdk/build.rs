fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_SERVICE").is_none() {
        return;
    }
    for (input, output) in [
        ("TARGET", "CRAB_SDK_BUILD_TARGET"),
        ("PROFILE", "CRAB_SDK_BUILD_PROFILE"),
    ] {
        let value = std::env::var(input).unwrap_or_default();
        println!("cargo::rustc-env={output}={value}");
    }
    let features = std::env::var("CARGO_CFG_FEATURE").unwrap_or_default();
    let mut features: Vec<_> = features.split(',').collect();
    features.sort_unstable();
    println!(
        "cargo::rustc-env=CRAB_SDK_BUILD_FEATURES={}",
        features.join(",")
    );
}
