#[path = "workflow_results/transport.rs"]
mod transport;

#[path = "workflow_results/reproduction.rs"]
mod reproduction;

#[path = "workflow_results/plots.rs"]
mod plots;

#[path = "workflow_results/saved.rs"]
mod saved;

#[path = "workflow_results/migration.rs"]
mod migration;

use crab_workflow::{WorkflowStore, results::*};
use object_store::aws::AmazonS3Builder;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

fn command(root: &Path, program: &str, args: &[&str]) -> String {
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn python() -> &'static str {
    static PYTHON: OnceLock<String> = OnceLock::new();
    // Reproduction verifies executable bytes, so fixtures resolve the selected
    // interpreter once and record its absolute path on every platform.
    PYTHON.get_or_init(|| {
        let program = if cfg!(windows) { "python" } else { "python3" };
        let resolved = command(
            Path::new("."),
            program,
            &["-c", "import sys; print(sys.executable)"],
        );
        let resolved = resolved.trim();
        assert!(
            Path::new(resolved).is_absolute(),
            "Python path must be absolute"
        );
        resolved.to_owned()
    })
}

fn cli(root: &Path, args: &[&str]) -> Value {
    let output = command(root, env!("CARGO_BIN_EXE_crab"), args);
    let data = serde_json::from_str::<Value>(&output).unwrap()["data"].clone();
    if args.first() == Some(&"results") {
        let schema: Value =
            serde_json::from_str(include_str!("../schemas/workflow.results.json")).unwrap();
        let validator = jsonschema::JSONSchema::compile(&schema).unwrap();
        if let Err(errors) = validator.validate(&data) {
            panic!(
                "Results payload violates its schema: {:?}",
                errors.map(|error| error.to_string()).collect::<Vec<_>>()
            );
        }
    }
    data
}

fn fixture() -> tempfile::TempDir {
    let target =
        std::env::var_os("CARGO_TARGET_DIR").expect("dedicated external target is required");
    let fixture = tempfile::tempdir_in(target).unwrap();
    let repo = fixture.path().join("repo");
    fs::create_dir(&repo).unwrap();
    command(&repo, "git", &["init", "--initial-branch=main"]);
    command(&repo, "git", &["config", "user.name", "Results fixture"]);
    command(
        &repo,
        "git",
        &["config", "user.email", "results@example.invalid"],
    );
    command(&repo, "git", &["config", "commit.gpgsign", "false"]);
    fixture
}

#[test]
fn admission_keeps_external_digest_when_command_mutates_the_source() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    let external = fixture.path().join("external.txt");
    fs::write(&external, "13").unwrap();
    fs::write(repo.join(".gitignore"), "/.crab/\n").unwrap();
    let url = url::Url::from_file_path(&external).unwrap().to_string();
    let program = "import json,sys; from pathlib import Path; p=Path(sys.argv[1]); value=int(p.read_text()); p.write_text('99'); Path('metrics.json').write_text(json.dumps({'score':value}))";
    fs::write(
        repo.join("crab.yaml"),
        serde_json::to_vec(&json!({
            "defaults":{"env":"empty"},
            "stages":{"evaluate":{"cmd":{"argv":[python(),"-c",program,external]},
                "deps":[{"url":{"url":url}}],"metrics":["metrics.json"]}}
        }))
        .unwrap(),
    )
    .unwrap();
    command(&repo, "git", &["add", "."]);
    command(
        &repo,
        "git",
        &["commit", "-m", "External admission witness"],
    );
    let created = cli(&repo, &["exp", "run", "--json"]);
    let captured = LocalResults::new(&repo)
        .unwrap()
        .run(created["exp_id"].as_str().unwrap())
        .unwrap();
    let input = captured
        .snapshot
        .stages
        .iter()
        .flat_map(|stage| &stage.inputs)
        .next()
        .unwrap();
    assert_eq!(input.hash, blake3::hash(b"13").to_hex().to_string());
    assert!(input.path.is_none());
    assert!(
        !serde_json::to_string(&captured.snapshot)
            .unwrap()
            .contains(url.as_str())
    );
    assert_eq!(fs::read_to_string(external).unwrap(), "99");
}

