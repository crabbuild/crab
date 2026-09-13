// Derived from denoland/celld, commit 10cb1303dac710dcb3b557e318e08c855261f68b.
// Apache-2.0; see LICENSE and UPSTREAM.md. Modified by Crab contributors.
// Split from upstream db.rs; see UPSTREAM.md for Crab's changes.

use super::*;

impl Db {
    pub(super) fn checkpoint_if_needed(
        &mut self,
        orig_wal_size: i64,
        new_wal_size: i64,
    ) -> Result<()> {
        if self.page_size == 0 {
            return Ok(());
        }

        // Priority 1: emergency TRUNCATE (blocking) on the *original* logical
        // size. A truncate ends in a boundary image of the whole database
        // (see `checkpoint`). For a small database that image is the cheap
        // price the threshold was tuned for, so below `RELATIVE_TRUNCATE_PAGES`
        // the threshold is absolute, as upstream's. Above it the WAL must also
        // have grown past the database before a truncate: the image then costs
        // at most what the WAL it replaces did, and the chain stays within 2x
        // of the writes. A fixed threshold made a 1MB-row whale pay a
        // database-sized capture every write, and its chain grew as the
        // square of its size.
        if self.truncate_page_n > 0 {
            let relative = if self.last_db_pages > RELATIVE_TRUNCATE_PAGES {
                self.last_db_pages
            } else {
                0
            };
            let threshold = self.truncate_page_n.max(relative);
            if orig_wal_size >= calc_wal_size(self.page_size, threshold) {
                return self.checkpoint(CheckpointMode::Truncate);
            }
        }

        // Priority 2: PASSIVE once the frames appended since the last
        // backfill reach the threshold. See `checkpointed_wal_offset` for why
        // this is not the whole logical size: a checkpoint whose sealing
        // write could not restart the WAL must cost one retry at the next
        // threshold, not a checkpoint per sync.
        let backfilled_through = self.checkpointed_wal_offset.clamp(
            WAL_HEADER_SIZE as i64,
            new_wal_size.max(WAL_HEADER_SIZE as i64),
        );
        let threshold =
            calc_wal_size(self.page_size, self.min_checkpoint_page_n) - WAL_HEADER_SIZE as i64;
        if new_wal_size - backfilled_through >= threshold {
            return self.checkpoint(CheckpointMode::Passive);
        }

        // Priority 3: time-based PASSIVE, gated on data synced since last
        // checkpoint (#896). Uses the DB-file mtime and a logical-size guard so an
        // idle DB does not spin LTX files (db.go:1133-1153).
        if self.checkpoint_interval > Duration::ZERO && self.synced_since_checkpoint {
            let elapsed = self.host.file_age(&self.path)?;
            if elapsed > self.checkpoint_interval && new_wal_size > calc_wal_size(self.page_size, 1)
            {
                return self.checkpoint(CheckpointMode::Passive);
            }
        }

        Ok(())
    }

    pub(super) fn checkpoint(&mut self, mode: CheckpointMode) -> Result<()> {
        // Self-heal, as in `sync`: `checkpoint` writes to both control tables
        // and re-acquires the read lock through `_litestream_seq`, and the
        // invariant is that the tables exist before any control-table
        // statement — not only before a capture.
        self.ensure_control_tables()?;

        // Read the WAL header before the checkpoint to detect a restart.
        let hdr = self.wal_header_bytes()?;

        // Copy the end of the WAL before the checkpoint to capture as much as
        // possible (db.go:1823-1826).
        self.verify_and_sync()?;

        let frame_size = self.page_size as i64 + WAL_FRAME_HEADER_SIZE as i64;
        let pre_checkpoint_frame_n = if self.last_synced_wal_offset > WAL_HEADER_SIZE as i64 {
            (self.last_synced_wal_offset - WAL_HEADER_SIZE as i64) / frame_size
        } else {
            0
        };

        // A passive checkpoint does not acquire SQLite's writer lock. Hold a
        // short write transaction on the dedicated read-lock connection, then
        // sync again to seal every commit before running the checkpoint on the
        // main connection. Keep the barrier until the checkpoint completes.
        let pragma = if mode == CheckpointMode::Passive {
            self.exec_passive_checkpoint_with_barrier(hdr)?
        } else {
            self.exec_checkpoint(mode)?
        };
        // The backfilled boundary in this WAL's coordinates. A short backfill
        // (a reader pinned the WAL) leaves the remainder counting toward the
        // next threshold, so a pinned WAL retries at the threshold and an
        // unpinned one does not retry at all.
        self.checkpointed_wal_offset =
            WAL_HEADER_SIZE as i64 + pragma.backfilled.max(0) * frame_size;

        // Force a write so a restarted WAL has a new header and at least one
        // frame that verify can read.
        self.conn
            .execute_batch(
                "INSERT INTO _litestream_seq (id, seq) VALUES (1, 1) \
                 ON CONFLICT (id) DO UPDATE SET seq = seq + 1",
            )
            .map_err(CrabError::Sqlite)?;

        // If the WAL header is unchanged, the WAL did not restart — done.
        let other = self.wal_header_bytes()?;
        if hdr == other {
            self.synced_since_checkpoint = false;
            return Ok(());
        }

        // The WAL restarted. Grab the write lock, then either copy the new WAL
        // tail or take a complete boundary image. TRUNCATE always needs the
        // boundary image because SQLite reports zero frames after resetting the
        // WAL. A forced checkpoint also needs one if it covered more frames than
        // the sealed pre-checkpoint sync observed.
        self.conn
            .prepare_cached("BEGIN")
            .and_then(|mut statement| statement.execute([]))
            .map_err(CrabError::Sqlite)?;
        let post = (|| -> Result<()> {
            self.conn
                .prepare_cached("INSERT INTO _litestream_lock (id) VALUES (1)")
                .and_then(|mut statement| statement.execute([]))
                .map_err(CrabError::Sqlite)?;
            if mode == CheckpointMode::Truncate
                || (mode != CheckpointMode::Passive && pragma.wal_frames > pre_checkpoint_frame_n)
            {
                let info = SyncInfo {
                    offset: WAL_HEADER_SIZE as i64,
                    salt1: be_u32(&other[16..]),
                    salt2: be_u32(&other[20..]),
                    snapshotting: true,

                    ..Default::default()
                };
                self.sync_inner(info)?;
            } else {
                self.verify_and_sync()?;
            }
            Ok(())
        })();
        // Always roll back the write transaction (db.go:1849,1867).
        let rb = rollback(&self.conn);
        post?;
        rb?;

        self.synced_since_checkpoint = false;
        Ok(())
    }

