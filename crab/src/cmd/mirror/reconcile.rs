//! Read-only mirror integrity inspection and plan-first reconciliation.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

use crab_types::pointer::Pointer;
use tokio_util::sync::CancellationToken;

use super::{
    CRAB_REMOTE, CommandRunner, MirrorArgs, MirrorExecution, check_cancelled, git_command,
    git_command_from_vec, load_local_refs, preflight, prepare_cache, resolve_cache_dir,
    resolve_source, run_required,
};
use crate::core::error::{CrabError, Result};
use crate::core::output::OutputMode;
use crate::git::url::CrabUrl;
use crate::metadata::manifest::{RepositorySnapshot, read_repository_snapshot};
use crate::storage::{Store, StoreLayout};

use super::types::{
    MirrorApplySummary, MirrorCheckSummary, MirrorCommandOutcome, MirrorDriftState,
    MirrorHookState, MirrorHookStatus, MirrorPlanAction, MirrorPlanActionKind, MirrorPointerIssue,
    MirrorPointerState, MirrorPointerStatus, MirrorReconciliationPlan, MirrorRefState,
    MirrorRefStatus,
};
use crab_cache::lifecycle::CacheUseGuard;

const PLAN_FORMAT_VERSION: u32 = 1;
const FETCH_BATCH_SIZE: usize = 128;
const POINTER_SCAN_LIMITS: crab_git::walk::PointerScanLimits = crab_git::walk::PointerScanLimits {
    objects: 2_000_000,
    lookups: 8_000_000,
    allocation_bytes: 64 * 1024 * 1024,
};

pub(super) async fn run_integrity_command(
    args: &MirrorArgs,
    cancel: &CancellationToken,
    options: MirrorExecution,
    runner: &mut dyn CommandRunner,
    store: Result<Store>,
) -> Result<MirrorCommandOutcome> {
    validate_integrity_args(args)?;
    let invocation_dir = std::env::current_dir()?;
    let mut resolved = args.clone();
    resolved.source = resolve_source(&resolved.source, &invocation_dir)?;
    CrabUrl::parse(&resolved.destination)?;
    let args = &resolved;
    let cache_dir = resolve_cache_dir(args, &invocation_dir);
    preflight(runner, &options, false)?;
    check_cancelled(cancel)?;
    let cache = CacheUseGuard::acquire(&cache_dir, cancel);
    if let Some(plan_path) = &args.apply_plan {
        // Apply owns the cache through the final destination read, not just
        // inspection. A second source refresh must not change its Git objects.
        let cache = cache?;
        let mode = options.mode;
        let summary = apply_plan(args, plan_path, &cache, cancel, options, runner, &store?).await?;
        render_apply(&summary, mode);
        return Ok(MirrorCommandOutcome::Apply(summary));
    }

    let check = match cache {
        Ok(cache) => match store {
            Ok(store) => inspect(args, &cache, cancel, &options, runner, &store).await?,
            Err(error) => unverifiable_check(
                args,
                &cache_dir,
                super::hook::mirror_hook_status(Path::new(&args.source)),
                format!("Crab provider unavailable: {error}"),
            ),
        },
        Err(error) => unverifiable_check(
            args,
            &cache_dir,
            super::hook::mirror_hook_status(Path::new(&args.source)),
            format!("mirror cache unavailable: {error}"),
        ),
    };
    if let Some(path) = &args.write_plan {
        let plan = build_plan(&check, args.allow_delete_refs)?;
        write_plan(path, &plan)?;
        if options.mode == OutputMode::Text {
            eprintln!(
                "mirror: wrote immutable reconciliation plan {} to {}",
                plan.plan_id,
                path.display()
            );
        }
    }
    render_check(&check, options.mode);
    Ok(MirrorCommandOutcome::Check(Box::new(check)))
}

