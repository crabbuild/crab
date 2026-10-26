use super::*;

struct PolicyFixture {
    repository: tempfile::TempDir,
    store: crate::storage::store::Store,
    router: crate::storage::StoreLayout,
    layout: crab_storage::StoreLayout<crab_storage::Store>,
}

impl PolicyFixture {
    async fn new(head: &str) -> Self {
        let repository = tempfile::tempdir().unwrap();
        git(repository.path(), &["init", "-b", "main"]);
        git(repository.path(), &["config", "user.name", "Crab Test"]);
        git(
            repository.path(),
            &["config", "user.email", "crab@example.invalid"],
        );
        git(repository.path(), &["config", "commit.gpgsign", "false"]);
        let store = crate::storage::store::Store::from_storage(crab_storage::Store::new(Arc::new(
            InMemory::new(),
        )));
        let router = crate::storage::StoreLayout::new(store.clone(), "repos/policy".to_owned());
        let layout =
            crab_storage::StoreLayout::new(store.as_storage().clone(), "repos/policy".to_owned());
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), head)
            .await
            .unwrap();
        Self {
            repository,
            store,
            router,
            layout,
        }
    }

    async fn push(&self, specs: &[PushSpec], mut config: PushConfig) -> Result<PushResult> {
        config.git_dir = Some(self.repository.path().join(".git"));
        run(
            &config,
            specs,
            &self.store,
            &self.router,
            None,
            &[],
            None,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .map(|(result, _)| result)
    }

    async fn refs(&self) -> BTreeMap<String, String> {
        crab_read::capsule_protocol::read_visible_refs(&self.layout)
            .await
            .unwrap()
    }
}

fn policy_spec(src: &str, dst: &str) -> PushSpec {
    PushSpec {
        force: false,
        src: src.to_owned(),
        dst: dst.to_owned(),
    }
}

#[tokio::test]
async fn current_branch_policy_uses_root_head_for_updates_and_deletes() {
    for head in ["refs/heads/main", "refs/heads/trunk"] {
        for policy in ["refuse", "warn", "ignore", "REFUSE"] {
            for delete in [false, true] {
                let fixture = PolicyFixture::new(head).await;
                let first = commit(fixture.repository.path(), "first");
                let seed = [
                    policy_spec("HEAD", head),
                    policy_spec("HEAD", "refs/heads/other"),
                ];
                assert!(
                    fixture
                        .push(&seed, PushConfig::default())
                        .await
                        .unwrap()
                        .all_ok()
                );
                let second = commit(fixture.repository.path(), "second");
                let specs = [
                    policy_spec(if delete { "" } else { "HEAD" }, head),
                    policy_spec("HEAD", "refs/heads/other"),
                ];
                let result = fixture
                    .push(
                        &specs,
                        PushConfig {
                            receive_deny_current_branch: policy.to_owned(),
                            ..PushConfig::default()
                        },
                    )
                    .await
                    .unwrap();
                let refused = policy.eq_ignore_ascii_case("refuse");
                assert!(
                    matches!(
                        result.outcomes.get(head),
                        Some(RefPushOutcome::Rejected(
                            PushRejectReason::DenyCurrentBranch
                        ))
                    ) == refused,
                    "head={head}, policy={policy}, delete={delete}: {result:?}"
                );
                let mut expected =
                    BTreeMap::from([("refs/heads/other".to_owned(), second.clone())]);
                if refused || !delete {
                    expected.insert(head.to_owned(), if refused { first } else { second });
                }
                assert_eq!(
                    fixture.refs().await,
                    expected,
                    "head={head}, policy={policy}, delete={delete}"
                );
            }
        }
    }
}

#[tokio::test]
async fn current_branch_refusal_also_applies_to_initial_publication() {
    let fixture = PolicyFixture::new("refs/heads/trunk").await;
    let tip = commit(fixture.repository.path(), "first");
    let specs = [
        policy_spec("HEAD", "refs/heads/trunk"),
        policy_spec("HEAD", "refs/heads/main"),
    ];
    let result = fixture
        .push(
            &specs,
            PushConfig {
                receive_deny_current_branch: "refuse".to_owned(),
                ..PushConfig::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        result.outcomes.get("refs/heads/trunk"),
        Some(RefPushOutcome::Rejected(
            PushRejectReason::DenyCurrentBranch
        ))
    ));
    assert_eq!(
        fixture.refs().await,
        BTreeMap::from([("refs/heads/main".to_owned(), tip)])
    );
}

