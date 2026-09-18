use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind,
    CapsuleTransaction, CapsuleVisibilityDelta,
};
use crab_metadata::git_visibility::GitVisibilityEdit;
use crab_storage::Store;
use serde::{Deserialize, Serialize};

use super::{
    Result, StoreLayout, ViewCrabObjects, copy_lfs_objects, generate_view_pack, list_view_refs,
    resolve_view_head, scan_reachable_pointers, upload_view_crab_objects,
    verify_crab_pointers_backed_by_uploaded_view, view_store_layout,
};
use crate::error::AuthServerError;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CapsuleViewReady {
    schema_version: u32,
    source_digest: String,
}

fn ready_path(router: &StoreLayout) -> object_store::path::Path {
    object_store::path::Path::from(format!(
        "{}/v2/view-ready.json",
        router.repo_prefix().trim_end_matches('/')
    ))
}

pub(super) async fn verify_ready(router: &StoreLayout, source_digest: &str) -> Result<()> {
    let (bytes, _) = router
        .store()
        .get_with_etag_bounded(&ready_path(router), 4096)
        .await?;
    let ready: CapsuleViewReady =
        serde_json::from_slice(&bytes).map_err(|error| AuthServerError::CorruptObject {
            path: ready_path(router).to_string(),
            reason: format!("invalid capsule view readiness record: {error}"),
        })?;
    if ready.schema_version != 1 || ready.source_digest != source_digest {
        return Err(AuthServerError::CorruptObject {
            path: ready_path(router).to_string(),
            reason: "capsule view readiness does not match its source state".to_owned(),
        });
    }
    let root = crab_metadata::capsule_protocol::load_root(router).await?;
    crab_read::capsule_protocol::open_view_from_root_with_control(
        router,
        root,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 2 * 1024 * 1024 * 1024,
            max_frontier_bytes: 2 * 1024 * 1024 * 1024,
        },
    )
    .await?;
    Ok(())
}

pub(super) async fn publish(
    source_repo: &str,
    repo_prefix: &str,
    source_digest: &str,
    store: &Store,
    filtered_git: &Path,
    crab_objects: ViewCrabObjects,
) -> Result<()> {
    let router = view_store_layout(store, repo_prefix);
    let refs = list_view_refs(filtered_git)?;
    let head = resolve_view_head(filtered_git, &refs)?;
    let repository_id = blake3::hash(repo_prefix.as_bytes()).to_hex().to_string();
    let base = crab_write::capsule_protocol::initialize(&router, &repository_id, &head).await?;

    let uploaded_crab = upload_view_crab_objects(store, &router, crab_objects).await?;
    let scan = scan_reachable_pointers(filtered_git)?;
    copy_lfs_objects(store.clone(), source_repo, repo_prefix, &scan.lfs_pointers).await?;
    verify_crab_pointers_backed_by_uploaded_view(
        store,
        &router,
        &uploaded_crab.shard_hashes,
        &scan.crab_pointers,
    )
    .await?;
    crab_metadata::ref_registry::union_register_repo_shards(
        store,
        &router,
        uploaded_crab.shard_hashes.clone(),
    )
    .await?;

    let current = crab_read::capsule_protocol::open_view_from_root_with_control(
        &router,
        base.clone(),
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 2 * 1024 * 1024 * 1024,
            max_frontier_bytes: 2 * 1024 * 1024 * 1024,
        },
    )
    .await?;
    if current.refs() != &refs {
        if !current.refs().is_empty() {
            return Err(AuthServerError::CorruptObject {
                path: router.capsule_root_path().to_string(),
                reason: "incomplete capsule view contains unexpected visible refs".to_owned(),
            });
        }
        let peeled_refs = crate::receive::derive_peeled_refs(
            filtered_git,
            &refs
                .iter()
                .map(|(name, oid)| (name.clone(), oid.clone()))
                .collect::<Vec<_>>(),
        )?;
        let generated = generate_view_pack(filtered_git)?;
        let packs = if generated.object_count == 0 {
            Vec::new()
        } else {
            vec![CapsuleGitPack::new(
                Bytes::from(generated.bytes),
                Bytes::from(generated.index),
                Bytes::from(generated.reverse_index),
                Bytes::from(generated.locator),
                generated.git_checksum,
                generated.object_count,
            )?]
        };
        let edits = refs
            .iter()
            .map(|(name, oid)| {
                CapsuleRefEdit::new(
                    name.clone(),
                    None,
                    Some(oid.clone()),
                    peeled_refs.get(name).cloned(),
                )
            })
            .collect::<Vec<_>>();
        if !edits.is_empty() {
            let transaction = CapsuleTransaction::new(base.record().digest(), edits)?;
            let ref_pairs = refs
                .iter()
                .map(|(name, oid)| (name.clone(), oid.clone()))
                .collect::<Vec<_>>();
            let closures = crab_git::walk_reachable_by_ref_bounded(
                filtered_git,
                &ref_pairs,
                &peeled_refs,
                usize::try_from(crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS)
                    .map_err(|_| {
                        AuthServerError::Internal(
                            "Git visibility object limit does not fit usize".to_owned(),
                        )
                    })?,
            )
            .map_err(|source| AuthServerError::GitVisibilityWalk { source })?;
            let visibility = CapsuleVisibilityDelta::new(
                refs.iter()
                    .map(|(name, oid)| {
                        let reachable = closures.get(name).ok_or_else(|| {
                            AuthServerError::Internal(format!(
                                "filtered view visibility omitted {name}"
                            ))
                        })?;
                        Ok((
                            name.clone(),
                            GitVisibilityEdit::from_replacement_objects(
                                None,
                                oid.clone(),
                                reachable_object_ids(reachable),
                            ),
                        ))
                    })
                    .collect::<Result<BTreeMap<_, _>>>()?,
            )?;
            let mut sections = vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visibility.encode()?,
            )];
            if !uploaded_crab.catalog().is_empty() {
                sections.push(CapsuleSection::new(
                    CapsuleSectionKind::CatalogDelta,
                    uploaded_crab.catalog().encode_delta()?,
                ));
            }
            let capsule = Capsule::build(&transaction, packs, sections)?;
            crab_write::capsule_protocol::publish(&router, base, &transaction, &capsule).await?;
        }
    }

    let ready = CapsuleViewReady {
        schema_version: 1,
        source_digest: source_digest.to_owned(),
    };
    let bytes = serde_json::to_vec(&ready).map_err(|error| {
        AuthServerError::Internal(format!("capsule view readiness serialize failed: {error}"))
    })?;
    store
        .put_if_absent_verified(&ready_path(&router), Bytes::from(bytes))
        .await?;
    verify_ready(&router, source_digest).await
}

fn reachable_object_ids(reachable: &crab_git::walk::ReachableSet) -> Vec<String> {
    let mut objects = reachable
        .commits
        .iter()
        .chain(&reachable.trees)
        .chain(&reachable.blobs)
        .chain(&reachable.tags)
        .map(|oid| {
            let mut encoded = String::with_capacity(40);
            for byte in oid {
                let _ = write!(encoded, "{byte:02x}");
            }
            encoded
        })
        .collect::<Vec<_>>();
    objects.sort_unstable();
    objects.dedup();
    objects
}
