use std::process::Command;
use std::sync::{Arc, Mutex};

use crab_storage::{StorageObservation, StorageObserver, StorageOperation};
use object_store::memory::InMemory;
use sha2::{Digest, Sha256};

use super::*;

mod cancellation {
    use super::*;

    #[tokio::test]
    async fn cancellation_during_preparation_does_not_publish_direct_or_coordinated_refs() {
        for coordinated in [false, true] {
            let hooked = Arc::new(crab_storage::test_support::CountingObjectStore::new(
                Arc::new(InMemory::new()),
            ));
            let layout = crab_storage::StoreLayout::new(
                crab_storage::Store::new(hooked.clone()),
                "repos/cancel".to_owned(),
            );
            let base = crab_write::capsule_protocol::initialize(
                &layout,
                &"1".repeat(64),
                "refs/heads/main",
            )
            .await
            .unwrap();
            let transaction = crab_metadata::capsule_protocol::CapsuleTransaction::new(
                base.record().digest(),
                vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                    "refs/heads/main",
                    None,
                    Some("2".repeat(40)),
                    None,
                )],
            )
            .unwrap();
            let capsule = crab_metadata::capsule_protocol::Capsule::build(
                &transaction,
                Vec::new(),
                Vec::new(),
            )
            .unwrap();
            let coordinator = Arc::new(crab_coordination::InMemoryWriteCoordinator::new());
            let config = if coordinated {
                PushConfig {
                    active_active_replication: Some(active_active_replication()),
                    active_active_writer: Some("east".to_owned()),
                    active_active_coordinator: Some(
                        crate::git::push::ActiveActiveWriteCoordinator::new(coordinator.clone()),
                    ),
                    ..PushConfig::default()
                }
            } else {
                PushConfig::default()
            };
            let cancel = CancellationToken::new();
            hooked.cancel_after_put(layout.repo_path("v2/capsules"), cancel.clone());
            let result = publish_capsule(
                &config,
                &layout,
                base,
                &transaction,
                &capsule,
                &[],
                &BTreeMap::new(),
                &cancel,
            )
            .await;
            assert!(
                matches!(result, Err(CrabError::Cancelled)),
                "coordinated={coordinated}"
            );
            assert!(
                crab_read::capsule_protocol::read_visible_refs(&layout)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                coordinator
                    .repair_snapshot()
                    .await
                    .unwrap()
                    .materialization_gaps
                    .is_empty()
            );
        }
    }
}

#[derive(Default)]
struct RecordingObserver {
    observations: Mutex<Vec<StorageObservation>>,
}

impl RecordingObserver {
    fn count(&self) -> usize {
        self.observations.lock().expect("observer lock").len()
    }
}

impl StorageObserver for RecordingObserver {
    fn started(&self, _operation: StorageOperation) {}

    fn finished(&self, observation: StorageObservation) {
        self.observations
            .lock()
            .expect("observer lock")
            .push(observation);
    }
}

fn git(repository: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("Git output is UTF-8")
        .trim()
        .to_owned()
}

fn commit(repository: &Path, contents: &str) -> String {
    std::fs::write(repository.join("tracked.txt"), contents).expect("write fixture");
    git(repository, &["add", "tracked.txt"]);
    git(repository, &["commit", "-m", contents]);
    git(repository, &["rev-parse", "HEAD"])
}

async fn publish_ref_edits_with_visibility(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    git_dir: &Path,
    edits: Vec<crab_metadata::capsule_protocol::CapsuleRefEdit>,
) {
    let root = crab_write::capsule_protocol::open_root(layout)
        .await
        .expect("open root");
    let visibility = prepare_visibility_delta(
        git_dir,
        &edits,
        &BTreeSet::new(),
        None,
        &CancellationToken::new(),
    )
    .expect("prepare Git visibility");
    let transaction =
        crab_metadata::capsule_protocol::CapsuleTransaction::new(root.record().digest(), edits)
            .expect("build ref transaction");
    let sections = visibility
        .map(|visibility| {
            visibility
                .encode()
                .map(|bytes| {
                    vec![crab_metadata::capsule_protocol::CapsuleSection::new(
                        crab_metadata::capsule_protocol::CapsuleSectionKind::VisibilityDelta,
                        bytes,
                    )]
                })
                .expect("encode Git visibility")
        })
        .unwrap_or_default();
    let capsule =
        crab_metadata::capsule_protocol::Capsule::build(&transaction, Vec::new(), sections)
            .expect("build ref capsule");
    crab_write::capsule_protocol::publish(layout, root, &transaction, &capsule)
        .await
        .expect("publish ref transaction");
}

fn active_active_replication() -> crate::replication::ReplicationConfig {
    crate::replication::ReplicationConfig {
        primary: None,
        mode: crate::replication::ReplicationMode::ActiveActive,
        coordinator: Some(crate::replication::ReplicationCoordinatorConfig {
            kind: crate::replication::ReplicationCoordinatorKind::Managed,
            url: "dynamodb://test".to_owned(),
            region: "us-east-1".to_owned(),
            failover_regions: vec!["us-west-2".to_owned()],
            consistency: crate::replication::ReplicationCoordinatorConsistency::Linearizable,
        }),
        writers: vec![
            crate::replication::WriterConfig {
                name: "east".to_owned(),
                url: "crab://east/test".to_owned(),
                region: "us-east-1".to_owned(),
                enabled: true,
            },
            crate::replication::WriterConfig {
                name: "west".to_owned(),
                url: "crab://west/test".to_owned(),
                region: "us-west-2".to_owned(),
                enabled: true,
            },
        ],
        replicas: Vec::new(),
    }
}

