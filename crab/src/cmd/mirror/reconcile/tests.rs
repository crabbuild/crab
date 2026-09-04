use super::super::{ProcessCommand, ProcessOutput, ProcessStatus, SystemCommandRunner};
use super::*;
use std::collections::VecDeque;

struct AncestryRunner {
    statuses: VecDeque<i32>,
}

impl CommandRunner for AncestryRunner {
    fn run(
        &mut self,
        _command: &super::super::ProcessCommand,
        _mode: OutputMode,
    ) -> Result<super::super::ProcessOutput> {
        let code = self.statuses.pop_front().unwrap();
        Ok(super::super::ProcessOutput {
            status: super::super::ProcessStatus {
                success: code == 0,
                code: Some(code),
            },
            stdout: String::new(),
            stderr: if code > 1 {
                "object unavailable".to_owned()
            } else {
                String::new()
            },
        })
    }
}

fn no_lfs_ids(
    _repo: &Path,
    _local: &[String],
    _remote: &[String],
    _cancel: &CancellationToken,
) -> Result<Vec<String>> {
    Ok(Vec::new())
}

fn options() -> MirrorExecution {
    MirrorExecution {
        mode: OutputMode::Json,
        require_remote_helper: false,
        helper_path: None,
        crab_binary: "crab".to_owned(),
        lfs_object_id_collector: no_lfs_ids,
        initialize_destination: |_, _, _| Ok(()),
    }
}

fn status(name: &str, state: MirrorRefState) -> MirrorRefStatus {
    MirrorRefStatus {
        name: name.to_owned(),
        source_oid: Some("a".repeat(40)),
        crab_oid: Some("b".repeat(40)),
        state,
        detail: None,
    }
}

fn check(refs: Vec<MirrorRefStatus>) -> MirrorCheckSummary {
    MirrorCheckSummary {
        source: "source".to_owned(),
        destination: "crab://bucket/repo".to_owned(),
        cache_dir: "cache".to_owned(),
        state: aggregate_state(&refs),
        refs,
        destination_snapshot: Some("d".repeat(64)),
        destination_identity: Some("e".repeat(64)),
        pointers: MirrorPointerStatus {
            discovered: 0,
            verified: 0,
            recipe_digest: Some("c".repeat(64)),
            state: MirrorPointerState::Verified,
            issues: Vec::new(),
        },
        hook: MirrorHookStatus {
            state: MirrorHookState::NotApplicable,
            path: None,
            detail: None,
        },
        ci_passed: false,
        issues: Vec::new(),
    }
}

fn args() -> MirrorArgs {
    MirrorArgs {
        source: "source".to_owned(),
        destination: "crab://bucket/repo".to_owned(),
        cache_dir: None,
        no_atomic: false,
        skip_lfs: false,
        force_lfs_check: false,
        check: false,
        write_plan: None,
        apply_plan: None,
        allow_delete_refs: false,
        ci: false,
        json: false,
        jsonl: false,
    }
}

fn memory_store() -> Store {
    Store::new(std::sync::Arc::new(object_store::memory::InMemory::new()))
        .with_target_identity([0; 32])
}

#[test]
fn plan_identity_binds_metadata_and_recipe_proofs_with_unchanged_refs() {
    let observed = check(vec![status("refs/heads/main", MirrorRefState::SourceAhead)]);
    let original = build_plan(&observed, false).unwrap();
    let mut metadata_change = observed.clone();
    metadata_change.destination_snapshot = Some("e".repeat(64));
    let mut target_change = observed.clone();
    target_change.destination_identity = Some("1".repeat(64));
    let mut recipe_change = observed;
    recipe_change.pointers.recipe_digest = Some("f".repeat(64));
    let plans = [
        original,
        build_plan(&metadata_change, false).unwrap(),
        build_plan(&recipe_change, false).unwrap(),
        build_plan(&target_change, false).unwrap(),
    ];
    assert_eq!(
        plans
            .iter()
            .map(|plan| &plan.plan_id)
            .collect::<BTreeSet<_>>()
            .len(),
        4
    );
}

