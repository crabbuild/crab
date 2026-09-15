// Derived from denoland/celld, commit 10cb1303dac710dcb3b557e318e08c855261f68b.
// Apache-2.0; see LICENSE and UPSTREAM.md. Modified by Crab contributors.
// Split from upstream db.rs; see UPSTREAM.md for Crab's changes.

use super::*;

impl Db {
    pub(super) fn read_valid_wal_image(
        &mut self,
        info: &SyncInfo,
        start: usize,
    ) -> Result<WalImage> {
        let frame_size = self.page_size as usize + WAL_FRAME_HEADER_SIZE;
        let offset = info.offset;
        let salt1 = info.salt1;
        let salt2 = info.salt2;
        Ok(self.with_wal_file(|file| {
            let file_len = file.file_len()? as usize;
            if file_len < WAL_HEADER_SIZE || start < WAL_HEADER_SIZE || start >= file_len {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            let complete_end =
                WAL_HEADER_SIZE + ((file_len - WAL_HEADER_SIZE) / frame_size) * frame_size;
            if start >= complete_end || !(start - WAL_HEADER_SIZE).is_multiple_of(frame_size) {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }

            let tail_base = if start == WAL_HEADER_SIZE { 0 } else { start };
            let mut bytes = file.read_exact_at(0, WAL_HEADER_SIZE)?;
            let mut cursor = start;
            let mut target_frames = 1_usize;
            loop {
                let target_end = start
                    .saturating_add(target_frames.saturating_mul(frame_size))
                    .min(complete_end);
                if target_end > cursor {
                    let chunk = file.read_exact_at(cursor as u64, target_end - cursor)?;
                    bytes.extend_from_slice(&chunk);
                    cursor = target_end;
                }

                let valid_end = {
                    let parsed = if offset == WAL_HEADER_SIZE as i64 {
                        WalReader::new(&bytes)
                    } else if tail_base == 0 {
                        WalReader::new_with_offset(&bytes, offset, salt1, salt2)
                    } else {
                        WalReader::new_with_offset_over_tail(
                            &bytes,
                            tail_base as i64,
                            offset,
                            salt1,
                            salt2,
                        )
                    };
                    let mut reader = parsed.map_err(|error| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error)
                    })?;
                    reader.page_map().map_err(|error| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error)
                    })?;
                    if reader.offset() == 0 {
                        WAL_HEADER_SIZE
                    } else {
                        reader.offset() as usize + frame_size
                    }
                };

                if valid_end < cursor {
                    let keep = if tail_base == 0 {
                        valid_end
                    } else {
                        WAL_HEADER_SIZE + valid_end.saturating_sub(tail_base)
                    };
                    bytes.truncate(keep);
                    return Ok(WalImage { bytes, tail_base });
                }
                if cursor == complete_end {
                    return Ok(WalImage { bytes, tail_base });
                }
                target_frames = target_frames.saturating_mul(2);
            }
        })?)
    }

    pub(super) fn sync_inner(&mut self, mut info: SyncInfo) -> Result<bool> {
        // A capture that starts at the WAL header reads a logical WAL with no
        // backfilled prefix: the first sync, a restart, or a boundary image.
        // The checkpoint trigger counts from the backfilled boundary, so it
        // must not carry an offset from the WAL that just ended, or the new
        // WAL would grow past that offset before its first checkpoint.
        if info.offset == WAL_HEADER_SIZE as i64 {
            self.checkpointed_wal_offset = WAL_HEADER_SIZE as i64;
        }
        let pos = self.position;
        let tx_id = Txid(pos.txid.0.checked_add(1).ok_or(CrabError::TxNotAvailable)?);
        let filename = self.ltx_path(0, tx_id, tx_id);

        let db_size = self.db_file_size()?;
        let mut commit = (db_size / self.page_size as i64) as u32;
        self.last_db_pages = commit;

        // The incremental path reads only the valid checksum chain: the
        // 32-byte WAL header, the previous frame when there is one, and the
        // frames from `info.offset` on. SQLite can retain a large stale physical
        // suffix after a logical restart. Stop at the valid prefix instead of
        // repeatedly reading that suffix; a sparse-tail mismatch needs a full
        // re-read so a zero-filled prefix cannot hide uncaptured commits.
        let frame_size_bytes = self.page_size as i64 + WAL_FRAME_HEADER_SIZE as i64;
        let mut sparse_tail = false;
        let mut wal = if info.snapshotting {
            let bytes = self.host.read(&self.wal_path())?;
            WalImage::whole(bytes)
        } else {
            let start = if info.offset <= WAL_HEADER_SIZE as i64 + frame_size_bytes {
                WAL_HEADER_SIZE
            } else {
                (info.offset - frame_size_bytes) as usize
            };
            match self.read_valid_wal_image(&info, start) {
                Ok(image) => {
                    sparse_tail = start != WAL_HEADER_SIZE;
                    image
                }
                Err(_) => {
                    let bytes = self.host.read(&self.wal_path())?;
                    WalImage::whole(bytes)
                }
            }
        };

        // Choose the WAL reader start: from the header, or seek to info.offset.
        // A previous-frame mismatch falls back to a full read (snapshot),
        // mirroring NewWALReaderWithOffset's PrevFrameMismatchError handling
        // (db.go:1565-1581).
        // A previous-frame mismatch restarts the read from the header. The
        // sparse tail image is zero-filled below `start`, so a from-header
        // reader over it would see no valid frame and report "nothing to
        // capture" — a silent miss the ship loop would then credit. The
        // mismatch path therefore re-reads the complete WAL first, which is
        // exactly the port's former full-read behavior on this branch.
        let mismatch = !(info.offset == WAL_HEADER_SIZE as i64)
            && matches!(
                wal.reader_at(info.offset, info.salt1, info.salt2),
                Err(crate::wal::WalError::PrevFrameMismatch)
            );
        if mismatch {
            info.offset = WAL_HEADER_SIZE as i64;
            if sparse_tail {
                let bytes = self.host.read(&self.wal_path())?;
                wal = WalImage::whole(bytes);
            }
        }
        let mut rd = if info.offset == WAL_HEADER_SIZE as i64 {
            WalReader::new(&wal.bytes).map_err(CrabError::from)?
        } else {
            wal.reader_at(info.offset, info.salt1, info.salt2)
                .map_err(CrabError::from)?
        };

        let (page_map, max_offset, wal_commit) = rd.page_map().map_err(CrabError::from)?;
        if wal_commit > 0 {
            commit = wal_commit;
        }

        let sz = if max_offset > 0 {
            max_offset - info.offset
        } else {
            0
        };
        if sz < 0 {
            return Err(CrabError::Other(
                format!(
                    "wal size must be positive: sz={sz}, maxOffset={max_offset}, info.offset={}",
                    info.offset
                )
                .into(),
            ));
        }

        // Exit if there are no new WAL pages and we are not snapshotting
        // (db.go:1603-1607).
        if !info.snapshotting && sz == 0 {
            return Ok(false);
        }

        let (rd_salt1, rd_salt2) = rd.salt();

        // Build the page set for the encoder.
        self.host
            .check_database_size(u64::from(commit) * u64::from(self.page_size))?;
        let pages: Vec<(u32, Vec<u8>)> = if info.snapshotting {
            self.collect_snapshot_pages(&wal, &page_map, commit)?
        } else {
            self.collect_wal_pages(&wal, &page_map, info.prev_commit, commit)?
        };

        let mut checksums = self.checksums.clone();
        checksums.apply(self.page_size, commit, &pages, self.host.max_database_bytes)?;
        let post_checksum = checksums.checksum();
        let header = ltx::Header {
            version: ltx::VERSION,
            flags: 0,
            page_size: self.page_size,
            commit,
            min_txid: tx_id,
            max_txid: tx_id,
            timestamp: self.host.now_unix_millis(),
            pre_apply_checksum: pos.post_apply_checksum,
            wal_offset: info.offset,
            wal_size: sz,
            wal_salt1: rd_salt1,
            wal_salt2: rd_salt2,
            node_id: 0,
        };

        // Emit a checksum-bearing cut; advance the page index only after fsync.
        let encoded = ltx::encode_file(&header, &pages, post_checksum)?;

        // Atomic tmp → fsync → rename (db.go:1609-1685).
        let tmp_filename = format!("{filename}.tmp");
        let parent = Path::new(&tmp_filename).parent().map(Path::to_path_buf);
        if !self.l0_dir_ready {
            if let Some(parent) = &parent {
                self.host.create_dir_all(parent)?;
            }
            self.l0_dir_ready = true;
        }
        // On rename failure, clear the L0 cache + invalidate pos
        // (db.go:1680-1684); the error path below does that. A directory
        // that vanished under a ready flag is recreated once and the cut
        // retried, so the flag saves a `mkdir` per sync without trusting it.
        match write_file_atomic(&self.host, &tmp_filename, &filename, &encoded) {
            Err(CrabError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = &parent {
                    self.host.create_dir_all(parent)?;
                }
                write_file_atomic(&self.host, &tmp_filename, &filename, &encoded)?
            }
            other => other?,
        };
        // The checksum candidate remains isolated until the cut is durable. A
        // failed local index update fences the owning ManagedDb, so partially
        // updated ephemeral state can never authorize another capture.
        checksums.persist()?;
        // The next verify reads exactly these fields back; caching them —
        // plus the final consumed WAL frame for the page check — is what
        // spares it re-reading the file it just watched being written.
        let frame_size = self.page_size as i64 + WAL_FRAME_HEADER_SIZE as i64;
        let final_frame_offset = max_offset - frame_size;
        let (final_pgno, final_page) = match (final_frame_offset >= WAL_HEADER_SIZE as i64)
            .then(|| wal.slice(final_frame_offset, frame_size as usize))
            .flatten()
        {
            Some(frame) => (be_u32(&frame[0..]), frame[WAL_FRAME_HEADER_SIZE..].to_vec()),
            None => (0, Vec::new()),
        };
        self.last_l0_header = Some((
            tx_id,
            LastL0Header {
                wal_offset: info.offset,
                wal_size: sz,
                wal_salt1: rd_salt1,
                wal_salt2: rd_salt2,
                commit,
                final_pgno,
                final_page,
            },
        ));

        // Advance cursor and checksum state together, only after the file is sealed.
        self.position = Pos::new(tx_id, post_checksum);
        self.checksums = checksums;

        // Track the logical end of WAL content for checkpoint decisions
        // (db.go:1704-1718, issues #997/#927).
        let final_offset = info.offset + sz;
        self.last_synced_wal_offset = final_offset;
        self.synced_to_wal_end = match self.wal_file_size() {
            Ok(wal_size) => final_offset == wal_size,
            Err(_) => false,
        };

        Ok(true)
    }

    pub(super) fn collect_wal_pages(
        &self,
        wal: &WalImage,
        page_map: &HashMap<u32, i64>,
        prev_commit: u32,
        commit: u32,
    ) -> Result<Vec<(u32, Vec<u8>)>> {
        let mut pgnos: Vec<u32> = page_map.keys().copied().collect();
        let lock = lock_pgno(self.page_size);
        if commit > prev_commit {
            for pgno in (prev_commit + 1)..=commit {
                if pgno != lock && !page_map.contains_key(&pgno) {
                    pgnos.push(pgno);
                }
            }
        }
        pgnos.sort_unstable();

        let mut out = Vec::with_capacity(pgnos.len());
        for pgno in pgnos {
            let data = match page_map.get(&pgno) {
                Some(&offset) => wal.page(offset, self.page_size)?,
                None => self.read_db_page(pgno)?,
            };
            out.push((pgno, data));
        }
        Ok(out)
    }

    pub(super) fn collect_snapshot_pages(
        &self,
        wal: &WalImage,
        page_map: &HashMap<u32, i64>,
        commit: u32,
    ) -> Result<Vec<(u32, Vec<u8>)>> {
        let lock = lock_pgno(self.page_size);
        let mut out = Vec::with_capacity(commit as usize);
        for pgno in (1..=commit).filter(|pgno| *pgno != lock) {
            let data = match page_map.get(&pgno) {
                Some(&offset) => wal.page(offset, self.page_size)?,
                None => self.read_db_page(pgno)?,
            };
            out.push((pgno, data));
        }
        Ok(out)
    }

    pub(super) fn read_db_page(&self, pgno: u32) -> Result<Vec<u8>> {
        let offset = u64::from(pgno.checked_sub(1).ok_or(CrabError::LTXCorrupted)?)
            * u64::from(self.page_size);
        // Read through SQLite's file, below its WAL-aware pager. A sparse VFS
        // must hydrate holes here too, not only on application SQL reads.
        crate::managed::read_main(&self.conn, offset, self.page_size as usize)
    }
}
