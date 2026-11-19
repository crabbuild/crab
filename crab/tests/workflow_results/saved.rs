use super::*;

#[test]
fn template_refs_cannot_publish_private_repository_state() {
    for reference in [
        ".crab/private.json",
        ".crab/plots/../private.json",
        ".git/private.json",
    ] {
        let fixture = fixture();
        let repo = fixture.path().join("repo");
        fs::write(repo.join(".gitignore"), "/.crab/\n").unwrap();
        fs::write(
            repo.join("crab.yaml"),
            serde_json::to_vec(&json!({
                "plots":[{"curve.csv":{"x":"step","y":"score","template":reference}}],
                "stages":{"observe":{"cmd":{"argv":[python(),"-c","pass"]}}}
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(repo.join("curve.csv"), "step,score\n0,0.25\n1,0.5\n").unwrap();
        command(&repo, "git", &["add", "."]);
        command(&repo, "git", &["commit", "-m", "Private asset boundary"]);
        fs::create_dir_all(repo.join(".crab/plots")).unwrap();
        let mut private =
            crab_workflow::results::plot_templates::builtin_plot_template_spec("linear").unwrap();
        private["usermeta"] = json!({"private_value":"fixture private configuration"});
        let private = serde_json::to_vec(&private).unwrap();
        fs::write(repo.join(reference), &private).unwrap();
        let saved = cli(&repo, &["exp", "save", "--json"]);
        let id = saved["exp_id"].as_str().unwrap();
        let run = LocalResults::new(&repo).unwrap().run(id).unwrap();
        let template = run
            .snapshot
            .attachments
            .iter()
            .find(|asset| asset.kind == AttachmentKind::Template)
            .unwrap();
        assert_eq!(
            (template.state, template.hash.as_deref()),
            (Availability::NotCaptured, None)
        );
        assert!(
            !serde_json::to_string(&run)
                .unwrap()
                .contains(&blake3::hash(&private).to_hex().to_string())
        );
    }
}

#[test]
fn execution_and_reproduction_keep_templates_from_the_recorded_tree() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    fs::write(repo.join(".gitignore"), "/.crab/\n").unwrap();
    fs::write(
        repo.join("crab.yaml"),
        serde_json::to_vec(&json!({
            "defaults":{"env":"empty"},
            "plots":[{"curve.csv":{"x":"step","y":"score","template":"custom.json"}}],
            "stages":{"observe":{"cmd":{"argv":[python(),"-c","pass"]}}}
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(repo.join("curve.csv"), "step,score\n0,0.25\n1,0.5\n").unwrap();
    let mut template =
        crab_workflow::results::plot_templates::builtin_plot_template_spec("linear").unwrap();
    template["title"] = json!("Committed template");
    let original = serde_json::to_vec(&template).unwrap();
    fs::write(repo.join("custom.json"), &original).unwrap();
    command(&repo, "git", &["add", "."]);
    command(&repo, "git", &["commit", "-m", "Recorded render asset"]);
    template["title"] = json!("Current workspace template");
    fs::write(
        repo.join("custom.json"),
        serde_json::to_vec(&template).unwrap(),
    )
    .unwrap();
    let created = cli(&repo, &["exp", "run", "--json"]);
    let local = LocalResults::new(&repo).unwrap();
    let run = local.run(created["exp_id"].as_str().unwrap()).unwrap();
    fs::remove_file(repo.join("custom.json")).unwrap();
    let reproduced = cli(
        &repo,
        &[
            "exp",
            "reproduce",
            &run.snapshot.experiment_id,
            "--digest",
            &run.digest,
            "--json",
        ],
    );
    for id in [
        run.snapshot.experiment_id.as_str(),
        reproduced["exp_id"].as_str().unwrap(),
    ] {
        let run = local.run(id).unwrap();
        let asset = run
            .snapshot
            .attachments
            .iter()
            .find(|asset| asset.kind == AttachmentKind::Template)
            .unwrap();
        assert_eq!(
            local.blob(asset.hash.as_deref().unwrap()).unwrap(),
            original
        );
    }
}

#[test]
fn uncaptured_custom_templates_do_not_select_same_name_builtins() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    fs::write(repo.join(".gitignore"), "/.crab/\n").unwrap();
    fs::write(
        repo.join("crab.yaml"),
        serde_json::to_vec(&json!({
            "metrics":["metrics.json"],
            "plots":[{"curve.csv":{"x":"step","y":"score","template":"linear"}}],
            "stages":{"observe":{"cmd":{"argv":[python(),"-c","pass"]}}}
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(repo.join("metrics.json"), "{\"score\":0.7}").unwrap();
    fs::write(repo.join("curve.csv"), "step,score\n0,0.25\n1,0.5\n").unwrap();
    command(&repo, "git", &["add", "."]);
    command(
        &repo,
        "git",
        &["commit", "-m", "Custom template capture limit"],
    );
    let assets = repo.join(".crab/plots");
    fs::create_dir_all(&assets).unwrap();
    fs::File::create(assets.join("linear.json"))
        .unwrap()
        .set_len(MAX_ATTACHMENT_BYTES as u64 + 1)
        .unwrap();
    let saved = cli(&repo, &["exp", "save", "--json"]);
    let id = saved["exp_id"].as_str().unwrap();
    let run = LocalResults::new(&repo).unwrap().run(id).unwrap();
    assert_eq!(run.snapshot.metrics[0].value, json!(0.7));
    let charts = cli(&repo, &["results", "charts", id, "--json"]);
    assert_eq!(
        (
            charts[0]["state"].as_str(),
            charts[0]["spec"].is_null(),
            charts[0]["points"].as_u64()
        ),
        (Some("not_captured"), true, Some(0)),
    );
}

#[test]
fn saved_templates_retain_declared_assets_without_private_crab_state() {
    for name in ["custom", "linear"] {
        let fixture = fixture();
        let repo = fixture.path().join("repo");
        fs::write(repo.join(".gitignore"), "/.crab/\n").unwrap();
        fs::write(
            repo.join("crab.yaml"),
            serde_json::to_vec(&json!({
                "plots":[{"curve.csv":{"x":"step","y":"score","template":name}}],
                "stages":{"observe":{"cmd":{"argv":[python(),"-c","pass"]}}}
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(repo.join("curve.csv"), "step,score\n0,0.25\n1,0.5\n").unwrap();
        command(&repo, "git", &["add", "."]);
        command(
            &repo,
            "git",
            &["commit", "-m", "Declared manual render asset"],
        );
        let assets = repo.join(".crab/plots");
        fs::create_dir_all(&assets).unwrap();
        let private = b"private local state must not enter report evidence";
        fs::write(repo.join(".crab/private-marker.txt"), private).unwrap();
        let template = serde_json::to_vec(&json!({
            "$schema":"https://vega.github.io/schema/vega-lite/v5.json",
            "title":"Retained manual template",
            "data":{"values":"<DVC_METRIC_DATA>"}, "mark":"line",
            "encoding":{
                "x":{"field":"<DVC_METRIC_X>","type":"quantitative"},
                "y":{"field":"<DVC_METRIC_Y>","type":"quantitative"},
                "color":"<DVC_METRIC_COLOR>"
            }
        }))
        .unwrap();
        fs::write(assets.join(format!("{name}.json")), &template).unwrap();
        let saved = cli(&repo, &["exp", "save", "--json"]);
        let id = saved["exp_id"].as_str().unwrap();
        let local = LocalResults::new(&repo).unwrap();
        let run = local.run(id).unwrap();
        let retained = run
            .snapshot
            .attachments
            .iter()
            .find(|attachment| attachment.kind == AttachmentKind::Template)
            .expect("the declared Crab template must be retained, including built-in overrides");
        assert_eq!(
            local.blob(retained.hash.as_deref().unwrap()).unwrap(),
            template
        );
        fs::remove_dir_all(&assets).unwrap();
        fs::write(repo.join("curve.csv"), "step,score\n0,99\n").unwrap();
        let chart = cli(&repo, &["results", "charts", id, "--json"]);
        assert_eq!(
            (chart[0]["state"].as_str(), chart[0]["points"].as_u64()),
            (Some("present"), Some(2))
        );
        assert!(
            chart[0]["spec"]
                .to_string()
                .contains("Retained manual template")
        );
        let report = cli(
            &repo,
            &[
                "results",
                "report",
                "create",
                id,
                "--title",
                "Manual template",
                "--json",
            ],
        );
        let output = fixture.path().join("report.html");
        cli(
            &repo,
            &[
                "results",
                "report",
                "export",
                report["id"].as_str().unwrap(),
                "--output",
                output.to_str().unwrap(),
                "--json",
            ],
        );
        let html = fs::read_to_string(output).unwrap();
        assert!(html.contains("Retained manual template"));
        let serialized = serde_json::to_string(&run).unwrap();
        assert!(!serialized.contains(std::str::from_utf8(private).unwrap()));
        assert!(!serialized.contains(&blake3::hash(private).to_hex().to_string()));
        assert!(
            !repo
                .join(format!(".crab/workflow/exp/{id}.workspace/.crab"))
                .exists()
        );
    }
}

#[test]
fn saved_experiments_retain_observations_without_claiming_execution() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    fs::write(repo.join(".gitignore"), "/.crab/\n").unwrap();
    fs::write(
        repo.join("crab.yaml"),
        serde_json::to_vec(&json!({
            "params":["params.json"],
            "metrics":["metrics.json"],
            "plots":[{"curve.csv":{"x":"step","y":"score"}}],
            "stages":{"train":{"cmd":{"argv":[python(),"-c","pass"]},
                "metrics":["metrics.json"],"plots":["curve.csv"],"outs":["model.bin"]}}
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(repo.join("params.json"), "{\"factor\":1}").unwrap();
    command(&repo, "git", &["add", "."]);
    command(&repo, "git", &["commit", "-m", "Manual observation base"]);
    fs::write(repo.join("params.json"), "{\"factor\":2}").unwrap();
    fs::write(repo.join("metrics.json"), "{\"score\":0.5}").unwrap();
    let curve = b"step,score\n0,0.25\n1,0.5\n";
    fs::write(repo.join("curve.csv"), curve).unwrap();
    fs::write(repo.join("model.bin"), b"trained outside Crab").unwrap();
    let prior_logs = repo.join(".crab/workflow/runs/prior");
    fs::create_dir_all(&prior_logs).unwrap();
    fs::write(prior_logs.join("stage-train.log"), "not this observation").unwrap();

    let saved = cli(&repo, &["exp", "save", "--name", "manual", "--json"]);
    let id = saved["exp_id"].as_str().unwrap();
    let page = cli(&repo, &["results", "list", "--json"]);
    assert_eq!(page["rows"][0]["id"], id);
    let comparison = cli(&repo, &["results", "compare", id, "--json"]);
    assert_eq!(
        comparison["rows"][0]["cells"][0]["reason"],
        "Saved workspace observations do not record execution completion",
    );
    fs::write(repo.join("params.json"), "{\"factor\":99}").unwrap();
    fs::write(repo.join("metrics.json"), "{\"score\":99}").unwrap();
    fs::write(repo.join("curve.csv"), "step,score\n0,99\n").unwrap();
    fs::write(repo.join("model.bin"), b"new workspace bytes").unwrap();

    let local = LocalResults::new(&repo).unwrap();
    let result = local.run(id).unwrap();
    let snapshot = &result.snapshot;
    let metric = snapshot
        .metrics
        .iter()
        .find(|field| field.file == "metrics.json" && field.pointer == "/score")
        .unwrap();
    let parameter = snapshot
        .parameters
        .iter()
        .find(|field| field.file == "params.json" && field.pointer == "/factor")
        .unwrap();
    assert_eq!(
        (
            snapshot.status.as_str(),
            &metric.value,
            &parameter.value,
            snapshot.stages.is_empty(),
            snapshot.reproduction.is_none(),
            snapshot
                .attachments
                .iter()
                .any(|attachment| attachment.kind == AttachmentKind::Log),
        ),
        ("saved", &json!(0.5), &json!(2), true, true, false),
    );
    let chart = charts(std::slice::from_ref(&result), |hash| local.blob(hash))
        .unwrap()
        .remove(0);
    assert_eq!((chart.state, chart.points), (Availability::Present, 2));
    let retained = snapshot
        .attachments
        .iter()
        .find(|attachment| attachment.path == "curve.csv")
        .unwrap();
    assert_eq!(
        local.blob(retained.hash.as_deref().unwrap()).unwrap(),
        curve
    );

    cli(
        &repo,
        &[
            "exp",
            "apply",
            id,
            "--result-digest",
            &result.digest,
            "--json",
        ],
    );
    assert_eq!(
        fs::read(repo.join("model.bin")).unwrap(),
        b"trained outside Crab"
    );
    fs::write(
        repo.join(".crab/workflow/exp")
            .join(format!("{id}.workspace/params.json")),
        "{\"factor\":3}",
    )
    .unwrap();
    fs::write(repo.join("params.json"), "{\"factor\":99}").unwrap();
    let changed = Command::new(env!("CARGO_BIN_EXE_crab"))
        .args([
            "exp",
            "apply",
            id,
            "--result-digest",
            &result.digest,
            "--json",
        ])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert_eq!(
        (
            changed.status.success(),
            fs::read(repo.join("params.json")).unwrap()
        ),
        (false, b"{\"factor\":99}".to_vec()),
    );
}

#[test]
fn result_workspace_actions_refuse_changed_outputs_before_touching_the_workspace() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    fs::write(repo.join(".gitignore"), "/.crab/\n").unwrap();
    fs::write(
        repo.join("train.py"),
        "from pathlib import Path\nPath('model.bin').write_bytes(b'correct')\nPath('deleted.txt').unlink()\n",
    )
    .unwrap();
    fs::write(
        repo.join("crab.yaml"),
        serde_json::to_vec(&json!({
            "defaults":{"env":"empty"},
            "stages":{"train":{"cmd":{"argv":[python(),"train.py"]},
                "deps":["train.py","deleted.txt"],"outs":["model.bin"]}}
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(repo.join("deleted.txt"), "base input").unwrap();
    command(&repo, "git", &["add", "."]);
    command(&repo, "git", &["commit", "-m", "Apply integrity base"]);
    let created = cli(&repo, &["exp", "run", "--json"]);
    let id = created["exp_id"].as_str().unwrap();
    let result = LocalResults::new(&repo).unwrap().run(id).unwrap();
    let workspace = repo
        .join(".crab/workflow/exp")
        .join(format!("{id}.workspace"));
    fs::write(workspace.join("model.bin"), b"changed").unwrap();
    fs::write(repo.join("model.bin"), b"keep current bytes").unwrap();
    fs::write(repo.join("train.py"), "keep current source").unwrap();
    fs::write(repo.join("deleted.txt"), "keep deletion target").unwrap();
    let paths = ["model.bin", "train.py", "deleted.txt"];
    let before: Vec<_> = paths
        .iter()
        .map(|path| fs::read(repo.join(path)).ok())
        .collect();
    for args in [
        vec![
            "results",
            "preview",
            id,
            "--digest",
            &result.digest,
            "--json",
        ],
        vec![
            "exp",
            "apply",
            id,
            "--result-digest",
            &result.digest,
            "--json",
        ],
        vec![
            "exp",
            "promote",
            id,
            "--result-digest",
            &result.digest,
            "--branch",
            "integrity-check",
            "--json",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_crab"))
            .args(&args)
            .env("TMPDIR", fixture.path())
            .current_dir(&repo)
            .output()
            .unwrap();
        let after: Vec<_> = paths
            .iter()
            .map(|path| fs::read(repo.join(path)).ok())
            .collect();
        assert_eq!(
            (output.status.success(), after),
            (false, before.clone()),
            "{args:?}"
        );
    }
    assert!(
        !Command::new("git")
            .args(["show-ref", "--verify", "refs/heads/integrity-check"])
            .current_dir(&repo)
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn result_apply_preserves_files_outside_workspace_directory_symlinks() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    let outside = fixture.path().join("outside");
    fs::create_dir(repo.join("data")).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(repo.join(".gitignore"), "/.crab/\n").unwrap();
    fs::write(repo.join("data/removed.txt"), "base input").unwrap();
    fs::write(repo.join("data/kept.txt"), "captured input").unwrap();
    fs::write(repo.join("train.py"), "from pathlib import Path\nPath('model.bin').write_bytes(b'correct')\nPath('data/removed.txt').unlink()\n").unwrap();
    fs::write(
        repo.join("crab.yaml"),
        serde_json::to_vec(&json!({
            "defaults":{"env":"empty"},
            "stages":{"train":{"cmd":{"argv":[python(),"train.py"]},
                "deps":["train.py","data"],"outs":["model.bin"]}}
        }))
        .unwrap(),
    )
    .unwrap();
    command(&repo, "git", &["add", "."]);
    command(
        &repo,
        "git",
        &["commit", "-m", "Workspace target boundaries"],
    );
    let created = cli(&repo, &["exp", "run", "--json"]);
    let id = created["exp_id"].as_str().unwrap();
    let result = LocalResults::new(&repo).unwrap().run(id).unwrap();
    fs::remove_dir_all(repo.join("data")).unwrap();
    fs::write(outside.join("removed.txt"), "outside removal target").unwrap();
    fs::write(outside.join("kept.txt"), "outside write target").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, repo.join("data")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&outside, repo.join("data")).unwrap();
    let _outcome = Command::new(env!("CARGO_BIN_EXE_crab"))
        .args([
            "exp",
            "apply",
            id,
            "--result-digest",
            &result.digest,
            "--json",
        ])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert_eq!(
        (
            fs::read(outside.join("removed.txt")).ok(),
            fs::read(outside.join("kept.txt")).ok(),
        ),
        (
            Some(b"outside removal target".to_vec()),
            Some(b"outside write target".to_vec())
        ),
    );
}

#[test]
fn result_apply_restores_directory_outputs_with_their_captured_modes() {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    let fixture = fixture();
    let repo = fixture.path().join("repo");
    fs::write(repo.join(".gitignore"), "/.crab/\n").unwrap();
    fs::write(
        repo.join("train.py"),
        "import os\nfrom pathlib import Path\np = Path('model/nested')\np.mkdir(parents=True)\n(p / 'weights.bin').write_bytes(b'directory model')\nif os.name != 'nt': p.chmod(0o700)\n",
    )
    .unwrap();
    fs::write(
        repo.join("crab.yaml"),
        serde_json::to_vec(&json!({
            "defaults":{"env":"empty"},
            "stages":{"train":{"cmd":{"argv":[python(),"train.py"]},
                "deps":["train.py"],"outs":[{"path":"model","kind":"directory"}]}}
        }))
        .unwrap(),
    )
    .unwrap();
    command(&repo, "git", &["add", "."]);
    command(&repo, "git", &["commit", "-m", "Directory output witness"]);
    let created = cli(&repo, &["exp", "run", "--json"]);
    let id = created["exp_id"].as_str().unwrap();
    let result = LocalResults::new(&repo).unwrap().run(id).unwrap();
    fs::create_dir_all(repo.join("model/nested")).unwrap();
    fs::write(repo.join("model/nested/weights.bin"), "today's model").unwrap();
    cli(
        &repo,
        &[
            "exp",
            "apply",
            id,
            "--result-digest",
            &result.digest,
            "--json",
        ],
    );
    assert_eq!(
        fs::read(repo.join("model/nested/weights.bin")).unwrap(),
        b"directory model",
    );
    // POSIX modes are only captured on Unix; retained directory bytes have
    // the same restoration contract on Windows.
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(repo.join("model/nested"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700,
    );
}
