use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{self, StreamExt, TryStreamExt};
use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;
use slatedb::config::{DbReaderOptions, ScanOptions};

use super::format::{
    METADATA_KEY, OBJECT_FAMILY, PACK_FAMILY, decode_metadata, decode_object_key,
    decode_object_location, decode_pack_key, decode_pack_record, object_key,
    validate_location_for_pack,
};
use super::{
    GitLocatorCoverage, GitObjectLocation, GitObjectLocator, GitPackInventoryEntry,
    GitPackLocatorBinding, GitPackLocatorRecord, git_object_locator_path,
};
use crate::error::{MetadataError, Result};

const DB_LABEL: &str = "git_locator_db";
const LOOKUP_CONCURRENCY: usize = 256;
// A scan must replace one full point-read wave and may inspect at most two
// rows per requested object before the exact-key path becomes cheaper.
const MIN_SCAN_LOOKUP_OBJECTS: usize = LOOKUP_CONCURRENCY;
const MAX_SCAN_AMPLIFICATION: usize = 2;
const SCAN_READ_AHEAD_BYTES: usize = 2 * 1024 * 1024;
const SCAN_FETCH_TASKS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LookupStrategy {
    Exact,
    Scan { row_limit: usize },
}

/// Result of validating one compact row against a pinned pack inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitObjectLookup {
    /// No usable current row exists for this snapshot.
    Miss,
    /// The row joins to an immutable pack in the pinned inventory.
    Hit(GitObjectLocator),
    /// The compact row or its referenced pack record is malformed.
    Corrupt,
}

/// Read-only session for exact compact Git locator queries.
pub struct GitObjectLocatorSession {
    reader: Option<Arc<slatedb::DbReader>>,
    coverage: Option<GitLocatorCoverage>,
    bindings: HashMap<u64, GitPackLocatorRecord>,
}

impl GitObjectLocatorSession {
    /// Open the compact locator, treating an absent database as an empty index.
    pub async fn open(store: Arc<dyn ObjectStore>, repo_prefix: &str) -> Result<Self> {
        Self::open_with_options(store, repo_prefix, locator_reader_options()).await
    }

    /// Open a locator whose SlateDB checkpoint cannot refresh before `minimum`.
    ///
    /// The caller must close the session before `minimum` elapses. This keeps
    /// coverage, pack bindings, and object rows on one immutable manifest. A
    /// published locator checkpoint is opened explicitly, so the operation
    /// does not create or refresh durable reader state. A legacy database with
    /// no checkpoint uses SlateDB's one-time compatibility checkpoint path.
    pub async fn open_for_operation(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
        minimum: Duration,
    ) -> Result<Self> {
        let manifest_poll_interval = minimum
            .checked_add(Duration::from_secs(1))
            .ok_or_else(|| MetadataError::Internal("locator operation duration overflow".into()))?;
        let checkpoint_lifetime = manifest_poll_interval.checked_mul(2).ok_or_else(|| {
            MetadataError::Internal("locator checkpoint duration overflow".into())
        })?;
        let options = DbReaderOptions {
            manifest_poll_interval,
            checkpoint_lifetime,
            ..locator_reader_options()
        };
        let path = git_object_locator_path(repo_prefix);
        let checkpoint = reader_checkpoint_id(Arc::clone(&store), &path).await?;
        Self::open_with_checkpoint(store, repo_prefix, options, checkpoint).await
    }

    async fn open_with_options(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
        options: DbReaderOptions,
    ) -> Result<Self> {
        Self::open_with_checkpoint(store, repo_prefix, options, None).await
    }

    async fn open_with_checkpoint(
        store: Arc<dyn ObjectStore>,
        repo_prefix: &str,
        options: DbReaderOptions,
        checkpoint: Option<slatedb::Checkpoint>,
    ) -> Result<Self> {
        let path = git_object_locator_path(repo_prefix);
        let mut builder = slatedb::DbReader::builder(ObjectPath::from(path.as_str()), store)
            .with_options(options);
        if let Some(checkpoint) = checkpoint {
            builder = builder.with_checkpoint_id(checkpoint.id);
        }
        let reader = match builder
            // Push sessions are short-lived and query each candidate once. The
            // default 640 MiB cache multiplies RSS across concurrent pushes.
            .with_db_cache_disabled()
            .build()
            .await
        {
            Ok(reader) => Arc::new(reader),
            Err(error) if is_manifest_missing(&error) => {
                return Ok(Self {
                    reader: None,
                    coverage: None,
                    bindings: HashMap::new(),
                });
            }
            Err(source) => {
                return Err(MetadataError::SlateDbOpen {
                    db: DB_LABEL.to_owned(),
                    path,
                    source,
                });
            }
        };

        match load_state(&reader).await {
            Ok((coverage, bindings)) => Ok(Self {
                reader: Some(reader),
                coverage,
                bindings,
            }),
            Err(operation) => close_after_error(reader, operation).await,
        }
    }