fn validate_integrity_args(args: &MirrorArgs) -> Result<()> {
    if args.write_plan.is_some() && !args.check {
        return Err(CrabError::Configuration {
            key: "--write-plan requires --check".to_owned(),
            origin: "crab mirror".to_owned(),
        });
    }
    if args.ci && !args.check {
        return Err(CrabError::Configuration {
            key: "--ci requires --check".to_owned(),
            origin: "crab mirror".to_owned(),
        });
    }
    if args.apply_plan.is_some() && (args.check || args.write_plan.is_some() || args.ci) {
        return Err(CrabError::Configuration {
            key: "--apply-plan conflicts with --check, --write-plan, and --ci".to_owned(),
            origin: "crab mirror".to_owned(),
        });
    }
    if args.allow_delete_refs && !args.check && args.apply_plan.is_none() {
        return Err(CrabError::Configuration {
            key: "--allow-delete-refs requires --check or --apply-plan".to_owned(),
            origin: "crab mirror".to_owned(),
        });
    }
    if (args.check || args.apply_plan.is_some())
        && (args.no_atomic || args.skip_lfs || args.force_lfs_check)
    {
        return Err(CrabError::Configuration {
            key: "--no-atomic, --skip-lfs, and --force-lfs-check apply only to legacy mirror execution"
                .to_owned(),
            origin: "crab mirror".to_owned(),
        });
    }
    Ok(())
}

async fn inspect(
    args: &MirrorArgs,
    cache: &CacheUseGuard,
    cancel: &CancellationToken,
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
    store: &Store,
) -> Result<MirrorCheckSummary> {
    let parsed = CrabUrl::parse(&args.destination)?;
    let cache_dir = cache.path();
    let hook = super::hook::mirror_hook_status(Path::new(&args.source));
    check_cancelled(cancel)?;
    let source_snapshot = prepare_cache(args, cache, cancel, options, runner)
        .and_then(|_| load_local_refs(cache_dir, options, runner));
    check_cancelled(cancel)?;
    let source_refs = match source_snapshot {
        Ok(snapshot) => snapshot,
        Err(error) => {
            // Never use refs left by an earlier successful refresh as evidence
            // that an unavailable source is currently equal or safe to apply.
            return Ok(unverifiable_check(
                args,
                cache_dir,
                hook,
                format!("source snapshot unavailable: {error}"),
            ));
        }
    };
    super::ensure_crab_remote(cache_dir, &args.destination, options, runner)?;
    check_cancelled(cancel)?;
    let router = StoreLayout::new(store.clone(), parsed.repo_path.clone());
    let snapshot_read = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
        result = read_repository_snapshot(store, &router) => result,
    };
    let snapshot = match snapshot_read {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return Ok(unverifiable_check(
                args,
                cache_dir,
                hook,
                format!("Crab provider unavailable: {error}"),
            ));
        }
    };
    let crab_refs = &snapshot.journal.refs;
    let destination_identity = match destination_identity(store, &router, &snapshot) {
        Ok(identity) => identity,
        Err(error) => return Ok(unverifiable_check(args, cache_dir, hook, error.to_string())),
    };
    let destination_snapshot = Some(snapshot_identity(&destination_identity, &snapshot)?);
    check_cancelled(cancel)?;

    if let Err(error) =
        fetch_changed_crab_objects(cache_dir, &source_refs, crab_refs, options, runner)
    {
        return Ok(unverifiable_check(
            args,
            cache_dir,
            hook,
            format!("Crab object fetch unavailable: {error}"),
        ));
    }
    let refs = classify_refs(cache_dir, &source_refs, crab_refs, options, runner);
    let mut issues = refs
        .iter()
        .filter(|status| status.state != MirrorRefState::Equal)
        .map(|status| format!("{}: {}", status.name, status.state))
        .collect::<Vec<_>>();
    let state = aggregate_state(&refs);

    let pointers = match collect_source_pointers(cache_dir, &source_refs, cancel, runner).await {
        Ok(pointers) => {
            verify_pointer_data(store, &parsed.repo_path, &snapshot, &pointers, cancel).await
        }
        Err(error) => {
            MirrorPointerStatus::unverifiable(format!("source pointer scan failed: {error}"))
        }
    };
    if pointers.state != MirrorPointerState::Verified {
        issues.extend(pointers.issues.iter().map(|issue| issue.detail.clone()));
    }
    if hook.state == MirrorHookState::Missing {
        issues.push("mirror pre-push hook is missing".to_owned());
    }
    if hook.state == MirrorHookState::Unverifiable {
        issues.push("mirror pre-push hook could not be inspected".to_owned());
    }

    let ci_passed = state == MirrorDriftState::Equal
        && pointers.state == MirrorPointerState::Verified
        && !matches!(
            hook.state,
            MirrorHookState::Missing | MirrorHookState::Unverifiable
        );
    Ok(MirrorCheckSummary {
        source: args.source.clone(),
        destination: args.destination.clone(),
        cache_dir: cache_dir.display().to_string(),
        state,
        refs,
        destination_identity: Some(destination_identity),
        destination_snapshot,
        pointers,
        hook,
        ci_passed,
        issues,
    })
}

