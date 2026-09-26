//! Offline retention of immutable Cell objects.
use std::path::Path as FilePath;
use std::sync::{Arc, Mutex};

use crab_ltx::CellStorageLayout;
use crab_storage::StorageError;
use futures_util::StreamExt as _;
use object_store::{ObjectMeta, path::Path};
use rusqlite::{Connection, OptionalExtension, params};

use crate::cell::application::ApplicationIdentity;
use crate::cell::catalog::CellCatalog;
use crate::control::authority::CellAuthority;
use crate::control::{Control, ControlState};
use crate::identity::RequestId;
use crate::identity::decode_hex;
use crate::ltx::{Host as ReplicaHost, Limits as ReplicaLimits};
use crate::recovery::backup::BackupPinStore;
use crate::recovery::release::{ReleaseRecord, ReleaseState, ReleaseStore};
use crate::{Error, Result};

const CANDIDATE_BATCH: usize = 256;
const MAX_DELETES_PER_RUN: u64 = 100_000;

/// Explicit bounds for one offline immutable-object collection pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GarbageCollectionPolicy {
    now_ms: i64,
    grace_ms: u64,
    max_deletes: u64,
}

impl GarbageCollectionPolicy {
    /// Creates a bounded collection policy: logical time, grace window, and a
    /// per-run deletion limit.
    pub fn new(now_ms: i64, grace_ms: u64, max_deletes: u64) -> Result<Self> {
        if now_ms < 0 || grace_ms == 0 || !(1..=MAX_DELETES_PER_RUN).contains(&max_deletes) {
            return Err(Error::Retention(
                "time, grace, or deletion limit is invalid",
            ));
        }
        Ok(Self {
            now_ms,
            grace_ms,
            max_deletes,
        })
    }

    fn cutoff_ms(self) -> i64 {
        self.now_ms
            .saturating_sub(i64::try_from(self.grace_ms).unwrap_or(i64::MAX))
    }
}

/// Auditable outcome of one complete mark scan and bounded sweep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GarbageCollectionReport {
    listed_objects: u64,
    immutable_candidates: u64,
    reachable_objects: u64,
    retained_objects: u64,
    grace_objects: u64,
    eligible_objects: u64,
    deleted_objects: u64,
    current_controls: u64,
    retained_pins: u64,
}

impl GarbageCollectionReport {
    /// Returns the objects listed in the pass.
    #[must_use]
    pub const fn listed_objects(&self) -> u64 {
        self.listed_objects
    }

    /// Returns the listed objects that are immutable candidates.
    #[must_use]
    pub const fn immutable_candidates(&self) -> u64 {
        self.immutable_candidates
    }

    /// Returns the candidates still reachable from a control record or pin.
    #[must_use]
    pub const fn reachable_objects(&self) -> u64 {
        self.reachable_objects
    }

    /// Returns the candidates a backup pin retains.
    #[must_use]
    pub const fn retained_objects(&self) -> u64 {
        self.retained_objects
    }

    /// Returns the candidates inside the grace window.
    #[must_use]
    pub const fn grace_objects(&self) -> u64 {
        self.grace_objects
    }

    /// Returns the candidates eligible for deletion.
    #[must_use]
    pub const fn eligible_objects(&self) -> u64 {
        self.eligible_objects
    }

    /// Returns the objects deleted in this pass.
    #[must_use]
    pub const fn deleted_objects(&self) -> u64 {
        self.deleted_objects
    }

    /// Returns the control records observed as current.
    #[must_use]
    pub const fn current_controls(&self) -> u64 {
        self.current_controls
    }

    /// Returns the backup pins that retained objects.
    #[must_use]
    pub const fn retained_pins(&self) -> u64 {
        self.retained_pins
    }

    /// Reports whether every eligible object was deleted.
    #[must_use]
    pub const fn complete(&self) -> bool {
        self.eligible_objects == self.deleted_objects
    }
}

/// Reclaims immutable Cell objects only while an application is fenced offline.
///
/// The caller must own the exclusive maintenance advertisement associated with
/// `maintenance` until this future returns, and that drain must include every
/// read replica, in-flight snapshot query, and backup-pin publisher.
/// The collector independently checks the exact release
/// record and rejects every owned control before sweeping.
#[derive(Clone)]
pub struct CellGarbageCollector {
    layout: CellStorageLayout,
    identity: ApplicationIdentity,
    limits: ReplicaLimits,
    host: ReplicaHost,
}

