// Derived from denoland/celld, commit 10cb1303dac710dcb3b557e318e08c855261f68b.
// Apache-2.0; see LICENSE and UPSTREAM.md. Modified by Crab contributors.
// Split from upstream db.rs; see UPSTREAM.md for Crab's changes.

use super::*;

impl CaptureEngine {
    pub(super) fn verify(&mut self) -> Result<SyncInfo> {
        let frame_size = self.page_size as i64 + WAL_FRAME_HEADER_SIZE as i64;
        let mut info = SyncInfo {
            snapshotting: true,
            ..Default::default()
        };

        let pos = self.position;
        if pos.txid == Txid(0) {
            info.offset = WAL_HEADER_SIZE as i64;
            return Ok(info); // first sync
        }

        // Only this session's successfully written cut can seed continuity.
        // Local directory listing must never promote unpublished state.
        let (txid, hdr) = self
            .last_l0_header
            .as_ref()
            .ok_or(CrabError::LTXCorrupted)?;
        if *txid != pos.txid {
            return Err(CrabError::LTXCorrupted);
        }
        let hdr = hdr.clone();
        info.offset = hdr.wal_offset + hdr.wal_size;
        info.salt1 = hdr.wal_salt1;
        info.salt2 = hdr.wal_salt2;
        info.prev_commit = hdr.commit;

        // If the LTX WAL offset exceeds the real WAL size, the WAL was truncated.
        let wal_size = self.wal_file_size()?;
        if info.offset > wal_size {
            // If we previously synced to the exact WAL end, this truncation is an
            // expected checkpoint: reset to the header and continue incrementally
            // rather than snapshotting (issue #927, db.go:1335-1355).
            if self.synced_to_wal_end {
                self.synced_to_wal_end = false;

                let wal_hdr = self.wal_header_bytes()?;
                info.offset = WAL_HEADER_SIZE as i64;
                info.salt1 = be_u32(&wal_hdr[16..]);
                info.salt2 = be_u32(&wal_hdr[20..]);
                info.snapshotting = false;
                return Ok(info);
            }

            return Ok(info);
        }

        // Compare WAL headers; restart from the beginning of the WAL if different.
        let wal_hdr = self.wal_header_bytes()?;
        let salt1 = be_u32(&wal_hdr[16..]);
        let salt2 = be_u32(&wal_hdr[20..]);
        let salt_match = salt1 == hdr.wal_salt1 && salt2 == hdr.wal_salt2;

        // Edge case: LTX represents the start of the WAL (WALOffset=32, WALSize=0).
        // Handle this before computing prev_wal_offset to avoid underflow
        // (32 - 4120 = -4088). See issue #900 (db.go:1375-1383).
        if info.offset == WAL_HEADER_SIZE as i64 {
            if salt_match {
                info.snapshotting = false;
                return Ok(info);
            }
            return Ok(info);
        }

        // If the offset is at the start of the first page, we can't check the
        // previous page (db.go:1386-1399).
        let prev_wal_offset = info.offset - frame_size;
        if prev_wal_offset == WAL_HEADER_SIZE as i64 {
            if salt_match {
                info.snapshotting = false;
                return Ok(info);
            }
            return Ok(info);
        } else if prev_wal_offset < WAL_HEADER_SIZE as i64 {
            return Err(CrabError::Other(
                format!("prev WAL offset is less than the header size: {prev_wal_offset}").into(),
            ));
        }

        // If we can't verify the last page is in the last LTX file, snapshot.
        let last_page_match = self.last_page_match_cached(&hdr, prev_wal_offset, frame_size)?;
        if !last_page_match {
            return Ok(info);
        }

        // Salt changed (possible FULL/RESTART checkpoint). With a last-page match
        // we assume the WAL was not overwritten (db.go:1412-1431).
        if !salt_match {
            info.offset = WAL_HEADER_SIZE as i64;
            info.salt1 = salt1;
            info.salt2 = salt2;

            let detected =
                self.detect_full_checkpoint(&[(salt1, salt2), (hdr.wal_salt1, hdr.wal_salt2)])?;
            if detected {
            } else {
                info.snapshotting = false;
            }
            return Ok(info);
        }

        info.snapshotting = false;
        Ok(info)
    }

    pub(super) fn last_page_match_cached(
        &mut self,
        hdr: &LastL0Header,
        prev_wal_offset: i64,
        frame_size: i64,
    ) -> Result<bool> {
        if prev_wal_offset <= WAL_HEADER_SIZE as i64 || hdr.final_page.is_empty() {
            return Ok(false);
        }
        let frame = self.wal_bytes_at(prev_wal_offset, frame_size)?;
        let pgno = be_u32(&frame[0..]);
        let fsalt1 = be_u32(&frame[8..]);
        let fsalt2 = be_u32(&frame[12..]);
        let data = &frame[WAL_FRAME_HEADER_SIZE..];
        Ok(fsalt1 == hdr.wal_salt1
            && fsalt2 == hdr.wal_salt2
            && pgno == hdr.final_pgno
            && data == hdr.final_page.as_slice())
    }

    pub(super) fn detect_full_checkpoint(&mut self, known_salts: &[(u32, u32)]) -> Result<bool> {
        let wal_bytes = self.read_whole_wal()?;
        let rd = WalReader::new(&wal_bytes).map_err(CrabError::from)?;
        let last_known = known_salts.last().copied().unwrap_or((0, 0));
        let mut m = rd.frame_salts_until(last_known);
        for s in known_salts {
            m.remove(s);
        }
        Ok(!m.is_empty())
    }
}
