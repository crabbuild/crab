use super::*;
use crab_workflow::{EXPERIMENT_METADATA_SCHEMA_VERSION, ExperimentId, ExperimentMetadata};
use std::collections::BTreeMap;

fn old_metadata(repo: &Path, status: &str, score: f64) -> ExperimentMetadata {
    let metadata = ExperimentMetadata {
        schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
        exp_id: ExperimentId::new_v7(),
        base_commit: command(repo, "git", &["rev-parse", "HEAD"]).trim().into(),
        queue_commit: None,
        name: Some(format!("Before Results {status} {score}")),
        message: None,
        status: status.into(),
        param_overrides: BTreeMap::from([("factor".into(), "2".into())]),
        stages: BTreeMap::from([("train".into(), "a".repeat(64))]),
        metrics: BTreeMap::from([("metrics.json".into(), json!({"score":score}))]),
        cli_args: vec![
            "exp".into(),
            "run".into(),
            "fixture-private-cli-value".into(),
        ],
        host_fingerprint: "recorded-host".into(),
        started_at: "2025-01-01T00:00:00.000Z".into(),
        ended_at: Some("2025-01-01T00:00:01.000Z".into()),
    };
    let directory = repo.join(".crab/workflow/exp");
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join(format!("{}.meta.json", metadata.exp_id)),
        metadata.canonical_json().unwrap(),
    )
    .unwrap();
    metadata
}