fn unverifiable_check(
    args: &MirrorArgs,
    cache_dir: &Path,
    hook: MirrorHookStatus,
    detail: String,
) -> MirrorCheckSummary {
    MirrorCheckSummary {
        source: args.source.clone(),
        destination: args.destination.clone(),
        cache_dir: cache_dir.display().to_string(),
        state: MirrorDriftState::Unverifiable,
        refs: Vec::new(),
        destination_identity: None,
        destination_snapshot: None,
        pointers: MirrorPointerStatus::unverifiable(detail.clone()),
        hook,
        ci_passed: false,
        issues: vec![detail],
    }
}

fn fetch_changed_crab_objects(
    cache_dir: &Path,
    source: &BTreeMap<String, String>,
    crab: &BTreeMap<String, String>,
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
) -> Result<()> {
    let refs = source
        .iter()
        .filter_map(|(name, oid)| crab.get(name).filter(|other| *other != oid))
        .cloned()
        .collect::<Vec<_>>();
    for chunk in refs.chunks(FETCH_BATCH_SIZE) {
        let mut args = vec![
            "fetch".to_owned(),
            "--no-auto-gc".to_owned(),
            "--no-tags".to_owned(),
            CRAB_REMOTE.to_owned(),
        ];
        args.extend(chunk.iter().cloned());
        run_required(
            runner,
            git_command_from_vec(args, Some(cache_dir), options, false),
            options.mode,
        )?;
    }
    Ok(())
}