impl CellGarbageCollector {
    /// Creates the collector after checking that the layout belongs to the same
    /// application as the identity.
    pub fn new(
        layout: CellStorageLayout,
        identity: ApplicationIdentity,
        limits: ReplicaLimits,
        host: ReplicaHost,
    ) -> Result<Self> {
        if layout.application_id() != identity.application().as_bytes() {
            return Err(Error::Retention("layout and application identity differ"));
        }
        Ok(Self {
            layout,
            identity,
            limits,
            host,
        })
    }

    /// Marks current controls and every retained pin, then deletes old unreachable objects.
    ///
    /// `scratch_dir` receives a private SQLite mark set and must be on bounded
    /// operator-owned local storage. A corrupt or missing reachable object fails
    /// before any deletion. The provider listing is streamed, so remote inventory
    /// size does not become resident memory.
    pub async fn collect(
        &self,
        maintenance: &ReleaseRecord,
        scratch_dir: &FilePath,
        policy: GarbageCollectionPolicy,
    ) -> Result<GarbageCollectionReport> {
        self.require_maintenance(maintenance).await?;
        let marks = MarkStore::open(scratch_dir).await?;
        let releases = ReleaseStore::new(self.layout.clone(), self.identity)?;
        let mut current_controls = 0u64;
        let mut retained_pins = 0u64;

        self.mark_release(&marks, maintenance, &releases).await?;
        self.mark_current_catalog(&marks, &mut current_controls)
            .await?;
        self.mark_pins(&marks, &mut retained_pins).await?;

        // Every graph has verified before this point. Recheck the global write
        // fence immediately before the first destructive operation.
        self.require_maintenance(maintenance).await?;
        let reachable_objects = marks.count().await?;
        let mut report = GarbageCollectionReport {
            listed_objects: 0,
            immutable_candidates: 0,
            reachable_objects,
            retained_objects: 0,
            grace_objects: 0,
            eligible_objects: 0,
            deleted_objects: 0,
            current_controls,
            retained_pins,
        };
        let prefix = self.layout.application_prefix();
        let mut stream = self.layout.store().list_stream(&prefix);
        let mut candidates = Vec::with_capacity(CANDIDATE_BATCH);
        while let Some(item) = stream.next().await {
            let meta = item?;
            report.listed_objects = checked_increment(report.listed_objects)?;
            if immutable_candidate(&prefix, &meta.location) {
                report.immutable_candidates = checked_increment(report.immutable_candidates)?;
                candidates.push(meta);
                if candidates.len() == CANDIDATE_BATCH {
                    self.sweep_batch(&marks, policy, &mut report, &mut candidates)
                        .await?;
                }
            }
        }
        self.sweep_batch(&marks, policy, &mut report, &mut candidates)
            .await?;
        self.require_maintenance(maintenance).await?;
        Ok(report)
    }

    async fn require_maintenance(&self, expected: &ReleaseRecord) -> Result<()> {
        if expected.state() != ReleaseState::Maintenance {
            return Err(Error::Retention("release is not fenced for maintenance"));
        }
        let current = ReleaseStore::new(self.layout.clone(), self.identity)?
            .load()
            .await?
            .ok_or(Error::Retention("maintenance release is absent"))?;
        if current.record() != expected {
            return Err(Error::Retention("maintenance release changed"));
        }
        Ok(())
    }