#[test]
fn metadata_migration_preserves_recorded_values_without_claiming_execution() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    fs::write(repo.join(".gitignore"), "/.crab/\n").unwrap();
    fs::write(repo.join("README"), "Historical experiment metadata\n").unwrap();
    command(&repo, "git", &["add", "."]);
    command(&repo, "git", &["commit", "-m", "Migration fixture"]);
    let old = [
        old_metadata(&repo, "success", 0.4),
        old_metadata(&repo, "success", 0.6),
        old_metadata(&repo, "saved", 0.2),
        old_metadata(&repo, "failed", 0.1),
    ];
    fs::write(repo.join("metrics.json"), "{\"score\":999}").unwrap();
    fs::write(repo.join("params.json"), "{\"factor\":999}").unwrap();
    fs::write(
        repo.join("crab.yaml"),
        "invalid: current workspace must not be parsed",
    )
    .unwrap();
    let id = old[0].exp_id.to_string();
    assert_eq!(
        cli(&repo, &["exp", "show", &id, "--json"])["metadata"]["metrics"]["metrics.json"]["score"],
        json!(0.4)
    );
    assert_eq!(cli(&repo, &["results", "list", "--json"])["total"], 0);
    let migrated = cli(&repo, &["results", "migrate", "--all", "--json"]);
    assert_eq!(migrated["runs"].as_array().unwrap().len(), 4);
    assert_eq!(
        cli(
            &repo,
            &["results", "list", "--search", "imported", "--json"]
        )["total"],
        4
    );
    let local = LocalResults::new(&repo).unwrap();
    for metadata in &old {
        let run = local.run(&metadata.exp_id.to_string()).unwrap();
        let value = serde_json::to_value(&run).unwrap();
        assert_eq!(value["snapshot"]["origin"], "experiment_metadata");
        assert_eq!(run.snapshot.status, metadata.status);
        assert_eq!(
            run.snapshot.source_metadata_hash,
            metadata.content_hash().unwrap()
        );
        assert_eq!(
            run.snapshot.metrics[0].value,
            metadata.metrics["metrics.json"]["score"]
        );
        assert!(run.snapshot.parameters.is_empty());
        assert!(run.snapshot.stages.is_empty());
        assert!(run.snapshot.attachments.is_empty());
        assert!(run.snapshot.plots.is_empty());
        assert!(run.snapshot.reproduction.is_none());
        assert!(!value.to_string().contains("fixture-private-cli-value"));
        for args in [
            vec![
                "results",
                "preview",
                &value["snapshot"]["experiment_id"].as_str().unwrap(),
                "--digest",
                &run.digest,
                "--json",
            ],
            vec![
                "exp",
                "apply",
                &value["snapshot"]["experiment_id"].as_str().unwrap(),
                "--result-digest",
                &run.digest,
                "--json",
            ],
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_crab"))
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(!output.status.success());
            let error: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert!(
                error["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("Imported experiment metadata")
            );
        }
    }
    assert_eq!(
        cli(&repo, &["results", "migrate", "--all", "--json"]),
        migrated,
    );
    let second = old[1].exp_id.to_string();
    let spec = fixture.path().join("comparison.json");
    fs::write(
        &spec,
        serde_json::to_vec(&json!({
            "baseline":id,
            "objectives":{"metric:[\"metrics.json\",\"/score\"]":"maximize"},
            "datasets":{
                &id:{"hash":"b".repeat(64),"split":"test","evaluator":"v1"},
                &second:{"hash":"b".repeat(64),"split":"test","evaluator":"v1"}
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let unjustified = Command::new(env!("CARGO_BIN_EXE_crab"))
        .args([
            "results",
            "compare",
            &id,
            &second,
            "--spec",
            spec.to_str().unwrap(),
            "--json",
        ])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(!unjustified.status.success());
    let error: Value = serde_json::from_slice(&unjustified.stdout).unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("retained input witness")
    );
    fs::write(
        &spec,
        serde_json::to_vec(&json!({
            "baseline":id,
            "objectives":{"metric:[\"metrics.json\",\"/score\"]":"maximize"}
        }))
        .unwrap(),
    )
    .unwrap();
    let comparison = cli(
        &repo,
        &[
            "results",
            "compare",
            &id,
            &second,
            "--spec",
            spec.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(
        comparison["rows"][1]["cells"][0]["absolute_delta"],
        json!(0.6_f64 - 0.4_f64)
    );
    assert!(comparison["rows"][1]["cells"][0]["improvement"].is_null());
    assert!(
        comparison["rows"][1]["cells"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("Imported experiment metadata")
    );
    let report = cli(
        &repo,
        &[
            "results",
            "report",
            "create",
            &id,
            &second,
            "--title",
            "Imported observations",
            "--json",
        ],
    );
    if let Some(output) = std::env::var_os("CRAB_RESULTS_EVIDENCE") {
        fs::write(
            Path::new(&output).join("metadata-report.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
    }
    fs::remove_dir_all(repo.join(".crab/workflow/exp")).unwrap();
    let reopened = cli(
        &repo,
        &[
            "results",
            "report",
            "show",
            report["id"].as_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(
        reopened["comparison"]["rows"][0]["run"]["snapshot"]["metrics"][0]["value"],
        json!(0.4)
    );
    assert_eq!(cli(&repo, &["results", "list", "--json"])["total"], 4);
}

#[test]
fn metadata_migration_rejects_invalid_sources_before_installing_roots() {
    for fault in ["schema", "identity", "oversize", "symlink", "fields"] {
        let fixture = fixture();
        let repo = fixture.path().join("repo");
        fs::write(repo.join("README"), "Migration preflight\n").unwrap();
        command(&repo, "git", &["add", "."]);
        command(&repo, "git", &["commit", "-m", "Migration preflight"]);
        let good = old_metadata(&repo, "success", 0.4);
        let bad = old_metadata(&repo, "success", 0.6);
        let path = repo.join(format!(".crab/workflow/exp/{}.meta.json", bad.exp_id));
        match fault {
            "fields" => {
                let fields = (0..=MAX_FIELDS / 2)
                    .map(|index| (format!("metric_{index}"), json!(index)))
                    .collect::<serde_json::Map<_, _>>();
                let mut value = serde_json::to_value(&bad).unwrap();
                value["metrics"] = json!({"one.json":fields,"two.json":fields});
                fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "schema" | "identity" => {
                let mut value = serde_json::to_value(&bad).unwrap();
                if fault == "schema" {
                    value["schema_version"] = json!(2);
                } else {
                    value["exp_id"] = json!(good.exp_id.to_string());
                }
                fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "oversize" => fs::File::create(&path)
                .unwrap()
                .set_len(MAX_DOCUMENT_BYTES as u64 + 1)
                .unwrap(),
            "symlink" => {
                let outside = fixture.path().join("outside.json");
                fs::write(&outside, bad.canonical_json().unwrap()).unwrap();
                fs::remove_file(&path).unwrap();
                #[cfg(unix)]
                std::os::unix::fs::symlink(outside, path).unwrap();
                #[cfg(windows)]
                std::os::windows::fs::symlink_file(outside, path).unwrap();
            }
            _ => unreachable!(),
        }
        let output = Command::new(env!("CARGO_BIN_EXE_crab"))
            .args([
                "results",
                "migrate",
                &good.exp_id.to_string(),
                &bad.exp_id.to_string(),
                "--json",
            ])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{fault}");
        let error: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(error["error"].is_object(), "{fault}");
        assert!(
            LocalResults::new(&repo).unwrap().list().unwrap().is_empty(),
            "{fault}"
        );
    }
}

#[test]
fn metadata_migration_preserves_existing_captures_and_refuses_corrupt_roots() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    fs::write(repo.join(".gitignore"), "/.crab/\n").unwrap();
    fs::write(
        repo.join("crab.yaml"),
        serde_json::to_vec(&json!({
            "metrics":["metrics.json"],
            "stages":{"observe":{"cmd":{"argv":[python(),"-c","pass"]}}}
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(repo.join("metrics.json"), "{\"score\":0.75}").unwrap();
    command(&repo, "git", &["add", "."]);
    command(&repo, "git", &["commit", "-m", "Existing capture"]);
    let saved = cli(&repo, &["exp", "save", "--json"]);
    let id = saved["exp_id"].as_str().unwrap();
    let local = LocalResults::new(&repo).unwrap();
    let captured = local.run(id).unwrap();
    let metadata_path = repo.join(format!(".crab/workflow/exp/{id}.meta.json"));
    let mut metadata: Value = serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    metadata["metrics"]["metrics.json"]["score"] = json!(999);
    fs::write(metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    let imported = old_metadata(&repo, "success", 0.4);
    cli(&repo, &["results", "migrate", "--all", "--json"]);
    let retained = local.run(id).unwrap();
    assert_eq!(
        (
            retained.digest,
            serde_json::to_value(retained.snapshot).unwrap()["origin"].clone()
        ),
        (captured.digest, json!("captured"))
    );

    let imported = local.run(&imported.exp_id.to_string()).unwrap();
    let root = repo.join(format!(
        ".crab/workflow/results/refs/{}.json",
        imported.snapshot.experiment_id
    ));
    let before = fs::read(&root).unwrap();
    let body = repo.join(format!(
        ".crab/workflow/results/runs/{}/{}.json",
        imported.snapshot.experiment_id, imported.digest
    ));
    fs::write(&body, "corrupt fixture body").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_crab"))
        .args([
            "results",
            "migrate",
            &imported.snapshot.experiment_id,
            "--json",
        ])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read(root).unwrap(), before);
    assert_eq!(fs::read(body).unwrap(), b"corrupt fixture body");
}