#[test]
fn empty_private_settings_allow_reproduction_without_retaining_private_values() {
    for (settings, available) in [
        ("# Crab local settings (not committed)\n\n", true),
        ("[workflow]\nenabled = true\n", false),
    ] {
        let fixture = fixture();
        let repo = fixture.path().join("repo");
        fs::create_dir(repo.join(".crab")).unwrap();
        fs::write(repo.join(".crab/local.toml"), settings).unwrap();
        fs::write(repo.join(".gitignore"), "/.crab/\n").unwrap();
        fs::write(
            repo.join("crab.yaml"),
            serde_json::to_vec(&json!({
                "defaults":{"env":"empty"},
                "stages":{"evaluate":{"cmd":{"argv":[python(),"-c","pass"]}}}
            }))
            .unwrap(),
        )
        .unwrap();
        command(&repo, "git", &["add", "."]);
        command(&repo, "git", &["commit", "-m", "Private settings capture"]);
        let created = cli(&repo, &["exp", "run", "--json"]);
        let captured = LocalResults::new(&repo)
            .unwrap()
            .run(created["exp_id"].as_str().unwrap())
            .unwrap();
        let reproduction = captured.snapshot.reproduction.unwrap();
        assert_eq!(reproduction.unavailable_reasons.is_empty(), available);
        assert!(
            reproduction
                .files
                .iter()
                .all(|file| file.path != ".crab/local.toml")
        );
    }
}

