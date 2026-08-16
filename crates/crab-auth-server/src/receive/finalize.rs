use std::collections::BTreeSet;

use crab_coordination::{
    active_active as coordination_active_active,
    write_coordinator::{CommitOutcome, CoordinatedRefUpdate, commit_uploaded_push_refs},
};
use crab_metadata::{
    manifest_store,
    manifests::{Manifest, validate_manifest_payload},
    ref_registry,
    segmented::{self, SegmentKind},
    segmented_store,
};
use crab_storage::{Store, StoreLayout};

use super::{
    ActiveActiveReceiveConfig, MaterializedSourcePush, ProtectedPushPlan,
    active_active_coordinator_registration, non_empty,
};
use crate::error::{AuthServerError, Result};

/// Inputs required to commit a finalized receive manifest.
pub struct ReceiveManifestCommit<'a> {
    pub repo_prefix: &'a str,
    pub active_active: Option<&'a ActiveActiveReceiveConfig>,
    pub plan: &'a ProtectedPushPlan,
    pub materialized: &'a MaterializedSourcePush,
    pub manifest: &'a Manifest,
    pub base_etag: Option<&'a str>,
}

/// Commits a finalized receive manifest through normal CAS or active-active coordination.
pub async fn commit_receive_manifest(
    store: &Store,
    router: &StoreLayout<Store>,
    commit: ReceiveManifestCommit<'_>,
) -> Result<Option<CommitOutcome>> {
    validate_manifest_payload(commit.manifest)?;
    if let Some(active_active) = commit.active_active {
        return commit_active_active_manifest(
            store,
            router,
            commit.repo_prefix,
            active_active,
            &commit,
        )
        .await
        .map(Some);
    }
    commit_manifest(store, router, commit.base_etag, commit.manifest).await?;
    Ok(None)
}

async fn commit_active_active_manifest(
    store: &Store,
    router: &StoreLayout<Store>,
    repo_prefix: &str,
    active_active: &ActiveActiveReceiveConfig,
    commit: &ReceiveManifestCommit<'_>,
) -> Result<CommitOutcome> {
    let refs = commit
        .materialized
        .ref_updates
        .iter()
        .map(|update| CoordinatedRefUpdate {
            name: update.ref_name.clone(),
            expected: update.old_oid.clone(),
            new: Some(update.new_oid.clone()),
            force: false,
        })
        .collect::<Vec<_>>();
    let uploaded_objects = active_active_uploaded_objects(
        store,
        router,
        commit.plan,
        commit.materialized,
        commit.manifest,
    )
    .await?;
    let push_plan = coordination_active_active::plan_active_active_push(
        &active_active.replication,
        Some(&active_active.writer),
        commit.manifest.generation,
        refs,
        uploaded_objects,
    )?;
    let registration = active_active_coordinator_registration(&active_active.replication)?;
    ref_registry::register_active_active_coordinator_for_repo(store, router, registration)
        .await
        .map_err(AuthServerError::from)?;

    let coordinator = crab_coordination::active_active_write_coordinator_for_repo(
        &active_active.replication,
        repo_prefix,
    )
    .await?;
    let mut outcome =
        commit_uploaded_push_refs(coordinator.as_ref(), push_plan.request.clone()).await?;
    manifest_store::materialize_active_active_manifest_projection(store, router, commit.manifest)
        .await
        .map_err(AuthServerError::from)?;
    outcome.state = coordinator
        .mark_region_materialized(&outcome.operation_id, &push_plan.request.region)
        .await?;
    Ok(outcome)
}