#[test]
fn active_active_mirror_plan_passes_protocol_validation() {
    let config = PushConfig {
        mirror_plan_id: Some("a".repeat(64)),
        active_active_replication: Some(active_active_replication()),
        ..PushConfig::default()
    };

    validate_publication_plan_context(&config).unwrap();
}

#[tokio::test]
async fn active_active_capsule_commit_materializes_local_ref_and_retains_repair_proof() {
    let store = crab_storage::Store::new(Arc::new(InMemory::new()));
    let layout = crab_storage::StoreLayout::new(store, "repositories/test".to_owned());
    let base =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    let transaction = crab_metadata::capsule_protocol::CapsuleTransaction::new(
        base.record().digest(),
        vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some("2".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let capsule =
        crab_metadata::capsule_protocol::Capsule::build(&transaction, Vec::new(), Vec::new())
            .unwrap();
    let coordinator = Arc::new(crab_coordination::InMemoryWriteCoordinator::new());
    let config = PushConfig {
        active_active_replication: Some(active_active_replication()),
        active_active_writer: Some("east".to_owned()),
        active_active_coordinator: Some(crate::git::push::ActiveActiveWriteCoordinator::new(
            coordinator.clone(),
        )),
        ..PushConfig::default()
    };
    let attempt = publish_capsule(
        &config,
        &layout,
        base,
        &transaction,
        &capsule,
        &[],
        &BTreeMap::new(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let CapsulePublishAttempt::Committed(Some(outcome)) = attempt else {
        panic!("active-active capsule publication did not commit");
    };
    assert_eq!(outcome.commit_sequence, 1);
    let view = crab_read::capsule_protocol::open_view(
        &layout,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 1024 * 1024,
            max_frontier_bytes: 1024 * 1024,
        },
    )
    .await
    .unwrap();
    assert_eq!(view.refs().get("refs/heads/main"), Some(&"2".repeat(40)));
    let repair = coordinator.repair_snapshot().await.unwrap();
    assert_eq!(repair.materialization_gaps.len(), 1);
    assert_eq!(repair.materialization_gaps[0].region, "us-west-2");
    assert_eq!(
        repair.materialization_gaps[0]
            .capsule_publication
            .transaction_id,
        transaction.id().unwrap()
    );
}

#[tokio::test]
async fn active_active_mirror_commit_publishes_receipt_and_replicates_intent() {
    let store = crab_storage::Store::new(Arc::new(InMemory::new()));
    let layout = crab_storage::StoreLayout::new(store, "repositories/test".to_owned());
    let base =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    let plan_id = "a".repeat(64);
    let transaction = crab_metadata::capsule_protocol::CapsuleTransaction::for_plan(
        base.record().digest(),
        &plan_id,
        vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some("2".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let capsule =
        crab_metadata::capsule_protocol::Capsule::build(&transaction, Vec::new(), Vec::new())
            .unwrap();
    let coordinator = Arc::new(crab_coordination::InMemoryWriteCoordinator::new());
    let config = PushConfig {
        mirror_plan_id: Some(plan_id.clone()),
        active_active_replication: Some(active_active_replication()),
        active_active_writer: Some("east".to_owned()),
        active_active_coordinator: Some(crate::git::push::ActiveActiveWriteCoordinator::new(
            coordinator.clone(),
        )),
        ..PushConfig::default()
    };

    let attempt = publish_capsule(
        &config,
        &layout,
        base,
        &transaction,
        &capsule,
        &[],
        &BTreeMap::new(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(matches!(attempt, CapsulePublishAttempt::Committed(Some(_))));
    let receipt = crab_metadata::capsule_protocol::resolve_capsule_plan_receipt(
        layout.store(),
        &layout,
        &plan_id,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(receipt.transaction(), &transaction);
    let repair = coordinator.repair_snapshot().await.unwrap();
    assert!(
        repair.materialization_gaps[0]
            .uploaded_objects
            .contains(&layout.capsule_plan_intent_path(&plan_id).to_string())
    );
}

#[tokio::test]
async fn force_rewrite_without_local_remote_tip_preserves_policy_and_visibility() {
    let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
    for (name, force, lease, deny, succeeds) in [
        ("ordinary", false, None, false, false),
        ("force", true, None, false, true),
        ("lease", false, Some(true), false, true),
        ("stale-lease", true, Some(false), false, false),
        ("deny-force", true, None, true, false),
        ("deny-lease", false, Some(true), true, false),
    ] {
        let source = tempfile::tempdir().expect("source repository");
        git(source.path(), &["init", "-b", "main"]);
        git(source.path(), &["config", "user.name", "Crab Test"]);
        git(
            source.path(),
            &["config", "user.email", "crab@example.invalid"],
        );
        let first = commit(source.path(), "first");
        let contender_root = tempfile::tempdir().expect("contender directory");
        let contender = contender_root.path().join("repo");
        git(
            contender_root.path(),
            &[
                "clone",
                "--no-local",
                source.path().to_str().unwrap(),
                "repo",
            ],
        );
        git(&contender, &["config", "user.name", "Crab Test"]);
        git(
            &contender,
            &["config", "user.email", "crab@example.invalid"],
        );
        let second = commit(source.path(), "second");
        let divergent = commit(&contender, "divergent");
        let git_dir = contender.join(".git");
        assert_eq!(
            is_ancestor(&git_dir, &second, &divergent).unwrap(),
            None,
            "{name}"
        );

        let store = crate::storage::store::Store::from_storage(crab_storage::Store::new(Arc::new(
            InMemory::new(),
        )));
        let router = crate::storage::StoreLayout::new(store.clone(), "repos/test".to_owned());
        let layout =
            crab_storage::StoreLayout::new(store.as_storage().clone(), "repos/test".to_owned());
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let limits = crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 16 * 1024 * 1024,
            max_frontier_bytes: 16 * 1024 * 1024,
        };
        let spec = PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        };
        let config = PushConfig {
            git_dir: Some(source.path().join(".git")),
            ..PushConfig::default()
        };
        let (initial, _) = run(
            &config,
            std::slice::from_ref(&spec),
            &store,
            &router,
            None,
            &[],
            None,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(initial.all_ok(), "{name}: initial publication");
        let mut config = PushConfig {
            git_dir: Some(git_dir),
            receive_deny_non_fast_forwards: deny,
            ..PushConfig::default()
        };
        if let Some(matches) = lease {
            config.expected_refs.insert(
                "refs/heads/main".to_owned(),
                Some(if matches { second.clone() } else { first }),
            );
        }
        let spec = PushSpec { force, ..spec };
        let (result, _) = run(
            &config,
            &[spec],
            &store,
            &router,
            None,
            &[],
            None,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.all_ok(), succeeds, "{name}");
        let view = crab_read::capsule_protocol::open_view(&layout, limits)
            .await
            .unwrap();
        assert_eq!(
            view.refs()["refs/heads/main"],
            if succeeds {
                divergent.clone()
            } else {
                second.clone()
            },
            "{name}: remote tip"
        );
        if succeeds {
            let visibility = view.git_visibility_index().await.unwrap();
            let refs = visibility.ref_closures();
            let closure = &refs["refs/heads/main"];
            assert!(
                closure.binary_search(&divergent).is_ok()
                    && closure.binary_search(&second).is_err(),
                "{name}: rewrite must retire old visibility"
            );
            let destination = tempfile::tempdir().unwrap();
            git(destination.path(), &["init", "--bare"]);
            crab_read::capsule_protocol::install_git_packs_from_store(
                &view,
                &layout,
                destination.path(),
                16 * 1024 * 1024,
                None,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            git(
                destination.path(),
                &["cat-file", "-e", &format!("{divergent}^{{commit}}")],
            );
            git(destination.path(), &["fsck", "--connectivity-only"]);
        }
    }
}

#[test]
fn missing_remote_tip_requests_refresh_instead_of_internal_failure() {
    let source = tempfile::tempdir().expect("source repository");
    git(source.path(), &["init", "--initial-branch=main"]);
    git(source.path(), &["config", "user.name", "Crab Test"]);
    git(
        source.path(),
        &["config", "user.email", "crab@example.invalid"],
    );
    let tip = commit(source.path(), "tip");

    assert_eq!(
        is_ancestor(&source.path().join(".git"), &"f".repeat(40), &tip)
            .expect("missing object is a normal refresh condition"),
        None
    );
}

#[test]
fn direct_parent_proves_fast_forward() {
    let source = tempfile::tempdir().expect("source repository");
    git(source.path(), &["init", "--initial-branch=main"]);
    git(source.path(), &["config", "user.name", "Crab Test"]);
    git(
        source.path(),
        &["config", "user.email", "crab@example.invalid"],
    );
    let parent = commit(source.path(), "parent");
    let child = commit(source.path(), "child");
    let git_dir = source.path().join(".git");

    assert!(has_direct_parent(&git_dir, &parent, &child));
    assert_eq!(is_ancestor(&git_dir, &parent, &child).unwrap(), Some(true));

    git(source.path(), &["branch", "side", &parent]);
    git(source.path(), &["checkout", "side"]);
    std::fs::write(source.path().join("side.txt"), "side").expect("write side fixture");
    git(source.path(), &["add", "side.txt"]);
    git(source.path(), &["commit", "-m", "side"]);
    let second_parent = git(source.path(), &["rev-parse", "HEAD"]);
    git(source.path(), &["checkout", "main"]);
    git(source.path(), &["merge", "--no-ff", "--no-edit", "side"]);
    let merge = git(source.path(), &["rev-parse", "HEAD"]);

    assert!(has_direct_parent(&git_dir, &child, &merge));
    assert!(has_direct_parent(&git_dir, &second_parent, &merge));
    assert_eq!(
        is_ancestor(&git_dir, &second_parent, &merge).unwrap(),
        Some(true)
    );
}

#[test]
fn non_direct_ancestry_and_divergence_keep_the_git_graph_check() {
    let source = tempfile::tempdir().expect("source repository");
    git(source.path(), &["init", "--initial-branch=main"]);
    git(source.path(), &["config", "user.name", "Crab Test"]);
    git(
        source.path(),
        &["config", "user.email", "crab@example.invalid"],
    );
    let root = commit(source.path(), "root");
    git(source.path(), &["branch", "side"]);
    let main_tip = commit(source.path(), "main");
    let main_descendant = commit(source.path(), "main descendant");
    git(source.path(), &["checkout", "side"]);
    let side_tip = commit(source.path(), "side");
    let git_dir = source.path().join(".git");

    assert!(!has_direct_parent(&git_dir, &root, &main_descendant));
    assert_eq!(
        is_ancestor(&git_dir, &root, &main_descendant).unwrap(),
        Some(true)
    );
    assert!(!has_direct_parent(&git_dir, &main_tip, &side_tip));
    assert_eq!(
        is_ancestor(&git_dir, &main_tip, &side_tip).unwrap(),
        Some(false)
    );
}

#[test]
fn new_ref_visibility_is_self_contained() {
    let source = tempfile::tempdir().expect("source repository");
    git(source.path(), &["init", "--initial-branch=main"]);
    git(source.path(), &["config", "user.name", "Crab Test"]);
    git(
        source.path(),
        &["config", "user.email", "crab@example.invalid"],
    );
    let tip = commit(source.path(), "tip");
    let ref_name = "refs/heads/feature";
    let edits = [crab_metadata::capsule_protocol::CapsuleRefEdit::new(
        ref_name,
        None,
        Some(tip.clone()),
        None,
    )];

    let visibility = prepare_visibility_delta(
        &source.path().join(".git"),
        &edits,
        &BTreeSet::new(),
        None,
        &CancellationToken::new(),
    )
    .expect("prepare Git visibility")
    .expect("new ref has visibility evidence");
    let evidence = visibility
        .edits()
        .get(ref_name)
        .expect("feature visibility evidence");

    assert!(evidence.replaces);
    assert_eq!(evidence.old_oid, None);
    assert!(evidence.added.binary_search(&tip).is_ok());
}

#[tokio::test]
async fn fast_forward_visibility_reuses_proven_ancestry_for_empty_removals() {
    let source = tempfile::tempdir().expect("source repository");
    git(source.path(), &["init", "--initial-branch=main"]);
    git(source.path(), &["config", "user.name", "Crab Test"]);
    git(
        source.path(),
        &["config", "user.email", "crab@example.invalid"],
    );
    let old_oid = commit(source.path(), "first");
    let new_oid = commit(source.path(), "second");
    let git_dir = source.path().join(".git");
    let ref_name = "refs/heads/main";
    let edits = [crab_metadata::capsule_protocol::CapsuleRefEdit::new(
        ref_name,
        Some(old_oid.clone()),
        Some(new_oid.clone()),
        None,
    )];
    let fast_forward_refs = BTreeSet::from([ref_name.to_owned()]);
    let remote_refs = BTreeMap::from([(ref_name.to_owned(), old_oid.clone())]);
    let updates = [RefUpdate {
        ref_name: ref_name.to_owned(),
        old_sha: Some(old_oid.clone()),
        new_sha: new_oid.clone(),
        force: false,
    }];
    let prepared = prepare_git_packs(&git_dir, &remote_refs, &updates, 16 * 1024 * 1024, false)
        .await
        .expect("prepare incremental Git pack");
    let pack_objects = prepared
        .visibility_objects
        .as_deref()
        .expect("single-ref pack is an exact visibility delta");
    let visibility = prepare_visibility_delta(
        &git_dir,
        &edits,
        &fast_forward_refs,
        Some(pack_objects),
        &CancellationToken::new(),
    )
    .expect("prepare fast-forward visibility")
    .expect("existing ref has visibility evidence");
    let evidence = visibility
        .edits()
        .get(ref_name)
        .expect("main visibility evidence");
    let expected_added = super::super::push::enumerate_visibility_difference(
        &git_dir,
        &new_oid,
        Some(&old_oid),
        usize::try_from(crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS)
            .expect("visibility limit fits usize"),
    )
    .expect("enumerate newly reachable objects")
    .expect("visibility is below its limit")
    .into_iter()
    .collect::<BTreeSet<_>>();

    assert_eq!(evidence.old_oid.as_deref(), Some(old_oid.as_str()));
    assert_eq!(evidence.new_oid, new_oid);
    assert_eq!(
        evidence.added.iter().cloned().collect::<BTreeSet<_>>(),
        expected_added
    );
    assert_eq!(
        pack_objects
            .iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>(),
        expected_added
    );
    assert!(evidence.removed.is_empty());
    evidence.validate().expect("visibility edit remains valid");
}

#[tokio::test]
async fn sibling_ref_objects_are_not_used_as_a_fast_forward_visibility_delta() {
    let source = tempfile::tempdir().expect("source repository");
    git(source.path(), &["init", "--initial-branch=main"]);
    git(source.path(), &["config", "user.name", "Crab Test"]);
    git(
        source.path(),
        &["config", "user.email", "crab@example.invalid"],
    );
    let old_oid = commit(source.path(), "root");
    git(source.path(), &["branch", "side"]);
    git(source.path(), &["checkout", "side"]);
    git(source.path(), &["commit", "--allow-empty", "-m", "side"]);
    let side_oid = git(source.path(), &["rev-parse", "HEAD"]);
    git(source.path(), &["checkout", "main"]);
    commit(source.path(), "main");
    git(source.path(), &["merge", "--no-ff", "--no-edit", "side"]);
    let new_oid = git(source.path(), &["rev-parse", "HEAD"]);
    let git_dir = source.path().join(".git");
    let ref_name = "refs/heads/main";
    let remote_refs = BTreeMap::from([
        (ref_name.to_owned(), old_oid.clone()),
        ("refs/heads/side".to_owned(), side_oid.clone()),
    ]);
    let updates = [RefUpdate {
        ref_name: ref_name.to_owned(),
        old_sha: Some(old_oid.clone()),
        new_sha: new_oid.clone(),
        force: false,
    }];

    let prepared = prepare_git_packs(&git_dir, &remote_refs, &updates, 16 * 1024 * 1024, false)
        .await
        .expect("prepare incremental Git pack");
    assert!(prepared.visibility_objects.is_none());

    let edits = [crab_metadata::capsule_protocol::CapsuleRefEdit::new(
        ref_name,
        Some(old_oid),
        Some(new_oid.clone()),
        None,
    )];
    let fast_forward_refs = BTreeSet::from([ref_name.to_owned()]);
    let visibility = prepare_visibility_delta(
        &git_dir,
        &edits,
        &fast_forward_refs,
        None,
        &CancellationToken::new(),
    )
    .expect("prepare full fast-forward visibility")
    .expect("existing ref has visibility evidence");
    let evidence = visibility
        .edits()
        .get(ref_name)
        .expect("main visibility evidence");

    assert!(evidence.added.binary_search(&side_oid).is_ok());
    assert_eq!(evidence.new_oid, new_oid);
    assert!(evidence.removed.is_empty());
}

#[test]
fn non_fast_forward_visibility_keeps_removed_objects() {
    let source = tempfile::tempdir().expect("source repository");
    git(source.path(), &["init", "--initial-branch=main"]);
    git(source.path(), &["config", "user.name", "Crab Test"]);
    git(
        source.path(),
        &["config", "user.email", "crab@example.invalid"],
    );
    let new_oid = commit(source.path(), "first");
    let old_oid = commit(source.path(), "second");
    let git_dir = source.path().join(".git");
    let ref_name = "refs/heads/main";
    let edits = [crab_metadata::capsule_protocol::CapsuleRefEdit::new(
        ref_name,
        Some(old_oid.clone()),
        Some(new_oid.clone()),
        None,
    )];
    let visibility = prepare_visibility_delta(
        &git_dir,
        &edits,
        &BTreeSet::new(),
        None,
        &CancellationToken::new(),
    )
    .expect("prepare non-fast-forward visibility")
    .expect("existing ref has visibility evidence");
    let evidence = visibility
        .edits()
        .get(ref_name)
        .expect("main visibility evidence");
    let expected_removed = super::super::push::enumerate_visibility_difference(
        &git_dir,
        &old_oid,
        Some(&new_oid),
        usize::try_from(crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS)
            .expect("visibility limit fits usize"),
    )
    .expect("enumerate no-longer-reachable objects")
    .expect("visibility is below its limit")
    .into_iter()
    .collect::<BTreeSet<_>>();

    assert!(!expected_removed.is_empty());
    assert_eq!(
        evidence.removed.iter().cloned().collect::<BTreeSet<_>>(),
        expected_removed
    );
    evidence.validate().expect("visibility edit remains valid");
}

#[tokio::test]
async fn full_graph_pack_contains_more_history_than_incremental_pack() {
    let source = tempfile::tempdir().expect("source repository");
    git(source.path(), &["init", "-b", "main"]);
    git(source.path(), &["config", "user.name", "Crab Test"]);
    git(
        source.path(),
        &["config", "user.email", "crab@example.invalid"],
    );
    let first = commit(source.path(), "first");
    let second = commit(source.path(), "second");
    let git_dir = source.path().join(".git");
    let remote_refs = BTreeMap::from([("refs/heads/main".to_owned(), first.clone())]);
    let updates = [RefUpdate {
        ref_name: "refs/heads/main".to_owned(),
        old_sha: Some(first),
        new_sha: second,
        force: false,
    }];

    let incremental = prepare_git_packs(&git_dir, &remote_refs, &updates, 16 * 1024 * 1024, false)
        .await
        .expect("prepare incremental pack");
    let full = prepare_git_packs(&git_dir, &remote_refs, &updates, 16 * 1024 * 1024, true)
        .await
        .expect("prepare full graph pack");
    let incremental_bytes = incremental
        .packs
        .iter()
        .map(crab_metadata::capsule_protocol::CapsuleGitPack::pack_size)
        .sum::<u64>();
    let full_bytes = full
        .packs
        .iter()
        .map(crab_metadata::capsule_protocol::CapsuleGitPack::pack_size)
        .sum::<u64>();

    assert!(full_bytes > incremental_bytes);
}

#[tokio::test]
async fn capsule_push_publishes_reachable_lfs_dependencies() {
    let source = tempfile::tempdir().expect("source repository");
    git(source.path(), &["init", "-b", "main"]);
    git(source.path(), &["config", "user.name", "Crab Test"]);
    git(
        source.path(),
        &["config", "user.email", "crab@example.invalid"],
    );
    let content = b"capsule LFS dependency".to_vec();
    let pointer = crab_git::lfs_pointer::LfsPointer {
        oid: Sha256::digest(&content).into(),
        size: content.len() as u64,
        extensions: Vec::new(),
    };
    std::fs::write(source.path().join("asset.bin"), pointer.serialize())
        .expect("write LFS pointer");
    git(source.path(), &["add", "asset.bin"]);
    git(source.path(), &["commit", "-m", "LFS dependency"]);
    let lfs_dir = crate::lfs::config::LfsConfig::resolve_storage_dir(source.path())
        .expect("resolve local LFS store");
    crate::lfs::cache::install_bytes(&lfs_dir, &pointer.oid, pointer.size, &content)
        .expect("install local LFS object");

    let store = crate::storage::store::Store::new(Arc::new(InMemory::new()));
    let router = crate::storage::StoreLayout::new(store.clone(), "repos/lfs".to_owned());
    let layout = crab_storage::StoreLayout::new(store.as_storage().clone(), "repos/lfs".to_owned());
    let root =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .expect("initialize root");
    let view = crab_read::capsule_protocol::open_view_from_root(
        &layout,
        root,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 16 * 1024 * 1024,
            max_frontier_bytes: 16 * 1024 * 1024,
        },
    )
    .await
    .expect("open initial view");
    let config = PushConfig {
        git_dir: Some(source.path().join(".git")),
        ..PushConfig::default()
    };

    let (result, _) = run(
        &config,
        &[PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        }],
        &store,
        &router,
        Some(view.into()),
        &[],
        None,
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("publish capsule ref and LFS dependency");

    assert!(result.all_ok());
    let remote = crab_lfs::LfsObjectStore::new(store.as_storage().clone(), "repos/lfs");
    assert_eq!(
        remote
            .verify(&pointer.oid)
            .await
            .expect("verify remote LFS"),
        Bytes::from(content)
    );
}

#[tokio::test]
async fn real_git_incremental_push_round_trips_with_bounded_requests() {
    let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
    let source = tempfile::tempdir().expect("source repository");
    git(source.path(), &["init", "-b", "main"]);
    git(source.path(), &["config", "user.name", "Crab Test"]);
    git(
        source.path(),
        &["config", "user.email", "crab@example.invalid"],
    );
    let first = commit(source.path(), "first");

    let observer = Arc::new(RecordingObserver::default());
    let storage = crab_storage::Store::new(Arc::new(InMemory::new()))
        .with_storage_observer(Arc::clone(&observer) as Arc<dyn StorageObserver>);
    let store = crate::storage::store::Store::from_storage(storage);
    let router = crate::storage::StoreLayout::new(store.clone(), "repos/test".to_owned());
    let layout =
        crab_storage::StoreLayout::new(store.as_storage().clone(), "repos/test".to_owned());
    let root =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .expect("initialize root");
    let limits = crab_read::capsule_protocol::CapsuleReadLimits {
        max_capsule_bytes: 16 * 1024 * 1024,
        max_frontier_bytes: 16 * 1024 * 1024,
    };
    let initial_view = crab_read::capsule_protocol::open_view_from_root(&layout, root, limits)
        .await
        .expect("open initial view");
    let mut config = PushConfig {
        git_dir: Some(source.path().join(".git")),
        ..PushConfig::default()
    };
    let spec = PushSpec {
        force: false,
        src: "refs/heads/main".to_owned(),
        dst: "refs/heads/main".to_owned(),
    };

    let before_first = observer.count();
    let (result, committed) = run(
        &config,
        std::slice::from_ref(&spec),
        &store,
        &router,
        Some(initial_view.into()),
        &[],
        None,
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("first push");
    assert!(result.all_ok());
    assert!(committed.is_none());
    let first_requests = observer.count() - before_first;
    assert!(
        first_requests <= 20,
        "first push used {first_requests} requests"
    );
    let committed = crab_read::capsule_protocol::open_view(&layout, limits)
        .await
        .expect("open committed first push");
    assert_eq!(committed.refs()["refs/heads/main"], first);

    let destination = tempfile::tempdir().expect("destination repository");
    git(destination.path(), &["init", "--bare"]);
    let view = crab_read::capsule_protocol::open_view(&layout, limits)
        .await
        .expect("open first generation");
    crab_read::capsule_protocol::install_git_packs(&view, destination.path(), 16 * 1024 * 1024)
        .await
        .expect("install first pack");
    git(
        destination.path(),
        &["cat-file", "-e", &format!("{first}^{{commit}}")],
    );

    let second = commit(source.path(), "second");
    config
        .expected_refs
        .insert("refs/heads/main".to_owned(), Some(first.clone()));
    let before_second = observer.count();
    let (result, committed) = run(
        &config,
        &[spec],
        &store,
        &router,
        None,
        &[],
        None,
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("incremental push");
    assert!(result.all_ok());
    assert!(committed.is_none());
    let second_requests = observer.count() - before_second;
    assert_eq!(
        second_requests, 6,
        "incremental push should reuse the admitted ref-head snapshot"
    );
    let committed = crab_read::capsule_protocol::open_view(&layout, limits)
        .await
        .expect("open committed second push");
    assert_eq!(committed.refs()["refs/heads/main"], second);
    assert_eq!(committed.capsules().len(), 2);

    let before_repack = observer.count();
    let repack = crate::cmd::repack::run_repack_from_root(
        &store,
        "repos/test",
        committed.root_snapshot().clone(),
        false,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint repack");
    assert_eq!(repack.packs_before, 2);
    assert_eq!(repack.packs_after, 1);
    assert!(repack.bytes_read > 0);
    assert_eq!(repack.bytes_read, repack.bytes_before);
    assert!(repack.bytes_written > 0);
    assert_eq!(repack.bytes_written, repack.bytes_after);
    let repack_requests = observer.count() - before_repack;
    let repack_operations = observer.observations.lock().expect("observer lock")[before_repack..]
        .iter()
        .map(|observation| observation.operation)
        .collect::<Vec<_>>();
    let checkpoint_root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .expect("checkpoint root");
    assert!(checkpoint_root.record().root().checkpoint().is_some());
    assert!(
        checkpoint_root
            .record()
            .root()
            .capsule_frontier()
            .is_empty()
    );
    let checkpoint_view =
        crab_read::capsule_protocol::open_view_from_root(&layout, checkpoint_root, limits)
            .await
            .expect("open checkpoint view");
    assert!(checkpoint_view.layered_checkpoint().is_some());
    assert_eq!(checkpoint_view.refs(), committed.refs());

    let third = commit(source.path(), "third");
    config
        .expected_refs
        .insert("refs/heads/main".to_owned(), Some(second.clone()));
    let (result, _) = run(
        &config,
        &[PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        }],
        &store,
        &router,
        Some(checkpoint_view.into()),
        &[],
        None,
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("post-checkpoint push");
    assert!(result.all_ok());

    let fresh = tempfile::tempdir().expect("fresh clone target");
    git(fresh.path(), &["init", "--bare"]);
    let view = crab_read::capsule_protocol::open_view(&layout, limits)
        .await
        .expect("open checkpoint and delta");
    assert!(view.layered_checkpoint().is_some());
    assert_eq!(view.capsules().len(), 1);
    crab_read::capsule_protocol::install_git_packs_from_store(
        &view,
        &layout,
        fresh.path(),
        16 * 1024 * 1024,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("install checkpoint and delta");
    git(
        fresh.path(),
        &["cat-file", "-e", &format!("{third}^{{commit}}")],
    );

    let orphan = layout.capsule_path(&"f".repeat(64));
    store
        .put(&orphan, Bytes::from_static(b"unreachable capsule"))
        .await
        .expect("write GC orphan");
    let gc = crate::cmd::gc::run_repo_remote_gc(
        &crate::cmd::gc::GcArgs {
            force: true,
            yes: true,
            ..crate::cmd::gc::GcArgs::default()
        },
        &store,
        &router,
        &std::collections::HashSet::new(),
        &CancellationToken::new(),
        std::time::Duration::ZERO,
        None,
    )
    .await
    .expect("capsule-protocol GC");
    assert_eq!(gc.packs_deleted, 0);
    store
        .head(&orphan)
        .await
        .expect("capsule GC preserves immutable-object grace even when forced");
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .expect("root after GC");
    assert!(root.record().root().gc_fence().is_none());
    crab_read::capsule_protocol::open_view(
        &layout,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 16 * 1024 * 1024,
            max_frontier_bytes: 16 * 1024 * 1024,
        },
    )
    .await
    .expect("GC preserves every referenced checkpoint and capsule");
    assert!(
        repack_requests <= 12,
        "repack used {repack_requests} requests: {repack_operations:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn capsule_push_retires_staging_only_after_confirmed_ref_publication() {
    use bstr::ByteSlice;
    use crab_staging::stream::{StreamStageProgress, stage_reader_streaming};
    let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
    for (stale, native_intent, restored_index, cancel_after_commit) in [
        (false, false, false, false),
        (true, false, false, false),
        (false, true, false, false),
        (true, true, false, false),
        (false, true, true, false),
        (false, false, false, true),
    ] {
        let source = tempfile::tempdir().unwrap();
        git(source.path(), &["init", "-b", "main"]);
        git(source.path(), &["config", "user.name", "Crab Test"]);
        git(
            source.path(),
            &["config", "user.email", "crab@example.invalid"],
        );
        let first = commit(source.path(), "initial");
        let staging_root = source.path().join(".crab/staging");
        let staging = crab_staging::StagingArea::open(staging_root.clone())
            .await
            .unwrap();
        let staged = stage_reader_streaming(
            &mut std::io::Cursor::new(b"capsule payload"),
            Path::new("model.bin"),
            &staging,
            StreamStageProgress::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        if !native_intent {
            staging
                .delegate_publication(&[(&staged.batch_id, Path::new("model.bin"))])
                .unwrap();
        }
        let pointer = crab_types::pointer::Pointer {
            file_hash: staged.file_hash,
            size: staged.size,
            shard_hint: None,
        };
        std::fs::write(source.path().join("model.bin"), pointer.serialize()).unwrap();
        if native_intent {
            let index = gix_index::File::at_or_default(
                source.path().join(".git/index"),
                gix_hash::Kind::Sha1,
                false,
                gix_index::decode::Options::default(),
            )
            .unwrap();
            let previous_index_state =
                super::super::index::git_index_path_state(&index, b"model.bin".as_bstr());
            staging
                .create_publication_intent(&[crab_staging::PublicationIntentEntry {
                    batch_id: staged.batch_id.clone(),
                    path: "model.bin".into(),
                    expected_pointer_oid: git(source.path(), &["hash-object", "-w", "model.bin"]),
                    previous_index_state,
                }])
                .unwrap();
        }
        git(source.path(), &["add", "model.bin"]);
        git(source.path(), &["commit", "-m", "managed payload"]);
        let tip = git(source.path(), &["rev-parse", "HEAD"]);
        if restored_index {
            git(source.path(), &["read-tree", "HEAD^"]);
        }
        staging.close().await.unwrap();
        let staging = Arc::new(StagingAreaReadOnly::open(staging_root).await.unwrap());
        let hooked = Arc::new(crab_storage::test_support::CountingObjectStore::new(
            Arc::new(InMemory::new()),
        ));
        let storage = crab_storage::Store::new(hooked.clone());
        let store = crate::storage::store::Store::from_storage(storage);
        let router = crate::storage::StoreLayout::new(store.clone(), "repos/test".to_owned());
        let layout =
            crab_storage::StoreLayout::new(store.as_storage().clone(), "repos/test".to_owned());
        let root =
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .unwrap();
        let limits = crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 16 * 1024 * 1024,
            max_frontier_bytes: 16 * 1024 * 1024,
        };
        let initial = crab_read::capsule_protocol::open_view_from_root(&layout, root, limits)
            .await
            .unwrap();
        if stale {
            publish_ref_edits_with_visibility(
                &layout,
                &source.path().join(".git"),
                vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                    "refs/heads/main",
                    None,
                    Some(first.clone()),
                    None,
                )],
            )
            .await;
        }
        let config = PushConfig {
            git_dir: Some(source.path().join(".git")),
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        if cancel_after_commit {
            hooked.cancel_after_put(layout.capsule_ref_heads_prefix(), cancel.clone());
        }
        let (result, _) = run(
            &config,
            &[PushSpec {
                force: false,
                src: "refs/heads/main".to_owned(),
                dst: "refs/heads/main".to_owned(),
            }],
            &store,
            &router,
            Some(initial.into()),
            &[],
            Some(&staging),
            None,
            None,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(cancel.is_cancelled(), cancel_after_commit);
        if cancel_after_commit {
            for domain in [layout.global_prefix(), layout.repo_prefix()] {
                let sweep = crab_coordination::GcFenceLease::acquire_sweep(
                    layout.store().inner(),
                    domain,
                    std::time::Duration::from_secs(60),
                )
                .await
                .unwrap();
                sweep.release().await.unwrap();
            }
        }
        let health = staging.lifecycle_health().unwrap();
        let view = crab_read::capsule_protocol::open_view(&layout, limits)
            .await
            .unwrap();
        assert!(
            result.all_ok() != stale
                && view.refs().get("refs/heads/main") == Some(if stale { &first } else { &tip })
                && health.path_leases == u64::from(stale)
                && health.payloads == u64::from(stale)
                && health.open_push_snapshots == 0
                && health.committed_push_snapshots == 0
                && health.unresolved_publications == 0
                && staging.list_inflight().unwrap().is_empty(),
            "stale={stale}, native_intent={native_intent}, restored_index={restored_index}, cancel_after_commit={cancel_after_commit}, health={health:?}"
        );
    }
}

#[tokio::test]
async fn single_ref_successor_extends_committed_multi_ref_history() {
    let source = tempfile::tempdir().expect("source repository");
    git(source.path(), &["init", "-b", "main"]);
    git(source.path(), &["config", "user.name", "Crab Test"]);
    git(
        source.path(),
        &["config", "user.email", "crab@example.invalid"],
    );
    let first = commit(source.path(), "first");
    let second = commit(source.path(), "second");
    git(source.path(), &["reset", "--hard", &first]);
    let third = commit(source.path(), "third");

    let storage = crab_storage::Store::new(Arc::new(InMemory::new()));
    let layout = crab_storage::StoreLayout::new(storage, "repos/ref-history".to_owned());
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .expect("initialize root");
    let limits = crab_read::capsule_protocol::CapsuleReadLimits {
        max_capsule_bytes: 16 * 1024 * 1024,
        max_frontier_bytes: 16 * 1024 * 1024,
    };
    let main = "refs/heads/main";
    let sibling = &format!("refs/heads/{}", "a".repeat(40));

    for edits in [
        vec![
            crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                main,
                None,
                Some(first.clone()),
                None,
            ),
            crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                sibling,
                None,
                Some(first.clone()),
                None,
            ),
        ],
        vec![
            crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                main,
                Some(first.clone()),
                Some(second.clone()),
                None,
            ),
            crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                sibling,
                Some(first.clone()),
                Some(second.clone()),
                None,
            ),
        ],
        vec![
            crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                main,
                Some(second.clone()),
                Some(first.clone()),
                None,
            ),
            crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                sibling,
                Some(second.clone()),
                None,
                None,
            ),
        ],
    ] {
        publish_ref_edits_with_visibility(&layout, &source.path().join(".git"), edits).await;
        crab_read::capsule_protocol::open_view(&layout, limits)
            .await
            .expect("multi-ref history remains readable");
    }

    publish_ref_edits_with_visibility(
        &layout,
        &source.path().join(".git"),
        vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            main,
            Some(first),
            Some(third.clone()),
            None,
        )],
    )
    .await;

    let view = crab_read::capsule_protocol::open_view(&layout, limits)
        .await
        .expect("single-ref successor must preserve readable multi-ref history");
    assert_eq!(view.refs().get(main), Some(&third));
    assert!(!view.refs().contains_key(sibling));
}

#[path = "capsule_push_policy_tests.rs"]
mod policy;

#[tokio::test]
async fn multi_ref_visibility_preparation_preserves_exact_shared_replacements_and_small_deltas() {
    let source = tempfile::tempdir().unwrap();
    git(source.path(), &["init", "--initial-branch=main"]);
    git(source.path(), &["config", "user.name", "Crab Test"]);
    git(
        source.path(),
        &["config", "user.email", "crab@example.invalid"],
    );
    let old = commit(source.path(), "first");
    let new = commit(source.path(), "second");
    let directory = source.path().join(".git");
    let edits = vec![
        crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            "refs/heads/main",
            Some(old.clone()),
            Some(new.clone()),
            None,
        ),
        crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            "refs/heads/new",
            None,
            Some(new.clone()),
            None,
        ),
        crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            "refs/tags/old",
            None,
            Some(old.clone()),
            None,
        ),
    ];
    let delta = prepare_visibility_delta(
        &directory,
        &edits,
        &BTreeSet::from(["refs/heads/main".to_owned()]),
        None,
        &CancellationToken::new(),
    )
    .unwrap()
    .unwrap();
    for (name, tip) in [("refs/heads/new", &new), ("refs/tags/old", &old)] {
        let evidence = &delta.edits()[name];
        assert!(evidence.replaces);
        assert!(evidence.old_oid.is_none());
        let expected = git(
            source.path(),
            &["rev-list", "--objects", "--no-object-names", tip],
        )
        .lines()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
        assert_eq!(
            evidence.added.iter().cloned().collect::<BTreeSet<_>>(),
            expected
        );
    }
    let main = &delta.edits()["refs/heads/main"];
    assert!(!main.replaces);
    assert_eq!(main.old_oid.as_ref(), Some(&old));
    assert!(main.removed.is_empty());
    assert!(!main.added.contains(&old));
    assert!(main.added.contains(&new));
    let bytes = delta.encode().unwrap();
    assert!(bytes.len() < 128 * 1024 * 1024);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        prepare_visibility_delta(&directory, &edits, &BTreeSet::new(), None, &cancelled),
        Err(CrabError::Cancelled)
    ));
}
