use super::*;
use tempfile::tempdir;

#[test]
fn persistent_promotion_preserves_negative_expiry() {
    let dir = tempdir().expect("tempdir");
    let cache = AddRemoteCandidateCache::open(&cache_path(&dir)).expect("open");
    let hash = MerkleHash::from([11; 32]);
    cache
        .persist_unique_results(&[(hash, None)])
        .expect("persist");
    let now = current_unix_timestamp().expect("clock");
    let observed_at = now - NEGATIVE_TTL.as_secs() as i64 + 10;
    cache
        .connection
        .lock()
        .expect("lock")
        .execute(
            "UPDATE remote_candidate_misses_v1 SET observed_at = ?1",
            [observed_at],
        )
        .expect("age negative");
    assert_eq!(
        cache.load_persistent(&[hash]).expect("promote").get(&hash),
        Some(&None)
    );
    assert!(
        cache
            .memory_get_batch_at(&[hash], now + 10)
            .expect("expired lookup")
            .is_empty()
    );
}

#[test]
fn memory_negatives_expire_but_positive_proofs_remain_candidates() {
    let dir = tempdir().expect("tempdir");
    let cache = AddRemoteCandidateCache::open(&cache_path(&dir)).expect("open");
    let negative = MerkleHash::from([12; 32]);
    let positive = MerkleHash::from([13; 32]);
    cache
        .memory_insert_batch(&[(negative, None), (positive, Some(candidate(13)))])
        .expect("insert");
    let later = current_unix_timestamp().expect("clock") + NEGATIVE_TTL.as_secs() as i64;
    assert_eq!(
        cache
            .memory_get_batch_at(&[negative, positive], later)
            .expect("lookup"),
        HashMap::from([(positive, Some(candidate(13)))])
    );
}

#[test]
fn concurrent_connections_preserve_capacity_and_counts() {
    let dir = tempdir().expect("tempdir");
    let path = cache_path(&dir);
    drop(AddRemoteCandidateCache::open(&path).expect("initialize"));
    std::thread::scope(|scope| {
        for writer in 0..4u64 {
            let path = &path;
            scope.spawn(move || {
                let cache = AddRemoteCandidateCache::open(path).expect("open writer");
                let entries = (0..400u64)
                    .map(|index| {
                        let mut hash = [0; 32];
                        hash[..8].copy_from_slice(&(writer * 400 + index).to_le_bytes());
                        (MerkleHash::from(hash), Some(candidate(writer as u8)))
                    })
                    .collect::<Vec<_>>();
                cache
                    .persist_unique_results(&entries)
                    .expect("positive write");
                let misses = entries
                    .iter()
                    .map(|(hash, _)| (*hash, None))
                    .collect::<Vec<_>>();
                cache
                    .persist_unique_results(&misses)
                    .expect("negative replacement");
                cache
                    .persist_unique_results(&entries)
                    .expect("positive replacement");
            });
        }
    });
    let cache = AddRemoteCandidateCache::open(&path).expect("reopen");
    let counts: (i64, i64) = cache
        .connection
        .lock()
        .expect("lock")
        .query_row(
            "SELECT positives + negatives,
                (SELECT COUNT(*) FROM remote_candidates_v1) +
                (SELECT COUNT(*) FROM remote_candidate_misses_v1) FROM cache_counts",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("count");
    assert_eq!(counts, (MAX_PERSISTENT_ENTRIES, MAX_PERSISTENT_ENTRIES));
}

#[test]
fn persistent_capacity_survives_repeated_reopens() {
    let dir = tempdir().expect("tempdir");
    for page in 0..4u64 {
        let cache = AddRemoteCandidateCache::open(&cache_path(&dir)).expect("open");
        let entries = (0..400u64)
            .map(|offset| {
                let mut hash = [0; 32];
                hash[..8].copy_from_slice(&(page * 400 + offset).to_le_bytes());
                (MerkleHash::from(hash), None)
            })
            .collect::<Vec<_>>();
        cache.persist_unique_results(&entries).expect("persist");
        let count: i64 = cache
            .connection
            .lock()
            .expect("lock")
            .query_row(
                "SELECT (SELECT COUNT(*) FROM remote_candidates_v1) +
                    (SELECT COUNT(*) FROM remote_candidate_misses_v1)",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert!(
            count <= MAX_PERSISTENT_ENTRIES,
            "cache grew to {count} entries"
        );
    }
}

fn candidate(seed: u8) -> ExistingChunkCandidate {
    ExistingChunkCandidate {
        xorb_ref: XorbRef {
            xorb_hash: MerkleHash::from([seed; 32]),
            chunk_index: u32::from(seed),
            uncompressed_size: 4096,
        },
        placement_id: [seed.wrapping_add(1); 32],
        origin_proof_id: [seed.wrapping_add(2); 32],
    }
}

fn cache_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let root = dir.path().join("cache");
    crab_cache::ensure_private_cache_directory(&root).expect("private cache root");
    root.join("cache.sqlite")
}

#[test]
fn persistent_candidates_round_trip() {
    let dir = tempdir().expect("tempdir");
    let path = cache_path(&dir);
    let cache = AddRemoteCandidateCache::open(&path).expect("open");
    let hash = MerkleHash::from([7; 32]);
    cache
        .persist_results(&[(hash, Some(candidate(7)))])
        .expect("persist");
    cache
        .persist_results(&[(hash, Some(candidate(8)))])
        .expect("refresh");
    let loaded = cache.load_persistent(&[hash]).expect("load");
    assert_eq!(loaded.get(&hash), Some(&Some(candidate(8))));
}

