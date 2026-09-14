//! Canonical protocol-v2 Git push path.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use bytes::Bytes;
use gix_object::{Exists, Find, FindHeader};
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result};
use crate::git::pack::{
    PushPackConfig, RemotePackExclusions, generate_push_pack_files_with_exclusions,
    install_pack_file_locally_with_timeout,
};
use crate::git::push::{
    PushConfig, PushRejectReason, PushResult, RefPushOutcome, RefUpdate, check_ref_update,
    duplicate_destination_result,
};
use crate::git::remote_helper::PushSpec;

const POINTER_SCAN_ALLOCATION_BYTES: usize = 64 * 1024 * 1024;

/// Publish one remote-helper push batch through a single capsule/root transaction.
///
/// Every policy and Git-integrity check completes before the first object-store
/// mutation. Root CAS is the only concurrency primitive; a stale advertised root
/// therefore cannot overwrite a concurrent winner.
pub async fn run(
    config: &PushConfig,
    specs: &[PushSpec],
    store: &crate::storage::store::Store,
    router: &crate::storage::StoreLayout,
    advertised: Option<crab_metadata::request_minimal::RootSnapshot>,
    hidden_ref_patterns: &[String],
    cancel: &CancellationToken,
) -> Result<(
    PushResult,
    Option<crab_metadata::request_minimal::RootSnapshot>,
)> {
    if let Some(result) = duplicate_destination_result(specs) {
        return Ok((result, advertised));
    }
    if config.protected_push.is_some() || config.active_active_replication.is_some() {
        return Err(CrabError::Configuration {
            key: "request-minimal push coordination".to_owned(),
            origin: "protected and active-active publication require a protocol-v2 authorization commit adapter"
                .to_owned(),
        });
    }
    if cancel.is_cancelled() {
        return Err(CrabError::Cancelled);
    }

    let layout = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let base = match advertised {
        Some(snapshot) => snapshot,
        None => crab_write::request_minimal::open_root(&layout).await?,
    };
    let git_dir = config
        .git_dir
        .clone()
        .map_or_else(super::discover::discover_git_dir, Ok)?;
    let common_git_dir = super::discover::resolve_common_dir(&git_dir);
    let source_names = specs
        .iter()
        .filter(|spec| !spec.src.is_empty())
        .map(|spec| spec.src.as_str())
        .collect::<Vec<_>>();
    let source_oids = crab_git::ref_resolve::resolve_refs_batch_at(&git_dir, &source_names)?;
    let peeled = crab_git::tag::peeled_revision_targets_at(
        &common_git_dir,
        &source_oids
            .iter()
            .map(|(name, oid)| (name.clone(), oid.clone()))
            .collect(),
    )?;
    let hidden = hidden_ref_matcher(hidden_ref_patterns)?;
    let root = base.record().root();
    let mut outcomes = HashMap::with_capacity(specs.len());
    let mut edits = Vec::with_capacity(specs.len());
    let mut updates = Vec::with_capacity(specs.len());

    for spec in specs {
        if hidden.is_match(&spec.dst) {
            outcomes.insert(
                spec.dst.clone(),
                RefPushOutcome::Rejected(PushRejectReason::UnknownRefname {
                    name: spec.dst.clone(),
                }),
            );
            continue;
        }
        let current = root.refs().get(&spec.dst).cloned();
        if config
            .expected_refs
            .get(&spec.dst)
            .is_some_and(|expected| expected != &current)
        {
            outcomes.insert(
                spec.dst.clone(),
                RefPushOutcome::Rejected(PushRejectReason::StaleInfo),
            );
            continue;
        }
        if spec.src.is_empty() {
            if current.is_none() {
                outcomes.insert(spec.dst.clone(), RefPushOutcome::Ok);
            } else if config.receive_deny_deletes {
                outcomes.insert(
                    spec.dst.clone(),
                    RefPushOutcome::Rejected(PushRejectReason::DenyDeletes),
                );
            } else if current_branch_is_denied(config, root.head(), &spec.dst) {
                outcomes.insert(
                    spec.dst.clone(),
                    RefPushOutcome::Rejected(PushRejectReason::DenyCurrentBranch),
                );
            } else {
                edits.push(crab_metadata::request_minimal::CapsuleRefEdit::new(
                    spec.dst.clone(),
                    current,
                    None,
                    None,
                ));
            }
            continue;
        }

        let new_oid = source_oids.get(&spec.src).cloned().ok_or_else(|| {
            CrabError::Internal(format!("resolved push source {} is absent", spec.src))
        })?;
        if current.as_deref() == Some(new_oid.as_str()) {
            outcomes.insert(spec.dst.clone(), RefPushOutcome::Ok);
            continue;
        }
        if current_branch_is_denied(config, root.head(), &spec.dst) {
            outcomes.insert(
                spec.dst.clone(),
                RefPushOutcome::Rejected(PushRejectReason::DenyCurrentBranch),
            );
            continue;
        }
        let update = RefUpdate {
            ref_name: spec.dst.clone(),
            old_sha: current.clone(),
            new_sha: new_oid.clone(),
            force: spec.force || config.expected_refs.contains_key(&spec.dst),
        };
        if let Some(old_oid) = current.as_deref() {
            let is_fast_forward = if spec.dst.starts_with("refs/tags/") {
                false
            } else {
                is_ancestor(&common_git_dir, old_oid, &new_oid)?
            };
            let decision = if !is_fast_forward && config.receive_deny_non_fast_forwards {
                Err(PushRejectReason::DenyNonFastForward)
            } else {
                match check_ref_update(&update, is_fast_forward, None) {
                    crate::git::push::RefUpdateDecision::Proceed { .. } => Ok(()),
                    crate::git::push::RefUpdateDecision::Reject(reason) => Err(reason),
                }
            };
            if let Err(reason) = decision {
                outcomes.insert(spec.dst.clone(), RefPushOutcome::Rejected(reason));
                continue;
            }
        }
        edits.push(crab_metadata::request_minimal::CapsuleRefEdit::new(
            spec.dst.clone(),
            current,
            Some(new_oid.clone()),
            peeled.get(&spec.src).cloned(),
        ));
        updates.push(update);
    }

    if let Some(blocked_by) = outcomes.iter().find_map(|(name, outcome)| {
        matches!(outcome, RefPushOutcome::Rejected(_)).then(|| name.clone())
    }) && config.atomic
    {
        for spec in specs {
            outcomes.entry(spec.dst.clone()).or_insert_with(|| {
                RefPushOutcome::Rejected(PushRejectReason::AtomicAbort {
                    blocked_by: blocked_by.clone(),
                })
            });
        }
        return Ok((PushResult::new(outcomes), Some(base)));
    }

    if edits.is_empty() {
        return Ok((PushResult::new(outcomes), Some(base)));
    }
    validate_candidate_namespace(root.refs(), &edits).map_err(|reason| {
        CrabError::Protocol(format!(
            "request-minimal ref transaction is invalid: {reason}"
        ))
    })?;
    if cancel.is_cancelled() {
        return Err(CrabError::Cancelled);
    }

    let capsule_packs = prepare_git_packs(
        &common_git_dir,
        root.refs(),
        &updates,
        config.receive_max_input_size,
    )
    .await?;
    let transaction =
        crab_metadata::request_minimal::CapsuleTransaction::new(base.record().digest(), edits)?;
    let capsule =
        crab_metadata::request_minimal::Capsule::build(&transaction, capsule_packs, Vec::new())?;
    if cancel.is_cancelled() {
        return Err(CrabError::Cancelled);
    }
    let committed =
        crab_write::request_minimal::publish(&layout, base, &transaction, &capsule).await?;
    for edit in transaction.edits() {
        outcomes.insert(edit.ref_name().to_owned(), RefPushOutcome::Ok);
    }
    Ok((PushResult::new(outcomes), Some(committed)))
}