fn classify_refs(
    cache_dir: &Path,
    source: &BTreeMap<String, String>,
    crab: &BTreeMap<String, String>,
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
) -> Vec<MirrorRefStatus> {
    let names = source
        .keys()
        .chain(crab.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    names
        .into_iter()
        .map(|name| {
            let source_oid = source.get(&name).cloned();
            let crab_oid = crab.get(&name).cloned();
            let (state, detail) = classify_ref(
                cache_dir,
                source_oid.as_deref(),
                crab_oid.as_deref(),
                options,
                runner,
            );
            MirrorRefStatus {
                name,
                source_oid,
                crab_oid,
                state,
                detail,
            }
        })
        .collect()
}

fn classify_ref(
    cache_dir: &Path,
    source_oid: Option<&str>,
    crab_oid: Option<&str>,
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
) -> (MirrorRefState, Option<String>) {
    match (source_oid, crab_oid) {
        (Some(source), Some(crab)) if source == crab => (MirrorRefState::Equal, None),
        (Some(_), None) => (MirrorRefState::SourceAhead, None),
        (None, Some(_)) => (MirrorRefState::CrabAhead, None),
        (Some(source), Some(crab)) => {
            match is_ancestor(cache_dir, crab, source, options, runner) {
                Ok(true) => return (MirrorRefState::SourceAhead, None),
                Ok(false) => {}
                Err(detail) => return (MirrorRefState::Unverifiable, Some(detail)),
            }
            match is_ancestor(cache_dir, source, crab, options, runner) {
                Ok(true) => (MirrorRefState::CrabAhead, None),
                Ok(false) => (MirrorRefState::Diverged, None),
                Err(detail) => (MirrorRefState::Unverifiable, Some(detail)),
            }
        }
        (None, None) => (
            MirrorRefState::Unverifiable,
            Some("ref has no source or Crab object id".to_owned()),
        ),
    }
}

fn is_ancestor(
    cache_dir: &Path,
    older: &str,
    newer: &str,
    options: &MirrorExecution,
    runner: &mut dyn CommandRunner,
) -> std::result::Result<bool, String> {
    let command = git_command(
        ["merge-base", "--is-ancestor", older, newer],
        Some(cache_dir),
        options,
        false,
    );
    match runner.run(&command, options.mode) {
        Ok(output) if output.status.success => Ok(true),
        Ok(output) if output.status.code == Some(1) => Ok(false),
        Ok(output) => Err(super::command_failure_detail(
            &command,
            &output,
            "Git could not determine ancestry",
        )),
        Err(error) => Err(error.to_string()),
    }
}

fn aggregate_state(refs: &[MirrorRefStatus]) -> MirrorDriftState {
    if refs
        .iter()
        .any(|status| status.state == MirrorRefState::Unverifiable)
    {
        return MirrorDriftState::Unverifiable;
    }
    if refs
        .iter()
        .any(|status| status.state == MirrorRefState::Diverged)
    {
        return MirrorDriftState::Diverged;
    }
    let source_ahead = refs
        .iter()
        .any(|status| status.state == MirrorRefState::SourceAhead);
    let crab_ahead = refs
        .iter()
        .any(|status| status.state == MirrorRefState::CrabAhead);
    match (source_ahead, crab_ahead) {
        (false, false) => MirrorDriftState::Equal,
        (true, false) => MirrorDriftState::SourceAhead,
        (false, true) => MirrorDriftState::CrabAhead,
        (true, true) => MirrorDriftState::Diverged,
    }
}

pub(super) async fn collect_source_pointers(
    cache_dir: &Path,
    refs: &BTreeMap<String, String>,
    cancel: &CancellationToken,
    runner: &mut dyn CommandRunner,
) -> Result<Vec<Pointer>> {
    check_cancelled(cancel)?;
    let git_dir = cache_dir.to_owned();
    let refs = refs
        .iter()
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect::<Vec<_>>();
    let scan_cancel = cancel.clone();
    // Await the cooperative worker even on cancellation: the caller's cache
    // guard must not be released while a background scan still reads its ODB.
    let reachable = tokio::task::spawn_blocking(move || {
        crab_git::walk::scan_pointers(&git_dir, &refs, POINTER_SCAN_LIMITS, &|| {
            scan_cancel.is_cancelled()
        })
    })
    .await
    .map_err(|source| CrabError::Io(std::io::Error::other(source)))??;
    check_cancelled(cancel)?;
    if !reachable.unchecked_blobs.is_empty() {
        // A plausible oversized header is not evidence that an object is not
        // a pointer. Stream its exact raw bytes and bind kind/size/hash before
        // allowing the candidate inventory to authorize a check or publication.
        let command = super::ProcessCommand::new(
            "git",
            vec![
                "--no-replace-objects".to_owned(),
                "--git-dir=.".to_owned(),
                "cat-file".to_owned(),
                "--batch".to_owned(),
            ],
        )
        .current_dir(Some(cache_dir))
        .env_remove(super::GIT_ENV_REMOVALS)
        .env("GIT_NO_LAZY_FETCH", "1".into())
        // Old Git clients ignore NO_LAZY_FETCH; inspection permits no transport.
        .env("GIT_ALLOW_PROTOCOL", "".into())
        .verify_blobs(reachable.unchecked_blobs);
        run_required(runner, command, OutputMode::Json)?;
    }
    let mut pointers = BTreeMap::<[u8; 32], u64>::new();
    for pointer in reachable.pointers {
        check_cancelled(cancel)?;
        match pointers.insert(pointer.file_hash, pointer.size) {
            Some(size) if size != pointer.size => {
                return Err(CrabError::CorruptObject {
                    path: crab_types::pointer::hex_encode(&pointer.file_hash),
                    reason: format!(
                        "the same file hash is declared with sizes {size} and {}",
                        pointer.size
                    ),
                });
            }
            _ => {}
        }
    }
    Ok(pointers
        .into_iter()
        .map(|(file_hash, size)| Pointer {
            file_hash,
            size,
            shard_hint: None,
        })
        .collect())
}

async fn verify_pointer_data(
    store: &Store,
    repo_prefix: &str,
    snapshot: &RepositorySnapshot,
    pointers: &[Pointer],
    cancel: &CancellationToken,
) -> MirrorPointerStatus {
    let checker = crate::cmd::fsck_store::StoreChecker::new(store.clone(), repo_prefix.to_owned());
    match checker
        .verify_pointer_data(snapshot, pointers, cancel)
        .await
    {
        Ok(verification) => {
            let issues = verification
                .issues
                .into_iter()
                .map(|issue| {
                    use crate::cmd::fsck_store::PointerDataIssueKind;
                    MirrorPointerIssue {
                        file_hash: issue.file_hash,
                        expected_size: issue.expected_size,
                        state: match issue.kind {
                            PointerDataIssueKind::Missing => MirrorPointerState::Missing,
                            PointerDataIssueKind::Corrupt => MirrorPointerState::Corrupt,
                            PointerDataIssueKind::Unverifiable => MirrorPointerState::Unverifiable,
                        },
                        detail: issue.detail,
                    }
                })
                .collect::<Vec<_>>();
            let state = [
                MirrorPointerState::Unverifiable,
                MirrorPointerState::Corrupt,
                MirrorPointerState::Missing,
            ]
            .into_iter()
            .find(|state| issues.iter().any(|issue| issue.state == *state))
            .unwrap_or(MirrorPointerState::Verified);
            MirrorPointerStatus {
                discovered: pointers.len() as u64,
                verified: verification.verified,
                recipe_digest: verification.recipe_digest,
                state,
                issues,
            }
        }
        Err(error) => MirrorPointerStatus::unverifiable(error.to_string()),
    }
}

fn destination_identity(
    store: &Store,
    router: &StoreLayout,
    snapshot: &RepositorySnapshot,
) -> Result<String> {
    let identity = store.bucket_identity();
    let target_identity = store.as_storage().target_identity().ok_or_else(|| {
        CrabError::Protocol("Crab storage target identity is unavailable".to_owned())
    })?;
    let fields = (
        identity.cloud,
        identity.host,
        identity.container,
        target_identity,
        router.repo_prefix(),
        router.global_prefix(),
        store.storage_scope(),
        &snapshot.layout.digest,
    );
    let mut hasher = blake3::Hasher::new_derive_key("crab mirror destination identity v1");
    serde_json::to_writer(&mut hasher, &fields).map_err(std::io::Error::other)?;
    Ok(hasher.finalize().to_hex().to_string())
}

fn snapshot_identity(identity: &str, snapshot: &RepositorySnapshot) -> Result<String> {
    let fields = (identity, snapshot.digest()?);
    let mut hasher = blake3::Hasher::new_derive_key("crab mirror destination snapshot v1");
    serde_json::to_writer(&mut hasher, &fields).map_err(std::io::Error::other)?;
    Ok(hasher.finalize().to_hex().to_string())
}

fn build_plan(
    check: &MirrorCheckSummary,
    allow_delete_refs: bool,
) -> Result<MirrorReconciliationPlan> {
    let mut blockers = Vec::new();
    let mut actions = Vec::new();
    for status in &check.refs {
        match status.state {
            MirrorRefState::Equal => {}
            MirrorRefState::SourceAhead => actions.push(MirrorPlanAction {
                kind: MirrorPlanActionKind::UpdateCrabRef,
                ref_name: status.name.clone(),
                expected_source_oid: status.source_oid.clone(),
                expected_crab_oid: status.crab_oid.clone(),
            }),
            MirrorRefState::CrabAhead if status.source_oid.is_none() && allow_delete_refs => {
                actions.push(MirrorPlanAction {
                    kind: MirrorPlanActionKind::DeleteCrabRef,
                    ref_name: status.name.clone(),
                    expected_source_oid: None,
                    expected_crab_oid: status.crab_oid.clone(),
                });
            }
            MirrorRefState::CrabAhead if status.source_oid.is_none() => blockers.push(format!(
                "{} exists only in Crab; recreate the plan with --allow-delete-refs after review",
                status.name
            )),
            MirrorRefState::CrabAhead => blockers.push(format!(
                "{} is ahead in Crab; retain it until the collaboration source advances or an operator chooses a source of truth",
                status.name
            )),
            MirrorRefState::Diverged => blockers.push(format!(
                "{} diverged; choose a source of truth before creating a plan",
                status.name
            )),
            MirrorRefState::Unverifiable => {
                blockers.push(format!("{} ancestry is unverifiable", status.name));
            }
        }
    }
    if check.pointers.state != MirrorPointerState::Verified
        || check.pointers.recipe_digest.is_none()
        || check.pointers.discovered != check.pointers.verified
    {
        blockers.push("source pointer recipes or data are not fully verified in Crab".to_owned());
    }
    if check.destination_snapshot.is_none() {
        blockers.push("Crab metadata snapshot is unverifiable".to_owned());
    }
    if check.destination_identity.is_none() {
        blockers.push("Crab storage target identity is unverifiable".to_owned());
    }
    if check.state == MirrorDriftState::Unverifiable {
        blockers.push("mirror state is unverifiable".to_owned());
    }

    let mut plan = MirrorReconciliationPlan {
        format_version: PLAN_FORMAT_VERSION,
        plan_id: String::new(),
        source: check.source.clone(),
        destination: check.destination.clone(),
        source_refs: ref_map(&check.refs, true),
        crab_refs: ref_map(&check.refs, false),
        destination_identity: check.destination_identity.clone(),
        destination_snapshot: check.destination_snapshot.clone(),
        recipe_digest: check.pointers.recipe_digest.clone(),
        pointer_count: check.pointers.discovered,
        allow_delete_refs,
        blocked: !blockers.is_empty(),
        blockers,
        actions,
    };
    plan.plan_id = plan_digest(&plan)?;
    Ok(plan)
}

fn ref_map(refs: &[MirrorRefStatus], source: bool) -> BTreeMap<String, String> {
    refs.iter()
        .filter_map(|status| {
            let oid = if source {
                status.source_oid.as_ref()
            } else {
                status.crab_oid.as_ref()
            }?;
            Some((status.name.clone(), oid.clone()))
        })
        .collect()
}

fn plan_digest(plan: &MirrorReconciliationPlan) -> Result<String> {
    let mut canonical = plan.clone();
    canonical.plan_id.clear();
    let bytes = serde_json::to_vec(&canonical).map_err(|error| {
        CrabError::Internal(format!(
            "failed to serialize mirror reconciliation plan: {error}"
        ))
    })?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn write_plan(path: &Path, plan: &MirrorReconciliationPlan) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| CrabError::Configuration {
            key: format!("failed to create reconciliation plan: {error}"),
            origin: path.display().to_string(),
        })?;
    let body = serde_json::to_vec_pretty(plan).map_err(|error| {
        CrabError::Internal(format!("failed to serialize reconciliation plan: {error}"))
    })?;
    file.write_all(&body)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn read_plan(path: &Path) -> Result<MirrorReconciliationPlan> {
    let body = std::fs::read(path)?;
    let plan: MirrorReconciliationPlan =
        serde_json::from_slice(&body).map_err(|error| CrabError::Configuration {
            key: format!("invalid mirror reconciliation plan: {error}"),
            origin: path.display().to_string(),
        })?;
    if plan.format_version != PLAN_FORMAT_VERSION {
        return Err(CrabError::IncompatibleFormat {
            required: format!("mirror reconciliation plan version {PLAN_FORMAT_VERSION}"),
            found: plan.format_version.to_string(),
        });
    }
    let digest = plan_digest(&plan)?;
    if digest != plan.plan_id {
        return Err(CrabError::Configuration {
            key: "mirror reconciliation plan digest mismatch".to_owned(),
            origin: path.display().to_string(),
        });
    }
    Ok(plan)
}

async fn apply_plan(
    args: &MirrorArgs,
    plan_path: &Path,
    cache: &CacheUseGuard,
    cancel: &CancellationToken,
    options: MirrorExecution,
    runner: &mut dyn CommandRunner,
    store: &Store,
) -> Result<MirrorApplySummary> {
    let plan = read_plan(plan_path)?;
    if plan.blocked {
        return Err(CrabError::Configuration {
            key: "reconciliation plan is blocked".to_owned(),
            origin: plan.blockers.join("; "),
        });
    }
    let source = args.source.clone();
    if source != plan.source || args.destination != plan.destination {
        return Err(CrabError::Configuration {
            key: "reconciliation plan target mismatch".to_owned(),
            origin: format!(
                "plan is for {} -> {}, invocation is for {} -> {}",
                plan.source, plan.destination, source, args.destination
            ),
        });
    }
    if plan.allow_delete_refs && !args.allow_delete_refs {
        return Err(CrabError::Configuration {
            key: "plan contains Crab ref deletions".to_owned(),
            origin: "pass --allow-delete-refs again after reviewing the plan".to_owned(),
        });
    }

    let before = inspect(args, cache, cancel, &options, runner, store).await?;
    // Metadata may advance after publication, but a converged replay must never
    // inherit success from another target containing the same refs and recipes.
    if before.destination_identity.is_none()
        || before.destination_identity != plan.destination_identity
    {
        return Err(CrabError::Protocol(
            "reconciliation storage target changed or is unavailable; create a new plan".to_owned(),
        ));
    }
    let parsed = CrabUrl::parse(&args.destination)?;
    let router = StoreLayout::new(store.clone(), parsed.repo_path);
    if !plan.actions.is_empty()
        && let Some(commit) = resolve_plan_commit(store, &router, &plan).await?
    {
        return Ok(MirrorApplySummary {
            plan_id: plan.plan_id,
            source,
            destination: args.destination.clone(),
            actions_planned: plan.actions.len() as u64,
            actions_applied: 0,
            already_applied: true,
            transaction_id: commit.transaction_id,
            manifest_digest: commit.manifest_digest,
            final_state: before.state,
            current: Box::new(before),
        });
    }
    if before.pointers.state != MirrorPointerState::Verified {
        return Err(CrabError::Protocol(
            "reconciliation refused because pointer data is not fully verified".to_owned(),
        ));
    }
    let current_source = ref_map(&before.refs, true);
    let current_crab = ref_map(&before.refs, false);
    if current_source == plan.source_refs && current_crab == current_source {
        if !plan.actions.is_empty() {
            return Err(CrabError::Protocol(
                "Crab refs match the plan but no plan-bound receipt exists; another writer cannot satisfy this reconciliation plan"
                    .to_owned(),
            ));
        }
        if plan.actions.is_empty() && before.destination_snapshot != plan.destination_snapshot {
            return Err(CrabError::Protocol(
                "reconciliation metadata snapshot changed; create a new plan".to_owned(),
            ));
        }
        if before.pointers.recipe_digest != plan.recipe_digest {
            return Err(CrabError::Protocol(
                "reconciliation recipe proof changed; create a new plan".to_owned(),
            ));
        }
        return Ok(MirrorApplySummary {
            plan_id: plan.plan_id,
            source,
            destination: args.destination.clone(),
            actions_planned: plan.actions.len() as u64,
            actions_applied: 0,
            already_applied: true,
            transaction_id: None,
            manifest_digest: None,
            final_state: MirrorDriftState::Equal,
            current: Box::new(before),
        });
    }
    if current_source != plan.source_refs || current_crab != plan.crab_refs {
        return Err(CrabError::Protocol(
            "reconciliation plan is stale; run `crab mirror --check --write-plan` again".to_owned(),
        ));
    }
    let canonical_plan = build_plan(&before, plan.allow_delete_refs)?;
    if canonical_plan.blocked || canonical_plan.plan_id != plan.plan_id {
        return Err(CrabError::Protocol(
            "reconciliation plan actions do not match the revalidated ref state".to_owned(),
        ));
    }

    let cache_dir = cache.path();
    let mut push_args = vec!["push".to_owned()];
    push_args.push("--atomic".to_owned());
    // Carry the reviewed old values through Git into the push pipeline;
    // an earlier read alone cannot protect a deletion from concurrent writes.
    for action in &plan.actions {
        push_args.push(format!(
            "--force-with-lease={}:{}",
            action.ref_name,
            action.expected_crab_oid.as_deref().unwrap_or_default()
        ));
    }
    push_args.push(CRAB_REMOTE.to_owned());
    for action in &plan.actions {
        match action.kind {
            MirrorPlanActionKind::UpdateCrabRef => {
                let oid = action.expected_source_oid.as_deref().ok_or_else(|| {
                    CrabError::Internal(format!(
                        "update action for {} has no source object id",
                        action.ref_name
                    ))
                })?;
                push_args.push(format!("{oid}:{}", action.ref_name));
            }
            MirrorPlanActionKind::DeleteCrabRef => {
                push_args.push(format!(":{}", action.ref_name));
            }
        }
    }
    if !plan.actions.is_empty() {
        let command = git_command_from_vec(push_args, Some(cache_dir), &options, true)
            .env(crate::git::push_native::MIRROR_GIT_ONLY_ENV, "1".into())
            .env(
                crate::git::push_native::MIRROR_PLAN_ID_ENV,
                plan.plan_id.clone().into(),
            );
        let push_result = run_required(runner, command, options.mode);
        if let Err(push_error) = push_result {
            match resolve_plan_commit(store, &router, &plan).await? {
                Some(commit) => {
                    let current = inspect(args, cache, cancel, &options, runner, store).await?;
                    return Ok(MirrorApplySummary {
                        plan_id: plan.plan_id,
                        source,
                        destination: args.destination.clone(),
                        actions_planned: plan.actions.len() as u64,
                        actions_applied: plan.actions.len() as u64,
                        already_applied: false,
                        transaction_id: commit.transaction_id,
                        manifest_digest: commit.manifest_digest,
                        final_state: current.state,
                        current: Box::new(current),
                    });
                }
                None => return Err(push_error),
            }
        }
    }
    check_cancelled(cancel)?;
    let commit = resolve_plan_commit(store, &router, &plan)
        .await?
        .ok_or_else(|| {
        CrabError::Protocol(
            "reconciliation push returned without a durable plan receipt; commit outcome is uncertain"
                .to_owned(),
        )
    })?;
    let current = inspect(args, cache, cancel, &options, runner, store).await?;
    let summary = MirrorApplySummary {
        plan_id: plan.plan_id,
        source,
        destination: args.destination.clone(),
        actions_planned: plan.actions.len() as u64,
        actions_applied: plan.actions.len() as u64,
        already_applied: false,
        transaction_id: commit.transaction_id,
        manifest_digest: commit.manifest_digest,
        final_state: current.state,
        current: Box::new(current),
    };
    Ok(summary)
}

#[derive(Debug)]
struct ResolvedPlanCommit {
    transaction_id: Option<String>,
    manifest_digest: Option<String>,
}

async fn resolve_plan_commit(
    store: &Store,
    router: &StoreLayout,
    plan: &MirrorReconciliationPlan,
) -> Result<Option<ResolvedPlanCommit>> {
    let Some(receipt) =
        crate::metadata::manifest::resolve_mirror_plan_receipt(store, router, &plan.plan_id)
            .await?
    else {
        return Ok(None);
    };
    match receipt.commit {
        crab_metadata::plan_receipt::MirrorPlanCommit::RefJournal { transaction_id, .. } => {
            let transaction = crate::metadata::manifest::read_ref_journal_transaction(
                store,
                router,
                &transaction_id,
            )
            .await?;
            let expected = plan
                .actions
                .iter()
                .map(|action| {
                    let next = match action.kind {
                        MirrorPlanActionKind::UpdateCrabRef => action.expected_source_oid.clone(),
                        MirrorPlanActionKind::DeleteCrabRef => None,
                    };
                    (
                        action.ref_name.clone(),
                        (action.expected_crab_oid.clone(), next),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let actual = transaction
                .edits
                .iter()
                .map(|edit| {
                    (
                        edit.ref_name.clone(),
                        (edit.old_oid.clone(), edit.new_oid.clone()),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            if expected.len() != plan.actions.len()
                || actual.len() != transaction.edits.len()
                || actual != expected
            {
                return Err(CrabError::Protocol(
                    "mirror plan receipt transaction does not match the reviewed ref edits"
                        .to_owned(),
                ));
            }
            Ok(Some(ResolvedPlanCommit {
                transaction_id: Some(transaction_id),
                manifest_digest: None,
            }))
        }
        crab_metadata::plan_receipt::MirrorPlanCommit::Manifest {
            base_generation,
            base_digest,
            generation,
            digest,
        } => {
            let base = crate::metadata::manifest::read_mirror_plan_manifest(
                store,
                router,
                base_generation,
                &base_digest,
            )
            .await?
            .ok_or_else(|| {
                CrabError::Protocol(
                    "managed mirror plan receipt base manifest is unavailable".to_owned(),
                )
            })?;
            let committed = crate::metadata::manifest::read_mirror_plan_manifest(
                store, router, generation, &digest,
            )
            .await?
            .ok_or_else(|| {
                CrabError::Protocol(
                    "managed mirror plan receipt manifest is unavailable".to_owned(),
                )
            })?;
            if base.refs != plan.crab_refs || committed.refs != plan.source_refs {
                return Err(CrabError::Protocol(
                    "managed mirror plan receipt does not match the reviewed ref snapshots"
                        .to_owned(),
                ));
            }
            Ok(Some(ResolvedPlanCommit {
                transaction_id: None,
                manifest_digest: Some(digest),
            }))
        }
    }
}

fn render_apply(summary: &MirrorApplySummary, mode: OutputMode) {
    if mode != OutputMode::Text {
        return;
    }
    let outcome = if summary.already_applied {
        "was already committed"
    } else {
        "committed"
    };
    let commit = summary
        .transaction_id
        .as_deref()
        .or(summary.manifest_digest.as_deref())
        .unwrap_or("none");
    eprintln!(
        "mirror: plan {} {outcome}; submitted {} of {} action(s); commit {commit}; current state {}",
        summary.plan_id, summary.actions_applied, summary.actions_planned, summary.final_state
    );
    render_check(&summary.current, mode);
}

fn render_check(check: &MirrorCheckSummary, mode: OutputMode) {
    if mode != OutputMode::Text {
        return;
    }
    eprintln!("Mirror integrity: {}", check.state);
    eprintln!(
        "  Refs: {} total, {} changed",
        check.refs.len(),
        check
            .refs
            .iter()
            .filter(|status| status.state != MirrorRefState::Equal)
            .count()
    );
    eprintln!(
        "  Pointers: {} verified of {}",
        check.pointers.verified, check.pointers.discovered
    );
    eprintln!("  Pre-push hook: {}", check.hook.state);
    for issue in &check.issues {
        eprintln!("  Issue: {issue}");
    }
}

#[cfg(test)]
mod tests;