#[tokio::test]
async fn receipt_for_a_different_ref_edit_cannot_satisfy_the_plan() {
    let plan = build_plan(
        &check(vec![status("refs/heads/main", MirrorRefState::SourceAhead)]),
        false,
    )
    .unwrap();
    let store = memory_store();
    let router = StoreLayout::new(store.clone(), "repo".to_owned());
    let ref_name = "refs/heads/main";
    let head = crate::metadata::manifest::read_ref_journal_head(&store, &router, ref_name)
        .await
        .unwrap();
    let transaction = crate::metadata::manifest::RefJournalTransaction::new(
        BTreeMap::from([(ref_name.to_owned(), head.visible_transaction.clone())]),
        vec![crate::metadata::manifest::RefJournalEdit {
            ref_name: ref_name.to_owned(),
            old_oid: None,
            new_oid: Some("f".repeat(40)),
            peeled_oid: None,
            lock_holder: None,
            visibility_evidence_hash: None,
        }],
        None,
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    crate::metadata::manifest::commit_ref_journal_transaction_for_plan(
        &store,
        &router,
        &transaction,
        &[head],
        &plan.plan_id,
    )
    .await
    .unwrap();

    let error = resolve_plan_commit(&store, &router, &plan)
        .await
        .unwrap_err();

    assert!(
        matches!(error, CrabError::Protocol(message) if message.contains("reviewed ref edits"))
    );
}

#[tokio::test]
async fn managed_receipt_binds_the_reviewed_base_and_result_snapshots() {
    let plan = build_plan(
        &check(vec![status("refs/heads/main", MirrorRefState::SourceAhead)]),
        false,
    )
    .unwrap();
    let store = memory_store();
    let router = StoreLayout::new(store.clone(), "repo".to_owned());
    let storage_router = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let mut base = crate::metadata::manifest::Manifest::default_for_repo("refs/heads/main");
    base.refs
        .insert("refs/heads/main".to_owned(), "b".repeat(40));
    base.seal_git_validation();
    crate::metadata::manifest::create_manifest(&store, &router, &base)
        .await
        .unwrap();
    let (_, etag) = crate::metadata::manifest::read_manifest(&store, &router)
        .await
        .unwrap();
    let mut committed = base;
    committed.generation += 1;
    committed
        .refs
        .insert("refs/heads/main".to_owned(), "a".repeat(40));
    committed.seal_git_validation();
    crab_metadata::plan_receipt::commit_manifest_for_plan(
        store.as_storage(),
        &storage_router,
        &committed,
        &etag,
        &plan.plan_id,
    )
    .await
    .unwrap();

    let resolved = resolve_plan_commit(&store, &router, &plan)
        .await
        .unwrap()
        .unwrap();

    assert!(resolved.manifest_digest.is_some() && resolved.transaction_id.is_none());
}

#[tokio::test]
async fn managed_receipt_for_another_result_cannot_satisfy_the_plan() {
    let plan = build_plan(
        &check(vec![status("refs/heads/main", MirrorRefState::SourceAhead)]),
        false,
    )
    .unwrap();
    let store = memory_store();
    let router = StoreLayout::new(store.clone(), "repo".to_owned());
    let storage_router = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let mut base = crate::metadata::manifest::Manifest::default_for_repo("refs/heads/main");
    base.refs
        .insert("refs/heads/main".to_owned(), "b".repeat(40));
    base.seal_git_validation();
    crate::metadata::manifest::create_manifest(&store, &router, &base)
        .await
        .unwrap();
    let (_, etag) = crate::metadata::manifest::read_manifest(&store, &router)
        .await
        .unwrap();
    let mut unrelated = base;
    unrelated.generation += 1;
    unrelated
        .refs
        .insert("refs/heads/main".to_owned(), "f".repeat(40));
    unrelated.seal_git_validation();
    crab_metadata::plan_receipt::commit_manifest_for_plan(
        store.as_storage(),
        &storage_router,
        &unrelated,
        &etag,
        &plan.plan_id,
    )
    .await
    .unwrap();

    let error = resolve_plan_commit(&store, &router, &plan)
        .await
        .unwrap_err();

    assert!(
        matches!(error, CrabError::Protocol(message) if message.contains("reviewed ref snapshots"))
    );
}

#[tokio::test]
async fn snapshot_identity_requires_and_binds_the_resolved_transport() {
    let raw = Store::new(std::sync::Arc::new(object_store::memory::InMemory::new()));
    let router = StoreLayout::new(raw.clone(), "repo".to_owned());
    crate::core::remote_layout::initialize(&raw, &router)
        .await
        .unwrap();
    crate::metadata::manifest::create_manifest(
        &raw,
        &router,
        &crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main"),
    )
    .await
    .unwrap();
    let snapshot = read_repository_snapshot(&raw, &router).await.unwrap();
    assert!(destination_identity(&raw, &router, &snapshot).is_err());
    let first = raw.clone().with_target_identity([1; 32]);
    let other = raw.with_target_identity([2; 32]);
    assert_ne!(
        snapshot_identity(
            &destination_identity(&first, &router, &snapshot).unwrap(),
            &snapshot
        )
        .unwrap(),
        snapshot_identity(
            &destination_identity(&other, &router, &snapshot).unwrap(),
            &snapshot
        )
        .unwrap()
    );
}

#[test]
fn incomplete_snapshot_or_recipe_proof_blocks_a_plan() {
    for missing in 0..3 {
        let mut observed = check(Vec::new());
        match missing {
            0 => observed.destination_identity = None,
            1 => observed.destination_snapshot = None,
            _ => observed.pointers.recipe_digest = None,
        }
        assert!(build_plan(&observed, false).unwrap().blocked);
    }
}

#[test]
fn aggregate_mixed_direction_is_diverged() {
    let refs = vec![
        status("refs/heads/a", MirrorRefState::SourceAhead),
        status("refs/heads/b", MirrorRefState::CrabAhead),
    ];
    assert_eq!(aggregate_state(&refs), MirrorDriftState::Diverged);
}

#[test]
fn ancestry_classifies_source_ahead() {
    let mut runner = AncestryRunner {
        statuses: VecDeque::from([0]),
    };
    let (state, _) = classify_ref(
        Path::new("."),
        Some("source"),
        Some("crab"),
        &options(),
        &mut runner,
    );
    assert_eq!(state, MirrorRefState::SourceAhead);
}

#[test]
fn ancestry_classifies_crab_ahead() {
    let mut runner = AncestryRunner {
        statuses: VecDeque::from([1, 0]),
    };
    let (state, _) = classify_ref(
        Path::new("."),
        Some("source"),
        Some("crab"),
        &options(),
        &mut runner,
    );
    assert_eq!(state, MirrorRefState::CrabAhead);
}

#[test]
fn ancestry_classifies_true_divergence() {
    let mut runner = AncestryRunner {
        statuses: VecDeque::from([1, 1]),
    };
    let (state, _) = classify_ref(
        Path::new("."),
        Some("source"),
        Some("crab"),
        &options(),
        &mut runner,
    );
    assert_eq!(state, MirrorRefState::Diverged);
}

#[test]
fn ancestry_failure_is_unverifiable() {
    let mut runner = AncestryRunner {
        statuses: VecDeque::from([128]),
    };
    let (state, _) = classify_ref(
        Path::new("."),
        Some("source"),
        Some("crab"),
        &options(),
        &mut runner,
    );
    assert_eq!(state, MirrorRefState::Unverifiable);
}

#[test]
fn plan_blocks_destination_only_ref_without_delete_approval() {
    let mut crab_only = status("refs/heads/recover", MirrorRefState::CrabAhead);
    crab_only.source_oid = None;
    let plan = build_plan(&check(vec![crab_only]), false).unwrap();
    assert!(plan.blocked);
    assert!(plan.actions.is_empty());
}

#[test]
fn delete_approval_is_bound_into_plan_digest() {
    let mut crab_only = status("refs/heads/recover", MirrorRefState::CrabAhead);
    crab_only.source_oid = None;
    let without = build_plan(&check(vec![crab_only.clone()]), false).unwrap();
    let with = build_plan(&check(vec![crab_only]), true).unwrap();
    assert_ne!(without.plan_id, with.plan_id);
    assert_eq!(with.actions[0].kind, MirrorPlanActionKind::DeleteCrabRef);
}

#[test]
fn delete_approval_does_not_rewrite_crab_ahead_ref() {
    let plan = build_plan(
        &check(vec![status(
            "refs/heads/recover",
            MirrorRefState::CrabAhead,
        )]),
        true,
    )
    .unwrap();
    assert!(plan.blocked);
    assert!(plan.actions.is_empty());
}

#[test]
fn plan_digest_rejects_mutation() {
    let plan = build_plan(
        &check(vec![status("refs/heads/main", MirrorRefState::SourceAhead)]),
        false,
    )
    .unwrap();
    let mut changed = plan.clone();
    changed.source = "other".to_owned();
    assert_ne!(plan.plan_id, plan_digest(&changed).unwrap());
}

#[test]
fn canonical_plan_diff_rejects_recomputed_action_mutation() {
    let check = check(vec![status("refs/heads/main", MirrorRefState::SourceAhead)]);
    let mut plan = build_plan(&check, false).unwrap();
    plan.actions[0].expected_source_oid = Some("c".repeat(40));
    plan.plan_id = plan_digest(&plan).unwrap();

    let canonical = build_plan(&check, false).unwrap();
    assert_ne!(canonical.plan_id, plan.plan_id);
}

#[test]
fn write_plan_refuses_to_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plan.json");
    let plan = build_plan(&check(Vec::new()), false).unwrap();
    write_plan(&path, &plan).unwrap();
    assert!(write_plan(&path, &plan).is_err());
}

