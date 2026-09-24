//! Durable Blob part store backed by the Crab object store.

use super::*;

/// Object-store backing for Blob parts.
///
/// SQLite stores only the bounded upload manifest and content digests. Part
/// bytes are immutable, content-addressed objects in the configured Crab
/// object store and are verified again on every range read.
#[derive(Clone)]
pub struct BlobArtifactStore {
    store: Store,
}

/// Result from one Blob part reachability sweep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobGarbageCollectionReport {
    scanned: u64,
    deleted: u32,
    has_more: bool,
}

impl BlobGarbageCollectionReport {
    /// Returns the number of object-store entries inspected by the sweep.
    #[must_use]
    pub const fn scanned(self) -> u64 {
        self.scanned
    }

    /// Returns the number of unreferenced part objects deleted by the sweep.
    #[must_use]
    pub const fn deleted(self) -> u32 {
        self.deleted
    }

    /// Returns whether the deletion budget was reached and another sweep may be needed.
    #[must_use]
    pub const fn has_more(self) -> bool {
        self.has_more
    }
}

impl BlobArtifactStore {
    /// Wraps a configured Crab object store for Blob artifact data.
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub(super) async fn put_part(&self, digest: [u8; 32], payload: &[u8]) -> Result<()> {
        if part_digest(payload) != digest {
            return Err(Error::Command("blob part digest does not match payload"));
        }
        self.store
            .put(&self.part_path(&digest), Bytes::copy_from_slice(payload))
            .await?;
        Ok(())
    }

    pub(super) async fn read_part(&self, digest: [u8; 32], size: u32) -> Result<Vec<u8>> {
        if usize::try_from(size)
            .ok()
            .is_none_or(|size| size > MAX_BLOB_PART_BYTES)
        {
            return Err(Error::Command("invalid stored blob part size"));
        }
        let (bytes, _) = self
            .store
            .get_with_etag_bounded(&self.part_path(&digest), u64::from(size))
            .await?;
        if bytes.len() != size as usize || part_digest(&bytes) != digest {
            return Err(Error::Command("blob part integrity check failed"));
        }
        Ok(bytes.to_vec())
    }

    /// Reclaims old part objects absent from a complete cross-Cell reference set.
    ///
    /// Callers must quiesce Blob writes in this object-store scope and build
    /// `live_digests` from every authoritative Cell database sharing it. A grace
    /// boundary alone cannot protect an old part reused by a concurrent upload.
    /// Objects newer than `cutoff_ms` are retained for uploads whose SQLite
    /// manifests have not committed. Each call inspects the complete unordered
    /// listing until 128 parts have been deleted; schedule another call while
    /// [`BlobGarbageCollectionReport::has_more`] is true.
    pub async fn sweep_unreferenced(
        &self,
        live_digests: &BTreeSet<[u8; 32]>,
        cutoff_ms: i64,
    ) -> Result<BlobGarbageCollectionReport> {
        if cutoff_ms < 0 {
            return Err(Error::Command("negative blob garbage-collection cutoff"));
        }
        let prefix = self
            .store
            .storage_scope()
            .map_or(GLOBAL_PREFIX, |scope| scope.global_prefix.as_str());
        let mut objects = self
            .store
            .list_stream(&global_content_prefix(prefix, BLOB_PART_KIND));
        let mut scanned = 0_u64;
        let mut candidates = Vec::with_capacity(MAX_BLOB_GC_DELETIONS as usize);
        while let Some(object) = objects.next().await {
            let object = object?;
            scanned = scanned.saturating_add(1);
            let Some(hash) = content_hash_from_path(object.location.as_ref(), BLOB_PART_KIND)
            else {
                continue;
            };
            let Some(digest) = decode_hex_digest(hash) else {
                continue;
            };
            if live_digests.contains(&digest) || object.last_modified.timestamp_millis() > cutoff_ms
            {
                continue;
            }
            candidates.push(object.location);
            if candidates.len() == MAX_BLOB_GC_DELETIONS as usize {
                break;
            }
        }
        drop(objects);
        let has_more = candidates.len() == MAX_BLOB_GC_DELETIONS as usize;
        let mut deleted = 0_u32;
        for path in candidates {
            match self.store.delete(&path).await {
                Ok(()) | Err(StorageError::NotFound { .. }) => deleted += 1,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(BlobGarbageCollectionReport {
            scanned,
            deleted,
            has_more,
        })
    }

    fn part_path(&self, digest: &[u8; 32]) -> ObjectPath {
        let hash = blake3::Hash::from_bytes(*digest).to_hex().to_string();
        let prefix = self
            .store
            .storage_scope()
            .map_or(GLOBAL_PREFIX, |scope| scope.global_prefix.as_str());
        global_content_path(prefix, BLOB_PART_KIND, &hash)
    }
}

fn decode_hex_digest(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        digest[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(digest)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

pub(super) fn part_digest(payload: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.blob-part.v1\0");
    hasher.update(payload);
    *hasher.finalize().as_bytes()
}