    /// Whether the compact locator database exists and passed format validation.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.reader.is_some()
    }

    /// Return the latest fully published manifest inventory, if any.
    #[must_use]
    pub fn coverage(&self) -> Option<GitLocatorCoverage> {
        self.coverage
    }

    /// Resolve OID keys and validate every hit against pinned inventory.
    pub async fn lookup_batch(
        &self,
        object_ids: &[[u8; 20]],
        inventory: &HashMap<crab_xet::hash::MerkleHash, GitPackInventoryEntry>,
    ) -> Result<Vec<GitObjectLookup>> {
        let Some(reader) = &self.reader else {
            return Ok(vec![GitObjectLookup::Miss; object_ids.len()]);
        };
        let inventory_objects = inventory
            .values()
            .fold(0_u64, |total, pack| total.saturating_add(pack.object_count));
        let mut unique_objects = object_ids.len();
        let mut strategy = lookup_strategy(unique_objects, inventory_objects);
        if matches!(strategy, LookupStrategy::Scan { .. }) {
            unique_objects = object_ids.iter().collect::<HashSet<_>>().len();
            strategy = lookup_strategy(unique_objects, inventory_objects);
        }
        if let LookupStrategy::Scan { row_limit } = strategy {
            tracing::debug!(
                locator_lookup_mode = "scan",
                requested_objects = object_ids.len(),
                unique_objects,
                inventory_objects,
                row_limit,
                "compact Git locator lookup selected"
            );
            if let Some(lookups) = self
                .lookup_batch_by_scan(reader, object_ids, inventory, row_limit)
                .await?
            {
                return Ok(lookups);
            }
            tracing::debug!(
                locator_lookup_mode = "exact_fallback",
                requested_objects = object_ids.len(),
                unique_objects,
                inventory_objects,
                row_limit,
                "compact Git locator scan exceeded its amplification bound"
            );
        }

        self.lookup_batch_exact(reader, object_ids, inventory).await
    }

    async fn lookup_batch_exact(
        &self,
        reader: &Arc<slatedb::DbReader>,
        object_ids: &[[u8; 20]],
        inventory: &HashMap<crab_xet::hash::MerkleHash, GitPackInventoryEntry>,
    ) -> Result<Vec<GitObjectLookup>> {
        let bindings = &self.bindings;
        let fetched: Vec<(usize, GitObjectLookup)> =
            stream::iter(object_ids.iter().copied().enumerate().map(|(index, oid)| {
                let reader = Arc::clone(reader);
                async move {
                    let value = reader.get(object_key(&oid)).await.map_err(read_error)?;
                    let lookup = value.map_or(GitObjectLookup::Miss, |value| {
                        classify_location(&value, bindings, inventory)
                    });
                    Ok::<_, MetadataError>((index, lookup))
                }
            }))
            .buffer_unordered(LOOKUP_CONCURRENCY.min(object_ids.len()).max(1))
            .try_collect()
            .await?;

        let mut lookups = vec![GitObjectLookup::Miss; object_ids.len()];
        for (index, lookup) in fetched {
            lookups[index] = lookup;
        }
        Ok(lookups)
    }

    async fn lookup_batch_by_scan(
        &self,
        reader: &slatedb::DbReader,
        object_ids: &[[u8; 20]],
        inventory: &HashMap<crab_xet::hash::MerkleHash, GitPackInventoryEntry>,
        row_limit: usize,
    ) -> Result<Option<Vec<GitObjectLookup>>> {
        let mut requested = object_ids
            .iter()
            .copied()
            .enumerate()
            .map(|(index, oid)| (oid, index))
            .collect::<Vec<_>>();
        requested.sort_unstable_by_key(|(oid, _)| *oid);
        let Some((first_oid, _)) = requested.first() else {
            return Ok(Some(Vec::new()));
        };
        let last_oid = requested
            .last()
            .map(|(oid, _)| oid)
            .ok_or_else(|| MetadataError::Internal("locator scan lost its request".to_owned()))?;
        let options = ScanOptions::default()
            .with_read_ahead_bytes(SCAN_READ_AHEAD_BYTES)
            .with_max_fetch_tasks(SCAN_FETCH_TASKS);
        let mut rows = reader
            .scan_prefix_with_options(
                [OBJECT_FAMILY],
                first_oid.as_slice()..=last_oid.as_slice(),
                &options,
            )
            .await
            .map_err(read_error)?;
        let mut lookups = vec![GitObjectLookup::Miss; object_ids.len()];
        let mut request_index = 0_usize;
        let mut rows_scanned = 0_usize;
        while let Some(row) = rows.next().await.map_err(read_error)? {
            rows_scanned = rows_scanned.saturating_add(1);
            if rows_scanned > row_limit {
                return Ok(None);
            }
            let oid = decode_object_key(&row.key)
                .ok_or_else(|| corrupt("object", "invalid compact locator object key"))?;
            while requested
                .get(request_index)
                .is_some_and(|(requested_oid, _)| *requested_oid < oid)
            {
                request_index += 1;
            }
            while requested
                .get(request_index)
                .is_some_and(|(requested_oid, _)| *requested_oid == oid)
            {
                let (_, output_index) = requested[request_index];
                lookups[output_index] = classify_location(&row.value, &self.bindings, inventory);
                request_index += 1;
            }
            if request_index == requested.len() {
                break;
            }
        }
        tracing::debug!(
            locator_lookup_mode = "scan",
            requested_objects = object_ids.len(),
            rows_scanned,
            "compact Git locator lookup completed"
        );
        Ok(Some(lookups))
    }

    /// Return every validated slot binding in numeric slot order.
    pub async fn pack_bindings(&self) -> Result<Vec<GitPackLocatorBinding>> {
        let mut bindings: Vec<_> = self
            .bindings
            .iter()
            .map(|(pack_slot, record)| GitPackLocatorBinding {
                pack_slot: *pack_slot,
                record: *record,
            })
            .collect();
        bindings.sort_unstable_by_key(|binding| binding.pack_slot);
        Ok(bindings)
    }

    /// Close the underlying SlateDB reader.
    pub async fn close(self) -> Result<()> {
        let Some(reader) = self.reader else {
            return Ok(());
        };
        reader
            .close()
            .await
            .map_err(|source| MetadataError::SlateDbClose {
                db: DB_LABEL.to_owned(),
                source,
            })
    }
}