#[test]
fn reproduction_retains_materialized_output_inputs_across_generations() {
    for resume in [true, false] {
        let fixture = fixture();
        let repo = fixture.path().join("repo");
        fs::write(repo.join("model.txt"), b"0").unwrap();
        fs::create_dir(repo.join("data")).unwrap();
        fs::File::create(repo.join("data/payload.bin"))
            .unwrap()
            .set_len(crab_workflow::results::MAX_REPRODUCTION_BYTES as u64 / 2 + 1)
            .unwrap();
        fs::write(repo.join("train.py"), r#"import json, os, subprocess
from pathlib import Path
value = int(Path('model.txt').read_text()) + 1
Path('model.txt').write_text(str(value))
Path('metrics.json').write_text(json.dumps({'count':value}))
subprocess.run([os.environ['CRAB_WORKFLOW_EXECUTABLE'],'workflow','checkpoint'], check=True, stdout=subprocess.DEVNULL)
"#).unwrap();
        fs::write(
            repo.join("crab.yaml"),
            serde_json::to_vec(&json!({
                "defaults":{"env":"empty"},
                "stages":{"train":{"cmd":{"argv":[python(),"train.py"]},
                    "deps":["train.py","data"], "metrics":["metrics.json"],
                    "outs":[{"path":"model.txt","checkpoint":true}]}}
            }))
            .unwrap(),
        )
        .unwrap();
        command(&repo, "git", &["add", "."]);
        command(&repo, "git", &["commit", "-m", "Checkpoint replay inputs"]);
        let local = LocalResults::new(&repo).unwrap();
        let created = if resume {
            let first = cli(&repo, &["exp", "run", "--json"]);
            fs::write(repo.join("model.txt"), b"100").unwrap();
            command(&repo, "git", &["add", "model.txt"]);
            command(
                &repo,
                "git",
                &["commit", "-m", "Different model in current HEAD"],
            );
            cli(
                &repo,
                &[
                    "exp",
                    "run",
                    "--resume",
                    first["exp_id"].as_str().unwrap(),
                    "--json",
                ],
            )
        } else {
            fs::write(repo.join("model.txt"), b"100").unwrap();
            cli(
                &repo,
                &["exp", "run", "--copy-paths", "model.txt", "--json"],
            )
        };
        let mut source = local.run(created["exp_id"].as_str().unwrap()).unwrap();
        let expected = if resume {
            b"2".as_slice()
        } else {
            b"101".as_slice()
        };
        let seed = if resume {
            b"1".as_slice()
        } else {
            b"100".as_slice()
        };
        for generation in 0..2 {
            let replay = cli(
                &repo,
                &[
                    "exp",
                    "reproduce",
                    &source.snapshot.experiment_id,
                    "--digest",
                    &source.digest,
                    "--json",
                ],
            );
            let replay = local.run(replay["exp_id"].as_str().unwrap()).unwrap();
            let output = replay
                .snapshot
                .attachments
                .iter()
                .find(|file| file.path == "model.txt")
                .unwrap();
            assert_eq!(
                local.blob(output.hash.as_deref().unwrap()).unwrap(),
                expected,
                "resume={resume}, generation={generation}"
            );
            let intent = replay.snapshot.reproduction.as_ref().unwrap();
            assert!(intent.unavailable_reasons.is_empty());
            let input = intent
                .files
                .iter()
                .find(|file| file.path == "model.txt")
                .unwrap();
            assert_eq!(local.blob(input.hash.as_deref().unwrap()).unwrap(), seed);
            assert_eq!(
                intent.source_result,
                Some(RunSelection {
                    id: source.snapshot.experiment_id,
                    digest: source.digest,
                })
            );
            source = replay;
        }
    }
}

#[test]
fn reproduction_regenerates_selected_producer_outputs_from_retained_source_inputs() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    fs::write(repo.join(".gitignore"), "/.crab/\n/work/raw/\n").unwrap();
    fs::create_dir_all(repo.join("work/raw")).unwrap();
    fs::write(repo.join("work/.keep"), "").unwrap();
    fs::write(repo.join("work/raw/value.txt"), "7").unwrap();
    let admitted_directory = crab_workflow::hasher::hash_directory(&repo.join("work/raw"), true)
        .unwrap()
        .hash;
    let prepare = "from pathlib import Path; p=Path('features'); p.mkdir(); p.joinpath('train.txt').write_text(Path('raw/value.txt').read_text()); Path('ready.txt').write_text('ready'); Path('raw/value.txt').write_text('mutated during execution')";
    let train = "import json; from pathlib import Path; Path('metrics.json').write_text(json.dumps({'score':int(Path('features/train.txt').read_text()),'marker':Path('ready.txt').read_text()}))";
    fs::write(repo.join("crab.yaml"), serde_json::to_vec(&json!({
        "defaults":{"env":"empty"},
        "stages":{
            "prepare":{"wdir":"work","cmd":{"argv":[python(),"-c",prepare]},
                "deps":["raw"], "outs":[{"path":"features","kind":"directory"},"ready.txt"]},
            "train":{"wdir":"work","cmd":{"argv":[python(),"-c",train]},
                "deps":["features/train.txt",{"stage_out":{"stage":"prepare","out":"work/ready.txt"}}],
                "metrics":["metrics.json"]}
        }
    })).unwrap()).unwrap();
    command(&repo, "git", &["add", "."]);
    command(
        &repo,
        "git",
        &["commit", "-m", "Pipeline with generated inputs"],
    );
    let created = cli(&repo, &["exp", "run", "--copy-paths", "work/raw", "--json"]);
    let local = LocalResults::new(&repo).unwrap();
    let captured = local.run(created["exp_id"].as_str().unwrap()).unwrap();
    let prepare_stage = captured
        .snapshot
        .stages
        .iter()
        .find(|stage| stage.name == "prepare")
        .unwrap();
    assert_eq!(
        prepare_stage
            .inputs
            .iter()
            .find(|input| input.path.as_deref() == Some("work/raw"))
            .unwrap()
            .hash,
        blake3::Hash::from(admitted_directory).to_hex().to_string()
    );
    let train_stage = captured
        .snapshot
        .stages
        .iter()
        .find(|stage| stage.name == "train")
        .unwrap();
    assert_eq!(
        train_stage
            .inputs
            .iter()
            .find(|input| input.path.as_deref() == Some("work/features/train.txt"))
            .unwrap()
            .hash,
        blake3::hash(b"7").to_hex().to_string()
    );
    assert_eq!(
        train_stage
            .inputs
            .iter()
            .find(|input| input.path.as_deref() == Some("prepare:work/ready.txt"))
            .unwrap()
            .hash,
        blake3::hash(b"ready").to_hex().to_string()
    );
    let spec = captured.snapshot.reproduction.as_ref().unwrap();
    assert!(
        spec.unavailable_reasons.is_empty(),
        "{:?}",
        spec.unavailable_reasons
    );
    assert!(
        spec.files
            .iter()
            .all(|file| !file.path.starts_with("work/features") && file.path != "work/ready.txt")
    );
    let input = spec
        .files
        .iter()
        .find(|file| file.path == "work/raw/value.txt")
        .unwrap();
    assert_eq!(local.blob(input.hash.as_deref().unwrap()).unwrap(), b"7");
    assert_eq!(
        captured
            .snapshot
            .metrics
            .iter()
            .find(|metric| metric.pointer == "/score")
            .unwrap()
            .value,
        json!(7)
    );
    fs::write(repo.join("work/raw/value.txt"), "99").unwrap();
    fs::write(repo.join("revision.txt"), "new HEAD").unwrap();
    command(&repo, "git", &["add", "."]);
    command(
        &repo,
        "git",
        &["commit", "-m", "Change current pipeline inputs"],
    );
    let recreated = cli(
        &repo,
        &[
            "exp",
            "reproduce",
            &captured.snapshot.experiment_id,
            "--digest",
            &captured.digest,
            "--json",
        ],
    );
    let replay = local.run(recreated["exp_id"].as_str().unwrap()).unwrap();
    assert_eq!(replay.snapshot.metrics, captured.snapshot.metrics);
}

