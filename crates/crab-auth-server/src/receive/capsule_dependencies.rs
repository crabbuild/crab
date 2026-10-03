use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use crab_metadata::capsule_protocol::PointerCatalog;
use crab_storage::content_hash_from_path;
use crab_xet::hash::{MerkleHash, compute_data_hash};
use crab_xet::shard::ShardReader;
use sha2::{Digest, Sha256};

use super::{
    ProtectedCapsulePushPlan, ReceiveContext, conflict, invalid, read_verified_staged_object,
    strict_xorb_references_from_shard, validate_staged_xorb,
};
use crate::error::{AuthServerError, Result};
use crate::git_pointer_scan::ReachablePointerScan;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DependencyCopy {
    source_key: String,
    target_key: String,
    size: u64,
    digest: String,
}

pub(super) fn source_pointer_delta(
    source: &PointerCatalog,
    candidate: &PointerCatalog,
    candidate_delta: &PointerCatalog,
    pointers: &ReachablePointerScan,
) -> Result<PointerCatalog> {
    let mut delta = PointerCatalog::new();
    for pointer in &pointers.crab_pointers {
        let file_hash = MerkleHash::from(pointer.file_hash).hex();
        let changed_by_candidate = candidate_delta.files().contains_key(&file_hash);
        let selected = if changed_by_candidate {
            candidate.files().get(&file_hash)
        } else {
            source
                .files()
                .get(&file_hash)
                .or_else(|| candidate.files().get(&file_hash))
        }
        .ok_or_else(|| {
            invalid("source pointer is absent from both source and filtered catalogs")
        })?;
        if selected.size() != pointer.size {
            return Err(invalid(
                "source pointer size differs from its selected catalog",
            ));
        }
        if !changed_by_candidate && source.files().get(&file_hash) == Some(selected) {
            continue;
        }
        let shard = candidate
            .shards()
            .get(selected.shard_hash())
            .or_else(|| source.shards().get(selected.shard_hash()))
            .ok_or_else(|| invalid("selected pointer shard is absent from both catalogs"))?;
        if changed_by_candidate || !source.shards().contains_key(selected.shard_hash()) {
            for xorb_hash in shard.xorb_hashes() {
                if !changed_by_candidate && source.xorbs().contains_key(xorb_hash) {
                    continue;
                }
                let xorb = candidate.xorbs().get(xorb_hash).ok_or_else(|| {
                    invalid("selected pointer xorb is absent from the filtered catalog")
                })?;
                delta.insert_xorb(xorb_hash.clone(), xorb.clone())?;
            }
            delta.insert_shard(selected.shard_hash().to_owned(), shard.clone())?;
        }
        delta.insert_file(file_hash, selected.clone())?;
    }
    delta.encode_delta()?;
    Ok(delta)
}