    async fn mark_release(
        &self,
        marks: &MarkStore,
        release: &ReleaseRecord,
        releases: &ReleaseStore,
    ) -> Result<()> {
        let mut digests = [release.current(), release.desired()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        digests.sort_unstable_by_key(|digest| *digest.as_bytes());
        digests.dedup();
        let mut paths = Vec::with_capacity(digests.len());
        for digest in digests {
            releases.descriptor(digest).await?;
            paths.push(self.layout.release_descriptor_path(digest.as_bytes()));
        }
        marks.insert(paths).await
    }

    async fn mark_current_catalog(
        &self,
        marks: &MarkStore,
        current_controls: &mut u64,
    ) -> Result<()> {
        let catalog = CellCatalog::new(self.layout.clone(), self.identity.tenant());
        let authority = CellAuthority::new(self.layout.clone());
        for shard in 0_u8..=u8::MAX {
            let mut scan = catalog.scan_shard(shard).await?;
            marks
                .insert(
                    scan.page_digests()
                        .iter()
                        .map(|digest| self.layout.catalog_object_path(digest.as_bytes()))
                        .collect(),
                )
                .await?;
            while let Some(page) = scan.next_page().await? {
                for proof in page.entries() {
                    let Some(observed) = authority.load(proof.entry().cell()).await? else {
                        continue;
                    };
                    *current_controls = checked_increment(*current_controls)?;
                    let control = observed.value();
                    if control.owner.is_some() {
                        return Err(Error::Retention(
                            "a current Cell still has an owner during maintenance",
                        ));
                    }
                    if control.state != ControlState::Tombstoned {
                        self.mark_control_root(marks, control).await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn mark_pins(&self, marks: &MarkStore, retained_pins: &mut u64) -> Result<()> {
        let pins = BackupPinStore::new(
            self.layout.clone(),
            self.identity,
            self.limits,
            self.host.clone(),
        )?;
        let prefix = self.layout.pin_prefix();
        let mut stream = self.layout.store().list_stream(&prefix);
        while let Some(item) = stream.next().await {
            let meta = item?;
            let Some(id) = pin_id(&prefix, &meta.location)? else {
                continue;
            };
            *retained_pins = checked_increment(*retained_pins)?;
            let pin = pins
                .load(id)
                .await?
                .ok_or(Error::Retention("retained pin disappeared during mark"))?;
            let manifest = pins.manifest(&pin).await?;
            let mut paths = manifest
                .release_descriptors
                .iter()
                .map(|digest| self.layout.release_descriptor_path(digest.as_bytes()))
                .collect::<Vec<_>>();
            for shard in &manifest.catalog {
                paths.extend(
                    shard
                        .pages
                        .iter()
                        .map(|digest| self.layout.catalog_object_path(digest.as_bytes())),
                );
            }
            paths.extend(
                manifest
                    .pin_objects
                    .iter()
                    .map(|digest| self.layout.pin_object_path(digest)),
            );
            marks.insert(paths).await?;
            for control in &manifest.controls {
                self.mark_control_root(marks, control).await?;
            }
        }
        Ok(())
    }

    async fn mark_control_root(&self, marks: &MarkStore, control: &Control) -> Result<()> {
        let Some(root) = control.ltx_root() else {
            return Ok(());
        };
        let objects = crab_ltx::CellReplica::new(
            self.layout.clone(),
            *control.cell.as_bytes(),
            *control.incarnation.as_bytes(),
            self.limits,
        )?
        .with_host(self.host.clone())
        .reachable_objects(&root)
        .await?;
        marks
            .insert(
                objects
                    .into_iter()
                    .map(|object| {
                        self.layout.incarnation_object_path(
                            control.cell.as_bytes(),
                            control.incarnation.as_bytes(),
                            &object.digest,
                            object.kind,
                        )
                    })
                    .collect(),
            )
            .await
    }

    async fn sweep_batch(
        &self,
        marks: &MarkStore,
        policy: GarbageCollectionPolicy,
        report: &mut GarbageCollectionReport,
        batch: &mut Vec<ObjectMeta>,
    ) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let paths = batch
            .iter()
            .map(|meta| meta.location.clone())
            .collect::<Vec<_>>();
        let retained = marks.contains(paths).await?;
        for (meta, retained) in batch.drain(..).zip(retained) {
            if retained {
                report.retained_objects = checked_increment(report.retained_objects)?;
                continue;
            }
            if meta.last_modified.timestamp_millis() >= policy.cutoff_ms() {
                report.grace_objects = checked_increment(report.grace_objects)?;
                continue;
            }
            report.eligible_objects = checked_increment(report.eligible_objects)?;
            if report.deleted_objects == policy.max_deletes {
                continue;
            }
            match self.layout.store().delete(&meta.location).await {
                Ok(()) | Err(StorageError::NotFound { .. }) => {
                    report.deleted_objects = checked_increment(report.deleted_objects)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

struct MarkStore {
    connection: Arc<Mutex<Connection>>,
    _path: tempfile::TempPath,
}

impl MarkStore {
    async fn open(directory: &FilePath) -> Result<Self> {
        let file = tempfile::Builder::new()
            .prefix("crab-cell-retention-")
            .suffix(".sqlite3")
            .tempfile_in(directory)
            .map_err(Error::RetentionIo)?;
        let path = file.into_temp_path();
        let open_path = path.to_path_buf();
        let connection = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let connection = Connection::open(open_path)?;
            connection.execute_batch(
                "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;\
                 CREATE TABLE marks(path TEXT PRIMARY KEY) WITHOUT ROWID;",
            )?;
            Ok(connection)
        })
        .await
        .map_err(Error::RetentionWorkerJoin)??;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            _path: path,
        })
    }

    async fn insert(&self, paths: Vec<Path>) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let paths = paths
            .into_iter()
            .map(|path| path.to_string())
            .collect::<Vec<_>>();
        let connection = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut connection = connection
                .lock()
                .map_err(|_| Error::Retention("retention scratch lock is poisoned"))?;
            let transaction = connection.transaction()?;
            {
                let mut insert = transaction.prepare_cached(
                    "INSERT INTO marks(path) VALUES(?1) ON CONFLICT(path) DO NOTHING",
                )?;
                for path in paths {
                    insert.execute(params![path])?;
                }
            }
            transaction.commit()?;
            Ok(())
        })
        .await
        .map_err(Error::RetentionWorkerJoin)??;
        Ok(())
    }

    async fn contains(&self, paths: Vec<Path>) -> Result<Vec<bool>> {
        let paths = paths
            .into_iter()
            .map(|path| path.to_string())
            .collect::<Vec<_>>();
        let connection = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || -> Result<Vec<bool>> {
            let connection = connection
                .lock()
                .map_err(|_| Error::Retention("retention scratch lock is poisoned"))?;
            let mut lookup = connection.prepare_cached("SELECT 1 FROM marks WHERE path = ?1")?;
            paths
                .into_iter()
                .map(|path| {
                    lookup
                        .query_row(params![path], |_| Ok(()))
                        .optional()
                        .map(|value| value.is_some())
                        .map_err(Error::from)
                })
                .collect()
        })
        .await
        .map_err(Error::RetentionWorkerJoin)?
    }

    async fn count(&self) -> Result<u64> {
        let connection = Arc::clone(&self.connection);
        tokio::task::spawn_blocking(move || -> Result<u64> {
            let connection = connection
                .lock()
                .map_err(|_| Error::Retention("retention scratch lock is poisoned"))?;
            let count = connection
                .query_row("SELECT COUNT(*) FROM marks", [], |row| row.get::<_, i64>(0))?;
            u64::try_from(count).map_err(|_| Error::Retention("reachable object count overflow"))
        })
        .await
        .map_err(Error::RetentionWorkerJoin)?
    }
}

fn immutable_candidate(application_prefix: &Path, location: &Path) -> bool {
    let Some(relative) = relative_path(application_prefix, location) else {
        return false;
    };
    let components = relative.split('/').collect::<Vec<_>>();
    match components.as_slice() {
        ["releases", object] | ["catalog", "objects", object] | ["pins", "objects", object] => {
            json_digest(object)
        }
        ["cells", cell, "inc", incarnation, "objects", object] => {
            lower_hex(cell, 64)
                && lower_hex(incarnation, 32)
                && object.rsplit_once('.').is_some_and(|(digest, extension)| {
                    lower_hex(digest, 64)
                        && matches!(extension, "ltx" | "index" | "dir" | "root" | "bundle")
                })
        }
        _ => false,
    }
}

fn pin_id(prefix: &Path, location: &Path) -> Result<Option<RequestId>> {
    let Some(relative) = relative_path(prefix, location) else {
        return Ok(None);
    };
    if relative.contains('/') {
        return Ok(None);
    }
    let Some(encoded) = relative.strip_suffix(".json") else {
        return Ok(None);
    };
    if !lower_hex(encoded, 32) {
        return Err(Error::Retention("pin pointer path is not canonical"));
    }
    Ok(Some(RequestId::from_bytes(decode_hex(encoded)?)))
}

fn relative_path<'a>(prefix: &Path, location: &'a Path) -> Option<&'a str> {
    location
        .as_ref()
        .strip_prefix(prefix.as_ref())?
        .strip_prefix('/')
}

fn json_digest(value: &str) -> bool {
    value
        .strip_suffix(".json")
        .is_some_and(|digest| lower_hex(digest, 64))
}

fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn checked_increment(value: u64) -> Result<u64> {
    value
        .checked_add(1)
        .ok_or(Error::Retention("retention object count overflow"))
}

#[cfg(test)]
mod tests;
