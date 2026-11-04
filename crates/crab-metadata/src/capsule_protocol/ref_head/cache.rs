use super::*;
use std::io::Read;
#[cfg(feature = "storage")]
use std::io::Write;

const CACHE_VERSION: u32 = 1;
pub(in crate::capsule_protocol) const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CACHE_HEADS: usize = MAX_CACHE_BYTES / 512;

/// Bounded ref-head bodies captured with provider identities, excluding activation outcomes.
#[derive(Debug, Clone)]
pub struct RefHeadCache {
    pub(in crate::capsule_protocol) origin: String,
    pub(in crate::capsule_protocol) bytes: Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::capsule_protocol) struct RefHeadCachePointer {
    pub(in crate::capsule_protocol) origin: String,
    pub(in crate::capsule_protocol) range: super::super::PackRange,
}

impl RefHeadCachePointer {
    pub(in crate::capsule_protocol) fn deserialize_optional<'de, D: serde::Deserializer<'de>>(
        decoder: D,
    ) -> std::result::Result<Option<Self>, D::Error> {
        Self::deserialize(decoder).map(Some)
    }

    pub(in crate::capsule_protocol) fn validate(&self, body_end: u64) -> Result<()> {
        validate_content_hash(&self.origin, "ref-head cache origin", "checkpoint")?;
        validate_content_hash(self.range.blake3(), "ref-head cache hash", "checkpoint")?;
        if self.range.length() == 0
            || self.range.length() > MAX_CACHE_BYTES as u64
            || self.range.offset().checked_add(self.range.length()) != Some(body_end)
        {
            return Err(corrupt(
                "ref-head cache range is outside the checkpoint body",
            ));
        }
        Ok(())
    }

    pub(in crate::capsule_protocol) fn verify(&self, bytes: Bytes) -> Result<RefHeadCache> {
        if bytes.len() as u64 != self.range.length()
            || blake3::hash(&bytes).to_hex().as_str() != self.range.blake3()
        {
            return Err(corrupt(
                "ref-head cache differs from its checkpoint commitment",
            ));
        }
        Ok(RefHeadCache {
            origin: self.origin.clone(),
            bytes,
        })
    }
}

type Row = (
    String,
    CapsuleRefState,
    Option<CapsulePreparedRefState>,
    Option<String>,
    Option<String>,
);

#[derive(Serialize, Deserialize)]
struct Record(
    u32,
    u32,
    String,
    String,
    #[serde(deserialize_with = "bounded_rows")] Vec<Row>,
);

fn bounded_rows<'de, D: serde::Deserializer<'de>>(
    decoder: D,
) -> std::result::Result<Vec<Row>, D::Error> {
    struct Rows;
    impl<'de> serde::de::Visitor<'de> for Rows {
        type Value = Vec<Row>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded array of ref-head cache entries")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut rows = Vec::new();
            while let Some(row) = sequence.next_element()? {
                if rows.len() == MAX_CACHE_HEADS {
                    return Err(serde::de::Error::custom(
                        "ref-head cache entry limit exceeded",
                    ));
                }
                rows.push(row);
            }
            Ok(rows)
        }
    }
    decoder.deserialize_seq(Rows)
}

fn expand(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() > MAX_CACHE_BYTES {
        return Err(corrupt("ref-head cache exceeds its byte limit"));
    }
    let mut decoder = flate2::bufread::GzDecoder::new(bytes);
    let mut expanded = Vec::new();
    // The authenticated range bounds encoded bytes; a separate decoded bound
    // prevents compression from bypassing the existing materialization limit.
    decoder
        .by_ref()
        .take(MAX_CACHE_BYTES as u64 + 1)
        .read_to_end(&mut expanded)
        .map_err(|source| MetadataError::RefHeadCacheCodec { source })?;
    if expanded.len() > MAX_CACHE_BYTES {
        return Err(corrupt("decoded ref-head cache exceeds its byte limit"));
    }
    // Single-member decoding verifies CRC/length at EOF. Reject extra members
    // or trailing bytes so the entire committed range has one interpretation.
    if !decoder.into_inner().is_empty() {
        return Err(corrupt("ref-head cache contains trailing compressed data"));
    }
    Ok(expanded)
}

