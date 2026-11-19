use super::{cli, command, fixture, python};
use crab_workflow::results::{Availability, LocalResults};
use serde_json::{Value, json};
use std::fs;

#[test]
#[ignore = "requires an owned S3-compatible bucket with conditional writes"]
fn partial_template_reports_keep_verified_curves_after_cold_retrieval() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    let bucket = std::env::var("CRAB_SDK_TEST_S3_BUCKET").expect("owned fixture bucket");
    let remote = format!(
        "crab://{bucket}/partial-templates-{}",
        crab_workflow::ExperimentId::new_v7()
    );
    cli(&repo, &["init", &remote, "--json"]);
    fs::write(repo.join(".gitignore"), "/.crab/\n").unwrap();
    fs::write(repo.join("crab.yaml"), serde_json::to_vec(&json!({
        "metrics":["metrics.json"],
        "plots":[{"curve.csv":{"x":"step","y":"loss","title":"Saved loss","template":"custom.json"}}],
        "stages":{"observe":{"cmd":{"argv":[python(),"-c","pass"]},"metrics":["metrics.json"],"plots":["curve.csv"]}}
    })).unwrap()).unwrap();
    fs::write(repo.join("metrics.json"), r#"{"score":0.1}"#).unwrap();
    fs::write(repo.join("curve.csv"), "step,loss\n0,1\n1,2\n").unwrap();
    let template =
        crab_workflow::results::plot_templates::builtin_plot_template_spec("linear").unwrap();
    fs::write(
        repo.join("custom.json"),
        serde_json::to_vec(&template).unwrap(),
    )
    .unwrap();
    command(&repo, "git", &["add", "."]);
    command(
        &repo,
        "git",
        &["commit", "-m", "Partial template observations"],
    );
    let verified = cli(
        &repo,
        &["exp", "save", "--name", "Verified template", "--json"],
    );
    let verified = verified["exp_id"].as_str().unwrap();
    fs::write(repo.join("metrics.json"), r#"{"score":0.2}"#).unwrap();
    fs::write(repo.join("curve.csv"), "step,loss\n0,3\n1,4\n").unwrap();
    fs::File::create(repo.join("custom.json"))
        .unwrap()
        .set_len(crab_workflow::results::MAX_ATTACHMENT_BYTES as u64 + 1)
        .unwrap();
    let unavailable = cli(
        &repo,
        &["exp", "save", "--name", "Uncaptured template", "--json"],
    );
    let unavailable = unavailable["exp_id"].as_str().unwrap();
    let local = LocalResults::new(&repo).unwrap();
    assert!(
        local
            .run(unavailable)
            .unwrap()
            .snapshot
            .attachments
            .iter()
            .any(
                |asset| asset.kind == crab_workflow::results::AttachmentKind::Template
                    && asset.state == Availability::NotCaptured
            )
    );
    for ids in [[verified, unavailable], [unavailable, verified]] {
        let chart = cli(&repo, &["results", "charts", ids[0], ids[1], "--json"]);
        assert_eq!(
            (
                chart[0]["state"].clone(),
                chart[0]["points"].clone(),
                chart[0]["spec"].is_object()
            ),
            (json!("not_captured"), json!(2), true)
        );
    }
    let created = cli(
        &repo,
        &[
            "results",
            "report",
            "create",
            unavailable,
            verified,
            "--title",
            "Partial template comparison",
            "--json",
        ],
    );
    let id = created["id"].as_str().unwrap();
    cli(&repo, &["results", "report", "push", id, "--json"]);
    fs::rename(
        repo.join(".crab/workflow/results"),
        fixture.path().join("before-cold-results"),
    )
    .unwrap();
    fs::remove_file(repo.join("custom.json")).unwrap();
    fs::remove_file(repo.join("crab.yaml")).unwrap();
    cli(&repo, &["results", "report", "pull", id, "--json"]);
    let cold = LocalResults::new(&repo).unwrap();
    let report = cold.report(id).unwrap();
    assert_eq!(serde_json::to_value(&report).unwrap(), created["report"]);
    assert!(cold.list().unwrap().is_empty());
    let html = repo.join("partial-template.html");
    cli(
        &repo,
        &[
            "results",
            "report",
            "export",
            id,
            "--output",
            html.to_str().unwrap(),
            "--json",
        ],
    );
    if let Some(output) = std::env::var_os("CRAB_RESULTS_EVIDENCE") {
        let output = std::path::PathBuf::from(output);
        fs::copy(html, output.join("partial-template.html")).unwrap();
        fs::write(
            output.join("partial-template-report.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
        let retained = output.join("partial-template-fixture");
        if retained.exists() {
            fs::remove_dir_all(&retained).unwrap();
        }
        fs::rename(repo, retained).unwrap();
    }
}

#[test]
fn authored_plot_failures_keep_real_metrics_and_export_the_retained_sources() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    fs::write(repo.join(".gitignore"), "/.crab/\n").unwrap();
    let names = ["healthy", "initial", "event", "syntax"];
    let mut plots = Vec::new();
    for name in names {
        let mut config = json!({"title":name,"x":"step","y":"loss"});
        if name != "healthy" {
            let mut template =
                crab_workflow::results::plot_templates::builtin_plot_template_spec("linear")
                    .unwrap();
            template["transform"] = json!([{"calculate":match name {
                "initial" => "datum.missing.value",
                "event" => "trigger ? datum.missing.value : datum.loss",
                _ => "(",
            },"as":"loss"}]);
            if name == "event" {
                template["params"] = json!([{"name":"trigger","value":false,
                    "bind":{"input":"checkbox","name":"Trigger calculation failure"}}]);
            }
            let path = format!("{name}.json");
            fs::write(repo.join(&path), serde_json::to_vec(&template).unwrap()).unwrap();
            config["template"] = path.into();
        }
        plots.push(json!({format!("{name}.csv"):config}));
    }
    let program = "import json; from pathlib import Path; [Path(name+'.csv').write_text('step,loss\\n0,1\\n1,2\\n') for name in ['healthy','initial','event','syntax']]; Path('metrics.json').write_text(json.dumps({'score':0.25}))";
    fs::write(
        repo.join("crab.yaml"),
        serde_json::to_vec(&json!({
            "defaults":{"env":"empty"},"plots":plots,
            "stages":{"observe":{"cmd":{"argv":[python(),"-c",program]},
                "plots":names.map(|name| format!("{name}.csv")),"metrics":["metrics.json"]}}
        }))
        .unwrap(),
    )
    .unwrap();
    command(&repo, "git", &["add", "."]);
    command(
        &repo,
        "git",
        &["commit", "-m", "Authored plot failure observations"],
    );
    let created = cli(&repo, &["exp", "run", "--json"]);
    let id = created["exp_id"].as_str().unwrap();
    let local = LocalResults::new(&repo).unwrap();
    let run = local.run(id).unwrap();
    assert!(run.snapshot.capture_issues.is_empty());
    let charts = crab_workflow::results::charts(&[run], |hash| local.blob(hash)).unwrap();
    assert_eq!(
        charts
            .iter()
            .filter(|chart| chart.state == Availability::Present)
            .count(),
        4
    );
    let report = cli(
        &repo,
        &[
            "results",
            "report",
            "create",
            id,
            "--title",
            "Authored plot failure observations",
            "--json",
        ],
    );
    let report_id = report["id"].as_str().unwrap();
    let html = repo.join("report.html");
    cli(
        &repo,
        &[
            "results",
            "report",
            "export",
            report_id,
            "--output",
            html.to_str().unwrap(),
            "--json",
        ],
    );
    cli(&repo, &["exp", "apply", id, "--json"]);
    command(
        &repo,
        env!("CARGO_BIN_EXE_crab"),
        &[
            "metrics",
            "plot",
            "--format",
            "html",
            "--output",
            "metrics.html",
        ],
    );
    if let Some(output) = std::env::var_os("CRAB_RESULTS_EVIDENCE") {
        let output = std::path::PathBuf::from(output);
        fs::copy(html, output.join("authored-failures.html")).unwrap();
        fs::copy(
            repo.join("metrics.html"),
            output.join("authored-metrics.html"),
        )
        .unwrap();
        fs::write(
            output.join("authored-failures-report.json"),
            serde_json::to_vec_pretty(&local.report(report_id).unwrap()).unwrap(),
        )
        .unwrap();
        // Native acceptance opens these actual CLI objects through its normal
        // local-session reader; the fixture contains no mocked Results replies.
        let retained = output.join("authored-fixture");
        if retained.exists() {
            fs::remove_dir_all(&retained).unwrap();
        }
        fs::rename(repo, retained).unwrap();
    }
}

#[test]
fn cli_matrix_export_counts_complete_inputs_and_preserves_tagged_json_rows() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    let mut bytes = String::from("actual,predicted\n");
    for row in 0..10_000 {
        bytes.push_str(&format!("{},{}\n", row % 2, row % 3));
    }
    fs::write(repo.join("classes.csv"), bytes).unwrap();
    command(
        &repo,
        env!("CARGO_BIN_EXE_crab"),
        &[
            "metrics",
            "plot",
            "classes.csv",
            "--x",
            "actual",
            "--y",
            "predicted",
            "--template",
            "confusion",
            "--format",
            "vega",
            "--output",
            "matrix.json",
        ],
    );
    let spec: Value = serde_json::from_slice(&fs::read(repo.join("matrix.json")).unwrap()).unwrap();
    let values = spec["data"]["values"].as_array().unwrap();
    assert_eq!(values.len(), 6);
    let weight = spec["transform"][0]["aggregate"][0]["field"]
        .as_str()
        .unwrap();
    assert_eq!(
        values
            .iter()
            .map(|row| row[weight].as_u64().unwrap())
            .sum::<u64>(),
        10_000
    );
    let raw: Value = serde_json::from_str(&command(
        &repo,
        env!("CARGO_BIN_EXE_crab"),
        &[
            "metrics",
            "plot",
            "classes.csv",
            "--x",
            "actual",
            "--y",
            "predicted",
            "--template",
            "confusion",
            "--json",
        ],
    ))
    .unwrap();
    let plot = &raw["data"][0];
    assert_eq!(plot["data_points"], 10_000);
    assert_eq!(plot["data"].as_array().unwrap().len(), 10_000);
    assert_eq!(plot["data"][9999], json!({"x":"1","y_values":["0"]}));
}
