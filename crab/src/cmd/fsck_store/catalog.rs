//! Complete object and per-file recipe inspection from a locally grouped capture.
use super::{CrabError, FsckIssue, MAX_FSCK_SHARD_BYTES, Result};
use crab_metadata::capsule_protocol::{
    FileCatalogEntry, PointerCatalogEntry, PointerCatalogLookup, PointerCatalogObject,
};
use crab_read::capsule_protocol::CapsuleRepositoryView;
use crab_storage::{StorageError, Store, StoreLayout};
use crab_xet::{
    hash::MerkleHash,
    shard::ShardReader,
    xorb::{format::MAX_XORB_SIZE, parser::XorbParser},
};
use std::{collections::BTreeSet, io::Cursor, sync::Arc};

pub(super) async fn check_objects(
    layout: &StoreLayout<Store>,
    view: &CapsuleRepositoryView,
    lookup: &PointerCatalogLookup,
) -> Result<Vec<FsckIssue>> {
    let mut catalog = view
        .pointer_catalog_inspection()
        .await?
        .group_by_shard()
        .await?;
    let result = check_entries(layout, lookup, &mut catalog).await;
    catalog.finish(result).await
}

async fn check_entries(
    layout: &StoreLayout<Store>,
    lookup: &PointerCatalogLookup,
    catalog: &mut crab_metadata::capsule_protocol::PointerCatalogShardInspection,
) -> Result<Vec<FsckIssue>> {
    let expected = catalog.counts();
    let mut seen = crab_metadata::capsule_protocol::PointerCatalogCounts::default();
    let mut issues = Vec::new();
    let mut current: Option<ShardState> = None;
    let mut files = Vec::new();
    while let Some(entry) = catalog.next_entry().await? {
        match entry {
            PointerCatalogEntry::File { hash, entry } => {
                seen.files += 1;
                let state = current.as_ref().ok_or_else(|| {
                    CrabError::Protocol("grouped file precedes its shard".to_owned())
                })?;
                if state.hash != entry.shard_hash() {
                    return Err(CrabError::Protocol(
                        "grouped file identifies a different shard".to_owned(),
                    ));
                }
                if state.validated.is_some() {
                    files.push((hash, entry));
                    if files.len() == 64 {
                        flush_files(&mut current, &mut files, &mut issues).await?;
                    }
                }
            }
            PointerCatalogEntry::Object(object) => {
                flush_files(&mut current, &mut files, &mut issues).await?;
                match object {
                    PointerCatalogObject::Xorb { hash, entry } => {
                        seen.xorbs += 1;
                        if entry.encoded_size() > MAX_XORB_SIZE as u64 {
                            issues.push(FsckIssue::corrupt_xorb(
                                &hash,
                                format!(
                                    "catalog size {} exceeds the xorb limit {MAX_XORB_SIZE}",
                                    entry.encoded_size()
                                ),
                            ));
                            continue;
                        }
                        let parsed = MerkleHash::from_hex(&hash)
                            .map_err(|source| CrabError::Io(std::io::Error::other(source)))?;
                        let body = match layout
                            .store()
                            .get_with_etag_bounded(&layout.xorb_path(&parsed), entry.encoded_size())
                            .await
                        {
                            Ok((body, _)) => body,
                            Err(StorageError::NotFound { .. }) => {
                                issues.push(FsckIssue::missing_xorb(&hash));
                                continue;
                            }
                            Err(source) => return Err(source.into()),
                        };
                        let checked_hash = hash.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            validate_catalog_xorb(&checked_hash, &entry, body)
                        })
                        .await
                        .map_err(|source| CrabError::Io(std::io::Error::other(source)))?;
                        if let Err(detail) = result {
                            issues.push(FsckIssue::corrupt_xorb(&hash, detail));
                        }
                    }
                    PointerCatalogObject::Shard { hash, entry } => {
                        seen.shards += 1;
                        current = Some(ShardState {
                            hash: hash.clone(),
                            validated: None,
                        });
                        if entry.encoded_size() > MAX_FSCK_SHARD_BYTES {
                            issues.push(FsckIssue::corrupt_shard(&hash, format!("catalog size {} exceeds the shard limit {MAX_FSCK_SHARD_BYTES}", entry.encoded_size())));
                            continue;
                        }
                        let parsed = MerkleHash::from_hex(&hash)
                            .map_err(|source| CrabError::Io(std::io::Error::other(source)))?;
                        let body = match layout
                            .store()
                            .get_with_etag_bounded(
                                &layout.shard_path(&parsed),
                                entry.encoded_size(),
                            )
                            .await
                        {
                            Ok((body, _)) => body,
                            Err(StorageError::NotFound { .. }) => {
                                issues.push(FsckIssue::missing_shard(&hash));
                                continue;
                            }
                            Err(source) => return Err(source.into()),
                        };
                        // One selected closure per shard, never per file binding.
                        // These are the same captured roots used by the full metadata proof.
                        let dependencies = lookup
                            .dependencies(layout.store(), &[], std::slice::from_ref(&hash), &[])
                            .await?;
                        if dependencies.shards().get(&hash) != Some(&entry) {
                            return Err(CrabError::Protocol(
                                "captured shard descriptor differs across inspection paths"
                                    .to_owned(),
                            ));
                        }
                        let checked_hash = hash.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            prepare_shard(&checked_hash, &entry, &dependencies, body)
                        })
                        .await
                        .map_err(|source| CrabError::Io(std::io::Error::other(source)))?;
                        match result {
                            Ok(shard) => {
                                current = Some(ShardState {
                                    hash,
                                    validated: Some(Arc::new(shard)),
                                });
                            }
                            Err(detail) => issues.push(FsckIssue::corrupt_shard(&hash, detail)),
                        }
                    }
                }
            }
        }
    }
    flush_files(&mut current, &mut files, &mut issues).await?;
    if seen != expected {
        return Err(CrabError::Protocol(
            "grouped catalog cardinalities differ from the complete proof".to_owned(),
        ));
    }
    Ok(issues)
}