#[cfg(feature = "storage")]
fn compress(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder
        .write_all(bytes)
        .map_err(|source| MetadataError::RefHeadCacheCodec { source })?;
    encoder
        .finish()
        .map_err(|source| MetadataError::RefHeadCacheCodec { source })
}

fn decode(bytes: &[u8]) -> Result<Record> {
    let bytes = expand(bytes)?;
    let record: Record = serde_json::from_slice(&bytes)
        .map_err(|source| MetadataError::RefHeadCacheRecord { source })?;
    if record.0 != CACHE_VERSION || record.1 != REF_HEAD_VERSION || record.4.is_empty() {
        return Err(corrupt("ref-head cache version or entry count is invalid"));
    }
    validate_content_hash(&record.2, "ref-head cache repository", "checkpoint")?;
    validate_content_hash(&record.3, "ref-head cache epoch", "checkpoint")?;
    if record.4.windows(2).any(|rows| rows[0].0 >= rows[1].0) {
        return Err(corrupt("ref-head cache entries are not canonical"));
    }
    for (name, committed, prepared, etag, version) in &record.4 {
        if !identifies_body(etag.as_deref(), version.as_deref()) {
            return Err(corrupt(
                "ref-head cache entry has no immutable provider identity",
            ));
        }
        validate_head(&CapsuleRefHead {
            version: REF_HEAD_VERSION,
            ref_name: name.clone(),
            ref_epoch: record.3.clone(),
            committed: committed.clone(),
            prepared: prepared.clone(),
        })
        .map_err(as_corruption)?;
    }
    let canonical = serde_json::to_vec(&record)
        .map_err(|source| MetadataError::RefHeadCacheRecord { source })?;
    if canonical != bytes {
        return Err(corrupt("ref-head cache is not canonically encoded"));
    }
    Ok(record)
}

pub(in crate::capsule_protocol) fn identifies_body(
    etag: Option<&str>,
    version: Option<&str>,
) -> bool {
    etag.is_some_and(|etag| !etag.is_empty() && !etag.starts_with("W/"))
        || version.is_some_and(|version| !version.is_empty() && version != "null")
}

#[cfg(feature = "storage")]
struct BoundedBytes {
    bytes: Vec<u8>,
    maximum: usize,
    exceeded: bool,
}