#[test]
fn reproduction_retains_overridden_params_and_copied_inputs_before_commands_mutate_them() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    fs::write(repo.join(".gitignore"), "/overlay.txt\n/.crab/\n").unwrap();
    fs::write(repo.join("params.json"), r#"{"factor":1}"#).unwrap();
    fs::write(repo.join("overlay.txt"), "copied admission bytes").unwrap();
    fs::write(
        repo.join("train.py"),
        r#"import json
from pathlib import Path
factor = json.loads(Path('params.json').read_text())['factor']
overlay = Path('overlay.txt').read_text()
Path('params.json').write_text('{"factor":99}')
Path('overlay.txt').write_text('mutated during execution')
Path('metrics.json').write_text(json.dumps({'score':factor,'overlay':overlay}))
Path('model.txt').write_text(str(factor))
"#,
    )
    .unwrap();
    fs::write(
        repo.join("crab.yaml"),
        serde_json::to_vec(&json!({
            "defaults":{"env":"empty"}, "params":["params.json"],
            "stages":{"train":{"cmd":{"argv":[python(),"train.py"]},
                "deps":["train.py","overlay.txt"], "params":[{"params.json":["factor"]}],
                "metrics":["metrics.json"], "outs":["model.txt"]}}
        }))
        .unwrap(),
    )
    .unwrap();
    command(&repo, "git", &["add", "."]);
    command(
        &repo,
        "git",
        &["commit", "-m", "Retain effective execution inputs"],
    );
    let created = cli(
        &repo,
        &[
            "exp",
            "run",
            "-S",
            "params.json:factor=2",
            "--copy-paths",
            "overlay.txt",
            "--json",
        ],
    );
    let id = created["exp_id"].as_str().unwrap();
    let local = LocalResults::new(&repo).unwrap();
    let captured = local.run(id).unwrap();
    let effective = captured.snapshot.reproduction.as_ref().unwrap();
    assert!(effective.unavailable_reasons.is_empty());
    let bytes = |path: &str| {
        let attachment = effective
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap();
        local.blob(attachment.hash.as_deref().unwrap()).unwrap()
    };
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes("params.json")).unwrap()["factor"],
        2
    );
    assert_eq!(bytes("overlay.txt"), b"copied admission bytes");
    let observed = captured
        .snapshot
        .stages
        .iter()
        .flat_map(|stage| &stage.inputs)
        .find(|input| input.path.as_deref() == Some("overlay.txt"))
        .unwrap();
    assert_eq!(
        observed.hash,
        blake3::hash(b"copied admission bytes").to_hex().to_string()
    );
    assert_eq!(
        captured
            .snapshot
            .parameters
            .iter()
            .find(|datum| datum.pointer == "/factor")
            .unwrap()
            .value,
        json!(2)
    );
    fs::write(repo.join("params.json"), r#"{"factor":9}"#).unwrap();
    fs::write(repo.join("overlay.txt"), "changed caller bytes").unwrap();
    let reproduced = cli(
        &repo,
        &[
            "exp",
            "reproduce",
            id,
            "--digest",
            &captured.digest,
            "--json",
        ],
    );
    let replay = local.run(reproduced["exp_id"].as_str().unwrap()).unwrap();
    assert_eq!(replay.snapshot.metrics, captured.snapshot.metrics);
    assert_eq!(
        fs::read_to_string(repo.join("overlay.txt")).unwrap(),
        "changed caller bytes"
    );
}

