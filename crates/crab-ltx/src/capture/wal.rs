// Derived from denoland/celld, commit 10cb1303dac710dcb3b557e318e08c855261f68b.
// Apache-2.0; see LICENSE and UPSTREAM.md. Modified by Crab contributors.
// Split from upstream db.rs; see UPSTREAM.md for Crab's changes.

use super::*;
use std::cell::Cell;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

const IN_MEMORY_INDEX_PAGE_LIMIT: usize = 64 << 10;

// The captured index only carries bytes when the replica feature is on; keeping
// the type uniform lets the cut path stay single-sourced without binding unit.
type CapturedIndex = Option<Vec<u8>>;

struct TimedWriter<W> {
    inner: W,
    host: crate::Host,
    write_nanos: Arc<AtomicU64>,
    bytes_written: u64,
    digest: blake3::Hasher,
}

impl<W: std::io::Write> std::io::Write for TimedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let started = self.host.now_monotonic();
        let result = self.inner.write(bytes);
        add_elapsed(&self.write_nanos, started, self.host.now_monotonic());
        if let Ok(written) = result {
            let written = written.min(bytes.len());
            self.bytes_written = self.bytes_written.saturating_add(written as u64);
            self.digest.update(&bytes[..written]);
        }
        result
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<W> TimedWriter<W> {
    fn finish(self) -> (W, u64, [u8; 32]) {
        (
            self.inner,
            self.bytes_written,
            *self.digest.finalize().as_bytes(),
        )
    }
}

struct TimedFileIo {
    inner: Box<dyn crate::environment::FileIo>,
    host: crate::Host,
    write_nanos: Arc<AtomicU64>,
}

impl crate::environment::FileIo for TimedFileIo {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let started = self.host.now_monotonic();
        let result = self.inner.write_all(bytes);
        add_elapsed(&self.write_nanos, started, self.host.now_monotonic());
        result
    }

    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        let started = self.host.now_monotonic();
        let result = self.inner.write_all_at(offset, bytes);
        add_elapsed(&self.write_nanos, started, self.host.now_monotonic());
        result
    }

    fn read_exact_at(&mut self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        self.inner.read_exact_at(offset, len)
    }

    fn sync_all(&mut self) -> std::io::Result<()> {
        self.inner.sync_all()
    }

    fn file_len(&self) -> std::io::Result<u64> {
        self.inner.file_len()
    }

    fn set_len(&mut self, len: u64) -> std::io::Result<()> {
        self.inner.set_len(len)
    }
}

fn add_elapsed(total: &AtomicU64, started: Instant, finished: Instant) {
    let elapsed = nanos(finished.saturating_duration_since(started));
    let _ = total.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_add(elapsed))
    });
}