#[test]
fn integrity_flags_reject_legacy_lfs_controls() {
    let mut args = args();
    args.check = true;
    args.skip_lfs = true;
    assert!(validate_integrity_args(&args).is_err());
}

#[test]
fn deletion_approval_requires_plan_or_check() {
    let mut args = args();
    args.allow_delete_refs = true;
    assert!(validate_integrity_args(&args).is_err());
}

#[tokio::test]
async fn busy_cache_returns_unverifiable_and_blocks_plan_without_source_commands() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.git");
    let _owner = CacheUseGuard::acquire(&path, &CancellationToken::new()).unwrap();
    let mut args = args();
    args.cache_dir = Some(path);
    args.check = true;
    args.ci = true;
    args.write_plan = Some(dir.path().join("blocked.json"));
    let mut runner = AncestryRunner {
        statuses: VecDeque::from([0]),
    };
    let result = run_integrity_command(
        &args,
        &CancellationToken::new(),
        options(),
        &mut runner,
        Ok(memory_store()),
    )
    .await
    .unwrap();
    let MirrorCommandOutcome::Check(check) = result else {
        panic!("expected check")
    };
    assert_eq!(check.state, MirrorDriftState::Unverifiable);
    assert!(!check.ci_passed);
    assert!(
        read_plan(args.write_plan.as_ref().unwrap())
            .unwrap()
            .blocked
    );
}

