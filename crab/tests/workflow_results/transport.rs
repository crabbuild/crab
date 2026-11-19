use super::{cli, command, fixture, python};
use crab_auth::managed::{
    ServiceProfile, ServiceProfileStore, ServiceTrust, service_profile_directory,
};
use serde_json::{Value, json};
use std::{fs, process::Command};

#[test]
fn results_schema_is_advertised_by_version() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    let output = command(
        &repo,
        env!("CARGO_BIN_EXE_crab"),
        &["results", "list", "--json"],
    );
    let result: Value = serde_json::from_str(&output).unwrap();
    let version = cli(&repo, &["version", "--json"]);
    assert_eq!(
        version["schemas"].get(result["schema"].as_str().unwrap()),
        result.get("version")
    );
}

#[test]
fn managed_results_transfers_refuse_before_direct_credentials() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    fs::write(repo.join(".gitignore"), "/.crab/\n").unwrap();
    fs::write(
        repo.join("crab.yaml"),
        serde_json::to_vec(&json!({
            "defaults":{"env":"empty"},
            "stages":{"evaluate":{
                "cmd":{"argv":[python(),"-c",
                    "from pathlib import Path; Path('metrics.json').write_text('{\"score\":7}')"]},
                "metrics":["metrics.json"]}}
        }))
        .unwrap(),
    )
    .unwrap();
    command(&repo, "git", &["add", "."]);
    command(&repo, "git", &["commit", "-m", "Results transfer boundary"]);
    let run = cli(&repo, &["exp", "run", "--json"]);
    let id = run["exp_id"].as_str().unwrap();
    let report = cli(
        &repo,
        &[
            "results",
            "report",
            "create",
            id,
            "--title",
            "Transport boundary",
            "--json",
        ],
    );
    let report_id = report["id"].as_str().unwrap();
    let home = fixture.path().join("home");
    fs::create_dir(&home).unwrap();
    let tokens = home.join("tokens");
    let profiles = service_profile_directory(&tokens);
    let profile_path = profiles.join("code.corp.example.json");
    ServiceProfileStore::new(profiles)
        .store(&ServiceProfile::new("code.corp.example", ServiceTrust::default()).unwrap())
        .unwrap();
    // The incomplete direct provider is a local rejection trap. Managed URLs
    // must be classified before it, without reading ambient credentials/network.
    fs::write(
        repo.join(".crab/local.toml"),
        format!(
            "[auth]\nprovider = 'aws-oidc'\ntoken_cache_path = {:?}\n",
            tokens.to_str().unwrap()
        ),
    )
    .unwrap();
    for (authority, corrupt, expected) in [
        (
            "code.corp.example",
            false,
            "managed repository support is not enabled",
        ),
        (
            "code.corp.example",
            true,
            "managed service profile for code.corp.example is invalid",
        ),
        (
            "crab.build",
            false,
            "managed repository support is not enabled",
        ),
        (
            "direct.bucket.example",
            false,
            "aws-oidc provider requires a role_arn",
        ),
    ] {
        if corrupt {
            fs::write(&profile_path, "{").unwrap();
        }
        fs::write(
            repo.join("crab.toml"),
            format!("[remote]\nurl = 'crab://{authority}/team/models'\n"),
        )
        .unwrap();
        for args in [
            vec!["results", "report", "push", report_id, "--json"],
            vec!["results", "report", "pull", report_id, "--json"],
            vec!["results", "pull", id, "--json"],
            vec!["exp", "push", id, "--json"],
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_crab"))
                .args(&args)
                .current_dir(&repo)
                .env("HOME", &home)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .output()
                .unwrap();
            let diagnostic = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !output.status.success() && diagnostic.contains(expected),
                "{args:?} for {authority}: {diagnostic}"
            );
        }
    }
    assert_eq!(
        cli(&repo, &["results", "report", "show", report_id, "--json"]),
        report["report"]
    );
}
