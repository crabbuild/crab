use std::sync::Arc;

use crate::{Result, StagedFilePin, StagingAreaReadOnly, lock_index, recipe::FileRecipe};

enum SnapshotKind {
    Push,
    Read,
}

/// Own one push reader's marker and exact immutable recipe snapshot.
#[must_use = "retain the snapshot through remote publication"]
pub struct StagingPushSnapshot {
    staging: Arc<StagingAreaReadOnly>,
    push_id: String,
    cleanup_on_drop: bool,
    kind: SnapshotKind,
}

impl StagingPushSnapshot {
    /// Fence retirement before selecting recipes for a uniquely identified push.
    ///
    /// The caller must supply a unique push ID.
    /// Failure or drop preserves their original leases; another reader can defer cleanup.
    pub fn begin(staging: Arc<StagingAreaReadOnly>, push_id: String) -> Result<Self> {
        Self::begin_kind(staging, push_id, SnapshotKind::Push)
    }

    pub(crate) fn begin_read(staging: Arc<StagingAreaReadOnly>, read_id: String) -> Result<Self> {
        Self::begin_kind(staging, read_id, SnapshotKind::Read)
    }

    fn begin_kind(
        staging: Arc<StagingAreaReadOnly>,
        push_id: String,
        kind: SnapshotKind,
    ) -> Result<Self> {
        let snapshot = Self {
            staging,
            push_id,
            cleanup_on_drop: true,
            kind,
        };
        snapshot.staging.mark_push_inflight(&snapshot.push_id)?;
        Ok(snapshot)
    }

    /// Pin published/delegated recipes selected from verified Git pointers.
    pub fn capture(&self, recipes: &[FileRecipe]) -> Result<()> {
        match self.kind {
            SnapshotKind::Push => self.staging.create_push_snapshot(&self.push_id, recipes),
            SnapshotKind::Read => {
                lock_index(&self.staging.index)?.create_read_snapshot(&self.push_id, recipes)
            }
        }
    }

    pub(crate) fn into_read_pin(mut self) -> Result<StagedFilePin> {
        let pin = StagedFilePin::from_snapshot(&self.staging, &self.push_id)?;
        self.cleanup_on_drop = false;
        Ok(pin)
    }

    /// Record confirmed remote publication and retire only captured local owners.
    ///
    /// The caller must establish remote commit before calling this method. Contention
    /// leaves a committed journal for exclusive maintenance; cleanup errors retain it.
    pub fn commit(mut self) -> Result<()> {
        // Remote commit is already authoritative. Even if the local transition
        // fails, Drop must retain its journal rather than treat it as a failed push.
        self.cleanup_on_drop = false;
        self.staging.commit_push_snapshot(&self.push_id)?;
        let Some(_retirement) = self.staging.begin_retirement(&self.push_id)? else {
            return Ok(());
        };
        self.staging.retire_push_snapshot_now(&self.push_id)?;
        self.staging.sweep_orphans()?;
        self.staging.remove_push_snapshot(&self.push_id)
    }
}

impl Drop for StagingPushSnapshot {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let discard = (|| -> Result<()> {
                // A sibling may still read a version this snapshot alone pins.
                // Exclusive maintenance consumes the journal if cleanup is deferred.
                if matches!(self.kind, SnapshotKind::Read) {
                    // A file read only pins existing owners. Drop its temporary
                    // journal even while sibling readers are active; payload
                    // reclamation stays with push/exclusive staging maintenance.
                    self.staging.remove_push_snapshot(&self.push_id)?;
                } else if let Some(_retirement) = self.staging.begin_retirement(&self.push_id)? {
                    self.staging.discard_open_push_snapshot_now(&self.push_id)?;
                }
                Ok(())
            })();
            if let Err(error) = discard {
                tracing::warn!(push_id = %self.push_id, %error, "staging snapshot cleanup needs maintenance");
            }
        }
        if let Err(error) = self.staging.clear_push_inflight(&self.push_id) {
            tracing::warn!(push_id = %self.push_id, %error, "staging push marker cleanup failed");
        }
    }
}