fn lookup_strategy(requested_objects: usize, inventory_objects: u64) -> LookupStrategy {
    let requested = u64::try_from(requested_objects).unwrap_or(u64::MAX);
    if requested_objects < MIN_SCAN_LOOKUP_OBJECTS
        || inventory_objects == 0
        || requested.saturating_mul(MAX_SCAN_AMPLIFICATION as u64) < inventory_objects
    {
        return LookupStrategy::Exact;
    }
    LookupStrategy::Scan {
        row_limit: requested_objects.saturating_mul(MAX_SCAN_AMPLIFICATION),
    }
}

async fn reader_checkpoint_id(
    store: Arc<dyn ObjectStore>,
    path: &str,
) -> Result<Option<slatedb::Checkpoint>> {
    let admin = slatedb::admin::AdminBuilder::new(ObjectPath::from(path), store).build();
    let named = match admin
        .list_checkpoints(Some(super::READER_CHECKPOINT_NAME))
        .await
    {
        Ok(checkpoints) => checkpoints,
        Err(error) if is_manifest_missing(&error) => return Ok(None),
        Err(source) => {
            return Err(MetadataError::SlateDbOpen {
                db: DB_LABEL.to_owned(),
                path: path.to_owned(),
                source,
            });
        }
    };
    if let Some(checkpoint) = named
        .into_iter()
        .max_by_key(|checkpoint| checkpoint.manifest_id)
    {
        return Ok(Some(checkpoint));
    }

    let checkpoints = match admin.list_checkpoints(None).await {
        Ok(checkpoints) => checkpoints,
        Err(error) if is_manifest_missing(&error) => return Ok(None),
        Err(source) => {
            return Err(MetadataError::SlateDbOpen {
                db: DB_LABEL.to_owned(),
                path: path.to_owned(),
                source,
            });
        }
    };
    Ok(checkpoints
        .into_iter()
        .max_by_key(|checkpoint| checkpoint.manifest_id))
}