#[test]
fn failed_immediate_and_queued_runs_retain_partial_evidence_after_cleanup() {
    for queued in [false, true] {
        let fixture = fixture();
        let repo = fixture.path().join("repo");
        fs::write(
            repo.join("train.py"),
            r#"import json, sys
from pathlib import Path
Path('metrics.json').write_text(json.dumps({'score':0,'nested':{'loss':0.25}}))
Path('curve.csv').write_text('step,loss\n0,4\n1,2\n')
print('partial evidence before failure', flush=True)
sys.exit(7)
"#,
        )
        .unwrap();
        fs::write(
            repo.join("crab.yaml"),
            serde_json::to_vec(&json!({
                "defaults":{"env":"empty"},
                "plots":[{"curve.csv":{"x":"step","y":"loss"}}],
                "stages":{"train":{"cmd":{"argv":[python(),"train.py"]},
                    "deps":["train.py"], "metrics":["metrics.json"], "plots":["curve.csv"],
                    "outs":["model.txt"]}}
            }))
            .unwrap(),
        )
        .unwrap();
        command(&repo, "git", &["add", "."]);
        command(
            &repo,
            "git",
            &["commit", "-m", "Fail after writing evidence"],
        );
        if queued {
            cli(&repo, &["exp", "run", "--queue", "--json"]);
            assert_eq!(
                cli(&repo, &["exp", "start", "--jobs", "1", "--json"])["failed"],
                1
            );
        } else {
            let output = Command::new(env!("CARGO_BIN_EXE_crab"))
                .args(["exp", "run", "--json"])
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(!output.status.success());
        }
        let local = LocalResults::new(&repo).unwrap();
        let runs = local.list().unwrap();
        assert_eq!(runs.len(), 1, "queued={queued}");
        let run = &runs[0];
        assert_eq!(run.snapshot.status, "failed");
        assert_eq!(
            run.snapshot
                .metrics
                .iter()
                .find(|metric| metric.pointer == "/score")
                .unwrap()
                .value,
            json!(0)
        );
        assert!(
            run.snapshot
                .attachments
                .iter()
                .any(|attachment| attachment.kind == AttachmentKind::Log
                    && attachment
                        .hash
                        .as_ref()
                        .is_some_and(|hash| String::from_utf8_lossy(&local.blob(hash).unwrap())
                            .contains("partial evidence before failure")))
        );
        assert_eq!(charts(&runs, |hash| local.blob(hash)).unwrap()[0].points, 2);
        assert!(
            !repo
                .join(".crab/workflow/exp")
                .join(&run.snapshot.experiment_id)
                .exists()
        );
        assert!(
            !repo
                .join(".crab/workflow/exp")
                .join(format!("{}.workspace", run.snapshot.experiment_id))
                .exists()
        );
    }
}