fn run_local_git(args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .stdin(std::process::Stdio::null())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct DamagedCacheRunner {
    inner: SystemCommandRunner,
    cache: std::path::PathBuf,
    blob: String,
    replacement: Vec<u8>,
    streamed: bool,
    damaged: bool,
    pushed: bool,
}

impl CommandRunner for DamagedCacheRunner {
    fn run(&mut self, command: &ProcessCommand, mode: OutputMode) -> Result<ProcessOutput> {
        self.pushed |= command.args.first().is_some_and(|arg| arg == "push");
        self.streamed |= !command.verify_blobs.is_empty();
        let output = self.inner.run(command, mode)?;
        if !self.damaged
            && output.status.success
            && command
                .args
                .starts_with(&["remote".into(), "add".into(), CRAB_REMOTE.into()])
        {
            let path = self
                .cache
                .join("objects")
                .join(&self.blob[..2])
                .join(&self.blob[2..]);
            // Local clone may hardlink objects. Unlink this fixture cache entry
            // before replacing it so the source's original bytes remain intact.
            std::fs::remove_file(&path)?;
            std::fs::write(path, &self.replacement)?;
            self.damaged = true;
        }
        Ok(output)
    }
}

#[tokio::test]
async fn corrupt_source_cache_blocks_plan_without_publishing_or_using_partial_proof() {
    for oversized_header in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let source_text = source.to_str().unwrap();
        run_local_git(&["init", "-b", "main", source_text]);
        let pointer = Pointer {
            file_hash: [0x42; 32],
            size: 1024,
            shard_hint: None,
        };
        std::fs::write(source.join("data"), pointer.serialize()).unwrap();
        run_local_git(&["-C", source_text, "add", "data"]);
        run_local_git(&[
            "-C",
            source_text,
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "pointer",
        ]);
        let blob = run_local_git(&["-C", source_text, "rev-parse", "HEAD:data"]);
        let replacement = if oversized_header {
            std::fs::write(source.join("large"), vec![0x80; 4096]).unwrap();
            let large = run_local_git(&["-C", source_text, "hash-object", "-w", "large"]);
            std::fs::read(
                source
                    .join(".git/objects")
                    .join(&large[..2])
                    .join(&large[2..]),
            )
            .unwrap()
        } else {
            b"corrupt fixture object".to_vec()
        };
        let mut args = args();
        args.source = source_text.to_owned();
        args.cache_dir = Some(dir.path().join("cache.git"));
        args.check = true;
        args.ci = true;
        args.write_plan = Some(dir.path().join("plan.json"));
        let store = memory_store();
        let router = StoreLayout::new(store.clone(), "repo".into());
        crate::core::remote_layout::initialize(&store, &router)
            .await
            .unwrap();
        crate::metadata::manifest::create_manifest(
            &store,
            &router,
            &crate::metadata::manifest::Manifest::default_for_repo("refs/heads/main"),
        )
        .await
        .unwrap();
        let before = read_repository_snapshot(&store, &router).await.unwrap();
        let cancel = CancellationToken::new();
        let mut runner = DamagedCacheRunner {
            inner: SystemCommandRunner::new(cancel.clone()),
            cache: args.cache_dir.clone().unwrap(),
            blob: blob.clone(),
            replacement,
            streamed: false,
            damaged: false,
            pushed: false,
        };
        let outcome =
            run_integrity_command(&args, &cancel, options(), &mut runner, Ok(store.clone()))
                .await
                .unwrap();
        let MirrorCommandOutcome::Check(check) = outcome else {
            panic!("expected check")
        };
        assert_eq!(check.pointers.state, MirrorPointerState::Unverifiable);
        assert!(!check.ci_passed);
        assert!(
            read_plan(args.write_plan.as_ref().unwrap())
                .unwrap()
                .blocked
        );
        assert!(runner.damaged && !runner.pushed);
        assert_eq!(runner.streamed, oversized_header);
        assert_eq!(
            read_repository_snapshot(&store, &router).await.unwrap(),
            before
        );
        assert_eq!(
            run_local_git(&["-C", source_text, "cat-file", "-p", &blob]),
            String::from_utf8(pointer.serialize()).unwrap().trim()
        );
        assert!(CacheUseGuard::acquire(args.cache_dir.as_ref().unwrap(), &cancel).is_ok());
    }
}

