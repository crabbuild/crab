//! Durable, content-addressed shard reachability closures.

use std::collections::HashSet;

use bytes::Bytes;
use object_store::path::Path;
use serde::{Deserialize, Serialize};

use crate::core::error::{CrabError, Result};
use crate::storage::store::Store;
use crab_xet::hash::{MerkleHash, compute_data_hash};
use crab_xet::shard::ShardReader;

pub const CLOSURE_SCHEMA_VERSION: u32 = 1;
pub const MAX_CLOSURE_BYTES: usize = 128 * 1024 * 1024;

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

#[must_use]
pub fn path(global_prefix: &str, shard_hash: &str) -> Path {
    Path::from(format!("{global_prefix}/gc/closures/{shard_hash}.json"))
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
    let encoded = serde_json::to_vec(&closure).map_err(|error| CrabError::CorruptObject {
        path: closure_path.to_string(),
        reason: format!("failed to encode shard closure: {error}"),
    })?;
    publish_encoded(store, &closure_path, Bytes::from(encoded)).await
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
}