#[test]
fn interrupted_queue_run_retains_partial_evidence_and_preserves_completed_runs() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    fs::write(repo.join("params.json"), r#"{"pause":false}"#).unwrap();
    fs::write(
        repo.join("train.py"),
        r#"import json, time
from pathlib import Path
pause = json.loads(Path('params.json').read_text())['pause']
Path('metrics.json').write_text(json.dumps({'score':0.2 if pause else 0.1}))
Path('curve.csv').write_text('step,loss\n0,4\n1,2\n')
Path('model.txt').write_text('partial' if pause else 'complete')
print('evidence ready', flush=True)
if pause:
    time.sleep(10)
    print('unexpected completion', flush=True)
"#,
    )
    .unwrap();
    fs::write(
        repo.join("crab.yaml"),
        serde_json::to_vec(&json!({
            "defaults":{"env":"empty"}, "params":["params.json"],
            "plots":[{"curve.csv":{"x":"step","y":"loss"}}],
            "stages":{"train":{"cmd":{"argv":[python(),"train.py"]},
                "deps":["train.py"], "params":[{"params.json":["pause"]}],
                "metrics":["metrics.json"], "plots":["curve.csv"], "outs":["model.txt"]}}
        }))
        .unwrap(),
    )
    .unwrap();
    command(&repo, "git", &["add", "."]);
    command(
        &repo,
        "git",
        &["commit", "-m", "Interrupt after retaining partial evidence"],
    );
    let completed = cli(&repo, &["exp", "run", "--json"]);
    let local = LocalResults::new(&repo).unwrap();
    let completed = local.run(completed["exp_id"].as_str().unwrap()).unwrap();
    let queued = cli(
        &repo,
        &["exp", "run", "--queue", "-S", "pause=true", "--json"],
    );
    let id = queued["experiment_ids"][0].as_str().unwrap();
    let mut worker = Command::new(env!("CARGO_BIN_EXE_crab"))
        .args(["exp", "start", "--json"])
        .current_dir(&repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut ready = false;
    while Instant::now() < deadline {
        let output = Command::new(env!("CARGO_BIN_EXE_crab"))
            .args(["queue", "logs", id, "--json"])
            .current_dir(&repo)
            .output()
            .unwrap();
        // Pending entries and not-yet-created active logs are unavailable;
        // wait for actual stage output before requesting interruption.
        if output.status.success() {
            let logs: Value = serde_json::from_slice(&output.stdout).unwrap();
            if logs["data"]["contents"]
                .as_str()
                .is_some_and(|text| text.contains("evidence ready"))
            {
                ready = true;
                break;
            }
        }
        if worker.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut cancel = Command::new(env!("CARGO_BIN_EXE_crab"));
    cancel
        .args(["queue", "kill", id, "--json"])
        .current_dir(&repo);
    if !ready {
        cancel.arg("--force");
    }
    let cancelled = cancel.output().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut stopped = false;
    while Instant::now() < deadline {
        if worker.try_wait().unwrap().is_some() {
            stopped = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !stopped {
        let _ = worker.kill();
    }
    let output = worker.wait_with_output().unwrap();
    assert!(
        ready,
        "queued stage did not produce evidence before cancellation: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        cancelled.status.success(),
        "{}",
        String::from_utf8_lossy(&cancelled.stderr)
    );
    assert!(
        stopped && output.status.success(),
        "queue cancellation did not complete: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let interrupted = local.run(id).unwrap();
    assert_eq!(interrupted.snapshot.status, "failed");
    assert_eq!(
        interrupted
            .snapshot
            .metrics
            .iter()
            .find(|metric| metric.pointer == "/score")
            .unwrap()
            .value,
        json!(0.2)
    );
    let retained = interrupted
        .snapshot
        .attachments
        .iter()
        .filter(|attachment| attachment.kind == AttachmentKind::Log)
        .filter_map(|attachment| attachment.hash.as_ref())
        .map(|hash| String::from_utf8_lossy(&local.blob(hash).unwrap()).into_owned())
        .collect::<String>();
    assert!(retained.contains("evidence ready") && !retained.contains("unexpected completion"));
    assert_eq!(
        local.run(&completed.snapshot.experiment_id).unwrap(),
        completed
    );
    assert!(!repo.join(".crab/workflow/exp").join(id).exists());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an owned S3-compatible bucket with conditional writes"]
async fn queued_runs_retain_real_results_and_reports_survive_cold_remote_retrieval() {
    let fixture = fixture();
    let repo = fixture.path().join("repo");
    fs::write(repo.join("params.json"), r#"{"factor":1}"#).unwrap();
    fs::write(repo.join("dataset.csv"), "x,y\n1,2\n").unwrap();
    fs::write(
        repo.join("train.py"),
        r##"import json
from pathlib import Path
factor = json.loads(Path('params.json').read_text())['factor']
Path('metrics.json').write_text(json.dumps({'score':factor / 10,'loss':1 / factor}))
Path('curve.csv').write_text('step,loss\n0,'+str(4/factor)+'\n1,'+str(2/factor)+'\n')
Path('curve2.csv').write_text('step,loss\n0,'+str(3/factor)+'\n1,'+str(1/factor)+'\n')
Path('prediction.svg').write_text('<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100"><rect width="'+str(factor*50)+'" height="100" fill="#3b82f6"/></svg>')
Path('model.txt').write_text('model-'+str(factor))
print('trained factor',factor)
"##,
    )
    .unwrap();
    fs::write(
        repo.join("crab.yaml"),
        serde_json::to_vec(&json!({
            "params":["params.json"], "defaults":{"env":"empty"},
            "artifacts":{"model":{"path":"model.txt","kind":"model"}},
            "plots":[{"curve.csv":{"x":"step","y":"loss"}},{"curve2.csv":{"x":"step","y":"loss","title":"Validation loss"}},"prediction.svg"],
            "stages":{"train":{"cmd":{"argv":[python(),"train.py"]},
                "deps":["train.py","dataset.csv"], "params":[{"params.json":["factor"]}],
                "metrics":["metrics.json"], "plots":["curve.csv","curve2.csv","prediction.svg"], "outs":["model.txt"]}}
        }))
        .unwrap(),
    )
    .unwrap();
    command(&repo, "git", &["add", "."]);
    command(&repo, "git", &["commit", "-m", "Results fixture"]);
    let queued = cli(
        &repo,
        &["exp", "queue", "--set-param", "factor=1,2", "--json"],
    );
    let ids = queued["experiment_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 2);
    cli(&repo, &["exp", "start", "--jobs", "2", "--json"]);
    let local = LocalResults::new(&repo).unwrap();
    let runs = ids
        .iter()
        .map(|id| local.run(id).unwrap())
        .collect::<Vec<_>>();
    let metric = |run: &SealedResult| {
        run.snapshot
            .metrics
            .iter()
            .find(|v| v.pointer == "/score")
            .unwrap()
            .value
            .clone()
    };
    assert_eq!(
        runs.iter().map(metric).collect::<Vec<_>>(),
        vec![json!(0.1), json!(0.2)]
    );
    let overlays = charts(&runs, |hash| local.blob(hash)).unwrap();
    assert_eq!(overlays[0].points, 4);
    assert!(overlays[0].spec.is_some(), "{:?}", overlays[0].reason);
    assert!(runs.iter().all(|run| {
        run.snapshot
            .attachments
            .iter()
            .any(|a| a.kind == AttachmentKind::Log && a.state == Availability::Present)
    }));
    assert!(
        runs[0]
            .snapshot
            .reproduction
            .as_ref()
            .unwrap()
            .unavailable_reasons
            .is_empty(),
        "{:?}",
        runs[0].snapshot.reproduction
    );
    cli(
        &repo,
        &[
            "exp",
            "apply",
            &ids[0],
            "--result-digest",
            &runs[0].digest,
            "--json",
        ],
    );
    assert_eq!(
        fs::read_to_string(repo.join("model.txt")).unwrap(),
        "model-1"
    );
    command(&repo, "git", &["add", "."]);
    command(&repo, "git", &["commit", "-m", "Apply observed model"]);
    let version = cli(
        &repo,
        &["artifacts", "version", "create", "model", "--json"],
    );
    let version = version["manifest"]["version_id"].as_str().unwrap();
    let promotion_args = [
        "results",
        "promote",
        &ids[0],
        "--digest",
        &runs[0].digest,
        "--name",
        "model",
        "--version",
        version,
        "--stage",
        "staging",
        "--json",
    ];
    let promoted = cli(&repo, &promotion_args);
    assert_eq!(promoted["version_id"], version);
    let mut conflict = promotion_args.to_vec();
    let wrong = format!("b3:{}", "f".repeat(64));
    conflict.extend(["--expected", &wrong]);
    let output = Command::new(env!("CARGO_BIN_EXE_crab"))
        .args(conflict)
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(!output.status.success(), "CAS conflict must fail");
    let history = cli(&repo, &["artifacts", "history", "model", "--json"]);
    assert!(history.to_string().contains(version));
    fs::write(repo.join("params.json"), r#"{"factor":99}"#).unwrap();
    fs::write(
        repo.join("train.py"),
        "raise RuntimeError('current HEAD must not execute')\n",
    )
    .unwrap();
    command(&repo, "git", &["add", "params.json", "train.py"]);
    command(
        &repo,
        "git",
        &["commit", "-m", "Move HEAD after original experiments"],
    );
    let reproduced = cli(
        &repo,
        &[
            "exp",
            "reproduce",
            &ids[0],
            "--digest",
            &runs[0].digest,
            "--json",
        ],
    );
    let reproduction = local.run(reproduced["exp_id"].as_str().unwrap()).unwrap();
    assert_eq!(metric(&reproduction), metric(&runs[0]));
    assert_eq!(
        reproduction.snapshot.base_commit,
        runs[0].snapshot.base_commit
    );
    assert_eq!(
        reproduction
            .snapshot
            .reproduction
            .as_ref()
            .unwrap()
            .source_result,
        Some(RunSelection {
            id: ids[0].clone(),
            digest: runs[0].digest.clone()
        })
    );
    let key = overlays[0].interaction.as_ref().unwrap().key.clone();
    let comparison = compare(
        &runs,
        &ComparisonSpec {
            baseline: ids[0].clone(),
            axes: std::collections::BTreeMap::from([(
                key,
                AxisRange {
                    lower: 0.0,
                    upper: 0.5,
                },
            )]),
            ..Default::default()
        },
    )
    .unwrap();
    let report = Report::new("Actual queued results".into(), comparison, overlays.clone()).unwrap();
    let id = local.save_report(&report).unwrap();
    cli(
        &repo,
        &["exp", "rename", &ids[0], "renamed after report", "--json"],
    );
    assert_ne!(local.run(&ids[0]).unwrap().digest, runs[0].digest);
    assert_eq!(local.report(&id).unwrap(), report);
    let output = Command::new(env!("CARGO_BIN_EXE_crab"))
        .args([
            "exp",
            "apply",
            &ids[0],
            "--result-digest",
            &runs[0].digest,
            "--json",
        ])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "historical result cannot apply mutable experiment state"
    );

    let bucket = std::env::var("CRAB_SDK_TEST_S3_BUCKET").expect("owned fixture bucket");
    let store = AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_allow_http(true)
        .build()
        .unwrap();
    let prefix = format!("results-{}", crab_workflow::ExperimentId::new_v7());
    let store = crab_storage::Store::new(Arc::new(store));
    let remote = RemoteResults::new(
        WorkflowStore::from_storage(store.clone()),
        &prefix,
        CancellationToken::new(),
    );
    let cli_store = crab::storage::Store::from_storage(store.clone());
    let router = crab::storage::StoreLayout::new(cli_store.clone(), prefix.clone());
    let layout = crab_storage::StoreLayout::with_global_prefix(
        store.clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    for run in &runs {
        remote.push(&local, run).await.unwrap();
    }
    remote.push_report(&local, &report).await.unwrap();
    for run in &runs {
        remote
            .remove_run(&run.snapshot.experiment_id)
            .await
            .unwrap();
    }
    assert!(remote.list().await.unwrap().is_empty());
    // Exercise the production repository GC entry, never bucket-wide GC.
    crab::cmd::gc::run_repo_remote_gc(
        &crab::cmd::gc::GcArgs::default(),
        &cli_store,
        &router,
        &std::collections::HashSet::new(),
        &CancellationToken::new(),
        std::time::Duration::ZERO,
        None,
    )
    .await
    .unwrap();
    // No local object/index/metadata is reused by the receiving application.
    let cold = fixture.path().join("cold");
    fs::create_dir(&cold).unwrap();
    let recovered = LocalResults::new(&cold).unwrap();
    let fetched = remote.pull_report(&recovered, &id).await.unwrap();
    assert_eq!(fetched, report);
    assert!(recovered.list().unwrap().is_empty());
    for run in &runs {
        local.remove_run(&run.snapshot.experiment_id).unwrap();
    }
    let reopened = LocalResults::new(&cold).unwrap();
    let view = ResultsReader::Local(reopened)
        .read(ResultsRead::Report { id: id.clone() })
        .await
        .unwrap();
    let ResultsResponse::Report(view) = view else {
        panic!("expected report")
    };
    assert_eq!(view.charts[0].points, 4);
    let html = report_html(&fetched, |hash| recovered.blob(hash)).unwrap();
    fs::write(fixture.path().join("report.html"), &html).unwrap();
    if let Some(output) = std::env::var_os("CRAB_RESULTS_EVIDENCE") {
        let output = std::path::PathBuf::from(output);
        fs::create_dir_all(&output).unwrap();
        fs::write(output.join("report.html"), html).unwrap();
        fs::write(
            output.join("report.json"),
            serde_json::to_vec_pretty(&fetched).unwrap(),
        )
        .unwrap();
        fs::write(
            output.join("charts.json"),
            serde_json::to_vec_pretty(&view.charts).unwrap(),
        )
        .unwrap();
        let provenance_runs = [runs[0].clone(), reproduction.clone()];
        let provenance = Report::new(
            "Reproduction provenance".into(),
            compare(
                &provenance_runs,
                &ComparisonSpec {
                    baseline: ids[0].clone(),
                    ..Default::default()
                },
            )
            .unwrap(),
            charts(&provenance_runs, |hash| local.blob(hash)).unwrap(),
        )
        .unwrap();
        fs::write(
            output.join("reproduction-report.json"),
            serde_json::to_vec_pretty(&json!({"id":provenance.id().unwrap(),"report":provenance}))
                .unwrap(),
        )
        .unwrap();
        // The HTTP journey installs the actual captured runs, including bodies
        // excluded from reports, before testing the render-only report closure.
        let bodies: std::collections::BTreeMap<_, _> = runs
            .iter()
            .flat_map(|run| run.snapshot.evidence())
            .filter(|attachment| attachment.state == Availability::Present)
            .map(|attachment| {
                let hash = attachment.hash.as_ref().unwrap();
                (hash, local.blob(hash).unwrap())
            })
            .collect();
        fs::write(
            output.join("run-bodies.json"),
            serde_json::to_vec(&bodies).unwrap(),
        )
        .unwrap();
    }
    assert!(
        fs::read_to_string(fixture.path().join("report.html"))
            .unwrap()
            .contains("vegaEmbed")
    );
}