#[tokio::test]
async fn plan_replay_requires_the_same_verified_repository_identity() {
    for nonempty in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.git");
        run_local_git(&["init", "--bare", source.to_str().unwrap()]);
        if nonempty {
            let source = source.to_str().unwrap();
            let tree = run_local_git(&["-C", source, "mktree"]);
            let oid = run_local_git(&[
                "-C",
                source,
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit-tree",
                &tree,
                "-m",
                "fixture",
            ]);
            run_local_git(&["-C", source, "update-ref", "refs/heads/main", &oid]);
            run_local_git(&["-C", source, "symbolic-ref", "HEAD", "refs/heads/main"]);
        }
        let mut args = args();
        args.source = source.display().to_string();
        args.cache_dir = Some(dir.path().join("cache.git"));
        let plan_path = dir.path().join("plan.json");
        args.check = true;
        args.write_plan = Some(plan_path.clone());
        let store = memory_store();
        let router = StoreLayout::new(store.clone(), "repo".to_owned());
        crate::core::remote_layout::initialize(&store, &router)
            .await
            .unwrap();
        crate::metadata::manifest::create_manifest(
            &store,
            &router,
            &crate::metadata::manifest::Manifest::default_for_repo("refs/heads/main"),
        )
        .await
        .unwrap();
        let cancel = CancellationToken::new();
        let mut runner = SystemCommandRunner::new(cancel.clone());
        run_integrity_command(&args, &cancel, options(), &mut runner, Ok(store.clone()))
            .await
            .unwrap();
        let plan = read_plan(&plan_path).unwrap();
        assert!(!plan.blocked);
        assert_eq!(!plan.actions.is_empty(), nonempty);
        if nonempty {
            let ref_name = "refs/heads/main";
            let head = crate::metadata::manifest::read_ref_journal_head(&store, &router, ref_name)
                .await
                .unwrap();
            let transaction = crate::metadata::manifest::RefJournalTransaction::new(
                BTreeMap::from([(ref_name.to_owned(), head.visible_transaction.clone())]),
                vec![crate::metadata::manifest::RefJournalEdit {
                    ref_name: ref_name.to_owned(),
                    old_oid: None,
                    new_oid: plan.source_refs.get(ref_name).cloned(),
                    peeled_oid: None,
                    lock_holder: None,
                    visibility_evidence_hash: None,
                }],
                None,
                Vec::new(),
                Vec::new(),
            )
            .unwrap();
            crate::metadata::manifest::commit_ref_journal_transaction_for_plan(
                &store,
                &router,
                &transaction,
                &[head],
                &plan.plan_id,
            )
            .await
            .unwrap();
        }
        let captured = read_repository_snapshot(&store, &router).await.unwrap();
        args.check = false;
        args.write_plan = None;
        args.apply_plan = Some(plan_path);
        let same =
            run_integrity_command(&args, &cancel, options(), &mut runner, Ok(store.clone())).await;
        assert!(matches!(
            same,
            Ok(MirrorCommandOutcome::Apply(MirrorApplySummary {
                already_applied: true,
                ..
            }))
        ));
        for other in [
            store.clone().with_target_identity([1; 32]),
            store
                .clone()
                .with_storage_scope(crab_types::storage::StorageScope {
                    repo_prefix: router.repo_prefix().to_owned(),
                    global_prefix: router.global_prefix().to_owned(),
                    source_repo: "logical/repo".to_owned(),
                    scope_hash: "new-read-scope".to_owned(),
                }),
        ] {
            let changed =
                run_integrity_command(&args, &cancel, options(), &mut runner, Ok(other)).await;
            assert!(
                matches!(changed, Err(CrabError::Protocol(message)) if message.contains("storage target changed"))
            );
        }
        assert_eq!(
            read_repository_snapshot(&store, &router).await.unwrap(),
            captured
        );

        let path = router.layout_descriptor_path();
        let (original, etag) = store.get_with_etag(&path).await.unwrap();
        let formatted = serde_json::to_vec_pretty(&captured.layout).unwrap();
        store.update(&path, formatted.into(), etag).await.unwrap();
        let equivalent =
            run_integrity_command(&args, &cancel, options(), &mut runner, Ok(store.clone())).await;
        assert!(matches!(
            equivalent,
            Ok(MirrorCommandOutcome::Apply(MirrorApplySummary {
                already_applied: true,
                ..
            }))
        ));

        let plan_bytes = std::fs::read(args.apply_plan.as_ref().unwrap()).unwrap();
        let mut unsupported = captured.layout.clone();
        unsupported.recipe_page_entries += 1;
        for (index, body) in [
            None,
            Some(b"{}".to_vec()),
            Some(serde_json::to_vec(&unsupported).unwrap()),
        ]
        .into_iter()
        .enumerate()
        {
            let (_, etag) = store.get_with_etag(&path).await.unwrap();
            if let Some(body) = body {
                store.update(&path, body.into(), etag).await.unwrap();
            } else {
                store.delete(&path).await.unwrap();
            }
            let refused =
                run_integrity_command(&args, &cancel, options(), &mut runner, Ok(store.clone()))
                    .await;
            assert!(matches!(refused, Err(CrabError::Protocol(_))));
            let mut check_args = args.clone();
            check_args.apply_plan = None;
            check_args.check = true;
            check_args.write_plan = Some(dir.path().join(format!("layout-blocked-{index}.json")));
            let checked = run_integrity_command(
                &check_args,
                &cancel,
                options(),
                &mut runner,
                Ok(store.clone()),
            )
            .await
            .unwrap();
            assert!(
                matches!(checked, MirrorCommandOutcome::Check(check) if check.state == MirrorDriftState::Unverifiable)
            );
            assert!(
                read_plan(check_args.write_plan.as_ref().unwrap())
                    .unwrap()
                    .blocked
            );
            assert_eq!(
                std::fs::read(args.apply_plan.as_ref().unwrap()).unwrap(),
                plan_bytes
            );
            assert_eq!(
                crate::metadata::manifest::read_manifest(&store, &router)
                    .await
                    .unwrap()
                    .0,
                captured.manifest
            );
            // Only this isolated fixture writer restores its descriptor. Neither
            // inspection nor apply may initialize/repair a damaged repository.
            match store.get_with_etag(&path).await {
                Ok((_, etag)) => {
                    store.update(&path, original.clone(), etag).await.unwrap();
                }
                Err(CrabError::NotFound { .. }) => {
                    store.put(&path, original.clone()).await.unwrap();
                }
                Err(error) => panic!("fixture layout read failed: {error}"),
            }
        }
    }
}