impl CaptureEngine {
    pub(super) fn read_valid_wal_image(
        &mut self,
        info: &SyncInfo,
        start: usize,
    ) -> Result<WalImage> {
        let frame_size = self.page_size as usize + WAL_FRAME_HEADER_SIZE;
        let offset = info.offset;
        let salt1 = info.salt1;
        let salt2 = info.salt2;
        let read_bytes = Cell::new(0_u64);
        let file_bytes = Cell::new(0_u64);
        let result = self.with_wal_file(|file| {
            let file_len = file.file_len()? as usize;
            file_bytes.set(file_len as u64);
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
            read_bytes.set(read_bytes.get().saturating_add(bytes.len() as u64));
            let mut cursor = start;
            let mut target_frames = 1_usize;
            loop {
                let target_end = start
                    .saturating_add(target_frames.saturating_mul(frame_size))
                    .min(complete_end);
                if target_end > cursor {
                    let chunk = file.read_exact_at(cursor as u64, target_end - cursor)?;
                    read_bytes.set(read_bytes.get().saturating_add(chunk.len() as u64));
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
        });
        self.timing_observe_wal_transfer(file_bytes.get(), read_bytes.get());
        Ok(result?)
    }

    pub(super) fn sync_inner(&mut self, mut info: SyncInfo) -> Result<bool> {
        let frame_size_bytes = self.page_size as i64 + WAL_FRAME_HEADER_SIZE as i64;
        // Decide the representation before reading the WAL. A delta whose
        // worst-case encoded size exceeds the incremental bound is captured as
        // a full database image instead of fencing the session, and a full
        // image needs the whole WAL: pages that only an earlier, already
        // captured WAL segment holds are not in the database file yet. The
        // bound uses the uncaptured frame count, so it never understates the
        // delta the encoder would produce.
        let uncaptured_frames = if info.snapshotting {
            0
        } else {
            let uncaptured_bytes = self.wal_file_size()?.saturating_sub(info.offset).max(0) as u64;
            uncaptured_bytes.div_ceil(frame_size_bytes.max(1) as u64)
        };
        let full_image = info.snapshotting
            || ltx::cut_upper_bound(self.page_size, uncaptured_frames)?
                > self.max_incremental_bytes;
        if full_image && !info.snapshotting {
            // A full image is anchored at the WAL header so every frame the
            // current database state still depends on is in the page map.
            info.offset = WAL_HEADER_SIZE as i64;
        }
        // A capture that starts at the WAL header reads a logical WAL with no
        // backfilled prefix: the first sync, a restart, or a boundary image.
        // The checkpoint trigger counts from the backfilled boundary, so it
        // must not carry an offset from the WAL that just ended, or the new
        // WAL would grow past that offset before its first checkpoint.
        if info.offset == WAL_HEADER_SIZE as i64 {
            self.checkpointed_wal_offset = WAL_HEADER_SIZE as i64;
        }
        self.timing_begin(crate::capture::TimingPhase::WalRead);
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
        let mut sparse_tail = false;
        let mut fallback = false;
        if info.snapshotting {
            self.timing_observe_wal_snapshot();
        }
        let mut wal = if info.snapshotting || full_image {
            let bytes = self.read_whole_wal()?;
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
                    fallback = true;
                    let bytes = self.read_whole_wal()?;
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
                fallback = true;
                let bytes = self.read_whole_wal()?;
                wal = WalImage::whole(bytes);
            }
        }
        self.timing_observe_wal_image(sparse_tail && !fallback, fallback, wal.bytes.capacity());
        let mut rd = if info.offset == WAL_HEADER_SIZE as i64 {
            WalReader::new(&wal.bytes).map_err(CrabError::from)?
        } else {
            wal.reader_at(info.offset, info.salt1, info.salt2)
                .map_err(CrabError::from)?
        };

        self.timing_end(crate::capture::TimingPhase::WalRead);
        self.timing_begin(crate::capture::TimingPhase::PageCollection);
        let page_map_result = rd.page_map().map_err(CrabError::from);
        self.timing_end(crate::capture::TimingPhase::PageCollection);
        let (page_map, max_offset, wal_commit) = page_map_result?;
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

        self.timing_add_wal_bytes(u64::try_from(sz).unwrap_or_default());
        self.timing_add_database_bytes(u64::from(commit).saturating_mul(u64::from(self.page_size)));

        let (rd_salt1, rd_salt2) = rd.salt();

        // Build the page stream for the encoder.
        self.host
            .check_database_size(u64::from(commit) * u64::from(self.page_size))?;
        // Admit the selected representation against its bound. The full image
        // keeps the current TXID, pre-apply checksum, and chain position, so it
        // stays a valid successor cut of the same lineage.
        let encoded_pages = if full_image {
            u64::from(commit)
        } else {
            page_map
                .len()
                .saturating_add(commit.saturating_sub(info.prev_commit) as usize) as u64
        };
        let cut_limit = if full_image {
            self.host.max_file_bytes
        } else {
            self.max_incremental_bytes
        };
        if ltx::cut_upper_bound(self.page_size, encoded_pages)? > cut_limit {
            return Err(CrabError::Limit(crate::LimitKind::LtxFileBytes));
        }
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

        // Atomic tmp → fsync → rename (db.go:1609-1685).
        let tmp_filename = format!("{filename}.tmp");
        let index_filename = format!("{filename}.index.tmp");
        let parent = Path::new(&tmp_filename).parent().map(Path::to_path_buf);
        if !self.l0_dir_ready {
            if let Some(parent) = &parent {
                self.host.create_dir_all(parent)?;
            }
            self.l0_dir_ready = true;
            self.l0_ancestors_durable = false;
        }
        // A directory that vanished under a ready flag is recreated once and
        // the complete cut is retried. The candidate checksum index remains
        // isolated until the output has been synced and renamed.
        let write_result = self.write_streamed_cut(
            &tmp_filename,
            &index_filename,
            &filename,
            header,
            &wal,
            &page_map,
            full_image,
            cut_limit,
            info.prev_commit,
            commit,
            !self.defer_durability,
        );
        let (mut checksums, size_bytes, digest, captured_index) = match write_result {
            Err(CrabError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = &parent {
                    self.host.create_dir_all(parent)?;
                }
                self.l0_ancestors_durable = false;
                self.write_streamed_cut(
                    &tmp_filename,
                    &index_filename,
                    &filename,
                    header,
                    &wal,
                    &page_map,
                    full_image,
                    cut_limit,
                    info.prev_commit,
                    commit,
                    !self.defer_durability,
                )?
            }
            other => other?,
        };
        if !self.defer_durability {
            // The first acknowledged cut also needs the newly created path to survive.
            self.sync_l0_ancestors()?;
        }
        let post_checksum = checksums.checksum();
        // The checksum candidate remains isolated until the cut is durable. A
        // failed local index update fences the owning Db, so partially
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
        // Checkpointing can seal another cut before Db collects this one.
        // Retain each writer-produced digest so collection need not reread it.
        self.sealed_l0_segments.insert(
            tx_id.0,
            crate::SegmentInfo {
                min_txid: tx_id.0,
                max_txid: tx_id.0,
                page_size: self.page_size,
                database_pages: commit,
                pre_checksum: pos.post_apply_checksum,
                post_checksum,
                size_bytes,
                blake3: digest,
            },
        );
        #[cfg(feature = "replica")]
        if let Some(index) = captured_index {
            self.sealed_l0_captured_indexes.insert(tx_id.0, index);
        }
        #[cfg(not(feature = "replica"))]
        let _ = captured_index;

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

    #[expect(clippy::too_many_arguments)]
    fn write_streamed_cut(
        &mut self,
        tmp_filename: &str,
        index_filename: &str,
        filename: &str,
        header: ltx::Header,
        wal: &WalImage,
        page_map: &HashMap<u32, i64>,
        full_image: bool,
        limit: u64,
        prev_commit: u32,
        commit: u32,
        durable: bool,
    ) -> Result<(crate::pages::PageChecksums, u64, [u8; 32], CapturedIndex)> {
        #[cfg(feature = "replica")]
        let captured_index_budget = RETAINED_CAPTURE_INDEX_BYTES.saturating_sub(
            self.sealed_l0_captured_indexes
                .values()
                .fold(0_usize, |total, index| total.saturating_add(index.len())),
        );
        let result = (|| -> Result<(crate::pages::PageChecksums, u64, [u8; 32], CapturedIndex)> {
            // The cut is written through a host limited by the representation's
            // bound, so an encoder that ever exceeded its admitted bound fails
            // instead of publishing an oversized artifact.
            let output_host = crate::LtxHost {
                facilities: self.host.facilities.clone(),
                max_database_bytes: self.host.max_database_bytes,
                max_file_bytes: limit,
            };
            let output = output_host.create(Path::new(tmp_filename))?;
            let estimated_pages = if full_image {
                commit as usize
            } else {
                page_map
                    .len()
                    .saturating_add(commit.saturating_sub(prev_commit) as usize)
            };
            let spool_index = estimated_pages > IN_MEMORY_INDEX_PAGE_LIMIT;
            let index = if spool_index {
                let index = self
                    .host
                    .facilities
                    .filesystem
                    .create(Path::new(index_filename))?;
                drop(index);
                Some(
                    self.host
                        .facilities
                        .filesystem
                        .open_rw(Path::new(index_filename))?,
                )
            } else {
                None
            };
            let write_nanos = Arc::new(AtomicU64::new(0));
            let output = TimedWriter {
                inner: output,
                host: self.host.facilities.clone(),
                write_nanos: Arc::clone(&write_nanos),
                bytes_written: 0,
                digest: blake3::Hasher::new(),
            };
            let output = std::io::BufWriter::with_capacity(64 << 10, output);
            let index = index.map(|index| {
                Box::new(TimedFileIo {
                    inner: index,
                    host: self.host.facilities.clone(),
                    write_nanos: Arc::clone(&write_nanos),
                }) as Box<dyn crate::environment::FileIo>
            });
            let mut encoder = crate::codec::Encoder::new_block_with_index(output, index);
            #[cfg(feature = "replica")]
            let mut captured_index = Some(Vec::with_capacity(
                estimated_pages
                    .saturating_mul(crate::paged::ENTRY_BYTES)
                    .min(captured_index_budget),
            ));
            let encode_started = self.host.now_monotonic();
            encoder.encode_header(header)?;

            let mut checksums = self.checksums.clone();
            if full_image {
                let lock = lock_pgno(self.page_size);
                let pages = (1..=commit).filter(|page| *page != lock).map(|pgno| {
                    let data = self.capture_page(wal, page_map, pgno)?;
                    let encoded = encoder.encode_page(ltx::PageHeader { pgno, flags: 0 }, &data)?;
                    #[cfg(feature = "replica")]
                    retain_encoded_page(&mut captured_index, &encoded, captured_index_budget)?;
                    #[cfg(not(feature = "replica"))]
                    let _ = encoded;
                    Ok((pgno, data))
                });
                checksums.apply_iter(
                    self.page_size,
                    commit,
                    pages,
                    self.host.max_database_bytes,
                )?;
            } else {
                let pgnos = self.wal_page_numbers(page_map, prev_commit, commit);
                let pages = pgnos.into_iter().map(|pgno| {
                    let data = self.capture_page(wal, page_map, pgno)?;
                    let encoded = encoder.encode_page(ltx::PageHeader { pgno, flags: 0 }, &data)?;
                    #[cfg(feature = "replica")]
                    retain_encoded_page(&mut captured_index, &encoded, captured_index_budget)?;
                    #[cfg(not(feature = "replica"))]
                    let _ = encoded;
                    Ok((pgno, data))
                });
                checksums.apply_iter(
                    self.page_size,
                    commit,
                    pages,
                    self.host.max_database_bytes,
                )?;
            }
            encoder.close(checksums.checksum())?;
            #[cfg(not(feature = "replica"))]
            let captured_index = None;
            let output = encoder
                .into_writer()
                .into_inner()
                .map_err(|error| error.into_error())?;
            let encode_elapsed = nanos(
                self.host
                    .now_monotonic()
                    .saturating_duration_since(encode_started),
            );
            let local_write_nanos = write_nanos.load(Ordering::Relaxed);
            self.timing_add_phase_nanos(
                crate::capture::TimingPhase::Encode,
                encode_elapsed.saturating_sub(local_write_nanos),
            );
            self.timing_add_phase_nanos(crate::capture::TimingPhase::LocalWrite, local_write_nanos);
            let (mut output, size_bytes, digest) = output.finish();
            if durable {
                self.timing_begin(crate::capture::TimingPhase::Fsync);
                output.sync_all()?;
                self.timing_end(crate::capture::TimingPhase::Fsync);
            }
            drop(output);
            if spool_index {
                self.host.remove_file(Path::new(index_filename))?;
            }
            self.timing_begin(crate::capture::TimingPhase::ParentSync);
            if durable {
                self.host
                    .rename(Path::new(tmp_filename), Path::new(filename))?;
            } else {
                self.host
                    .rename_uncommitted(Path::new(tmp_filename), Path::new(filename))?;
            }
            self.timing_end(crate::capture::TimingPhase::ParentSync);
            Ok((checksums, size_bytes, digest, captured_index))
        })();
        if result.is_err() {
            let _ = self.host.remove_file(Path::new(tmp_filename));
            let _ = self.host.remove_file(Path::new(index_filename));
        }
        result
    }

    fn wal_page_numbers(
        &self,
        page_map: &HashMap<u32, i64>,
        prev_commit: u32,
        commit: u32,
    ) -> Vec<u32> {
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
        pgnos
    }

    pub(super) fn capture_page(
        &self,
        wal: &WalImage,
        page_map: &HashMap<u32, i64>,
        pgno: u32,
    ) -> Result<Vec<u8>> {
        match page_map.get(&pgno) {
            Some(&offset) => wal.page(offset, self.page_size),
            None => self.read_db_page(pgno),
        }
    }

    pub(super) fn read_db_page(&self, pgno: u32) -> Result<Vec<u8>> {
        let offset = u64::from(pgno.checked_sub(1).ok_or(CrabError::LTXCorrupted)?)
            * u64::from(self.page_size);
        // Read through SQLite's file, below its WAL-aware pager. A sparse VFS
        // must hydrate holes here too, not only on application SQL reads.
        crate::db::read_main(&self.conn, offset, self.page_size as usize)
    }
}

#[cfg(feature = "replica")]
fn retain_encoded_page(
    index: &mut Option<Vec<u8>>,
    page: &crate::codec::EncodedPage,
    budget: usize,
) -> Result<()> {
    let Some(bytes) = index else {
        return Ok(());
    };
    if crate::paged::ENTRY_BYTES > budget.saturating_sub(bytes.len()) {
        *index = None;
        return Ok(());
    }
    crate::paged::append_index_page(bytes, page)
}

#[cfg(all(test, feature = "replica"))]
mod tests {
    use super::*;

    fn encoded_page(page: u32) -> crate::codec::EncodedPage {
        crate::codec::EncodedPage {
            page,
            offset: u64::from(page) * 4096,
            size: 4096,
            frame_hash: [page as u8; 32],
            checksum: u64::from(page),
        }
    }

    #[test]
    fn captured_index_falls_back_when_page_would_exceed_budget() {
        let mut index = Some(Vec::new());

        retain_encoded_page(&mut index, &encoded_page(1), crate::paged::ENTRY_BYTES).unwrap();
        assert_eq!(index.as_ref().unwrap().len(), crate::paged::ENTRY_BYTES);

        retain_encoded_page(&mut index, &encoded_page(2), crate::paged::ENTRY_BYTES).unwrap();
        assert!(index.is_none());
    }
}