#[test]
fn persistent_negative_entries_round_trip_and_refresh() {
    let dir = tempdir().expect("tempdir");
    let cache = AddRemoteCandidateCache::open(&cache_path(&dir)).expect("open");
    let hash = MerkleHash::from([4; 32]);
    cache
        .persist_results(&[(hash, None)])
        .expect("persist negative");
    assert_eq!(
        cache.load_persistent(&[hash]).expect("load").get(&hash),
        Some(&None)
    );

    cache
        .persist_results(&[(hash, Some(candidate(4)))])
        .expect("refresh positive");
    assert_eq!(
        cache.load_persistent(&[hash]).expect("load").get(&hash),
        Some(&Some(candidate(4)))
    );
}

#[test]
fn persistent_duplicate_results_keep_input_order() {
    let dir = tempdir().expect("tempdir");
    let cache = AddRemoteCandidateCache::open(&cache_path(&dir)).expect("open");
    let hash = MerkleHash::from([6; 32]);

    cache
        .persist_results(&[(hash, Some(candidate(6))), (hash, None)])
        .expect("persist negative last");
    assert_eq!(
        cache.load_persistent(&[hash]).expect("load").get(&hash),
        Some(&None)
    );

    cache
        .persist_results(&[(hash, None), (hash, Some(candidate(6)))])
        .expect("persist positive last");
    assert_eq!(
        cache.load_persistent(&[hash]).expect("load").get(&hash),
        Some(&Some(candidate(6)))
    );
}

#[test]
fn expired_negative_entries_are_not_reused() {
    let dir = tempdir().expect("tempdir");
    let cache = AddRemoteCandidateCache::open(&cache_path(&dir)).expect("open");
    let hash = MerkleHash::from([5; 32]);
    cache
        .persist_results(&[(hash, None)])
        .expect("persist negative");
    cache
        .connection
        .lock()
        .expect("database lock")
        .execute(
            "UPDATE remote_candidate_misses_v1 SET observed_at = 0 WHERE chunk_hash = ?1",
            params![<[u8; 32]>::from(hash).as_slice()],
        )
        .expect("age negative");

    assert!(
        !cache
            .load_persistent(&[hash])
            .expect("load")
            .contains_key(&hash)
    );
    let remaining: i64 = cache
        .connection
        .lock()
        .expect("database lock")
        .query_row(
            "SELECT COUNT(*) FROM remote_candidate_misses_v1 WHERE chunk_hash = ?1",
            params![<[u8; 32]>::from(hash).as_slice()],
            |row| row.get(0),
        )
        .expect("count expired");
    assert_eq!(remaining, 0);
}

#[test]
fn memory_cache_distinguishes_negative_entries() {
    let dir = tempdir().expect("tempdir");
    let cache = AddRemoteCandidateCache::open(&cache_path(&dir)).expect("open");
    let hash = MerkleHash::from([3; 32]);
    assert!(
        !cache
            .memory_get_batch(&[hash])
            .expect("lookup")
            .contains_key(&hash)
    );
    cache.memory_insert_batch(&[(hash, None)]).expect("insert");
    assert_eq!(
        cache.memory_get_batch(&[hash]).expect("lookup").get(&hash),
        Some(&None)
    );
}

#[test]
fn memory_batch_cache_preserves_positive_negative_and_misses() {
    let dir = tempdir().expect("tempdir");
    let cache = AddRemoteCandidateCache::open(&cache_path(&dir)).expect("open");
    let positive_hash = MerkleHash::from([8; 32]);
    let negative_hash = MerkleHash::from([9; 32]);
    let missing_hash = MerkleHash::from([10; 32]);
    cache
        .memory_insert_batch(&[(positive_hash, Some(candidate(8))), (negative_hash, None)])
        .expect("insert batch");

    let loaded = cache
        .memory_get_batch(&[positive_hash, negative_hash, missing_hash])
        .expect("lookup batch");
    assert_eq!(loaded.get(&positive_hash), Some(&Some(candidate(8))));
    assert_eq!(loaded.get(&negative_hash), Some(&None));
    assert!(!loaded.contains_key(&missing_hash));
}

#[test]
fn persistent_lookup_batches_large_requests() {
    let dir = tempdir().expect("tempdir");
    let cache = AddRemoteCandidateCache::open(&cache_path(&dir)).expect("open");
    let entries = (0..600u16)
        .map(|seed| {
            let mut bytes = [0; 32];
            bytes[..2].copy_from_slice(&seed.to_le_bytes());
            (MerkleHash::from(bytes), candidate((seed % 256) as u8))
        })
        .collect::<Vec<_>>();
    let entries = entries
        .into_iter()
        .map(|(hash, candidate)| (hash, Some(candidate)))
        .collect::<Vec<_>>();
    cache
        .persist_unique_results(&entries)
        .expect("persist unique");
    let hashes = entries.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
    assert_eq!(cache.load_persistent(&hashes).expect("load").len(), 600);
}

#[test]
fn persistent_negative_writes_batch_large_requests() {
    let dir = tempdir().expect("tempdir");
    let cache = AddRemoteCandidateCache::open(&cache_path(&dir)).expect("open");
    let entries = (0..600u16)
        .map(|seed| {
            let mut bytes = [0; 32];
            bytes[..2].copy_from_slice(&seed.to_le_bytes());
            (MerkleHash::from(bytes), None)
        })
        .collect::<Vec<_>>();
    cache
        .persist_unique_results(&entries)
        .expect("persist unique negatives");
    let hashes = entries.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
    let loaded = cache.load_persistent(&hashes).expect("load negatives");
    assert_eq!(loaded.len(), 600);
    assert!(loaded.values().all(Option::is_none));
}