#[tokio::test]
async fn unavailable_source_never_reuses_cached_refs_or_applies_a_plan() {
    for existing_cache in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.git");
        let path = dir.path().join("cache.git");
        if existing_cache {
            run_local_git(&["init", "--bare", source.to_str().unwrap()]);
            run_local_git(&[
                "clone",
                "--mirror",
                source.to_str().unwrap(),
                path.to_str().unwrap(),
            ]);
            std::fs::rename(&source, dir.path().join("offline.git")).unwrap();
        }
        let mut args = args();
        args.source = source.display().to_string();
        args.cache_dir = Some(path.clone());
        args.check = true;
        args.ci = true;
        let cancel = CancellationToken::new();
        let result = run_integrity_command(
            &args,
            &cancel,
            options(),
            &mut SystemCommandRunner::default(),
            Ok(memory_store()),
        )
        .await
        .unwrap();
        let MirrorCommandOutcome::Check(observed) = result else {
            panic!("expected check")
        };
        assert_eq!(observed.state, MirrorDriftState::Unverifiable);
        assert!(!observed.ci_passed);
        assert!(observed.refs.is_empty());
        assert!(observed.issues[0].contains("source snapshot unavailable"));

        let mut healthy = check(Vec::new());
        healthy.source = args.source.clone();
        let plan_path = dir.path().join("plan.json");
        write_plan(&plan_path, &build_plan(&healthy, false).unwrap()).unwrap();
        args.check = false;
        args.ci = false;
        args.apply_plan = Some(plan_path);
        assert!(
            run_integrity_command(
                &args,
                &cancel,
                options(),
                &mut SystemCommandRunner::default(),
                Ok(memory_store()),
            )
            .await
            .is_err()
        );
        assert!(
            CacheUseGuard::acquire(&path, &cancel).is_ok(),
            "error path leaked cache owner"
        );
    }
}