#[tokio::test]
async fn delete_policy_preserves_refs_and_absent_deletes_are_noops() {
    let fixture = PolicyFixture::new("refs/heads/main").await;
    let first = commit(fixture.repository.path(), "first");
    assert!(
        fixture
            .push(
                &[policy_spec("HEAD", "refs/heads/old")],
                PushConfig::default()
            )
            .await
            .unwrap()
            .all_ok()
    );
    let before = fixture.refs().await;
    let result = fixture
        .push(
            &[
                policy_spec("", "refs/heads/old"),
                policy_spec("", "refs/heads/absent"),
            ],
            PushConfig {
                receive_deny_deletes: true,
                ..PushConfig::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        result.outcomes.get("refs/heads/old"),
        Some(RefPushOutcome::Rejected(PushRejectReason::DenyDeletes))
    ));
    assert!(matches!(
        result.outcomes.get("refs/heads/absent"),
        Some(RefPushOutcome::Ok)
    ));
    assert_eq!(fixture.refs().await, before);
    let result = fixture
        .push(
            &[
                policy_spec("", "refs/heads/old"),
                policy_spec("HEAD", "refs/heads/new"),
            ],
            PushConfig::default(),
        )
        .await
        .unwrap();
    assert!(result.all_ok());
    assert_eq!(
        fixture.refs().await,
        BTreeMap::from([("refs/heads/new".to_owned(), first)])
    );
}

#[tokio::test]
async fn atomic_rejection_preserves_all_refs_and_non_atomic_publishes_only_valid_edits() {
    for atomic in [false, true] {
        let fixture = PolicyFixture::new("refs/heads/main").await;
        let first = commit(fixture.repository.path(), "first");
        let specs = [
            policy_spec("refs/heads/main", "refs/heads/main"),
            policy_spec("refs/heads/feature", "refs/heads/feature"),
        ];
        git(fixture.repository.path(), &["branch", "feature"]);
        assert!(
            fixture
                .push(&specs, PushConfig::default())
                .await
                .unwrap()
                .all_ok()
        );
        git(fixture.repository.path(), &["checkout", "feature"]);
        let feature = commit(fixture.repository.path(), "feature");
        git(fixture.repository.path(), &["checkout", "main"]);
        git(
            fixture.repository.path(),
            &["commit", "--amend", "-m", "divergent"],
        );
        let result = fixture
            .push(
                &specs,
                PushConfig {
                    atomic,
                    ..PushConfig::default()
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            result.outcomes.get("refs/heads/main"),
            Some(RefPushOutcome::Rejected(
                PushRejectReason::NonFastForward { .. }
            ))
        ));
        assert_eq!(
            matches!(
                result.outcomes.get("refs/heads/feature"),
                Some(RefPushOutcome::Rejected(
                    PushRejectReason::AtomicAbort { .. }
                ))
            ),
            atomic
        );
        assert_eq!(
            fixture.refs().await,
            BTreeMap::from([
                ("refs/heads/main".to_owned(), first.clone()),
                (
                    "refs/heads/feature".to_owned(),
                    if atomic { first } else { feature }
                )
            ])
        );
    }
}

