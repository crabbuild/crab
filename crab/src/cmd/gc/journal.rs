//! Durable object-store journal for destructive GC sweeps.
//!
//! The journal is deliberately append-only for candidate batches. A crash can
//! therefore restart at the last committed batch without rebuilding the root
//! snapshot or guessing which deletes were observed by the provider.

use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::TryStreamExt;
use object_store::path::Path;
use serde::{Deserialize, Serialize};
use tracing::info;
use uuid::Uuid;

use crate::cmd::gc::ObjectMeta;
use crate::core::error::{CrabError, Result};
use crate::storage::store::Store;

const SCHEMA_VERSION: u32 = 2;
pub const DEFAULT_BATCH_SIZE: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcRunState {
    pub schema_version: u32,
    pub run_id: String,
    pub scope: String,
    pub domain: String,
    pub created_at_unix_ms: i64,
    pub snapshot_at_unix_ms: i64,
    pub grace_secs: u64,
    pub force: bool,
    pub planned_batches: u64,
    pub next_batch: u64,
    /// Bucket runs set this after the generation-pinned file-index sweep has
    /// committed. Older journals default to false and are reconciled on the
    /// next resume before their object batches can complete.
    #[serde(default)]
    pub file_index_complete: bool,
    #[serde(default)]
    pub root_identity: String,
    #[serde(default)]
    pub fence_epoch: Option<u64>,
    pub phase: GcRunPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GcRunPhase {
    Planning,
    Deleting,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateBatch {
    schema_version: u32,
    run_id: String,
    batch: u64,
    objects: Vec<JournalObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchOutcome {
    schema_version: u32,
    run_id: String,
    batch: u64,
    deleted_keys: Vec<String>,
    bytes_reclaimed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalObject {
    key: String,
    size: u64,
    last_modified_unix_ms: i64,
    #[serde(default)]
    e_tag: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

pub struct GcRunJournal {
    store: Store,
    root: String,
    state: GcRunState,
    etag: crab_storage::ETag,
}

#[cfg(feature = "crash-injection")]
pub(crate) fn crash_at(point: &str) {
    if std::env::var("CRAB_GC_CRASH_AT").ok().as_deref() == Some(point) {
        std::process::exit(86);
    }
}

#[cfg(not(feature = "crash-injection"))]
pub(crate) fn crash_at(_point: &str) {}

impl GcRunJournal {
    /// Starts a new run under an immutable object-store run namespace.
    pub async fn start(
        store: Store,
        root: &str,
        scope: &str,
        domain: &str,
        snapshot_at: SystemTime,
        grace: Duration,
        force: bool,
    ) -> Result<Self> {
        let run_id = Uuid::now_v7().to_string();
        let root = format!("{root}/gc/runs/{run_id}");
        let now = unix_ms(SystemTime::now());
        let state = GcRunState {
            schema_version: SCHEMA_VERSION,
            run_id,
            scope: scope.to_owned(),
            domain: domain.to_owned(),
            created_at_unix_ms: now,
            snapshot_at_unix_ms: unix_ms(snapshot_at),
            grace_secs: grace.as_secs(),
            force,
            planned_batches: 0,
            next_batch: 0,
            file_index_complete: false,
            root_identity: String::new(),
            fence_epoch: None,
            phase: GcRunPhase::Planning,
        };
        let path = state_path(&root);
        store.create_strict(&path, encode(&state, &path)?).await?;
        let (_, etag) = store.get_with_etag(&path).await?;
        info!(run_id = %state.run_id, scope, domain, "started durable GC run");
        Ok(Self {
            store,
            root,
            state,
            etag,
        })
    }

    /// Opens an unfinished run after validating its immutable scope contract.
    pub async fn resume(
        store: Store,
        root: &str,
        run_id: &str,
        scope: &str,
        domain: &str,
    ) -> Result<Self> {
        validate_run_id(run_id)?;
        let root = format!("{root}/gc/runs/{run_id}");
        let path = state_path(&root);
        let (body, etag) = store.get_with_etag(&path).await?;
        let state: GcRunState = decode(&body, &path)?;
        validate_state(&state, run_id, scope, domain)?;
        if state.phase == GcRunPhase::Complete {
            return Err(CrabError::Configuration {
                key: "gc.resume".to_owned(),
                origin: format!("GC run {run_id} is already complete"),
            });
        }
        if state.root_identity.is_empty() && state.phase != GcRunPhase::Planning {
            return Err(CrabError::Configuration {
                key: "gc.resume.root_identity".to_owned(),
                origin: format!(
                    "GC run {run_id} has no sealed root identity for its destructive phase"
                ),
            });
        }
        Ok(Self {
            store,
            root,
            state,
            etag,
        })
    }

    #[must_use]
    pub fn state(&self) -> &GcRunState {
        &self.state
    }

    /// Returns a run-owned prefix for auxiliary durable mark sets.
    pub(crate) fn marks_prefix(&self) -> String {
        format!("{}/marks", self.root)
    }

    pub(crate) async fn planned_batch(&self, batch: u64) -> Result<Vec<ObjectMeta>> {
        if batch >= self.state.planned_batches {
            return Err(CrabError::Configuration {
                key: "gc.preview.batch".to_owned(),
                origin: format!("preview batch {batch} is outside the durable plan"),
            });
        }
        self.read_batch(batch)
            .await?
            .objects
            .into_iter()
            .map(ObjectMeta::try_from)
            .collect()
    }

    pub(crate) async fn discard_preview(self) -> Result<()> {
        if self.state.scope != "bucket-preview" || self.state.phase != GcRunPhase::Planning {
            return Err(CrabError::Configuration {
                key: "gc.preview".to_owned(),
                origin: "only a non-executable bucket preview plan may be discarded".to_owned(),
            });
        }
        self.retire_artifacts().await?;
        let path = state_path(&self.root);
        match self.store.delete(&path).await {
            Ok(()) | Err(CrabError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Returns the immutable snapshot time used to apply the grace policy.
    pub fn snapshot_at(&self) -> Result<SystemTime> {
        let millis = u64::try_from(self.state.snapshot_at_unix_ms).map_err(|_| {
            CrabError::CorruptObject {
                path: state_path(&self.root).to_string(),
                reason: "GC snapshot time is negative".to_owned(),
            }
        })?;
        UNIX_EPOCH
            .checked_add(Duration::from_millis(millis))
            .ok_or_else(|| CrabError::CorruptObject {
                path: state_path(&self.root).to_string(),
                reason: "GC snapshot time overflows".to_owned(),
            })
    }

    /// Verifies immutable policy fields before a process resumes a run.
    pub fn ensure_policy(&self, grace: Duration, force: bool) -> Result<()> {
        if self.state.force != force || self.state.grace_secs != grace.as_secs() {
            return Err(CrabError::Configuration {
                key: "gc.resume.policy".to_owned(),
                origin: "GC resume must use the run's original grace and force policy".to_owned(),
            });
        }
        Ok(())
    }

    /// Discards a non-sealed candidate plan without making it executable.
    ///
    /// Planning is deliberately restartable from the beginning: the caller
    /// must revalidate the root identity, then this method resets the durable
    /// batch cursor before the bounded source is replayed. No object delete
    /// can observe a run while it remains in the planning phase.
    pub async fn reset_partial_plan(&mut self) -> Result<()> {
        if self.state.phase != GcRunPhase::Planning {
            return Err(CrabError::Configuration {
                key: "gc.resume".to_owned(),
                origin: "only an interrupted GC plan can be reset".to_owned(),
            });
        }
        if self.state.next_batch != 0 {
            return Err(CrabError::CorruptObject {
                path: state_path(&self.root).to_string(),
                reason: "planning GC run has an executed batch cursor".to_owned(),
            });
        }
        let old_batches = self.state.planned_batches;
        for batch in 0..old_batches {
            match self.store.delete(&batch_path(&self.root, batch)).await {
                Ok(()) | Err(CrabError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        let marks_prefix = Path::from(format!("{}/marks/", self.root));
        let mut marks = self.store.inner().list(Some(&marks_prefix));
        while let Some(meta) = marks.try_next().await.map_err(CrabError::Storage)? {
            match self.store.delete(&meta.location).await {
                Ok(()) | Err(CrabError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        // Delete the old immutable batches before publishing the reset cursor.
        // If the process dies between these operations, the old Planning state
        // makes the next resume repeat this idempotent cleanup rather than
        // allowing a stale batch to collide with the replay.
        self.state.planned_batches = 0;
        self.persist_state().await?;
        Ok(())
    }

    /// Binds the sealed plan to the mark/root snapshot used to create it.
    pub async fn set_root_identity(&mut self, identity: &str) -> Result<()> {
        if self.state.phase != GcRunPhase::Planning {
            return Err(CrabError::Configuration {
                key: "gc.journal.root_identity".to_owned(),
                origin: "GC root identity must be recorded before plan sealing".to_owned(),
            });
        }
        if identity.trim().is_empty() {
            return Err(CrabError::Configuration {
                key: "gc.journal.root_identity".to_owned(),
                origin: "GC root identity must not be empty".to_owned(),
            });
        }
        if !self.state.root_identity.is_empty() && self.state.root_identity != identity {
            return Err(CrabError::CorruptObject {
                path: state_path(&self.root).to_string(),
                reason: "GC root identity changed while sealing the plan".to_owned(),
            });
        }
        self.state.root_identity = identity.to_owned();
        self.persist_state().await
    }

    /// Refuses execution when the current roots differ from the sealed plan.
    pub fn ensure_root_identity(&self, identity: &str) -> Result<()> {
        if self.state.root_identity.is_empty() || self.state.root_identity != identity {
            return Err(CrabError::Configuration {
                key: "gc.resume.root_identity".to_owned(),
                origin:
                    "GC roots changed or the run predates root identity sealing; start a new sweep"
                        .to_owned(),
            });
        }
        Ok(())
    }

    /// Pins the exclusive-fence epoch that sealed the current root snapshot.
    pub async fn seal_fence_epoch(&mut self, epoch: u64) -> Result<()> {
        if self.state.fence_epoch.is_some() {
            return Err(CrabError::Configuration {
                key: "gc.journal.fence_epoch".to_owned(),
                origin: "GC run already has a sealed fence epoch".to_owned(),
            });
        }
        self.state.fence_epoch = Some(epoch);
        self.persist_state().await
    }

    /// Refuses a delete batch when any writer crossed the fence since the
    /// root snapshot or previous committed batch.
    pub fn ensure_next_fence_epoch(&self, observed: u64) -> Result<()> {
        let expected = self
            .state
            .fence_epoch
            .ok_or_else(|| CrabError::Configuration {
                key: "gc.resume.fence_epoch".to_owned(),
                origin: "GC run has no valid sealed fence epoch".to_owned(),
            })?;
        if observed != expected {
            return Err(CrabError::Configuration {
                key: "gc.resume.fence_epoch".to_owned(),
                origin: "GC roots may have changed since planning; start a new sweep".to_owned(),
            });
        }
        Ok(())
    }

    /// Commits a successful bounded fenced phase that did not advance an
    /// object candidate batch.
    pub async fn advance_fence_epoch(&mut self, observed: u64) -> Result<()> {
        self.ensure_next_fence_epoch(observed)?;
        self.state.fence_epoch = Some(observed);
        self.persist_state().await
    }

    #[must_use]
    pub fn file_index_complete(&self) -> bool {
        self.state.file_index_complete
    }

    /// Records the file-index reconciliation only after MetaDb has committed
    /// every tombstone. The marker is idempotent so a retry after a lost
    /// response cannot skip reconciliation.
    pub async fn mark_file_index_complete(&mut self) -> Result<()> {
        if self.state.phase != GcRunPhase::Deleting {
            return Err(CrabError::Configuration {
                key: "gc.journal.file_index".to_owned(),
                origin: "file-index reconciliation can only be sealed during deletion".to_owned(),
            });
        }
        if !self.state.file_index_complete {
            self.state.file_index_complete = true;
            self.persist_state().await?;
        }
        Ok(())
    }

    /// Persists every candidate batch before the first destructive delete.
    pub async fn plan(&mut self, objects: &[ObjectMeta]) -> Result<()> {
        if self.state.phase != GcRunPhase::Planning {
            return Err(CrabError::Configuration {
                key: "gc.journal".to_owned(),
                origin: "candidate plan was already persisted".to_owned(),
            });
        }
        for chunk in objects.chunks(DEFAULT_BATCH_SIZE) {
            self.append_candidates(chunk).await?;
        }
        self.finish_plan().await
    }

    /// Appends one bounded candidate batch while the run is still planning.
    /// Callers can feed a LIST stream without retaining the full candidate
    /// namespace in memory. Planning remains non-destructive until
    /// [`Self::finish_plan`] persists the deleting phase.
    pub async fn append_candidates(&mut self, objects: &[ObjectMeta]) -> Result<()> {
        if self.state.phase != GcRunPhase::Planning {
            return Err(CrabError::Configuration {
                key: "gc.journal".to_owned(),
                origin: "candidate plan was already persisted".to_owned(),
            });
        }
        if objects.is_empty() {
            return Ok(());
        }
        if objects.len() > DEFAULT_BATCH_SIZE {
            return Err(CrabError::Configuration {
                key: "gc.journal.batch_size".to_owned(),
                origin: format!(
                    "candidate append contains {} objects; maximum is {DEFAULT_BATCH_SIZE}",
                    objects.len()
                ),
            });
        }
        for object in objects {
            validate_candidate_key(&self.state, &object.key)?;
        }
        let batch_number = self.state.planned_batches;
        let batch = CandidateBatch {
            schema_version: SCHEMA_VERSION,
            run_id: self.state.run_id.clone(),
            batch: batch_number,
            objects: objects.iter().map(JournalObject::from).collect(),
        };
        let path = batch_path(&self.root, batch_number);
        match self
            .store
            .create_strict(&path, encode(&batch, &path)?)
            .await
        {
            Ok(()) => {}
            Err(
                CrabError::CasConflict { .. }
                | CrabError::Storage(
                    object_store::Error::AlreadyExists { .. }
                    | object_store::Error::Precondition { .. },
                ),
            ) => {
                let (body, _) = self.store.get_with_etag(&path).await?;
                let existing: CandidateBatch = decode(&body, &path)?;
                if existing.schema_version != SCHEMA_VERSION
                    || existing.run_id != self.state.run_id
                    || existing.batch != batch_number
                    || existing.objects.len() != batch.objects.len()
                    || existing
                        .objects
                        .iter()
                        .zip(batch.objects.iter())
                        .any(|(left, right)| {
                            left.key != right.key
                                || left.size != right.size
                                || left.last_modified_unix_ms != right.last_modified_unix_ms
                                || left.e_tag != right.e_tag
                                || left.version != right.version
                        })
                {
                    return Err(CrabError::CorruptObject {
                        path: path.to_string(),
                        reason: "GC candidate batch identity mismatch".to_owned(),
                    });
                }
            }
            Err(error) => return Err(error),
        }
        self.state.planned_batches =
            self.state.planned_batches.checked_add(1).ok_or_else(|| {
                CrabError::Internal("GC candidate batch count overflow".to_owned())
            })?;
        self.persist_state().await
    }

    /// Seals planning and permits destructive batches to start.
    pub async fn finish_plan(&mut self) -> Result<()> {
        if self.state.phase != GcRunPhase::Planning {
            return Err(CrabError::Configuration {
                key: "gc.journal".to_owned(),
                origin: "candidate plan was already persisted".to_owned(),
            });
        }
        if self.state.root_identity.is_empty() {
            return Err(CrabError::Configuration {
                key: "gc.journal.root_identity".to_owned(),
                origin: "candidate plan cannot be sealed before its root identity".to_owned(),
            });
        }
        self.state.phase = GcRunPhase::Deleting;
        self.persist_state().await
    }

    /// Returns the next durable batch and its candidate metadata.
    pub async fn next_batch(&self) -> Result<Option<Vec<ObjectMeta>>> {
        if self.state.phase == GcRunPhase::Planning {
            return Err(CrabError::Configuration {
                key: "gc.journal".to_owned(),
                origin: "GC candidate plan is not sealed".to_owned(),
            });
        }
        if self.state.next_batch >= self.state.planned_batches {
            return Ok(None);
        }
        let batch = self.read_batch(self.state.next_batch).await?;
        Ok(Some(
            batch
                .objects
                .into_iter()
                .map(ObjectMeta::try_from)
                .collect::<Result<Vec<_>>>()?,
        ))
    }

    /// Records a fully successful batch before advancing the durable cursor.
    pub async fn complete_batch(
        &mut self,
        deleted_keys: &[String],
        bytes: u64,
        fence_epoch: Option<u64>,
    ) -> Result<()> {
        let batch = self.state.next_batch;
        let candidates = self.read_batch(batch).await?;
        let candidate_keys = candidates
            .objects
            .iter()
            .map(|object| object.key.as_str())
            .collect::<HashSet<_>>();
        let mut unique = HashSet::with_capacity(deleted_keys.len());
        let expected_bytes = deleted_keys.iter().try_fold(0u64, |total, key| {
            if !candidate_keys.contains(key.as_str()) || !unique.insert(key.as_str()) {
                return None;
            }
            candidates
                .objects
                .iter()
                .find(|object| object.key == *key)
                .and_then(|object| total.checked_add(object.size))
        });
        if expected_bytes != Some(bytes) {
            return Err(CrabError::CorruptObject {
                path: outcome_path(&self.root, batch).to_string(),
                reason: "GC batch outcome does not match its candidate batch".to_owned(),
            });
        }
        let outcome = BatchOutcome {
            schema_version: SCHEMA_VERSION,
            run_id: self.state.run_id.clone(),
            batch,
            deleted_keys: deleted_keys.to_vec(),
            bytes_reclaimed: bytes,
        };
        let path = outcome_path(&self.root, batch);
        match self
            .store
            .create_strict(&path, encode(&outcome, &path)?)
            .await
        {
            Ok(()) => {}
            Err(
                CrabError::CasConflict { .. }
                | CrabError::Storage(
                    object_store::Error::AlreadyExists { .. }
                    | object_store::Error::Precondition { .. },
                ),
            ) => {
                let (body, _) = self.store.get_with_etag(&path).await?;
                let existing: BatchOutcome = decode(&body, &path)?;
                validate_outcome(&existing, &self.state.run_id, batch)?;
                let mut existing_keys = existing.deleted_keys.clone();
                let mut requested_keys = deleted_keys.to_vec();
                existing_keys.sort_unstable();
                requested_keys.sort_unstable();
                if existing_keys != requested_keys || existing.bytes_reclaimed != bytes {
                    return Err(CrabError::CorruptObject {
                        path: path.to_string(),
                        reason: "GC batch outcome differs from the durable retry".to_owned(),
                    });
                }
            }
            Err(error) => return Err(error),
        }
        crash_at("after-journal-outcome");
        self.state.next_batch = self
            .state
            .next_batch
            .checked_add(1)
            .ok_or_else(|| CrabError::Internal("GC next batch cursor overflow".to_owned()))?;
        if let Some(fence_epoch) = fence_epoch {
            self.state.fence_epoch = Some(fence_epoch);
        }
        self.persist_state().await
    }

    /// Returns all durable outcomes for reconciliation after a crash.
    pub async fn deleted_keys(&self) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        for batch in 0..self.state.next_batch {
            let path = outcome_path(&self.root, batch);
            let (body, _) = self.store.get_with_etag(&path).await?;
            let outcome: BatchOutcome = decode(&body, &path)?;
            validate_outcome(&outcome, &self.state.run_id, batch)?;
            keys.extend(outcome.deleted_keys);
        }
        Ok(keys)
    }

    /// Counts durable successful deletions without materializing their keys.
    pub async fn deleted_key_count(&self) -> Result<u64> {
        let mut count = 0u64;
        for batch in 0..self.state.next_batch {
            let path = outcome_path(&self.root, batch);
            let (body, _) = self.store.get_with_etag(&path).await?;
            let outcome: BatchOutcome = decode(&body, &path)?;
            validate_outcome(&outcome, &self.state.run_id, batch)?;
            count = count
                .checked_add(u64::try_from(outcome.deleted_keys.len()).map_err(|_| {
                    CrabError::CorruptObject {
                        path: path.to_string(),
                        reason: "GC deleted-key count overflows".to_owned(),
                    }
                })?)
                .ok_or_else(|| CrabError::CorruptObject {
                    path: path.to_string(),
                    reason: "GC deleted-key count overflows".to_owned(),
                })?;
        }
        Ok(count)
    }

    /// Sums bytes from durable successful outcomes without replaying deletes.
    pub async fn deleted_bytes_reclaimed(&self) -> Result<u64> {
        let mut bytes = 0u64;
        for batch in 0..self.state.next_batch {
            let path = outcome_path(&self.root, batch);
            let (body, _) = self.store.get_with_etag(&path).await?;
            let outcome: BatchOutcome = decode(&body, &path)?;
            validate_outcome(&outcome, &self.state.run_id, batch)?;
            bytes = bytes.checked_add(outcome.bytes_reclaimed).ok_or_else(|| {
                CrabError::CorruptObject {
                    path: path.to_string(),
                    reason: "GC reclaimed-byte count overflows".to_owned(),
                }
            })?;
        }
        Ok(bytes)
    }

    /// Marks reconciliation complete only after all batch outcomes are durable.
    pub async fn complete(&mut self) -> Result<()> {
        if self.state.next_batch != self.state.planned_batches {
            return Err(CrabError::Configuration {
                key: "gc.journal".to_owned(),
                origin: "cannot complete GC before every candidate batch succeeds".to_owned(),
            });
        }
        // Retire first while the run is still resumable. A crash during this
        // idempotent cleanup leaves a Deleting run at the terminal cursor, so
        // the next invocation can finish cleanup instead of leaking artifacts.
        self.retire_artifacts().await?;
        self.state.phase = GcRunPhase::Complete;
        self.persist_state().await
    }

    async fn retire_artifacts(&self) -> Result<()> {
        for batch in 0..self.state.planned_batches {
            for path in [
                batch_path(&self.root, batch),
                outcome_path(&self.root, batch),
            ] {
                match self.store.delete(&path).await {
                    Ok(()) | Err(CrabError::NotFound { .. }) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        let marks_prefix = Path::from(format!("{}/marks/", self.root));
        let mut marks = self.store.inner().list(Some(&marks_prefix));
        while let Some(meta) = marks.try_next().await.map_err(CrabError::Storage)? {
            match self.store.delete(&meta.location).await {
                Ok(()) | Err(CrabError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    async fn read_batch(&self, batch: u64) -> Result<CandidateBatch> {
        let path = batch_path(&self.root, batch);
        let (body, _) = self.store.get_with_etag(&path).await?;
        let candidate: CandidateBatch = decode(&body, &path)?;
        if candidate.schema_version != SCHEMA_VERSION
            || candidate.run_id != self.state.run_id
            || candidate.batch != batch
        {
            return Err(CrabError::CorruptObject {
                path: path.to_string(),
                reason: "GC candidate batch identity mismatch".to_owned(),
            });
        }
        for object in &candidate.objects {
            validate_candidate_key(&self.state, &object.key)?;
        }
        Ok(candidate)
    }

    async fn persist_state(&mut self) -> Result<()> {
        let path = state_path(&self.root);
        self.etag = self
            .store
            .update(&path, encode(&self.state, &path)?, self.etag.clone())
            .await?;
        Ok(())
    }
}

impl From<&ObjectMeta> for JournalObject {
    fn from(object: &ObjectMeta) -> Self {
        Self {
            key: object.key.clone(),
            size: object.size,
            last_modified_unix_ms: unix_ms(object.last_modified),
            e_tag: object.e_tag.clone(),
            version: object.version.clone(),
        }
    }
}

impl TryFrom<JournalObject> for ObjectMeta {
    type Error = CrabError;

    fn try_from(object: JournalObject) -> Result<Self> {
        let millis =
            u64::try_from(object.last_modified_unix_ms).map_err(|_| CrabError::CorruptObject {
                path: object.key.clone(),
                reason: "GC candidate has a negative modification time".to_owned(),
            })?;
        Ok(Self {
            key: object.key,
            size: object.size,
            last_modified: UNIX_EPOCH
                .checked_add(Duration::from_millis(millis))
                .ok_or_else(|| CrabError::CorruptObject {
                    path: "gc candidate".to_owned(),
                    reason: "GC candidate modification time overflows".to_owned(),
                })?,
            e_tag: object.e_tag,
            version: object.version,
            storage_class: None,
            transitioned_at: None,
        })
    }
}

fn state_path(root: &str) -> object_store::path::Path {
    object_store::path::Path::from(format!("{root}/state.json"))
}

fn batch_path(root: &str, batch: u64) -> object_store::path::Path {
    object_store::path::Path::from(format!("{root}/batches/{batch:020}.json"))
}

fn outcome_path(root: &str, batch: u64) -> object_store::path::Path {
    object_store::path::Path::from(format!("{root}/outcomes/{batch:020}.json"))
}

fn encode<T: Serialize>(value: &T, path: &object_store::path::Path) -> Result<Bytes> {
    serde_json::to_vec(value)
        .map(Bytes::from)
        .map_err(|error| CrabError::CorruptObject {
            path: path.to_string(),
            reason: format!("GC journal serialization failed: {error}"),
        })
}

fn decode<T: for<'de> Deserialize<'de>>(body: &[u8], path: &object_store::path::Path) -> Result<T> {
    serde_json::from_slice(body).map_err(|error| CrabError::CorruptObject {
        path: path.to_string(),
        reason: format!("GC journal JSON is invalid: {error}"),
    })
}

fn validate_run_id(run_id: &str) -> Result<()> {
    Uuid::parse_str(run_id)
        .map(|_| ())
        .map_err(|error| CrabError::Configuration {
            key: "gc.resume".to_owned(),
            origin: format!("invalid GC run id: {error}"),
        })
}

fn validate_state(state: &GcRunState, run_id: &str, scope: &str, domain: &str) -> Result<()> {
    if state.schema_version != SCHEMA_VERSION
        || state.run_id != run_id
        || state.scope != scope
        || state.domain != domain
        || state.next_batch > state.planned_batches
    {
        return Err(CrabError::CorruptObject {
            path: format!("gc/runs/{run_id}/state.json"),
            reason: "GC run state does not match the requested scope".to_owned(),
        });
    }
    Ok(())
}

fn validate_candidate_key(state: &GcRunState, key: &str) -> Result<()> {
    let valid_prefix = match state.scope.as_str() {
        "repo" => {
            let domain = state.domain.trim_end_matches('/');
            !domain.is_empty()
                && key
                    .strip_prefix(&format!("{domain}/"))
                    .is_some_and(|relative| {
                        super::REPO_GC_PREFIXES
                            .iter()
                            .any(|prefix| relative.starts_with(prefix))
                    })
        }
        "bucket" | "bucket-preview" => [
            ".crab/shards/",
            ".crab/xorbs/",
            ".crab/gc/closures/",
            ".crab/gc/closure-segments/",
        ]
        .iter()
        .any(|prefix| key.starts_with(prefix)),
        _ => false,
    };
    let safe_segments = !key.is_empty()
        && !key.contains('\0')
        && !key
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..");
    if valid_prefix && safe_segments {
        return Ok(());
    }
    Err(CrabError::CorruptObject {
        path: key.to_owned(),
        reason: format!(
            "GC journal candidate is outside the {} deletion namespace",
            state.scope
        ),
    })
}

fn validate_outcome(outcome: &BatchOutcome, run_id: &str, batch: u64) -> Result<()> {
    if outcome.schema_version != SCHEMA_VERSION
        || outcome.run_id != run_id
        || outcome.batch != batch
    {
        return Err(CrabError::CorruptObject {
            path: format!("gc/runs/{run_id}/outcomes/{batch:020}.json"),
            reason: "GC batch outcome identity mismatch".to_owned(),
        });
    }
    Ok(())
}

fn unix_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;

    use super::*;

    fn object(index: usize) -> ObjectMeta {
        ObjectMeta {
            key: format!("repo/packs/{index}"),
            size: index as u64,
            last_modified: UNIX_EPOCH + Duration::from_secs(1),
            e_tag: Some(format!("etag-{index}")),
            version: None,
            storage_class: None,
            transitioned_at: None,
        }
    }

    #[tokio::test]
    async fn plan_is_bounded_and_resumes_at_the_next_batch() {
        let store = Store::new(Arc::new(InMemory::new()));
        let mut journal = GcRunJournal::start(
            store.clone(),
            "repo",
            "repo",
            "repo",
            SystemTime::now(),
            Duration::from_secs(3600),
            false,
        )
        .await
        .unwrap();
        let objects = (0..DEFAULT_BATCH_SIZE + 1).map(object).collect::<Vec<_>>();
        journal.set_root_identity("root").await.unwrap();
        journal.plan(&objects).await.unwrap();
        assert_eq!(journal.state().planned_batches, 2);
        assert_eq!(
            journal.next_batch().await.unwrap().unwrap().len(),
            DEFAULT_BATCH_SIZE
        );
        journal
            .complete_batch(&[objects[0].key.clone()], objects[0].size, None)
            .await
            .unwrap();

        let resumed = GcRunJournal::resume(store, "repo", &journal.state().run_id, "repo", "repo")
            .await
            .unwrap();
        assert_eq!(resumed.state().next_batch, 1);
        assert_eq!(resumed.next_batch().await.unwrap().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn resume_rejects_a_different_scope() {
        let store = Store::new(Arc::new(InMemory::new()));
        let journal = GcRunJournal::start(
            store.clone(),
            "repo",
            "repo",
            "repo",
            SystemTime::now(),
            Duration::from_secs(3600),
            false,
        )
        .await
        .unwrap();
        let result =
            GcRunJournal::resume(store, "repo", &journal.state().run_id, "bucket", "repo").await;
        assert!(matches!(result, Err(CrabError::CorruptObject { .. })));
    }

    #[tokio::test]
    async fn resume_rejects_a_different_root_identity() {
        let store = Store::new(Arc::new(InMemory::new()));
        let mut journal = GcRunJournal::start(
            store.clone(),
            "repo",
            "repo",
            "repo",
            SystemTime::now(),
            Duration::from_secs(3600),
            false,
        )
        .await
        .unwrap();
        journal.set_root_identity("root-a").await.unwrap();
        journal.plan(&[object(1)]).await.unwrap();
        let resumed = GcRunJournal::resume(store, "repo", &journal.state().run_id, "repo", "repo")
            .await
            .unwrap();
        assert!(matches!(
            resumed.ensure_root_identity("root-b"),
            Err(CrabError::Configuration { .. })
        ));
    }

    #[tokio::test]
    async fn candidate_plan_rejects_objects_outside_the_scope_namespace() {
        let store = Store::new(Arc::new(InMemory::new()));
        let mut journal = GcRunJournal::start(
            store,
            "repo",
            "repo",
            "repo",
            SystemTime::now(),
            Duration::from_secs(3600),
            false,
        )
        .await
        .unwrap();
        journal.set_root_identity("root").await.unwrap();
        let error = journal
            .append_candidates(&[ObjectMeta {
                key: "other/xorbs/not-in-repo".to_owned(),
                size: 1,
                last_modified: UNIX_EPOCH,
                e_tag: Some("etag".to_owned()),
                version: None,
                storage_class: None,
                transitioned_at: None,
            }])
            .await
            .expect_err("out-of-scope candidate must fail closed");
        assert!(matches!(error, CrabError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn planning_resume_restarts_partial_plan_before_execution() {
        let store = Store::new(Arc::new(InMemory::new()));
        let mut journal = GcRunJournal::start(
            store.clone(),
            "repo",
            "repo",
            "repo",
            SystemTime::now(),
            Duration::from_secs(3600),
            false,
        )
        .await
        .unwrap();
        journal.set_root_identity("root").await.unwrap();
        journal.append_candidates(&[object(1)]).await.unwrap();
        let run_id = journal.state().run_id.clone();
        let partial_batch = batch_path(&journal.root, 0);

        let mut resumed = GcRunJournal::resume(store.clone(), "repo", &run_id, "repo", "repo")
            .await
            .unwrap();
        assert_eq!(resumed.state().phase, GcRunPhase::Planning);
        resumed
            .ensure_policy(Duration::from_secs(3600), false)
            .unwrap();
        resumed.reset_partial_plan().await.unwrap();
        assert_eq!(resumed.state().planned_batches, 0);
        assert!(matches!(
            store.head(&partial_batch).await,
            Err(CrabError::NotFound { .. })
        ));
        assert!(matches!(
            resumed.next_batch().await,
            Err(CrabError::Configuration { .. })
        ));

        resumed.append_candidates(&[object(2)]).await.unwrap();
        resumed.finish_plan().await.unwrap();
        assert_eq!(
            resumed.next_batch().await.unwrap().unwrap()[0].key,
            "repo/packs/2"
        );
    }

    #[tokio::test]
    async fn an_unsealed_planning_run_can_be_resumed_for_root_replay() {
        let store = Store::new(Arc::new(InMemory::new()));
        let mut journal = GcRunJournal::start(
            store.clone(),
            "repo",
            "repo",
            "repo",
            SystemTime::now(),
            Duration::from_secs(3600),
            false,
        )
        .await
        .unwrap();
        journal.append_candidates(&[object(1)]).await.unwrap();
        let run_id = journal.state().run_id.clone();

        let resumed = GcRunJournal::resume(store, "repo", &run_id, "repo", "repo")
            .await
            .expect("planning runs may replay roots before sealing identity");
        assert_eq!(resumed.state().phase, GcRunPhase::Planning);
        assert!(resumed.state().root_identity.is_empty());
    }

    #[tokio::test]
    async fn resume_rejects_a_changed_policy() {
        let store = Store::new(Arc::new(InMemory::new()));
        let mut journal = GcRunJournal::start(
            store.clone(),
            "repo",
            "repo",
            "repo",
            SystemTime::now(),
            Duration::from_secs(3600),
            false,
        )
        .await
        .unwrap();
        journal.set_root_identity("root").await.unwrap();
        journal.plan(&[object(1)]).await.unwrap();
        let resumed = GcRunJournal::resume(store, "repo", &journal.state().run_id, "repo", "repo")
            .await
            .unwrap();
        assert!(matches!(
            resumed.ensure_policy(Duration::from_secs(7200), false),
            Err(CrabError::Configuration { .. })
        ));
    }

    #[tokio::test]
    async fn file_index_completion_marker_survives_resume() {
        let store = Store::new(Arc::new(InMemory::new()));
        let mut journal = GcRunJournal::start(
            store.clone(),
            ".crab",
            "bucket",
            ".crab",
            SystemTime::now(),
            Duration::from_secs(3600),
            false,
        )
        .await
        .unwrap();
        journal.set_root_identity("root").await.unwrap();
        journal
            .append_candidates(&[ObjectMeta {
                key: ".crab/xorbs/aa/hash".to_owned(),
                size: 1,
                last_modified: UNIX_EPOCH,
                e_tag: Some("etag".to_owned()),
                version: None,
                storage_class: None,
                transitioned_at: None,
            }])
            .await
            .unwrap();
        journal.finish_plan().await.unwrap();
        journal.mark_file_index_complete().await.unwrap();

        let resumed =
            GcRunJournal::resume(store, ".crab", &journal.state().run_id, "bucket", ".crab")
                .await
                .unwrap();
        assert!(resumed.file_index_complete());
    }

    #[tokio::test]
    async fn completed_run_retires_batches_outcomes_and_marks() {
        let store = Store::new(Arc::new(InMemory::new()));
        let mut journal = GcRunJournal::start(
            store.clone(),
            "repo",
            "repo",
            "repo",
            SystemTime::now(),
            Duration::from_secs(3600),
            false,
        )
        .await
        .unwrap();
        journal.set_root_identity("root").await.unwrap();
        journal.plan(&[object(1)]).await.unwrap();
        let batch = batch_path(&journal.root, 0);
        let outcome = outcome_path(&journal.root, 0);
        let mark = Path::from(format!("{}/marks/live/0000/mark.json", journal.root));
        store.put(&mark, Bytes::from_static(b"mark")).await.unwrap();
        journal
            .complete_batch(&["repo/packs/1".to_owned()], 1, None)
            .await
            .unwrap();

        let state = state_path(&journal.root);
        journal.complete().await.unwrap();

        for path in [batch, outcome, mark] {
            assert!(matches!(
                store.head(&path).await,
                Err(CrabError::NotFound { .. })
            ));
        }
        let (body, _) = store.get_with_etag(&state).await.unwrap();
        let persisted: GcRunState = decode(&body, &state).unwrap();
        assert_eq!(persisted.phase, GcRunPhase::Complete);
    }

    #[tokio::test]
    async fn retry_after_outcome_commit_accepts_the_identical_batch() {
        let store = Store::new(Arc::new(InMemory::new()));
        let mut journal = GcRunJournal::start(
            store,
            "repo",
            "repo",
            "repo",
            SystemTime::now(),
            Duration::from_secs(3600),
            false,
        )
        .await
        .unwrap();
        journal.set_root_identity("root").await.unwrap();
        journal.plan(&[object(1)]).await.unwrap();
        let deleted = ["repo/packs/1".to_owned()];
        journal.complete_batch(&deleted, 1, None).await.unwrap();

        // Model a process exit after the immutable outcome was published but
        // before the mutable state cursor reached the provider.
        journal.state.next_batch = 0;
        journal.persist_state().await.unwrap();
        journal.complete_batch(&deleted, 1, None).await.unwrap();

        assert_eq!(journal.state.next_batch, 1);
    }

    #[tokio::test]
    async fn delete_batch_rejects_a_writer_epoch_crossing() {
        let store = Store::new(Arc::new(InMemory::new()));
        let mut journal = GcRunJournal::start(
            store,
            "repo",
            "repo",
            "repo",
            SystemTime::now(),
            Duration::from_secs(3600),
            false,
        )
        .await
        .unwrap();
        journal.seal_fence_epoch(10).await.unwrap();

        journal.ensure_next_fence_epoch(10).unwrap();
        assert!(matches!(
            journal.ensure_next_fence_epoch(12),
            Err(CrabError::Configuration { .. })
        ));
    }
}
