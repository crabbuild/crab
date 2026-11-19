use super::*;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[test]
fn directory_replay_preserves_deleted_children_and_empty_directories() {
    for resume in [true, false] {
        let fixture = fixture();
        let repo = fixture.path().join("repo");
        fs::create_dir(repo.join("state")).unwrap();
        fs::write(repo.join("state/keep.txt"), "seed").unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            repo.join("state/keep.txt"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::write(repo.join("state/stale.txt"), "from base commit").unwrap();
        fs::write(repo.join("train.py"), r#"import json, os, stat, subprocess
from pathlib import Path
state = Path('state')
members = sorted(str(p.relative_to(state)) + ('/' if p.is_dir() else '') for p in state.rglob('*'))
Path('metrics.json').write_text(json.dumps({'members':members, 'mode':stat.S_IMODE(state.joinpath('keep.txt').stat().st_mode)}))
state.joinpath('stale.txt').unlink(missing_ok=True)
state.joinpath('empty').mkdir(exist_ok=True)
subprocess.run([os.environ['CRAB_WORKFLOW_EXECUTABLE'],'workflow','checkpoint'], check=True, stdout=subprocess.DEVNULL)
"#).unwrap();
        fs::write(
            repo.join("crab.yaml"),
            serde_json::to_vec(&json!({
                "defaults":{"env":"empty"},
                "stages":{"train":{"cmd":{"argv":[python(),"train.py"]},
                    "deps":["train.py"], "metrics":["metrics.json"],
                "outs":[{"path":"state","kind":"directory","checkpoint":true}]}}
            }))
            .unwrap(),
        )
        .unwrap();
        command(&repo, "git", &["add", "."]);
        command(&repo, "git", &["commit", "-m", "Directory replay tree"]);
        let created = if resume {
            let first = cli(&repo, &["exp", "run", "--json"]);
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
            fs::remove_file(repo.join("state/stale.txt")).unwrap();
            fs::create_dir(repo.join("state/empty")).unwrap();
            cli(&repo, &["exp", "run", "--copy-paths", "state", "--json"])
        };
        let local = LocalResults::new(&repo).unwrap();
        let mut source = local.run(created["exp_id"].as_str().unwrap()).unwrap();
        let metrics = |run: &SealedResult| {
            let file = run
                .snapshot
                .attachments
                .iter()
                .find(|file| file.path == "metrics.json")
                .unwrap();
            serde_json::from_slice::<Value>(&local.blob(file.hash.as_deref().unwrap()).unwrap())
                .unwrap()
        };
        assert_eq!(metrics(&source)["members"], json!(["empty/", "keep.txt"]));
        #[cfg(unix)]
        assert_eq!(metrics(&source)["mode"], json!(493));
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
            assert_eq!(
                metrics(&replay),
                metrics(&source),
                "resume={resume}, generation={generation}"
            );
            source = replay;
        }
    }
}
