use super::*;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

fn isolated_git(directory: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(directory);
    for key in process::GIT_ENV_REMOVALS {
        command.env_remove(key);
    }
    command
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        );
    command
}

fn slow_indexer(directory: &Path, started: &Path, late: &Path) -> Command {
    let mut command = isolated_git(directory);
    command
        .args([
            "-c",
            "alias.crab-index-probe=!printf ready > \"$CRAB_INDEX_STARTED\"; sleep 5; printf late > \"$CRAB_INDEX_LATE\"",
            "crab-index-probe",
        ])
        .env("CRAB_INDEX_STARTED", started)
        .env("CRAB_INDEX_LATE", late);
    command
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_drains_indexer_tree_before_releasing_workspace() {
    let observations = tempfile::tempdir().unwrap();
    let started = observations.path().join("started");
    let late = observations.path().join("late");
    let workspace = Arc::new(tempfile::tempdir().unwrap());
    let retained = Arc::downgrade(&workspace);
    let command = slow_indexer(observations.path(), &started, &late);
    let result = run_install(
        Some(Duration::from_secs(2)),
        "fixture".to_owned(),
        move |cancel| {
            run_indexer(command, None, &cancel)?;
            Err(crab_git::pack::PackError::IndexMissing {
                path: workspace.path().join("unfinished.idx"),
            })
        },
    )
    .await;

    assert_eq!(
        (
            matches!(result, Err(CrabError::Internal(_))),
            retained.upgrade().is_none(),
            started.exists(),
            late.exists()
        ),
        (true, true, true, false),
        "deadline must reap descendants before returning and releasing scratch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_install_cancels_indexer_and_retains_workspace_until_exit() {
    let observations = tempfile::tempdir().unwrap();
    let started = observations.path().join("started");
    let late = observations.path().join("late");
    let workspace = Arc::new(tempfile::tempdir().unwrap());
    let retained = Arc::downgrade(&workspace);
    let command = slow_indexer(observations.path(), &started, &late);
    let operation = tokio::spawn(run_install(None, "fixture".to_owned(), move |cancel| {
        run_indexer(command, None, &cancel)?;
        Err(crab_git::pack::PackError::IndexMissing {
            path: workspace.path().join("unfinished.idx"),
        })
    }));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    operation.abort();
    let cancelled = operation.await.unwrap_err().is_cancelled();
    tokio::time::timeout(Duration::from_secs(10), async {
        while retained.upgrade().is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    assert_eq!((cancelled, late.exists()), (true, false));
}

fn git_output(directory: &Path, args: &[&str], input: Option<File>) -> Output {
    let mut command = isolated_git(directory);
    command.args(args);
    let output = run_indexer(command, input, &CancellationToken::new()).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervised_full_and_external_thin_installs_preserve_object_bytes() {
    use crab_git::incoming_pack::{
        ExternalDeltaBase, GeneratedObject, IncomingPack, ReceiveLimits,
    };

    let fixture = tempfile::tempdir().unwrap();
    let base_data = vec![b'a'; 16 * 1024];
    let mut target_data = base_data.clone();
    target_data[7_000..7_100].fill(b'b');
    let base = GeneratedObject::new(gix_object::Kind::Blob, base_data.clone())
        .unwrap()
        .oid();
    let target = GeneratedObject::new(gix_object::Kind::Blob, target_data.clone())
        .unwrap()
        .oid();
    for thin in [false, true] {
        let incoming = IncomingPack::from_generated_objects(
            [(gix_object::Kind::Blob, target_data.clone())],
            fixture.path(),
            ReceiveLimits {
                max_pack_bytes: 16 * 1024 * 1024,
                max_objects: 4,
                max_object_bytes: 2 * 1024 * 1024,
                max_inflated_bytes: 4 * 1024 * 1024,
                max_delta_depth: 8,
            },
            || false,
        )
        .unwrap();
        let (bases, external) = if thin {
            (
                BTreeMap::from([(target, base)]),
                BTreeMap::from([(
                    target,
                    ExternalDeltaBase::new(base, gix_object::Kind::Blob, base_data.clone(), 0),
                )]),
            )
        } else {
            (BTreeMap::new(), BTreeMap::new())
        };
        let prepared = incoming
            .prepare_with_external_delta_bases(
                fixture.path(),
                16 * 1024 * 1024,
                &AtomicBool::new(false),
                &bases,
                &external,
                8,
                2 * 1024 * 1024,
            )
            .unwrap()
            .unwrap();
        assert_eq!(prepared.external_delta_count(), u32::from(thin));
        let destination = fixture
            .path()
            .join(if thin { "thin.git" } else { "full.git" });
        git_output(
            fixture.path(),
            &["init", "--bare", destination.to_str().unwrap()],
            None,
        );
        let input = fixture.path().join("base-input");
        std::fs::write(&input, &base_data).unwrap();
        git_output(
            &destination,
            &["hash-object", "-w", "--stdin"],
            Some(File::open(input).unwrap()),
        );
        let pack_dir = destination.join("objects/pack");
        let installed = if thin {
            install_thin_pack_file_locally_with_timeout(
                &pack_dir,
                prepared.pack_path(),
                "fixture",
                0,
                true,
            )
            .await
        } else {
            install_pack_file_locally_with_timeout(
                &pack_dir,
                prepared.pack_path(),
                "fixture",
                0,
                true,
            )
            .await
        }
        .unwrap();
        let output = git_output(
            &destination,
            &["cat-file", "blob", &target.to_string()],
            None,
        );
        let locations = crab_git::pack_locator::PackLocationIter::open(
            &installed.idx_path,
            &installed.rev_path,
            std::fs::metadata(&installed.pack_path).unwrap().len(),
        )
        .unwrap();
        assert_eq!(
            (output.stdout, locations.object_count()),
            (target_data.clone(), if thin { 2 } else { 1 })
        );
    }
}