    pub(super) fn exec_passive_checkpoint_with_barrier(
        &mut self,
        pre_checkpoint_header: [u8; WAL_HEADER_SIZE],
    ) -> Result<CheckpointPragma> {
        self.release_read_lock()?;

        let result = (|| -> Result<CheckpointPragma> {
            self.rtx_conn
                .prepare_cached("BEGIN")
                .and_then(|mut statement| statement.execute([]))
                .map_err(CrabError::Sqlite)?;
            self.rtx_conn
                .prepare_cached("INSERT INTO _litestream_lock (id) VALUES (1)")
                .and_then(|mut statement| statement.execute([]))
                .map_err(CrabError::Sqlite)?;

            // Writers can cross their own autocheckpoint threshold after the
            // read lock is released but before this barrier wins SQLite's
            // writer lock. A changed WAL header means some of those commits
            // can already live only in the database file. The normal
            // synced-to-end shortcut cannot distinguish that race from our own
            // completed checkpoint, so an incremental LTX can omit the
            // checkpointed pages and produce a malformed restore. Seal the
            // complete boundary while the writer lock makes the database file
            // and the new WAL tail a stable pair. The cost is one database-size
            // LTX file only when another checkpoint wins this narrow gap.
            let barrier_header = self.wal_header_bytes()?;
            if barrier_header != pre_checkpoint_header {
                let info = SyncInfo {
                    offset: WAL_HEADER_SIZE as i64,
                    salt1: be_u32(&barrier_header[16..]),
                    salt2: be_u32(&barrier_header[20..]),
                    snapshotting: true,

                    ..Default::default()
                };
                self.sync_inner(info)?;
            } else {
                // Commits can land between the earlier sync and acquisition of
                // the barrier. This second sync seals them before the
                // checkpoint.
                self.verify_and_sync()?;
            }
            self.run_checkpoint_pragma(CheckpointMode::Passive)
        })();

        // Release the writer barrier before restoring the long-lived read lock.
        // Preserve the operation error if both the operation and cleanup fail.
        let rollback_result = rollback(&self.rtx_conn);
        let reacquire_result = self.acquire_read_lock();
        match result {
            Err(error) => Err(error),
            Ok(pragma) => {
                rollback_result?;
                reacquire_result?;
                Ok(pragma)
            }
        }
    }

    pub(super) fn exec_checkpoint(&mut self, mode: CheckpointMode) -> Result<CheckpointPragma> {
        // Ensure the read lock is removed before the checkpoint; defer the
        // re-acquire so it runs even on early return.
        self.release_read_lock()?;

        let result = self.run_checkpoint_pragma(mode);

        // Re-acquire the read lock immediately after the checkpoint (the deferred
        // re-acquire in Go). If the pragma succeeded, propagate any re-acquire
        // error; otherwise surface the original pragma error.
        let reacquire = self.acquire_read_lock();
        match (result, reacquire) {
            (Ok(pragma), Ok(())) => Ok(pragma),
            (Ok(_), Err(e)) => Err(e),
            (Err(e), _) => Err(e),
        }
    }

    pub(super) fn run_checkpoint_pragma(&self, mode: CheckpointMode) -> Result<CheckpointPragma> {
        let sql = format!("PRAGMA wal_checkpoint({mode})");
        self.conn
            .prepare_cached(&sql)
            .map_err(CrabError::Sqlite)?
            .query_row([], |row| {
                Ok(CheckpointPragma {
                    wal_frames: row.get::<_, i64>(1)?,
                    backfilled: row.get::<_, i64>(2)?,
                })
            })
            .map_err(CrabError::Sqlite)
    }
}