pub(super) async fn verify_pointer_dependencies(
    ctx: &ReceiveContext,
    plan: &ProtectedCapsulePushPlan,
    catalog: &PointerCatalog,
    pointers: &ReachablePointerScan,
    fallback: Option<&crab_storage::StoreLayout<crab_storage::Store>>,
) -> Result<(BTreeSet<String>, Vec<DependencyCopy>)> {
    let mut referenced = BTreeSet::from([ctx
        .router()
        .capsule_path(&plan.run_hash)
        .as_ref()
        .to_owned()]);
    let mut verified_shards = BTreeMap::<String, Bytes>::new();
    let mut verified_xorbs = BTreeSet::new();
    let mut copies = Vec::new();
    for pointer in &pointers.crab_pointers {
        let file_hash = MerkleHash::from(pointer.file_hash).hex();
        let file = catalog
            .files()
            .get(&file_hash)
            .ok_or_else(|| invalid("Crab pointer is absent from the candidate catalog"))?;
        if file.size() != pointer.size {
            return Err(invalid(
                "Crab pointer size differs from the candidate catalog",
            ));
        }
        let shard_hash = MerkleHash::from_hex(file.shard_hash())
            .map_err(|error| invalid(format!("candidate shard hash is invalid: {error}")))?;
        let shard = catalog
            .shards()
            .get(file.shard_hash())
            .ok_or_else(|| invalid("candidate catalog omits a pointer shard"))?;
        let shard_key = ctx.router().shard_path(&shard_hash).as_ref().to_owned();
        referenced.insert(shard_key.clone());
        let first_shard_use = !verified_shards.contains_key(file.shard_hash());
        let bytes = match verified_shards.get(file.shard_hash()) {
            Some(bytes) => bytes.clone(),
            None => {
                let fallback_key = fallback.map(|layout| layout.shard_path(&shard_hash));
                let bytes = read_candidate_object(
                    ctx,
                    plan,
                    &shard_key,
                    fallback_key.as_ref(),
                    shard.encoded_size(),
                    &mut copies,
                )
                .await?;
                verified_shards.insert(file.shard_hash().to_owned(), bytes.clone());
                bytes
            }
        };
        if first_shard_use {
            if compute_data_hash(&bytes) != shard_hash {
                return Err(invalid(
                    "candidate shard body hash differs from its catalog",
                ));
            }
            let xorb_refs = strict_xorb_references_from_shard(&bytes)?;
            let actual = xorb_refs
                .keys()
                .filter_map(|key| content_hash_from_path(key, "xorbs"))
                .map(str::to_owned)
                .collect::<BTreeSet<_>>();
            let expected = shard.xorb_hashes().iter().cloned().collect::<BTreeSet<_>>();
            if actual != expected {
                return Err(invalid(
                    "candidate shard dependency closure differs from its catalog",
                ));
            }
            for (relative_key, chunks) in xorb_refs {
                let hash = content_hash_from_path(&relative_key, "xorbs")
                    .ok_or_else(|| invalid("candidate shard contains an invalid xorb key"))?;
                let xorb = catalog
                    .xorbs()
                    .get(hash)
                    .ok_or_else(|| invalid("candidate catalog omits a shard xorb"))?;
                if xorb.chunks().len() != chunks.len()
                    || xorb.chunks().iter().zip(&chunks).any(|(catalog, shard)| {
                        catalog.hash() != shard.hash.hex()
                            || catalog.uncompressed_size() != shard.uncompressed_size
                    })
                {
                    return Err(invalid(
                        "candidate xorb chunk catalog differs from its shard",
                    ));
                }
                let xorb_hash = MerkleHash::from_hex(hash)
                    .map_err(|error| invalid(format!("candidate xorb hash is invalid: {error}")))?;
                let key = ctx.router().xorb_path(&xorb_hash).as_ref().to_owned();
                referenced.insert(key.clone());
                if verified_xorbs.insert(hash.to_owned()) {
                    let fallback_key = fallback.map(|layout| layout.xorb_path(&xorb_hash));
                    let bytes = read_candidate_object(
                        ctx,
                        plan,
                        &key,
                        fallback_key.as_ref(),
                        xorb.encoded_size(),
                        &mut copies,
                    )
                    .await?;
                    if blake3::hash(&bytes).to_hex().as_str() != xorb.body_digest() {
                        return Err(invalid(
                            "candidate xorb body digest differs from its catalog",
                        ));
                    }
                    validate_staged_xorb(&key, &bytes, &chunks)?;
                }
            }
        }
        let reader = ShardReader::from_bytes(bytes, shard_hash);
        let file_info = reader
            .get_file_info(&MerkleHash::from(pointer.file_hash))?
            .ok_or_else(|| invalid("candidate shard does not contain its pointer recipe"))?;
        if file_info.file_size() != pointer.size {
            return Err(invalid(
                "candidate shard recipe size differs from its pointer",
            ));
        }
    }
    for pointer in &pointers.lfs_pointers {
        let key = crab_lfs::LfsObjectStore::object_path_for_prefix(
            ctx.router().repo_prefix(),
            &pointer.oid,
        )
        .to_string();
        referenced.insert(key.clone());
        let bytes = read_candidate_object(ctx, plan, &key, None, pointer.size, &mut copies).await?;
        if <[u8; 32]>::from(Sha256::digest(&bytes)) != pointer.oid {
            return Err(invalid(
                "candidate LFS object digest differs from its pointer",
            ));
        }
    }
    if let Some(unreferenced) = plan
        .staged_objects
        .iter()
        .find(|object| !referenced.contains(&object.canonical_key))
    {
        return Err(invalid(format!(
            "staged object is not reachable from the candidate capsule: {}",
            unreferenced.canonical_key
        )));
    }
    copies.sort_by(|left, right| left.target_key.cmp(&right.target_key));
    for pair in copies.windows(2) {
        if pair[0].target_key == pair[1].target_key && pair[0] != pair[1] {
            return Err(invalid(
                "filtered view dependencies disagree on one source object",
            ));
        }
    }
    copies.dedup_by(|left, right| left.target_key == right.target_key);
    Ok((referenced, copies))
}

async fn read_candidate_object(
    ctx: &ReceiveContext,
    plan: &ProtectedCapsulePushPlan,
    canonical_key: &str,
    fallback_key: Option<&object_store::path::Path>,
    expected_size: u64,
    copies: &mut Vec<DependencyCopy>,
) -> Result<Bytes> {
    let bytes = match plan
        .staged_objects
        .iter()
        .find(|object| object.canonical_key == canonical_key)
    {
        Some(object) => {
            if object.size != expected_size {
                return Err(invalid(
                    "staged dependency size differs from the candidate catalog",
                ));
            }
            read_verified_staged_object(ctx.store(), object).await?
        }
        None => match ctx
            .store()
            .get_with_etag_bounded(
                &object_store::path::Path::from(canonical_key.to_owned()),
                expected_size,
            )
            .await
        {
            Ok((bytes, _)) => bytes,
            Err(crab_storage::StorageError::NotFound { .. }) => {
                let fallback_key = fallback_key.ok_or_else(|| AuthServerError::NotFound {
                    path: canonical_key.to_owned(),
                })?;
                let (bytes, _) = ctx
                    .store()
                    .get_with_etag_bounded(fallback_key, expected_size)
                    .await?;
                copies.push(DependencyCopy {
                    source_key: fallback_key.to_string(),
                    target_key: canonical_key.to_owned(),
                    size: expected_size,
                    digest: blake3::hash(&bytes).to_hex().to_string(),
                });
                bytes
            }
            Err(error) => return Err(error.into()),
        },
    };
    if bytes.len() as u64 != expected_size {
        return Err(invalid(
            "candidate dependency size differs from the candidate catalog",
        ));
    }
    Ok(bytes)
}

pub(super) async fn promote_dependency_copies(
    ctx: &ReceiveContext,
    copies: &[DependencyCopy],
) -> Result<()> {
    for copy in copies {
        let (bytes, _) = ctx
            .store()
            .get_with_etag_bounded(
                &object_store::path::Path::from(copy.source_key.clone()),
                copy.size,
            )
            .await?;
        if bytes.len() as u64 != copy.size || blake3::hash(&bytes).to_hex().as_str() != copy.digest
        {
            return Err(conflict(
                "filtered view dependency changed after protected verification",
            ));
        }
        ctx.store()
            .put_if_absent_verified(
                &object_store::path::Path::from(copy.target_key.clone()),
                bytes,
            )
            .await?;
    }
    Ok(())
}