fn locator_reader_options() -> DbReaderOptions {
    DbReaderOptions {
        // The locator writer disables WAL and flushes every published batch.
        // Replaying WALs can only add open latency; no locator rows live there.
        skip_wal_replay: true,
        ..DbReaderOptions::default()
    }
}

fn classify_location(
    bytes: &[u8],
    bindings: &HashMap<u64, GitPackLocatorRecord>,
    inventory: &HashMap<crab_xet::hash::MerkleHash, GitPackInventoryEntry>,
) -> GitObjectLookup {
    let Some(stored) = decode_object_location(bytes) else {
        return GitObjectLookup::Corrupt;
    };
    let Some(pack) = bindings.get(&stored.pack_slot) else {
        return GitObjectLookup::Corrupt;
    };
    let Some(canonical) = inventory.get(&pack.pack_id) else {
        return GitObjectLookup::Miss;
    };
    if canonical.pack_id != pack.pack_id
        || canonical.object_count != pack.object_count
        || canonical.pack_size != pack.pack_size
    {
        return GitObjectLookup::Miss;
    }
    let location = GitObjectLocation {
        pack_offset: stored.pack_offset,
        entry_len: stored.entry_len,
        crc32: stored.crc32,
    };
    if !validate_location_for_pack(location, canonical.pack_size) {
        return GitObjectLookup::Corrupt;
    }
    GitObjectLookup::Hit(GitObjectLocator {
        pack_id: pack.pack_id,
        location,
    })
}

async fn load_state(
    reader: &slatedb::DbReader,
) -> Result<(
    Option<GitLocatorCoverage>,
    HashMap<u64, GitPackLocatorRecord>,
)> {
    let value = reader
        .get(METADATA_KEY)
        .await
        .map_err(read_error)?
        .ok_or_else(|| corrupt("metadata", "compact locator metadata is missing"))?;
    let metadata = decode_metadata(&value)
        .ok_or_else(|| corrupt("metadata", "invalid compact locator metadata"))?;

    let mut rows = reader
        .scan_prefix([PACK_FAMILY], ..)
        .await
        .map_err(read_error)?;
    let mut bindings = HashMap::new();
    let mut pack_ids = std::collections::HashSet::new();
    while let Some(row) = rows.next().await.map_err(read_error)? {
        let slot = decode_pack_key(&row.key)
            .ok_or_else(|| corrupt("pack", "invalid compact locator pack key"))?;
        let record = decode_pack_record(&row.value)
            .ok_or_else(|| corrupt("pack", "invalid compact locator pack record"))?;
        if slot >= metadata.next_pack_slot
            || bindings.insert(slot, record).is_some()
            || !pack_ids.insert(record.pack_id)
        {
            return Err(corrupt(
                "pack",
                "pack slot is unallocated or duplicates an existing binding",
            ));
        }
    }
    Ok((metadata.coverage, bindings))
}

fn is_manifest_missing(error: &slatedb::Error) -> bool {
    matches!(error.kind(), slatedb::ErrorKind::Data)
        && error
            .to_string()
            .contains("failed to find latest transactional object")
}

fn read_error(source: slatedb::Error) -> MetadataError {
    MetadataError::SlateDbRead {
        db: DB_LABEL.to_owned(),
        source,
    }
}

fn corrupt(path: &str, reason: &str) -> MetadataError {
    MetadataError::CorruptObject {
        path: format!("{DB_LABEL}:{path}"),
        reason: reason.to_owned(),
    }
}