fn current_branch_is_denied(config: &PushConfig, head: &str, destination: &str) -> bool {
    destination == head
        && config
            .receive_deny_current_branch
            .eq_ignore_ascii_case("refuse")
}

fn hidden_ref_matcher(patterns: &[String]) -> Result<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(globset::Glob::new(pattern).map_err(|error| {
            CrabError::Protocol(format!("invalid transfer.hideRefs pattern: {error}"))
        })?);
    }
    builder.build().map_err(|error| {
        CrabError::Protocol(format!("invalid transfer.hideRefs patterns: {error}"))
    })
}

fn is_ancestor(git_dir: &Path, old_oid: &str, new_oid: &str) -> Result<bool> {
    let output = std::process::Command::new("git")
        .args(["--git-dir"])
        .arg(git_dir)
        .args(["merge-base", "--is-ancestor", old_oid, new_oid])
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .output()?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(CrabError::Internal(format!(
            "git merge-base could not prove ancestry: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
}

fn validate_candidate_namespace(
    base_refs: &BTreeMap<String, String>,
    edits: &[crab_metadata::request_minimal::CapsuleRefEdit],
) -> std::result::Result<(), crab_git::refname::RefNamespaceError> {
    let mut refs = base_refs.clone();
    for edit in edits {
        match edit.new_oid() {
            Some(oid) => {
                refs.insert(edit.ref_name().to_owned(), oid.to_owned());
            }
            None => {
                refs.remove(edit.ref_name());
            }
        }
    }
    crab_git::refname::validate_ref_namespace(refs.keys().map(String::as_str))
}

async fn prepare_git_packs(
    git_dir: &Path,
    remote_refs: &BTreeMap<String, String>,
    updates: &[RefUpdate],
    max_input_size: u64,
) -> Result<Vec<crab_metadata::request_minimal::CapsuleGitPack>> {
    if updates.is_empty() {
        return Ok(Vec::new());
    }
    let exclusions = locally_available_remote_tips(git_dir, remote_refs)?;
    let generated = generate_push_pack_files_with_exclusions(
        updates,
        Some(RemotePackExclusions::RefTips(&exclusions)),
        &PushPackConfig {
            thin_packs: false,
            max_input_size,
            git_dir: Some(git_dir.to_owned()),
        },
    )
    .await?;
    let evidence_dir = tempfile::Builder::new()
        .prefix(".crab-v2-push-evidence-")
        .tempdir_in(git_dir.join("objects"))?;
    let mut packs = Vec::with_capacity(generated.len());
    for generated in generated {
        if generated.object_count == 0 {
            continue;
        }
        let installed = install_pack_file_locally_with_timeout(
            evidence_dir.path(),
            generated.pack_path.as_ref(),
            &generated.pack_blake3_hex,
            max_input_size,
            true,
        )
        .await
        .map_err(map_push_pack_error)?;
        let mut locations = crab_git::pack_locator::PackLocationIter::open(
            &installed.idx_path,
            &installed.rev_path,
            generated.pack_size,
        )
        .map_err(crab_git::pack::PackError::from)?;
        if locations.object_count() != generated.object_count
            || locations.pack_checksum().to_string() != installed.git_sha1
        {
            return Err(CrabError::CorruptObject {
                path: generated.pack_path.display().to_string(),
                reason: "generated pack identity disagrees with its verified locator".to_owned(),
            });
        }
        let object_ids = locations
            .by_ref()
            .map(|location| location.map(|location| location.oid))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(crab_git::pack::PackError::from)?;
        let kinds = crab_git::object_kinds_from_git_dir(git_dir, &object_ids)?;
        reject_unrepresented_pointers(git_dir, &object_ids, &kinds)?;
        let ordered_kinds = object_ids
            .iter()
            .map(|oid| {
                kinds
                    .get(oid)
                    .copied()
                    .ok_or_else(|| CrabError::CorruptObject {
                        path: generated.pack_path.display().to_string(),
                        reason: format!("Git object-kind query omitted {oid}"),
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let checksum =
            gix_hash::ObjectId::from_hex(installed.git_sha1.as_bytes()).map_err(|error| {
                CrabError::Internal(format!("generated pack checksum is invalid: {error}"))
            })?;
        let locator = crab_git::pack_locator::encode_pack_kind_metadata(checksum, &ordered_kinds)
            .map_err(crab_git::pack::PackError::from)?;
        packs.push(crab_metadata::request_minimal::CapsuleGitPack::new(
            Bytes::from(std::fs::read(
                generated.pack_path.as_ref() as &std::path::Path
            )?),
            Bytes::from(std::fs::read(&installed.idx_path)?),
            Bytes::from(std::fs::read(&installed.rev_path)?),
            Bytes::from(locator),
            installed.git_sha1,
            generated.object_count,
        )?);
    }
    Ok(packs)
}

fn locally_available_remote_tips(
    git_dir: &Path,
    remote_refs: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let objects = gix_odb::at(git_dir.join("objects")).map_err(|error| {
        CrabError::Internal(format!("failed to open local Git object database: {error}"))
    })?;
    Ok(remote_refs
        .values()
        .filter_map(|oid| gix_hash::ObjectId::from_hex(oid.as_bytes()).ok())
        .filter(|oid| objects.exists(oid))
        .map(|oid| oid.to_string())
        .collect())
}

fn reject_unrepresented_pointers(
    git_dir: &Path,
    object_ids: &[gix_hash::ObjectId],
    kinds: &HashMap<gix_hash::ObjectId, gix_object::Kind>,
) -> Result<()> {
    let objects = gix_odb::at_opts(
        git_dir.join("objects"),
        [],
        gix_odb::store::init::Options {
            alloc_limit_bytes: Some(POINTER_SCAN_ALLOCATION_BYTES),
            ..Default::default()
        },
    )
    .map_err(|error| CrabError::Internal(format!("failed to open local Git objects: {error}")))?;
    let mut buffer = Vec::new();
    for oid in object_ids
        .iter()
        .filter(|oid| kinds.get(*oid) == Some(&gix_object::Kind::Blob))
    {
        let header = objects
            .try_header(oid)
            .map_err(|error| {
                CrabError::Internal(format!("failed to inspect Git blob {oid}: {error}"))
            })?
            .ok_or_else(|| CrabError::CorruptObject {
                path: git_dir.display().to_string(),
                reason: format!("generated pack object {oid} is missing locally"),
            })?;
        if header.size > crab_types::pointer::MAX_POINTER_SIZE as u64 {
            continue;
        }
        let data = objects
            .try_find(oid, &mut buffer)
            .map_err(|error| {
                CrabError::Internal(format!("failed to read Git blob {oid}: {error}"))
            })?
            .ok_or_else(|| CrabError::CorruptObject {
                path: git_dir.display().to_string(),
                reason: format!("generated pack object {oid} is missing locally"),
            })?;
        data.verify_checksum(oid)
            .map_err(|error| CrabError::CorruptObject {
                path: git_dir.display().to_string(),
                reason: format!("Git blob {oid} failed checksum validation: {error}"),
            })?;
        if crab_types::pointer::Pointer::parse(data.data).is_ok() {
            return Err(CrabError::Configuration {
                key: "request-minimal capsule content".to_owned(),
                origin: format!(
                    "Git object {oid} is a Crab pointer; protocol v2 file-data and recipe sections are not yet wired"
                ),
            });
        }
    }
    Ok(())
}

fn map_push_pack_error(error: CrabError) -> CrabError {
    match error {
        CrabError::FetchMalformedObject {
            oid, kind, detail, ..
        } => CrabError::PushMalformedObject { oid, kind, detail },
        other => other,
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use crab_storage::{StorageObservation, StorageObserver, StorageOperation};
    use object_store::memory::InMemory;

    use super::*;

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

    #[tokio::test]
    async fn real_git_incremental_push_round_trips_with_bounded_requests() {
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
            crab_write::request_minimal::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .expect("initialize root");
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
            Some(root),
            &[],
            &CancellationToken::new(),
        )
        .await
        .expect("first push");
        assert!(result.all_ok());
        assert_eq!(observer.count() - before_first, 3);
        let committed = committed.expect("committed root");
        assert_eq!(committed.record().root().refs()["refs/heads/main"], first);

        let destination = tempfile::tempdir().expect("destination repository");
        git(destination.path(), &["init", "--bare"]);
        let view = crab_read::request_minimal::open_view(
            &layout,
            crab_read::request_minimal::RequestMinimalReadLimits {
                max_capsule_bytes: 16 * 1024 * 1024,
                max_frontier_bytes: 16 * 1024 * 1024,
            },
        )
        .await
        .expect("open first generation");
        crab_read::request_minimal::install_git_packs(&view, destination.path(), 16 * 1024 * 1024)
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
            Some(committed),
            &[],
            &CancellationToken::new(),
        )
        .await
        .expect("incremental push");
        assert!(result.all_ok());
        assert_eq!(observer.count() - before_second, 4);
        let committed = committed.expect("second root");
        assert_eq!(committed.record().root().refs()["refs/heads/main"], second);
        assert_eq!(committed.record().root().capsule_frontier().len(), 1);
        assert_eq!(
            committed.record().root().capsule_frontier()[0].capsule_count(),
            2
        );
    }
}