struct ValidatedShard {
    reader: ShardReader,
    dependencies: Vec<crab_xet::shard::MDBXorbInfo>,
}

impl ValidatedShard {
    fn files(&self, files: &[(String, FileCatalogEntry)]) -> std::result::Result<(), String> {
        let validator = crab_xet::shard::FileBundleValidator::new(&self.dependencies);
        for (file_hash, expected) in files {
            let parsed_hash = MerkleHash::from_hex(file_hash).map_err(|error| error.to_string())?;
            let file = self
                .reader
                .get_file_info(&parsed_hash)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("stored shard lacks catalogued file {file_hash}"))?;
            if file.metadata.file_hash != parsed_hash
                || file.metadata.num_entries as usize != file.segments.len()
            {
                return Err(format!(
                    "stored recipe header for file {file_hash} is inconsistent"
                ));
            }
            let size = file.segments.iter().try_fold(0_u64, |total, segment| {
                total
                    .checked_add(u64::from(segment.unpacked_segment_bytes))
                    .ok_or_else(|| format!("stored recipe size for file {file_hash} overflowed"))
            })?;
            if size != expected.size() {
                return Err(format!(
                    "stored recipe for file {file_hash} covers {size} bytes, catalog declares {}",
                    expected.size()
                ));
            }
            validator
                .validate(&file)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

struct ShardState {
    hash: String,
    // A missing/corrupt shard still owns its following bindings. Report it once,
    // then consume those rows without claiming their recipes were verified.
    validated: Option<Arc<ValidatedShard>>,
}

async fn flush_files(
    current: &mut Option<ShardState>,
    files: &mut Vec<(String, FileCatalogEntry)>,
    issues: &mut Vec<FsckIssue>,
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let state = current
        .as_mut()
        .ok_or_else(|| CrabError::Protocol("grouped bindings lack a shard".to_owned()))?;
    let shard = state.validated.clone().ok_or_else(|| {
        CrabError::Protocol("unverifiable shard retained pending bindings".to_owned())
    })?;
    let files = std::mem::take(files);
    let result = tokio::task::spawn_blocking(move || shard.files(&files))
        .await
        .map_err(|source| CrabError::Io(std::io::Error::other(source)))?;
    if let Err(detail) = result {
        issues.push(FsckIssue::corrupt_shard(&state.hash, detail));
        state.validated = None;
    }
    Ok(())
}

fn validate_catalog_xorb(
    hash: &str,
    entry: &crab_metadata::capsule_protocol::XorbCatalogEntry,
    body: bytes::Bytes,
) -> std::result::Result<(), String> {
    if body.len() as u64 != entry.encoded_size() {
        return Err(format!(
            "stored size is {}, catalog declares {}",
            body.len(),
            entry.encoded_size()
        ));
    }
    let body_digest = blake3::hash(&body).to_hex().to_string();
    if body_digest != entry.body_digest() {
        return Err(format!(
            "stored body digest is {body_digest}, catalog declares {}",
            entry.body_digest()
        ));
    }
    let parser = XorbParser::parse(body).map_err(|error| error.to_string())?;
    if parser.hash().hex() != hash {
        return Err(format!(
            "stored logical identity is {}, catalog key is {hash}",
            parser.hash().hex()
        ));
    }
    parser
        .verify_payload_digest()
        .and_then(|()| parser.verify_all_chunks())
        .map_err(|error| error.to_string())?;
    if parser.num_chunks() as usize != entry.chunks().len() {
        return Err(format!(
            "stored chunk count is {}, catalog declares {}",
            parser.num_chunks(),
            entry.chunks().len()
        ));
    }
    for (index, expected) in entry.chunks().iter().enumerate() {
        let index = u32::try_from(index)
            .map_err(|_| "catalog xorb chunk index cannot be represented".to_owned())?;
        let actual = parser
            .chunk_meta(index)
            .map_err(|error| error.to_string())?;
        if actual.hash.hex() != expected.hash()
            || actual.uncompressed_len != expected.uncompressed_size()
        {
            return Err(format!(
                "stored chunk {index} does not match its catalog hash and size"
            ));
        }
    }
    Ok(())
}

fn prepare_shard(
    hash: &str,
    entry: &crab_metadata::capsule_protocol::ShardCatalogEntry,
    catalog: &crab_metadata::capsule_protocol::PointerCatalog,
    body: bytes::Bytes,
) -> std::result::Result<ValidatedShard, String> {
    if body.len() as u64 != entry.encoded_size() {
        return Err(format!(
            "stored size is {}, catalog declares {}",
            body.len(),
            entry.encoded_size()
        ));
    }
    let shard_hash = MerkleHash::from_hex(hash).map_err(|error| error.to_string())?;
    let actual_hash = crab_xet::hash::compute_data_hash(&body);
    if actual_hash != shard_hash {
        return Err(format!(
            "stored content identity is {}, catalog key is {hash}",
            actual_hash.hex()
        ));
    }

    let mut actual_xorbs = BTreeSet::new();
    let mut cursor = Cursor::new(crab_xet::shard_parse::strip_bloom_trailer(&body));
    crab_xet::shard_parse::visit_xorb_chunks_from_reader(&mut cursor, |xorb_hash, _, _, _| {
        actual_xorbs.insert(xorb_hash.hex());
        Ok(())
    })
    .map_err(|error| error.to_string())?;
    let expected_xorbs = entry.xorb_hashes().iter().cloned().collect::<BTreeSet<_>>();
    if actual_xorbs != expected_xorbs {
        return Err("stored xorb closure differs from the authenticated catalog".to_owned());
    }

    let reader = ShardReader::from_bytes(body, shard_hash);
    let mut dependencies = Vec::with_capacity(entry.xorb_hashes().len());
    for xorb_hash in entry.xorb_hashes() {
        let parsed_hash = MerkleHash::from_hex(xorb_hash).map_err(|error| error.to_string())?;
        let dependency = reader
            .get_xorb_info(&parsed_hash)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("stored shard lacks catalogued xorb {xorb_hash}"))?;
        let expected = catalog
            .xorbs()
            .get(xorb_hash)
            .ok_or_else(|| format!("catalog lacks shard xorb {xorb_hash}"))?;
        if dependency.chunks.len() != expected.chunks().len()
            || dependency
                .chunks
                .iter()
                .zip(expected.chunks())
                .any(|(actual, expected)| {
                    actual.chunk_hash.hex() != expected.hash()
                        || actual.unpacked_segment_bytes != expected.uncompressed_size()
                })
        {
            return Err(format!(
                "stored shard metadata for xorb {xorb_hash} differs from the catalog"
            ));
        }
        dependencies.push(dependency);
    }

    Ok(ValidatedShard {
        reader,
        dependencies,
    })
}

#[cfg(test)]
mod tests;
