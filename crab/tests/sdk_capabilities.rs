use std::process::Command;

fn crab_json(args: &[&str]) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_crab"))
        .args(args)
        .output()
        .expect("run crab");
    assert!(
        output.status.success(),
        "crab {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse crab JSON output")
}

#[test]
fn sdk_capabilities_report_the_executable_version() {
    let capabilities = crab_json(&["sdk-capabilities", "--json"]);
    let version = crab_json(&["version", "--json"]);

    assert_eq!(
        capabilities["crab_version"],
        version["data"]["crab_version"]
    );
}

#[test]
fn sdk_capabilities_report_canonical_staging_format() {
    let capabilities = crab_json(&["sdk-capabilities", "--json"]);

    assert_eq!(capabilities["staging_format_version"], 1);
}
