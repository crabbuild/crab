//! Durable, content-addressed shard reachability closures.

use std::collections::HashSet;

use bytes::Bytes;
use object_store::path::Path;
use serde::{Deserialize, Serialize};

use crate::core::error::{CrabError, Result};
use crate::storage::store::Store;
use crab_xet::hash::{MerkleHash, compute_data_hash};
use crab_xet::shard::ShardReader;

pub const CLOSURE_SCHEMA_VERSION: u32 = 2;
pub const MAX_CLOSURE_BYTES: usize = 8 * 1024 * 1024;
pub const CLOSURE_HASHES_PER_SEGMENT: usize = 4096;
const MAX_CLOSURE_SEGMENT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardClosure {
    pub schema_version: u32,
    pub shard_hash: String,
    pub content_hash: String,
    pub content_size: u64,
    pub xorb_count: u64,
    pub file_count: u64,
    pub xorb_hashes: Vec<String>,
    pub file_hashes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureManifest {
    pub schema_version: u32,
    pub shard_hash: String,
    pub content_hash: String,
    pub content_size: u64,
    pub xorb_count: u64,
    pub file_count: u64,
    pub segments: Vec<ClosureSegmentRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureSegmentRef {
    pub index: u64,
    pub digest: String,
    pub xorb_count: u64,
    pub file_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureSegment {
    schema_version: u32,
    shard_hash: String,
    index: u64,
    xorb_hashes: Vec<String>,
    file_hashes: Vec<String>,
}

impl ClosureSegment {
    pub fn xorb_hashes(&self) -> &[String] {
        &self.xorb_hashes
    }

    pub fn file_hashes(&self) -> &[String] {
        &self.file_hashes
    }
}

#[must_use]
pub fn path(global_prefix: &str, shard_hash: &str) -> Path {
    Path::from(format!("{global_prefix}/gc/closures/{shard_hash}.json"))
}

pub(crate) fn segment_path(global_prefix: &str, shard_hash: &str, index: u64) -> Path {
    Path::from(format!(
        "{global_prefix}/gc/closure-segments/{shard_hash}/{index:020}.json"
    ))
}

pub fn build(shard_hash: &MerkleHash, body: Bytes, object_path: &str) -> Result<ShardClosure> {
    let actual = compute_data_hash(&body);
    let content_size = u64::try_from(body.len()).map_err(|_| CrabError::CorruptObject {
        path: object_path.to_owned(),
        reason: "shard body size does not fit in closure metadata".to_owned(),
    })?;
    if actual != *shard_hash {
        return Err(CrabError::CorruptObject {
            path: object_path.to_owned(),
            reason: format!(
                "shard body hash is {}, expected {}",
                actual.hex(),
                shard_hash.hex()
            ),
        });
    }

    let reader = ShardReader::from_bytes(body, *shard_hash);
    let shard_info = reader
        .shard_info_public()
        .map_err(|error| corrupt(object_path, format!("failed to parse shard: {error}")))?;
    let v1_bytes = reader.v1_data();

    let mut xorb_hashes = HashSet::new();
    let mut cursor = std::io::Cursor::new(v1_bytes);
    let blocks = shard_info
        .read_all_xorb_blocks_full(&mut cursor)
        .map_err(|error| {
            corrupt(
                object_path,
                format!("failed to read shard xorb closure: {error}"),
            )
        })?;
    for block in &blocks {
        xorb_hashes.insert(block.metadata.xorb_hash.hex());
    }

    let mut file_hashes = HashSet::new();
    let mut cursor = std::io::Cursor::new(v1_bytes);
    let files = shard_info
        .read_all_file_info_sections(&mut cursor)
        .map_err(|error| {
            corrupt(
                object_path,
                format!("failed to read shard file closure: {error}"),
            )
        })?;
    for file in &files {
        file_hashes.insert(file.metadata.file_hash.hex());
    }

    let mut xorb_hashes = xorb_hashes.into_iter().collect::<Vec<_>>();
    let mut file_hashes = file_hashes.into_iter().collect::<Vec<_>>();
    xorb_hashes.sort_unstable();
    file_hashes.sort_unstable();
    Ok(ShardClosure {
        schema_version: CLOSURE_SCHEMA_VERSION,
        shard_hash: shard_hash.hex(),
        content_hash: actual.hex(),
        content_size,
        xorb_count: xorb_hashes.len() as u64,
        file_count: file_hashes.len() as u64,
        xorb_hashes,
        file_hashes,
    })
}

pub fn decode(body: &[u8], path: &Path, expected_hash: &MerkleHash) -> Result<ShardClosure> {
    if body.len() > MAX_CLOSURE_BYTES {
        return Err(CrabError::Configuration {
            key: "gc.closure.memory_budget".to_owned(),
            origin: format!(
                "closure {} is {} bytes, above the {}-byte decode budget",
                path,
                body.len(),
                MAX_CLOSURE_BYTES
            ),
        });
    }
    let closure: ShardClosure = serde_json::from_slice(body).map_err(|error| {
        corrupt(
            path.as_ref(),
            format!("invalid shard closure JSON: {error}"),
        )
    })?;
    if closure.schema_version != CLOSURE_SCHEMA_VERSION
        || closure.shard_hash != expected_hash.hex()
        || closure.content_hash != expected_hash.hex()
        || closure.xorb_count != closure.xorb_hashes.len() as u64
        || closure.file_count != closure.file_hashes.len() as u64
        || !is_sorted_unique(&closure.xorb_hashes)
        || !is_sorted_unique(&closure.file_hashes)
        || closure
            .xorb_hashes
            .iter()
            .chain(&closure.file_hashes)
            .any(|hash| MerkleHash::from_hex(hash).is_err())
    {
        return Err(corrupt(
            path.as_ref(),
            "shard closure identity or coverage is invalid".to_owned(),
        ));
    }
    Ok(closure)
}

/// Build and durably publish the closure for one canonical shard.
pub async fn publish(
    store: &Store,
    global_prefix: &str,
    shard_hash: &MerkleHash,
    body: Bytes,
    object_path: &str,
) -> Result<()> {
    let closure = build(shard_hash, body, object_path)?;
    let closure_path = path(global_prefix, &shard_hash.hex());
    let segment_count = closure
        .xorb_hashes
        .len()
        .max(closure.file_hashes.len())
        .div_ceil(CLOSURE_HASHES_PER_SEGMENT);
    let mut segments = Vec::with_capacity(segment_count);
    for index in 0..segment_count {
        let start = index * CLOSURE_HASHES_PER_SEGMENT;
        let xorb_end = (start + CLOSURE_HASHES_PER_SEGMENT).min(closure.xorb_hashes.len());
        let file_end = (start + CLOSURE_HASHES_PER_SEGMENT).min(closure.file_hashes.len());
        let segment = ClosureSegment {
            schema_version: CLOSURE_SCHEMA_VERSION,
            shard_hash: closure.shard_hash.clone(),
            index: index as u64,
            xorb_hashes: closure
                .xorb_hashes
                .get(start..xorb_end)
                .unwrap_or_default()
                .to_vec(),
            file_hashes: closure
                .file_hashes
                .get(start..file_end)
                .unwrap_or_default()
                .to_vec(),
        };
        let segment_path = segment_path(global_prefix, &closure.shard_hash, segment.index);
        let encoded = encode_bounded(&segment, &segment_path, MAX_CLOSURE_SEGMENT_BYTES)?;
        let digest = blake3::hash(&encoded).to_hex().to_string();
        publish_encoded(store, &segment_path, encoded).await?;
        segments.push(ClosureSegmentRef {
            index: segment.index,
            digest,
            xorb_count: segment.xorb_hashes.len() as u64,
            file_count: segment.file_hashes.len() as u64,
        });
    }
    let manifest = ClosureManifest {
        schema_version: CLOSURE_SCHEMA_VERSION,
        shard_hash: closure.shard_hash,
        content_hash: closure.content_hash,
        content_size: closure.content_size,
        xorb_count: closure.xorb_count,
        file_count: closure.file_count,
        segments,
    };
    let encoded = encode_bounded(&manifest, &closure_path, MAX_CLOSURE_BYTES)?;
    publish_encoded(store, &closure_path, encoded).await
}

pub async fn read_manifest(
    store: &Store,
    global_prefix: &str,
    shard_hash: &MerkleHash,
) -> Result<ClosureManifest> {
    let manifest_path = path(global_prefix, &shard_hash.hex());
    let (body, _) = store.get_with_etag(&manifest_path).await?;
    if body.len() > MAX_CLOSURE_BYTES {
        return Err(closure_budget_error(
            &manifest_path,
            body.len(),
            MAX_CLOSURE_BYTES,
        ));
    }
    let manifest: ClosureManifest = serde_json::from_slice(&body).map_err(|error| {
        corrupt(
            &manifest_path.to_string(),
            format!("invalid closure manifest JSON: {error}"),
        )
    })?;
    validate_manifest(&manifest, &manifest_path, shard_hash)?;
    Ok(manifest)
}

pub async fn read_segment(
    store: &Store,
    global_prefix: &str,
    manifest: &ClosureManifest,
    segment_ref: &ClosureSegmentRef,
) -> Result<ClosureSegment> {
    let path = segment_path(global_prefix, &manifest.shard_hash, segment_ref.index);
    let (body, _) = store.get_with_etag(&path).await?;
    if body.len() > MAX_CLOSURE_SEGMENT_BYTES {
        return Err(closure_budget_error(
            &path,
            body.len(),
            MAX_CLOSURE_SEGMENT_BYTES,
        ));
    }
    if blake3::hash(&body).to_hex().as_str() != segment_ref.digest {
        return Err(corrupt(
            path.as_ref(),
            "closure segment digest mismatch".to_owned(),
        ));
    }
    let segment: ClosureSegment = serde_json::from_slice(&body).map_err(|error| {
        corrupt(
            path.as_ref(),
            format!("invalid closure segment JSON: {error}"),
        )
    })?;
    if segment.schema_version != CLOSURE_SCHEMA_VERSION
        || segment.shard_hash != manifest.shard_hash
        || segment.index != segment_ref.index
        || segment.xorb_hashes.len() as u64 != segment_ref.xorb_count
        || segment.file_hashes.len() as u64 != segment_ref.file_count
        || !is_sorted_unique(&segment.xorb_hashes)
        || !is_sorted_unique(&segment.file_hashes)
        || segment
            .xorb_hashes
            .iter()
            .chain(&segment.file_hashes)
            .any(|hash| MerkleHash::from_hex(hash).is_err())
    {
        return Err(corrupt(
            path.as_ref(),
            "invalid closure segment identity".to_owned(),
        ));
    }
    Ok(segment)
}

pub(crate) async fn publish_encoded(
    store: &Store,
    closure_path: &Path,
    encoded: Bytes,
) -> Result<()> {
    if encoded.len() > MAX_CLOSURE_BYTES {
        return Err(CrabError::Configuration {
            key: "gc.closure.memory_budget".to_owned(),
            origin: format!(
                "closure {} is {} bytes, above the {}-byte publication budget",
                closure_path,
                encoded.len(),
                MAX_CLOSURE_BYTES
            ),
        });
    }
    match store.put(closure_path, encoded).await {
        Ok(()) => Ok(()),
        Err(CrabError::CasConflict { .. }) => Err(CrabError::CorruptObject {
            path: closure_path.to_string(),
            reason: "existing shard closure differs from the content-addressed publication"
                .to_owned(),
        }),
        Err(error) => Err(error),
    }
}

fn encode_bounded<T: Serialize>(value: &T, path: &Path, limit: usize) -> Result<Bytes> {
    let encoded = serde_json::to_vec(value).map_err(|error| CrabError::CorruptObject {
        path: path.to_string(),
        reason: format!("failed to encode shard closure: {error}"),
    })?;
    if encoded.len() > limit {
        return Err(closure_budget_error(path, encoded.len(), limit));
    }
    Ok(Bytes::from(encoded))
}

fn validate_manifest(
    manifest: &ClosureManifest,
    path: &Path,
    expected_hash: &MerkleHash,
) -> Result<()> {
    let xorb_count = manifest
        .segments
        .iter()
        .try_fold(0u64, |count, segment| count.checked_add(segment.xorb_count));
    let file_count = manifest
        .segments
        .iter()
        .try_fold(0u64, |count, segment| count.checked_add(segment.file_count));
    let valid_segments = manifest
        .segments
        .iter()
        .enumerate()
        .all(|(index, segment)| {
            segment.index == index as u64
                && segment.digest.len() == 64
                && segment.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                && segment.xorb_count <= CLOSURE_HASHES_PER_SEGMENT as u64
                && segment.file_count <= CLOSURE_HASHES_PER_SEGMENT as u64
        });
    if manifest.schema_version != CLOSURE_SCHEMA_VERSION
        || manifest.shard_hash != expected_hash.hex()
        || manifest.content_hash != expected_hash.hex()
        || xorb_count != Some(manifest.xorb_count)
        || file_count != Some(manifest.file_count)
        || !valid_segments
    {
        return Err(corrupt(
            path.as_ref(),
            "shard closure manifest identity or coverage is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn closure_budget_error(path: &Path, size: usize, limit: usize) -> CrabError {
    CrabError::Configuration {
        key: "gc.closure.memory_budget".to_owned(),
        origin: format!("closure {path} is {size} bytes, above the {limit}-byte budget"),
    }
}

fn is_sorted_unique(values: &[String]) -> bool {
    values.windows(2).all(|window| window[0] < window[1])
}

fn corrupt(path: &str, reason: String) -> CrabError {
    CrabError::CorruptObject {
        path: path.to_owned(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crab_xet::shard::ShardWriter;
    use object_store::memory::InMemory;

    #[test]
    fn empty_shard_closure_round_trips_with_identity_and_counts() {
        let (body, hash) = ShardWriter::new()
            .finalize()
            .expect("empty shard serializes");
        let body = Bytes::from(body);
        let closure = build(&hash, body.clone(), "shard").expect("closure builds");
        assert_eq!(closure.content_hash, hash.hex());
        assert_eq!(closure.content_size, body.len() as u64);
        assert_eq!(closure.xorb_count, 0);
        assert_eq!(closure.file_count, 0);
        let encoded = serde_json::to_vec(&closure).expect("closure encodes");
        let decoded = decode(&encoded, &Path::from("closure"), &hash).expect("closure decodes");
        assert_eq!(decoded.xorb_hashes, closure.xorb_hashes);
        assert_eq!(decoded.file_hashes, closure.file_hashes);
    }

    #[test]
    fn decode_rejects_count_mismatch() {
        let (body, hash) = ShardWriter::new()
            .finalize()
            .expect("empty shard serializes");
        let closure = build(&hash, Bytes::from(body), "shard").expect("closure builds");
        let mut value = serde_json::to_value(closure).expect("closure encodes");
        value["xorb_count"] = serde_json::json!(1);
        let encoded = serde_json::to_vec(&value).expect("tampered closure encodes");
        let error = decode(&encoded, &Path::from("closure"), &hash).expect_err("count mismatch");
        assert!(matches!(error, CrabError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn publish_does_not_overwrite_a_conflicting_sidecar() {
        let store = Store::new(Arc::new(InMemory::new()));
        let (body, hash) = ShardWriter::new()
            .finalize()
            .expect("empty shard serializes");
        let closure_path = path(".crab", &hash.hex());
        store
            .put(&closure_path, Bytes::from_static(b"conflicting"))
            .await
            .expect("seed sidecar");
        let error = publish(&store, ".crab", &hash, Bytes::from(body), "shard")
            .await
            .expect_err("conflicting sidecar must fail closed");
        assert!(matches!(error, CrabError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn published_closure_uses_a_bounded_manifest() {
        let store = Store::new(Arc::new(InMemory::new()));
        let (body, hash) = ShardWriter::new()
            .finalize()
            .expect("empty shard serializes");

        publish(&store, ".crab", &hash, Bytes::from(body), "shard")
            .await
            .unwrap();
        let manifest = read_manifest(&store, ".crab", &hash).await.unwrap();

        assert_eq!(manifest.schema_version, CLOSURE_SCHEMA_VERSION);
        assert!(manifest.segments.is_empty());
        assert_eq!(manifest.xorb_count, 0);
        assert_eq!(manifest.file_count, 0);
    }
}