#[tokio::test]
async fn missing_source_blob_never_publishes_any_ref_in_the_batch() {
    let fixture = PolicyFixture::new("refs/heads/main").await;
    commit(fixture.repository.path(), "first");
    let specs = [
        policy_spec("HEAD", "refs/heads/main"),
        policy_spec("HEAD", "refs/heads/feature"),
    ];
    assert!(
        fixture
            .push(&specs, PushConfig::default())
            .await
            .unwrap()
            .all_ok()
    );
    let before = fixture.refs().await;
    commit(fixture.repository.path(), "second");
    let blob = git(
        fixture.repository.path(),
        &["rev-parse", "HEAD:tracked.txt"],
    );
    std::fs::remove_file(
        fixture
            .repository
            .path()
            .join(".git/objects")
            .join(&blob[..2])
            .join(&blob[2..]),
    )
    .unwrap();
    assert!(fixture.push(&specs, PushConfig::default()).await.is_err());
    assert_eq!(fixture.refs().await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn unresolved_plan_blocks_replay_before_ref_admission() {
    let store = crate::storage::store::Store::new(Arc::new(InMemory::new()));
    let router = crate::storage::StoreLayout::new(store.clone(), "repos/replay".to_owned());
    let layout =
        crab_storage::StoreLayout::new(store.as_storage().clone(), "repos/replay".to_owned());
    let plan_id = "e".repeat(64);
    let transaction = crab_metadata::capsule_protocol::CapsuleTransaction::for_plan(
        &"1".repeat(64),
        &plan_id,
        vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some("2".repeat(40)),
            None,
        )],
    )
    .unwrap();
    crab_metadata::capsule_protocol::prepare_capsule_plan(
        layout.store(),
        &layout,
        &transaction,
        &"3".repeat(64),
    )
    .await
    .unwrap();
    let config = PushConfig {
        mirror_plan_id: Some(plan_id.clone()),
        ..PushConfig::default()
    };
    // No Root or local repository exists: replay admission must precede both reads.
    for specs in [vec![policy_spec("HEAD", "refs/heads/main")], Vec::new()] {
        let error = run(
            &config,
            &specs,
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
        .unwrap_err();
        let metadata = match &error {
            CrabError::Io(source) => source
                .get_ref()
                .and_then(|source| source.downcast_ref::<crab_metadata::error::MetadataError>()),
            _ => None,
        };
        assert!(matches!(metadata,
            Some(crab_metadata::error::MetadataError::PlanAlreadyAttempted { plan_id: existing }) if existing == &plan_id));
        let released = crab_coordination::PushLock::acquire_internal(
            store.inner(),
            router.repo_prefix(),
            &format!("publication-plan-{plan_id}"),
            config.lock_ttl,
        )
        .await
        .unwrap();
        released.release().await.unwrap();
    }
}

#[test]
fn publication_plan_identity_rejects_noncanonical_values() {
    for plan_id in [
        "A".repeat(64),
        "g".repeat(64),
        "a".repeat(63),
        "a".repeat(65),
    ] {
        assert!(
            validate_publication_plan_context(&PushConfig {
                mirror_plan_id: Some(plan_id),
                ..PushConfig::default()
            })
            .is_err()
        );
    }
}

#[tokio::test]
async fn empty_batch_needs_no_repository_or_remote_root() {
    let store = crate::storage::store::Store::new(Arc::new(InMemory::new()));
    let router = crate::storage::StoreLayout::new(store.clone(), "repos/empty".to_owned());
    let (result, _) = run(
        &PushConfig::default(),
        &[],
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
    assert!(result.outcomes.is_empty());
    assert!(
        store
            .list_prefix(&router.repo_path(""))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn shallow_first_push_never_publishes_an_incomplete_history() {
    let fixture = PolicyFixture::new("refs/heads/main").await;
    commit(fixture.repository.path(), "first");
    commit(fixture.repository.path(), "second");
    let shallow_root = tempfile::tempdir().unwrap();
    git(
        shallow_root.path(),
        &[
            "clone",
            "--depth=1",
            &format!("file://{}", fixture.repository.path().display()),
            "shallow",
        ],
    );
    let shallow = shallow_root.path().join("shallow/.git");
    assert!(shallow.join("shallow").is_file());
    let config = PushConfig {
        git_dir: Some(shallow),
        ..PushConfig::default()
    };
    let result = run(
        &config,
        &[policy_spec("HEAD", "refs/heads/main")],
        &fixture.store,
        &fixture.router,
        None,
        &[],
        None,
        None,
        None,
        &CancellationToken::new(),
    )
    .await;
    assert!(
        result.is_err(),
        "shallow history must not become visible: {result:?}"
    );
    assert!(fixture.refs().await.is_empty());
}

async fn verify_complete_remote_history(fixture: &PolicyFixture, tip: &str, ancestor: &str) {
    let view = crab_read::capsule_protocol::open_view(
        &fixture.layout,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 16 * 1024 * 1024,
            max_frontier_bytes: 16 * 1024 * 1024,
        },
    )
    .await
    .unwrap();
    let visibility = view.git_visibility_index().await.unwrap();
    assert!(
        visibility.ref_closures()["refs/heads/main"]
            .iter()
            .any(|oid| oid == ancestor)
    );
    let destination = tempfile::tempdir().unwrap();
    git(destination.path(), &["init", "--bare"]);
    crab_read::capsule_protocol::install_git_packs_from_store(
        &view,
        &fixture.layout,
        destination.path(),
        16 * 1024 * 1024,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    git(destination.path(), &["update-ref", "refs/heads/main", tip]);
    git(
        destination.path(),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    );
    git(
        destination.path(),
        &["cat-file", "-e", &format!("{ancestor}^{{commit}}")],
    );
    git(destination.path(), &["fsck", "--full", "--strict"]);
}

#[tokio::test]
async fn shallow_grafts_do_not_truncate_available_first_push_history() {
    let fixture = PolicyFixture::new("refs/heads/main").await;
    let first = commit(fixture.repository.path(), "first");
    let tip = commit(fixture.repository.path(), "second");
    let shallow_file = fixture.repository.path().join(".git/shallow");
    let boundary = format!("{tip}\n");
    std::fs::write(&shallow_file, &boundary).unwrap();
    let result = fixture
        .push(
            &[policy_spec("HEAD", "refs/heads/main")],
            PushConfig::default(),
        )
        .await
        .unwrap();
    assert!(result.all_ok());
    verify_complete_remote_history(&fixture, &tip, &first).await;
    assert_eq!(std::fs::read_to_string(shallow_file).unwrap(), boundary);
}

#[tokio::test]
async fn shallow_incremental_push_reuses_complete_remote_ancestry() {
    let fixture = PolicyFixture::new("refs/heads/main").await;
    let first = commit(fixture.repository.path(), "first");
    commit(fixture.repository.path(), "second");
    let specs = [policy_spec("HEAD", "refs/heads/main")];
    assert!(
        fixture
            .push(&specs, PushConfig::default())
            .await
            .unwrap()
            .all_ok()
    );
    let shallow_root = tempfile::tempdir().unwrap();
    git(
        shallow_root.path(),
        &[
            "clone",
            "--depth=1",
            &format!("file://{}", fixture.repository.path().display()),
            "shallow",
        ],
    );
    let shallow = shallow_root.path().join("shallow");
    git(&shallow, &["config", "user.name", "Crab Test"]);
    git(&shallow, &["config", "user.email", "crab@example.invalid"]);
    git(&shallow, &["config", "commit.gpgsign", "false"]);
    let shallow_file = shallow.join(".git/shallow");
    let boundary = std::fs::read(&shallow_file).unwrap();
    let tip = commit(&shallow, "third");
    let config = PushConfig {
        git_dir: Some(shallow.join(".git")),
        ..PushConfig::default()
    };
    let (result, _) = run(
        &config,
        &specs,
        &fixture.store,
        &fixture.router,
        None,
        &[],
        None,
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(result.all_ok());
    verify_complete_remote_history(&fixture, &tip, &first).await;
    assert_eq!(std::fs::read(shallow_file).unwrap(), boundary);
}

fn install_local_history_control(
    fixture: &PolicyFixture,
    tip: &str,
    parent: Option<&str>,
    replacement: bool,
) -> std::path::PathBuf {
    let repository = fixture.repository.path();
    if replacement {
        let mut arguments = vec!["replace", "--graft", tip];
        arguments.extend(parent);
        git(repository, &arguments);
        return repository.join(".git/refs/replace").join(tip);
    }
    let path = repository.join(".git/info/grafts");
    let record = parent.map_or_else(|| format!("{tip}\n"), |parent| format!("{tip} {parent}\n"));
    std::fs::write(&path, record).unwrap();
    path
}

async fn assert_local_history_retains_complete_ancestry(replacement: bool) {
    let fixture = PolicyFixture::new("refs/heads/main").await;
    let first = commit(fixture.repository.path(), "first");
    let tip = commit(fixture.repository.path(), "second");
    let control = install_local_history_control(&fixture, &tip, None, replacement);
    let before = std::fs::read(&control).unwrap();
    let result = fixture
        .push(
            &[policy_spec("HEAD", "refs/heads/main")],
            PushConfig::default(),
        )
        .await
        .unwrap();
    assert!(result.all_ok());
    verify_complete_remote_history(&fixture, &tip, &first).await;
    assert_eq!(std::fs::read(control).unwrap(), before);
}

async fn assert_local_history_cannot_forge_fast_forward(replacement: bool) {
    let fixture = PolicyFixture::new("refs/heads/main").await;
    let first = commit(fixture.repository.path(), "first");
    let remote_tip = commit(fixture.repository.path(), "second");
    let specs = [policy_spec("HEAD", "refs/heads/main")];
    assert!(
        fixture
            .push(&specs, PushConfig::default())
            .await
            .unwrap()
            .all_ok()
    );
    git(fixture.repository.path(), &["checkout", "--detach", &first]);
    let divergent = commit(fixture.repository.path(), "divergent");
    let control =
        install_local_history_control(&fixture, &divergent, Some(&remote_tip), replacement);
    let before = std::fs::read(&control).unwrap();
    let result = fixture
        .push(
            &specs,
            PushConfig {
                receive_deny_non_fast_forwards: true,
                ..PushConfig::default()
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(
            result.outcomes.get("refs/heads/main"),
            Some(RefPushOutcome::Rejected(
                PushRejectReason::DenyNonFastForward
            ))
        ),
        "{result:?}"
    );
    assert_eq!(
        fixture.refs().await,
        BTreeMap::from([("refs/heads/main".to_owned(), remote_tip)])
    );
    assert_eq!(std::fs::read(control).unwrap(), before);
}

#[tokio::test]
async fn local_history_grafts_preserve_complete_ancestry() {
    assert_local_history_retains_complete_ancestry(false).await;
}

#[tokio::test]
async fn local_history_replacements_preserve_complete_ancestry() {
    assert_local_history_retains_complete_ancestry(true).await;
}

#[tokio::test]
async fn local_history_grafts_cannot_forge_fast_forward() {
    assert_local_history_cannot_forge_fast_forward(false).await;
}

#[tokio::test]
async fn local_history_replacements_cannot_forge_fast_forward() {
    assert_local_history_cannot_forge_fast_forward(true).await;
}