struct ApplyOwnershipRunner {
    cache: std::path::PathBuf,
    destination_ref: bool,
    push_count: usize,
    store: Store,
    router: StoreLayout,
    plan_id: String,
    lose_push_response: bool,
}

impl CommandRunner for ApplyOwnershipRunner {
    fn run(&mut self, command: &ProcessCommand, mode: OutputMode) -> Result<ProcessOutput> {
        if command.args != ["--version"] {
            assert!(
                matches!(CacheUseGuard::acquire(&self.cache, &CancellationToken::new()), Err(crab_cache::CacheError::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock),
                "cache unlocked during {:?}",
                command.args
            );
        }
        let mut stdout = String::new();
        let mut success = true;
        let mut stderr = String::new();
        if command.args == ["ls-remote", "--refs", CRAB_REMOTE] {
            if self.destination_ref {
                stdout = format!("{}\trefs/heads/recover\n", "b".repeat(40));
            }
        } else if command.args.first().is_some_and(|arg| arg == "push") {
            let store = self.store.clone();
            let router = self.router.clone();
            let plan_id = self.plan_id.clone();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Runtime::new().unwrap();
                runtime.block_on(async move {
                    let ref_name = "refs/heads/recover";
                    let head =
                        crate::metadata::manifest::read_ref_journal_head(&store, &router, ref_name)
                            .await
                            .unwrap();
                    let transaction = crate::metadata::manifest::RefJournalTransaction::new(
                        BTreeMap::from([(ref_name.to_owned(), head.visible_transaction.clone())]),
                        vec![crate::metadata::manifest::RefJournalEdit {
                            ref_name: ref_name.to_owned(),
                            old_oid: Some("b".repeat(40)),
                            new_oid: None,
                            peeled_oid: None,
                            lock_holder: None,
                            visibility_evidence_hash: None,
                        }],
                        None,
                        Vec::new(),
                        Vec::new(),
                    )
                    .unwrap();
                    crate::metadata::manifest::commit_ref_journal_transaction_for_plan(
                        &store,
                        &router,
                        &transaction,
                        &[head],
                        &plan_id,
                    )
                    .await
                    .unwrap();
                });
            })
            .join()
            .unwrap();
            self.destination_ref = false;
            self.push_count += 1;
            if self.lose_push_response {
                self.lose_push_response = false;
                success = false;
                stderr = "injected lost push response".to_owned();
            }
        } else if !command.args.first().is_some_and(|arg| arg == "fetch") {
            return SystemCommandRunner::default().run(command, mode);
        }
        Ok(ProcessOutput {
            status: ProcessStatus {
                success,
                code: Some(if success { 0 } else { 1 }),
            },
            stdout,
            stderr,
        })
    }
}

