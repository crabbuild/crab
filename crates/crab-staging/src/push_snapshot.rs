use std::sync::Arc;

use crate::{Result, StagingAreaReadOnly, recipe::FileRecipe};

/// Own one push reader's marker and exact immutable recipe snapshot.
#[must_use = "retain the snapshot through remote publication"]
pub struct StagingPushSnapshot {
    staging: Arc<StagingAreaReadOnly>,
    push_id: String,
    committed: bool,
}

impl StagingPushSnapshot {
    /// Fence retirement before selecting recipes for a uniquely identified push.
    ///
    /// The caller must supply a unique push ID.
    /// Failure or drop preserves their original leases; another reader can defer cleanup.
    pub fn begin(staging: Arc<StagingAreaReadOnly>, push_id: String) -> Result<Self> {
        let snapshot = Self {
            staging,
            push_id,
            committed: false,
        };
        snapshot.staging.mark_push_inflight(&snapshot.push_id)?;
        Ok(snapshot)
    }

    /// Pin published/delegated recipes selected from verified Git pointers.
    pub fn capture(&self, recipes: &[FileRecipe]) -> Result<()> {
        self.staging.create_push_snapshot(&self.push_id, recipes)
    }

    /// Record confirmed remote publication and retire only captured local owners.
    ///
    /// The caller must establish remote commit before calling this method. Contention
    /// leaves a committed journal for exclusive maintenance; cleanup errors retain it.
    pub fn commit(mut self) -> Result<()> {
        // Remote commit is already authoritative. Even if the local transition
        // fails, Drop must retain its journal rather than treat it as a failed push.
        self.committed = true;
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
        if !self.committed {
            let discard = (|| -> Result<()> {
                // A sibling may still read a version this snapshot alone pins.
                // Exclusive maintenance consumes the journal if cleanup is deferred.
                if let Some(_retirement) = self.staging.begin_retirement(&self.push_id)? {
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