#[cfg(feature = "storage")]
impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.maximum.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(std::io::Error::other("ref-head cache byte limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl RefHeadCache {
    pub(in crate::capsule_protocol) fn validate(&self) -> Result<()> {
        decode(&self.bytes).map(|_| ())
    }

    #[cfg(feature = "storage")]
    pub(in crate::capsule_protocol) fn capture<'a>(
        origin: String,
        repository_id: &str,
        epoch: &str,
        heads: impl Iterator<Item = (&'a CapsuleRefHead, Option<&'a str>, Option<&'a str>)>,
    ) -> Result<Option<Self>> {
        let mut bytes =
            serde_json::to_vec(&(CACHE_VERSION, REF_HEAD_VERSION, repository_id, epoch))
                .map_err(|source| MetadataError::RefHeadCacheRecord { source })?;
        bytes.pop();
        bytes.extend_from_slice(b",[");
        let mut output = BoundedBytes {
            bytes,
            maximum: MAX_CACHE_BYTES - 2,
            exceeded: false,
        };
        let mut count = 0;
        for (head, etag, version) in heads {
            if count == MAX_CACHE_HEADS {
                break;
            }
            if !identifies_body(etag, version) {
                continue;
            }
            let start = output.bytes.len();
            if count > 0 {
                output.bytes.push(b',');
            }
            let row = (
                &head.ref_name,
                &head.committed,
                &head.prepared,
                etag,
                version,
            );
            if let Err(source) = serde_json::to_writer(&mut output, &row) {
                if !output.exceeded {
                    return Err(MetadataError::RefHeadCacheRecord { source });
                }
                output.bytes.truncate(start);
                break;
            }
            count += 1;
        }
        // One cached head cannot save a request: its range replaces one GET
        // on a hit and adds a read whenever that head changes.
        if count < 2 {
            return Ok(None);
        }
        output.bytes.extend_from_slice(b"]]");
        let bytes = compress(&output.bytes)?;
        if bytes.len() > MAX_CACHE_BYTES {
            return Ok(None);
        }
        Ok(Some(Self {
            origin,
            bytes: Bytes::from(bytes),
        }))
    }

    #[cfg(feature = "storage")]
    pub(in crate::capsule_protocol) fn heads(
        &self,
        layout: &crab_storage::StoreLayout<crab_storage::Store>,
        root: &super::super::RepositoryRoot,
    ) -> Result<
        std::collections::BTreeMap<object_store::path::Path, (CapsuleRefHead, crab_storage::ETag)>,
    > {
        let record = decode(&self.bytes)?;
        if record.2 != root.repository_id() || record.3 != root.ref_epoch() {
            return Ok(Default::default());
        }
        Ok(record
            .4
            .into_iter()
            .map(|(name, committed, prepared, e_tag, version)| {
                let head = CapsuleRefHead {
                    version: REF_HEAD_VERSION,
                    ref_name: name,
                    ref_epoch: record.3.clone(),
                    committed,
                    prepared,
                };
                (
                    layout.capsule_ref_head_path(&capsule_ref_name_key(head.ref_name())),
                    (head, crab_storage::ETag { e_tag, version }),
                )
            })
            .collect())
    }
}

#[cfg(feature = "storage")]
pub(in crate::capsule_protocol) fn origin(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
) -> Option<String> {
    let prefix = layout.capsule_ref_heads_prefix();
    let target = layout.store().read_prefix_target_identity(&prefix)?;
    let mut hasher = blake3::Hasher::new_derive_key("crab checkpoint ref-head origin v1");
    hasher.update(target);
    hasher.update(prefix.as_ref().as_bytes());
    Some(hasher.finalize().to_hex().to_string())
}

#[cfg(all(test, feature = "storage"))]
mod tests {
    use super::*;

    fn cache() -> RefHeadCache {
        let heads = ["main", "other"].map(|name| {
            CapsuleRefHead::from_root(&format!("refs/heads/{name}"), "1".repeat(64), None, None)
                .unwrap()
        });
        RefHeadCache::capture(
            "2".repeat(64),
            &"1".repeat(64),
            &"1".repeat(64),
            heads.iter().map(|head| (head, Some("opaque"), None)),
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn cache_requires_one_complete_checked_compression_member() {
        let cache = cache();
        let mut checksum = cache.bytes.to_vec();
        let crc_offset = checksum.len() - 8;
        checksum[crc_offset] ^= 1;
        let mut member = cache.bytes.to_vec();
        member.extend_from_slice(&compress(b"[]").unwrap());
        let mut trailing = cache.bytes.to_vec();
        trailing.push(0);
        for (fault, bytes) in [
            ("checksum", checksum),
            ("truncated", cache.bytes[..cache.bytes.len() - 1].to_vec()),
            ("extra member", member),
            ("trailing bytes", trailing),
            ("retired plaintext", expand(&cache.bytes).unwrap()),
        ] {
            assert!(decode(&bytes).is_err(), "accepted {fault}");
        }
    }

    #[test]
    fn cache_limits_decoded_bytes_before_parsing_compressed_input() {
        let encoded = compress(&vec![b' '; MAX_CACHE_BYTES + 1]).unwrap();
        assert!(encoded.len() < MAX_CACHE_BYTES);
        assert!(
            matches!(decode(&encoded), Err(MetadataError::CorruptObject { reason, .. })
            if reason.contains("decoded ref-head cache exceeds"))
        );
    }

    #[test]
    fn cache_retains_canonical_records_across_valid_compression_encodings() {
        let cache = cache();
        let plain = expand(&cache.bytes).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&plain).unwrap();
        let encoded = encoder.finish().unwrap();
        assert_eq!(
            serde_json::to_vec(&decode(&encoded).unwrap()).unwrap(),
            plain
        );
    }

    #[test]
    fn cache_encoding_stops_at_whole_entries_within_the_byte_budget() {
        let heads = ["first", "second", "third"].map(|name| {
            CapsuleRefHead::from_root(&format!("refs/heads/{name}"), "1".repeat(64), None, None)
                .unwrap()
        });
        let token = "a".repeat(MAX_CACHE_BYTES / 3);
        let cache = RefHeadCache::capture(
            "2".repeat(64),
            &"1".repeat(64),
            &"1".repeat(64),
            heads.iter().map(|head| (head, Some(token.as_str()), None)),
        )
        .unwrap()
        .unwrap();
        let record = decode(&cache.bytes).unwrap();
        assert_eq!(
            (cache.bytes.len() <= MAX_CACHE_BYTES, record.4.len()),
            (true, 2)
        );
    }

    #[test]
    fn cache_encoding_and_decoding_bound_entry_counts_independently_of_bytes() {
        let heads = (0..MAX_CACHE_HEADS + 1)
            .map(|index| {
                CapsuleRefHead::from_root(
                    &format!("refs/heads/{index:06}"),
                    "1".repeat(64),
                    None,
                    None,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let cache = RefHeadCache::capture(
            "2".repeat(64),
            &"1".repeat(64),
            &"1".repeat(64),
            heads.iter().map(|head| (head, Some("opaque"), None)),
        )
        .unwrap()
        .unwrap();
        let mut record = decode(&cache.bytes).unwrap();
        assert_eq!(record.4.len(), MAX_CACHE_HEADS);
        record.4.push(record.4.last().unwrap().clone());
        let bytes = serde_json::to_vec(&record).unwrap();
        assert!(
            matches!(decode(&compress(&bytes).unwrap()), Err(MetadataError::RefHeadCacheRecord { source }) if source.to_string().contains("entry limit"))
        );
    }

    #[test]
    fn cache_rejects_relabelled_versions_and_unidentified_or_noncanonical_rows() {
        let heads = ["main", "other"].map(|name| {
            CapsuleRefHead::from_root(&format!("refs/heads/{name}"), "1".repeat(64), None, None)
                .unwrap()
        });
        let cache = RefHeadCache::capture(
            "2".repeat(64),
            &"1".repeat(64),
            &"1".repeat(64),
            heads.iter().map(|head| (head, Some("opaque"), None)),
        )
        .unwrap()
        .unwrap();
        for field in ["version", "head_version", "identity", "duplicate", "state"] {
            let mut value: serde_json::Value =
                serde_json::from_slice(&expand(&cache.bytes).unwrap()).unwrap();
            match field {
                "version" => value[0] = 2.into(),
                "head_version" => value[1] = 6.into(),
                "identity" => {
                    value[4][0][3] = "".into();
                    value[4][0][4] = "null".into();
                }
                "duplicate" => {
                    let row = value[4][0].clone();
                    value[4].as_array_mut().unwrap().push(row);
                }
                _ => {
                    value[4][0][1]["peeled_oid"] = "3".repeat(40).into();
                }
            }
            assert!(
                decode(&compress(&serde_json::to_vec(&value).unwrap()).unwrap()).is_err(),
                "{field}"
            );
        }
    }
}