async fn close_after_error<T>(
    reader: Arc<slatedb::DbReader>,
    operation: MetadataError,
) -> Result<T> {
    match reader.close().await {
        Ok(()) => Err(operation),
        Err(close) => Err(MetadataError::SlateDbOperationAndClose {
            db: DB_LABEL.to_owned(),
            operation: Box::new(operation),
            close,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use object_store::memory::InMemory;
    use object_store::{ObjectStore, ObjectStoreExt};

    use super::*;
    use crate::git_object_locator::{
        GitObjectLocatorEntry, GitObjectLocatorWriter, GitPackLocatorRecord,
    };
    use crab_xet::hash::MerkleHash;

    struct Fixture {
        oid: [u8; 20],
        pack: GitPackLocatorRecord,
        inventory: GitPackInventoryEntry,
    }

    fn hash(seed: u64) -> MerkleHash {
        MerkleHash::from([seed, seed + 1, seed + 2, seed + 3])
    }

    fn pack(seed: u64) -> GitPackLocatorRecord {
        GitPackLocatorRecord {
            pack_id: hash(seed),
            committed_generation: seed,
            pack_index_hash: hash(seed + 10),
            object_count: 1,
            pack_size: 128,
        }
    }

    fn oid(seed: u32) -> [u8; 20] {
        let mut oid = [0_u8; 20];
        oid[16..].copy_from_slice(&seed.to_be_bytes());
        oid
    }

    #[test]
    fn locator_reader_skips_wal_replay() {
        assert!(locator_reader_options().skip_wal_replay);
    }

    #[test]
    fn dense_lookup_requires_a_full_exact_wave_and_bounded_scan_amplification() {
        assert_eq!(
            lookup_strategy(MIN_SCAN_LOOKUP_OBJECTS, MIN_SCAN_LOOKUP_OBJECTS as u64),
            LookupStrategy::Scan {
                row_limit: MIN_SCAN_LOOKUP_OBJECTS * 2,
            }
        );
        assert_eq!(
            lookup_strategy(
                MIN_SCAN_LOOKUP_OBJECTS - 1,
                (MIN_SCAN_LOOKUP_OBJECTS - 1) as u64
            ),
            LookupStrategy::Exact
        );
        assert_eq!(
            lookup_strategy(
                MIN_SCAN_LOOKUP_OBJECTS,
                (MIN_SCAN_LOOKUP_OBJECTS * 2 + 1) as u64
            ),
            LookupStrategy::Exact
        );
    }

    async fn publish(
        store: Arc<dyn ObjectStore>,
        pack: GitPackLocatorRecord,
        oid: [u8; 20],
        coverage: Option<GitLocatorCoverage>,
    ) -> Fixture {
        let mut writer = GitObjectLocatorWriter::open(store, "org/repo")
            .await
            .expect("open writer");
        let binding = writer.bind_packs(&[pack]).await.expect("bind pack")[0];
        writer
            .write_locations(
                binding,
                &[GitObjectLocatorEntry {
                    oid,
                    location: GitObjectLocation {
                        pack_offset: 12,
                        entry_len: 96,
                        crc32: 7,
                    },
                }],
            )
            .await
            .expect("write object");
        writer.flush_objects().await.expect("flush object");
        if let Some(coverage) = coverage {
            writer
                .set_coverage(coverage)
                .await
                .expect("publish coverage");
        }
        writer.close().await.expect("close writer");
        Fixture {
            oid,
            pack,
            inventory: GitPackInventoryEntry {
                pack_id: pack.pack_id,
                object_count: pack.object_count,
                pack_size: pack.pack_size,
            },
        }
    }

    async fn publish_many(
        store: Arc<dyn ObjectStore>,
        object_count: usize,
    ) -> (Vec<[u8; 20]>, HashMap<MerkleHash, GitPackInventoryEntry>) {
        let mut pack = pack(1);
        pack.object_count = object_count as u64;
        let object_ids = (0..object_count)
            .map(|seed| oid(seed as u32))
            .collect::<Vec<_>>();
        let entries = object_ids
            .iter()
            .copied()
            .map(|oid| GitObjectLocatorEntry {
                oid,
                location: GitObjectLocation {
                    pack_offset: 12,
                    entry_len: 96,
                    crc32: 7,
                },
            })
            .collect::<Vec<_>>();
        let mut writer = GitObjectLocatorWriter::open(store, "org/repo")
            .await
            .expect("open writer");
        let binding = writer.bind_packs(&[pack]).await.expect("bind pack")[0];
        writer
            .write_locations(binding, &entries)
            .await
            .expect("write objects");
        writer.flush_objects().await.expect("flush objects");
        writer.close().await.expect("close writer");
        let inventory = GitPackInventoryEntry {
            pack_id: pack.pack_id,
            object_count: pack.object_count,
            pack_size: pack.pack_size,
        };
        (object_ids, HashMap::from([(pack.pack_id, inventory)]))
    }

    #[tokio::test]
    async fn exact_get_joins_pack_slot_and_requires_pinned_inventory() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let fixture = publish(Arc::clone(&store), pack(1), [3; 20], None).await;
        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open reader");
        let hit_inventory = HashMap::from([(fixture.pack.pack_id, fixture.inventory)]);
        assert!(matches!(
            session
                .lookup_batch(&[fixture.oid], &hit_inventory)
                .await
                .expect("hit")
                .as_slice(),
            [GitObjectLookup::Hit(_)]
        ));
        assert_eq!(
            session
                .lookup_batch(&[fixture.oid], &HashMap::new())
                .await
                .expect("miss"),
            vec![GitObjectLookup::Miss]
        );
        session.close().await.expect("close reader");
    }

    #[tokio::test]
    async fn operation_reader_does_not_write_a_checkpoint() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        publish(Arc::clone(&store), pack(1), [31; 20], None).await;
        let prefix = ObjectPath::from("org/repo/git_locator_db");
        let before = store
            .list(Some(&prefix))
            .try_collect::<Vec<_>>()
            .await
            .expect("list before reader");

        let session = GitObjectLocatorSession::open_for_operation(
            Arc::clone(&store),
            "org/repo",
            Duration::from_secs(1),
        )
        .await
        .expect("open operation reader");
        session.close().await.expect("close operation reader");

        let after = store
            .list(Some(&prefix))
            .try_collect::<Vec<_>>()
            .await
            .expect("list after reader");
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn newer_current_row_is_a_miss_for_an_old_snapshot() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let old = publish(Arc::clone(&store), pack(1), [4; 20], None).await;
        publish(Arc::clone(&store), pack(2), old.oid, None).await;

        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open reader");
        let old_inventory = HashMap::from([(old.pack.pack_id, old.inventory)]);
        assert_eq!(
            session
                .lookup_batch(&[old.oid], &old_inventory)
                .await
                .expect("old snapshot lookup"),
            vec![GitObjectLookup::Miss]
        );
        session.close().await.expect("close reader");
    }

    #[tokio::test]
    async fn batch_lookup_preserves_request_order_and_reports_missing_ids() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let fixture = publish(Arc::clone(&store), pack(1), [5; 20], None).await;
        let inventory = HashMap::from([(fixture.pack.pack_id, fixture.inventory)]);
        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open reader");

        let lookups = session
            .lookup_batch(&[[9; 20], fixture.oid, [8; 20]], &inventory)
            .await
            .expect("batch lookup");
        assert!(matches!(
            lookups.as_slice(),
            [
                GitObjectLookup::Miss,
                GitObjectLookup::Hit(_),
                GitObjectLookup::Miss
            ]
        ));
        session.close().await.expect("close reader");
    }

    #[tokio::test]
    async fn dense_scan_preserves_request_order_and_reports_missing_ids() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let (mut object_ids, inventory) =
            publish_many(Arc::clone(&store), MIN_SCAN_LOOKUP_OBJECTS).await;
        object_ids.reverse();
        let missing_index = MIN_SCAN_LOOKUP_OBJECTS / 2;
        object_ids[missing_index] = [0xff; 20];
        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open reader");

        let lookups = session
            .lookup_batch(&object_ids, &inventory)
            .await
            .expect("dense lookup");
        assert_eq!(lookups.len(), object_ids.len());
        assert_eq!(lookups[missing_index], GitObjectLookup::Miss);
        assert!(lookups.iter().enumerate().all(|(index, lookup)| {
            index == missing_index || matches!(lookup, GitObjectLookup::Hit(_))
        }));
        session.close().await.expect("close reader");
    }

    #[tokio::test]
    async fn dense_scan_abandons_work_beyond_its_row_limit() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let (object_ids, inventory) = publish_many(Arc::clone(&store), 3).await;
        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open reader");
        let reader = session.reader.as_ref().expect("reader exists");

        assert_eq!(
            session
                .lookup_batch_by_scan(reader, &[object_ids[0], object_ids[2]], &inventory, 1)
                .await
                .expect("bounded scan"),
            None
        );
        session.close().await.expect("close reader");
    }

    #[tokio::test]
    async fn missing_new_database_ignores_any_old_prefix_and_closes() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        store
            .put(
                &ObjectPath::from("org/repo/git_object_locator_db/legacy"),
                bytes::Bytes::from_static(b"legacy").into(),
            )
            .await
            .expect("write old prefix");

        let session = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open missing compact reader");
        assert!(!session.is_available());
        assert_eq!(
            session
                .lookup_batch(&[[1; 20]], &HashMap::new())
                .await
                .expect("lookup unavailable"),
            vec![GitObjectLookup::Miss]
        );
        session.close().await.expect("close unavailable reader");
    }

    #[tokio::test]
    async fn refreshing_reader_can_mix_open_state_with_concurrent_publication() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let old_pack = pack(1);
        let old = publish(
            Arc::clone(&store),
            old_pack,
            [6; 20],
            Some(GitLocatorCoverage {
                generation: old_pack.committed_generation,
                pack_index_hash: old_pack.pack_index_hash,
            }),
        )
        .await;
        let options = DbReaderOptions {
            manifest_poll_interval: Duration::from_millis(10),
            checkpoint_lifetime: Duration::from_secs(1),
            ..locator_reader_options()
        };
        let session =
            GitObjectLocatorSession::open_with_options(Arc::clone(&store), "org/repo", options)
                .await
                .expect("open old reader");
        let reader = Arc::clone(session.reader.as_ref().expect("reader exists"));
        assert_eq!(
            session.coverage(),
            Some(GitLocatorCoverage {
                generation: old.pack.committed_generation,
                pack_index_hash: old.pack.pack_index_hash,
            })
        );

        let new_pack = pack(2);
        let new = publish(
            Arc::clone(&store),
            new_pack,
            [7; 20],
            Some(GitLocatorCoverage {
                generation: new_pack.committed_generation,
                pack_index_hash: new_pack.pack_index_hash,
            }),
        )
        .await;
        let inventory = HashMap::from([
            (old.pack.pack_id, old.inventory),
            (new.pack.pack_id, new.inventory),
        ]);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let lookup = session
                    .lookup_batch(&[new.oid], &inventory)
                    .await
                    .expect("poll refreshed reader");
                if lookup == [GitObjectLookup::Corrupt] {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("reader refreshes");

        assert_eq!(
            session.coverage(),
            Some(GitLocatorCoverage {
                generation: old.pack.committed_generation,
                pack_index_hash: old.pack.pack_index_hash,
            })
        );
        session.close().await.expect("close old reader");
        let error = reader
            .get(object_key(&old.oid))
            .await
            .expect_err("closed reader rejects reads");
        assert_eq!(
            error.kind(),
            slatedb::ErrorKind::Closed(slatedb::CloseReason::Clean)
        );

        let refreshed = GitObjectLocatorSession::open(store, "org/repo")
            .await
            .expect("open refreshed reader");
        assert_eq!(
            refreshed.coverage(),
            Some(GitLocatorCoverage {
                generation: new.pack.committed_generation,
                pack_index_hash: new.pack.pack_index_hash,
            })
        );
        assert!(matches!(
            refreshed
                .lookup_batch(&[new.oid], &inventory)
                .await
                .expect("lookup new object")
                .as_slice(),
            [GitObjectLookup::Hit(_)]
        ));
        refreshed.close().await.expect("close refreshed reader");
    }

    #[tokio::test]
    async fn operation_reader_does_not_refresh_before_its_bound() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let old_pack = pack(1);
        let old = publish(
            Arc::clone(&store),
            old_pack,
            [8; 20],
            Some(GitLocatorCoverage {
                generation: old_pack.committed_generation,
                pack_index_hash: old_pack.pack_index_hash,
            }),
        )
        .await;
        let session = GitObjectLocatorSession::open_for_operation(
            Arc::clone(&store),
            "org/repo",
            Duration::from_secs(1),
        )
        .await
        .expect("open operation reader");

        let new_pack = pack(2);
        let new = publish(
            Arc::clone(&store),
            new_pack,
            [9; 20],
            Some(GitLocatorCoverage {
                generation: new_pack.committed_generation,
                pack_index_hash: new_pack.pack_index_hash,
            }),
        )
        .await;
        let inventory = HashMap::from([
            (old.pack.pack_id, old.inventory),
            (new.pack.pack_id, new.inventory),
        ]);
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(matches!(
            session
                .lookup_batch(&[old.oid], &inventory)
                .await
                .expect("lookup pinned object")
                .as_slice(),
            [GitObjectLookup::Hit(_)]
        ));
        assert_eq!(
            session
                .lookup_batch(&[new.oid], &inventory)
                .await
                .expect("lookup post-open object"),
            [GitObjectLookup::Miss]
        );
        assert_eq!(
            session.coverage(),
            Some(GitLocatorCoverage {
                generation: old.pack.committed_generation,
                pack_index_hash: old.pack.pack_index_hash,
            })
        );
        session.close().await.expect("close operation reader");
    }
}
