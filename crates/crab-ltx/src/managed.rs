use std::path::{Path, PathBuf};

use rusqlite::{Connection, Transaction};

use crate::{CaptureBatch, CrabError, Limits, LocalSegment, Position, Result, SegmentInfo};
use crate::{db::Db, host::LtxHost, ltx, recovery::persist_new, types::Txid};

/// One exclusive local capture session with a serialized SQLite writer.
///
/// The caller owns the database and its directory: no external writers, direct
/// checkpoints, control-table edits, or deletion of retained LTX files. Opening
/// claims a fresh metadata directory; reactivation requires exact restore into
/// a fresh directory, not reusing potentially unpublished local state.
pub struct ManagedDb {
    db: Db,
    writer: Connection,
    observer: crate::commit::CommitObserver,
    required_cut: Option<crate::commit::WalCut>,
    limits: Limits,
    fenced: bool,
    retained_bytes: u64,
    retained_segments: usize,
    path: PathBuf,
}

impl ManagedDb {
    /// Opens a new capture session on a new or exactly restored SQLite file.
    ///
    /// The parent must exist and the local path must be UTF-8. An existing
    /// capture directory is refused, even after a clean close. Failed opens
    /// leave that directory quarantined for caller-owned cleanup.
    pub fn open(path: &Path, limits: Limits) -> Result<Self> {
        let limits = limits.validate()?;
        if path.to_str().is_none() || path.file_name().is_none() {
            return Err(CrabError::InvalidState(
                "database path must be a UTF-8 file path",
            ));
        }
        // Resolve aliases before claiming the session directory, and keep all
        // later WAL reads independent of the process working directory.
        let resolved = match path.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                parent.canonicalize()?.join(
                    path.file_name()
                        .ok_or(CrabError::InvalidState("missing database filename"))?,
                )
            }
            Err(error) => return Err(error.into()),
        };
        let path = resolved.as_path();
        if path.to_str().is_none() {
            return Err(CrabError::InvalidState(
                "resolved database path must be UTF-8",
            ));
        }
        let host = LtxHost {
            max_database_bytes: limits.max_database_bytes,
            max_file_bytes: limits.max_file_bytes,
        };
        match std::fs::metadata(path) {
            Ok(metadata) => host.check_database_size(metadata.len())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // Atomic directory creation fences concurrent handles and stale sessions.
        // Never unlink it on close: an old open file must not acquire a new epoch.
        std::fs::create_dir(Db::meta_path_for(path))?;
        let db = Db::open_with_host(path, host)?;
        let writer = Connection::open(path)?;
        writer.busy_timeout(std::time::Duration::from_secs(1))?;
        writer.pragma_update(None, "wal_autocheckpoint", 0)?;
        writer.pragma_update(None, "synchronous", "FULL")?;
        writer.pragma_update(None, "foreign_keys", true)?;
        let page_size: u32 = writer.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        let max_pages = limits.max_database_bytes / u64::from(page_size);
        if max_pages == 0 {
            return Err(CrabError::Limit("database page size"));
        }
        writer.pragma_update(None, "max_page_count", max_pages)?;
        let observer = crate::commit::CommitObserver::install(&writer);
        Ok(Self {
            db,
            writer,
            observer,
            required_cut: None,
            limits,
            fenced: false,
            retained_bytes: 0,
            retained_segments: 0,
            path: path.to_owned(),
        })
    }

    /// Commits one local SQL transaction; call `capture` before publishing it.
    ///
    /// SQL is trusted: do not issue transaction-control statements or change
    /// pager pragmas through the callback. Success is NOT remote durability.
    pub fn transaction<T>(
        &mut self,
        operation: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<T>,
    ) -> Result<T> {
        self.ensure_active()?;
        self.ensure_capacity()?;
        self.observer.reset();
        let result: rusqlite::Result<T> = (|| {
            let tx = self
                .writer
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let result = operation(&tx)?;
            if let Err(error) = tx.commit() {
                // Commit failure is potentially ambiguous even if SQLite did
                // not invoke the WAL hook. Never accept another mutation here.
                self.fenced = true;
                return Err(error);
            }
            Ok(result)
        })();
        if result.is_err() && (self.observer.frames() != 0 || !self.writer.is_autocommit()) {
            self.fenced = true;
        }
        let value = result?;
        match self.observer.cut(&self.path) {
            Ok(Some(cut)) => self.required_cut = Some(cut),
            Ok(None) => {}
            Err(error) => {
                self.fenced = true;
                return Err(error);
            }
        }
        Ok(value)
    }

    /// Captures committed WAL pages and all cuts made by checkpoint maintenance.
    ///
    /// Any failure fences further use, since some local cuts may already exist.
    /// Retain returned files until publication/retention policy releases this
    /// entire session. This first implementation does not prune local artifacts.
    pub fn capture(&mut self) -> Result<CaptureBatch> {
        self.ensure_active()?;
        let result = self.capture_inner();
        if result.is_err() {
            self.fenced = true;
        }
        result
    }

    fn capture_inner(&mut self) -> Result<CaptureBatch> {
        self.ensure_capacity()?;
        let before = self.db.pos();
        self.db.sync(self.required_cut)?;
        self.required_cut = None;
        let after = self.db.pos();
        let mut segments = Vec::new();
        if after.txid.0 > before.txid.0 {
            for txid in before.txid.0 + 1..=after.txid.0 {
                let path = PathBuf::from(self.db.ltx_path(0, Txid(txid), Txid(txid)));
                let bytes = crate::host::read_bounded(&path, self.limits.max_file_bytes)?;
                let file = ltx::decode_file(&bytes)?;
                let info = SegmentInfo::from_decoded(&bytes, &file);
                self.account(&info)?;
                segments.push(LocalSegment::new(path, info));
            }
        }
        Ok(CaptureBatch {
            segments,
            position: after.into(),
        })
    }

    /// Captures pending commits, then writes a full checksum-bearing snapshot.
    ///
    /// Its range is `1..=position.txid`. The snapshot can replace all preceding
    /// cuts in a new manifest, but this method never publishes or deletes them.
    pub fn snapshot(&mut self, destination: &Path) -> Result<LocalSegment> {
        self.ensure_active()?;
        let result = self.snapshot_inner(destination);
        if result.is_err() {
            self.fenced = true;
        }
        result
    }

    fn snapshot_inner(&mut self, destination: &Path) -> Result<LocalSegment> {
        let batch = self.capture_inner()?;
        let mut bytes = Vec::new();
        let pos: Position = self.db.snapshot_to_writer(&mut bytes)?.into();
        if pos != batch.position {
            return Err(CrabError::ChecksumMismatch);
        }
        let file = ltx::decode_file(&bytes)?;
        let info = SegmentInfo::from_decoded(&bytes, &file);
        self.account(&info)?;
        persist_new(destination, &bytes)?;
        Ok(LocalSegment::new(destination.to_owned(), info))
    }

    /// Returns the local file path; it must not be independently mutated.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Releases the writer and checkpoint read lock without claiming publication.
    pub fn close(self) -> Result<()> {
        drop(self.writer);
        self.db.close()
    }

    fn ensure_active(&self) -> Result<()> {
        if self.fenced {
            return Err(CrabError::Fenced);
        }
        Ok(())
    }

    fn ensure_capacity(&self) -> Result<()> {
        if self.retained_segments >= self.limits.max_segments
            || self.retained_bytes >= self.limits.max_plan_bytes
        {
            return Err(CrabError::Limit(
                "retained capture artifacts; rotate session",
            ));
        }
        Ok(())
    }

    fn account(&mut self, info: &SegmentInfo) -> Result<()> {
        if info.size_bytes > self.limits.max_file_bytes {
            return Err(CrabError::Limit("LTX file bytes"));
        }
        self.retained_bytes = self
            .retained_bytes
            .checked_add(info.size_bytes)
            .ok_or(CrabError::Limit("retained bytes"))?;
        self.retained_segments += 1;
        if self.retained_segments > self.limits.max_segments
            || self.retained_bytes > self.limits.max_plan_bytes
        {
            return Err(CrabError::Limit(
                "retained capture artifacts; rotate session",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_checkpoint_and_auto_vacuum_preserve_every_cut() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("source.sqlite");
        let initial = Connection::open(&path).unwrap();
        initial
            .execute_batch("PRAGMA auto_vacuum=FULL; VACUUM;")
            .unwrap();
        drop(initial);
        let mut db = ManagedDb::open(&path, Limits::default()).unwrap();
        db.db.truncate_page_n = 20;
        db.db.min_checkpoint_page_n = 10;
        let mut segments = Vec::new();
        let mut last_pages = 0;
        let mut shrank = false;
        let mut multiple_cuts = false;
        for round in 0..6 {
            db.transaction(|tx| {
                tx.execute("CREATE TABLE IF NOT EXISTS t (data BLOB)", [])?;
                if round % 2 == 0 {
                    for _ in 0..80 {
                        tx.execute("INSERT INTO t VALUES (randomblob(8000))", [])?;
                    }
                } else {
                    tx.execute("DELETE FROM t", [])?;
                }
                Ok(())
            })
            .unwrap();
            let batch = db.capture().unwrap();
            multiple_cuts |= batch.segments.len() > 1;
            for segment in &batch.segments {
                shrank |= last_pages > segment.info().database_pages;
                last_pages = segment.info().database_pages;
            }
            segments.extend(batch.segments);
            let plan = crate::VerifiedLocalPlan::new(&segments, batch.position, Limits::default())
                .unwrap();
            let restored = temp.path().join(format!("restored-{round}.sqlite"));
            crate::restore_exact(&plan, &restored).unwrap();
            let conn = Connection::open(&restored).unwrap();
            let check: String = conn
                .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap();
            assert_eq!(check, "ok");
            let count: u32 = conn
                .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, if round % 2 == 0 { 80 } else { 0 });
            crate::compact_exact(&plan, &temp.path().join(format!("compacted-{round}.ltx")))
                .unwrap();
        }
        assert!(shrank);
        assert!(multiple_cuts);
    }
}