async fn active_active_uploaded_objects(
    store: &Store,
    router: &StoreLayout<Store>,
    plan: &ProtectedPushPlan,
    materialized: &MaterializedSourcePush,
    manifest: &Manifest,
) -> Result<Vec<String>> {
    let mut keys = BTreeSet::new();
    for object in &plan.staged_objects {
        keys.insert(object.canonical_key.clone());
    }
    for pack in &materialized.packs {
        keys.insert(router.pack_path(&pack.pack_id).as_ref().to_owned());
        keys.insert(router.pack_metadata_path(&pack.pack_id).as_ref().to_owned());
    }
    add_active_active_index_objects(
        store,
        router,
        &mut keys,
        SegmentKind::Shard,
        &manifest.shard_index_hash,
    )
    .await?;
    add_active_active_index_objects(
        store,
        router,
        &mut keys,
        SegmentKind::Pack,
        &manifest.pack_index_hash,
    )
    .await?;
    Ok(keys.into_iter().collect())
}

async fn add_active_active_index_objects(
    store: &Store,
    router: &StoreLayout<Store>,
    keys: &mut BTreeSet<String>,
    kind: SegmentKind,
    hash: &str,
) -> Result<()> {
    let Some(hash) = non_empty(hash) else {
        return Ok(());
    };
    keys.insert(
        router
            .repo_path(&segmented::index_relative_path(kind, hash))
            .as_ref()
            .to_owned(),
    );
    let index = segmented_store::read_index(store, router, kind, hash)
        .await
        .map_err(AuthServerError::from)?;
    for segment in index.segments {
        keys.insert(router.repo_path(&segment.path).as_ref().to_owned());
    }
    Ok(())
}

async fn commit_manifest(
    store: &Store,
    router: &StoreLayout<Store>,
    base_etag: Option<&str>,
    manifest: &Manifest,
) -> Result<()> {
    if let Some(etag) = base_etag {
        manifest_store::write_manifest_cas(store, router, manifest, etag)
            .await
            .map_err(AuthServerError::from)?;
    } else {
        manifest_store::create_manifest(store, router, manifest)
            .await
            .map_err(AuthServerError::from)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crab_auth::PushRefUpdate;
    use object_store::memory::InMemory;

    use super::*;

    fn oid(ch: char) -> String {
        std::iter::repeat_n(ch, 40).collect()
    }

    fn context() -> (Store, StoreLayout<Store>) {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        (store, router)
    }

    fn plan_with_candidate(candidate_manifest: Manifest) -> ProtectedPushPlan {
        ProtectedPushPlan {
            schema_version: 1,
            repo_prefix: "org/repo".to_owned(),
            push_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            upload_prefix: "org/repo/staging/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/".to_owned(),
            base_manifest_generation: None,
            base_manifest_etag: None,
            ref_updates: vec![PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: None,
                new_oid: oid('2'),
            }],
            candidate_manifest,
            push_commit_receipt: None,
            staged_objects: Vec::new(),
        }
    }

    #[tokio::test]
    async fn commit_manifest_conflicts_when_initial_manifest_appears() -> Result<()> {
        let (store, router) = context();
        let mut existing = Manifest::default_for_repo("refs/heads/main");
        existing.generation = 1;
        existing.refs.insert("refs/heads/main".to_owned(), oid('8'));
        existing.seal_git_validation();
        manifest_store::create_manifest(&store, &router, &existing)
            .await
            .map_err(AuthServerError::from)?;

        let mut candidate = Manifest::default_for_repo("refs/heads/main");
        candidate.generation = 1;
        candidate
            .refs
            .insert("refs/heads/main".to_owned(), oid('2'));
        candidate.seal_git_validation();
        let plan = plan_with_candidate(candidate.clone());
        let materialized = MaterializedSourcePush {
            ref_updates: Vec::new(),
            packs: Vec::new(),
        };

        let err = commit_receive_manifest(
            &store,
            &router,
            ReceiveManifestCommit {
                repo_prefix: "org/repo",
                active_active: None,
                plan: &plan,
                materialized: &materialized,
                manifest: &candidate,
                base_etag: None,
            },
        )
        .await
        .expect_err("empty-repo push must not overwrite a concurrently created manifest");

        assert!(
            matches!(err, AuthServerError::CasConflict { .. }),
            "expected CasConflict, got {err:?}"
        );
        Ok(())
    }
}