async fn run_delete_apply(
    lose_push_response: bool,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    ApplyOwnershipRunner,
    MirrorApplySummary,
) {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.git");
    run_local_git(&["init", "--bare", source.to_str().unwrap()]);
    let mut args = args();
    args.source = source.display().to_string();
    let path = dir.path().join("cache.git");
    args.cache_dir = Some(path.clone());
    args.allow_delete_refs = true;
    let plan_path = dir.path().join("plan.json");
    args.apply_plan = Some(plan_path.clone());
    let mut crab_only = status("refs/heads/recover", MirrorRefState::CrabAhead);
    crab_only.source_oid = None;
    let mut observed = check(vec![crab_only]);
    observed.source = args.source.clone();
    let store = memory_store();
    let router = StoreLayout::new(store.clone(), "repo".to_owned());
    let mut manifest = crate::metadata::manifest::Manifest::default_for_repo("refs/heads/recover");
    crate::core::remote_layout::initialize(&store, &router)
        .await
        .unwrap();
    manifest
        .refs
        .insert("refs/heads/recover".to_owned(), "b".repeat(40));
    manifest.seal_git_validation();
    crate::metadata::manifest::create_manifest(&store, &router, &manifest)
        .await
        .unwrap();
    let snapshot = read_repository_snapshot(&store, &router).await.unwrap();
    let identity = destination_identity(&store, &router, &snapshot).unwrap();
    observed.destination_snapshot = Some(snapshot_identity(&identity, &snapshot).unwrap());
    observed.destination_identity = Some(identity);
    observed.pointers =
        verify_pointer_data(&store, "repo", &snapshot, &[], &CancellationToken::new()).await;
    let plan = build_plan(&observed, true).unwrap();
    write_plan(&plan_path, &plan).unwrap();
    let mut runner = ApplyOwnershipRunner {
        cache: path.clone(),
        destination_ref: true,
        push_count: 0,
        store: store.clone(),
        router,
        plan_id: plan.plan_id,
        lose_push_response,
    };
    let outcome = run_integrity_command(
        &args,
        &CancellationToken::new(),
        options(),
        &mut runner,
        Ok(store),
    )
    .await
    .unwrap();
    let MirrorCommandOutcome::Apply(summary) = outcome else {
        panic!("expected apply")
    };
    (dir, path, runner, summary)
}

#[tokio::test]
async fn apply_keeps_one_cache_owner_through_final_destination_read() {
    let (_dir, path, runner, summary) = run_delete_apply(false).await;
    assert_eq!(summary.actions_applied, 1);
    assert_eq!(runner.push_count, 1);
    assert!(summary.transaction_id.is_some());
    CacheUseGuard::acquire(&path, &CancellationToken::new())
        .expect("apply must release cache ownership after its final destination read");
}

#[tokio::test]
async fn lost_success_response_recovers_the_plan_receipt_without_another_push() {
    let (_dir, _path, runner, summary) = run_delete_apply(true).await;

    assert_eq!(summary.actions_applied, 1);
    assert_eq!(runner.push_count, 1);
    assert!(summary.transaction_id.is_some());
    assert_eq!(summary.current.state, MirrorDriftState::Equal);
}
