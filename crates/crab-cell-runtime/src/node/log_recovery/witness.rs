//! Durable witness records for one node-log recovery attempt.
//!
//! Recovery writes what it sealed (and later verifies it) through these
//! bounded, checksummed records so a retry can prove it replayed the same
//! frames instead of trusting an in-memory summary.

use super::*;

const WITNESS_RECORD_HEADER_BYTES: usize = 8 + 32;
pub(super) const WITNESS_DIGEST_RECORD_BYTES: u64 = 8 + 32;

pub(super) struct WitnessWriter {
    path: tempfile::TempPath,
    file: File,
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
    frame_count: u64,
}

impl WitnessWriter {
    fn new(directory: &Path) -> Result<Self> {
        let temporary = tempfile::Builder::new()
            .prefix(".crab-witness-")
            .tempfile_in(directory)?;
        let path = temporary.into_temp_path();
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(Self {
            path,
            file,
            first_sequence: None,
            last_sequence: None,
            frame_count: 0,
        })
    }

    fn push(&mut self, frame: &crab_ltx::VerifiedNodeFrame) -> Result<()> {
        let encoded = frame.encoded();
        let length = u64::try_from(encoded.len())
            .map_err(|_| Error::Node("recovery witness frame length overflows"))?;
        self.file.write_all(&length.to_le_bytes())?;
        self.file.write_all(&frame.digest())?;
        self.file.write_all(encoded)?;
        self.first_sequence
            .get_or_insert(frame.scope().node_sequence);
        self.last_sequence = Some(frame.scope().node_sequence);
        self.frame_count = self
            .frame_count
            .checked_add(1)
            .ok_or(Error::Node("recovery witness frame count overflows"))?;
        Ok(())
    }

    fn matches_range(&self, first: u64, last: u64) -> bool {
        self.first_sequence == Some(first)
            && self.last_sequence == Some(last)
            && self.frame_count == last.saturating_sub(first).saturating_add(1)
    }

    fn finish(self) -> Result<SealedWitness> {
        self.file.sync_all()?;
        if self.first_sequence.is_none() || self.last_sequence.is_none() {
            return Err(Error::Node("recovery witness is empty"));
        }
        Ok(SealedWitness {
            path: self.path,
            frame_count: self.frame_count,
        })
    }
}

pub(super) struct SealedWitness {
    path: tempfile::TempPath,
    pub(super) frame_count: u64,
}

impl SealedWitness {
    pub(super) fn reader(&self, limits: crab_ltx::Limits) -> Result<WitnessReader> {
        Ok(WitnessReader {
            file: File::open(&self.path)?,
            limits,
            remaining: self.frame_count,
        })
    }
}

/// Disk-backed sequence-to-digest table used to compare every reachable
/// follower without retaining one digest map per recovered frame in memory.
pub(super) struct WitnessDigestWriter {
    path: tempfile::TempPath,
    file: File,
    first_sequence: u64,
    expected_frames: u64,
    written_frames: u64,
}

pub(super) struct SealedWitnessDigests {
    _path: tempfile::TempPath,
    file: File,
    first_sequence: u64,
    frame_count: u64,
}

impl WitnessDigestWriter {
    pub(super) fn new(directory: &Path, first_sequence: u64, expected_frames: u64) -> Result<Self> {
        if expected_frames == 0 {
            return Err(Error::Node("recovery witness digest range is empty"));
        }
        let temporary = tempfile::Builder::new()
            .prefix(".crab-witness-digests-")
            .tempfile_in(directory)?;
        let path = temporary.into_temp_path();
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(Self {
            path,
            file,
            first_sequence,
            expected_frames,
            written_frames: 0,
        })
    }

    pub(super) fn push(&mut self, sequence: u64, digest: [u8; 32]) -> Result<()> {
        let expected = self
            .first_sequence
            .checked_add(self.written_frames)
            .ok_or(Error::Node("recovery witness digest sequence overflow"))?;
        if sequence != expected || self.written_frames == self.expected_frames {
            return Err(Error::Node("recovery witness digest range differs"));
        }
        self.file.write_all(&sequence.to_le_bytes())?;
        self.file.write_all(&digest)?;
        self.written_frames = self
            .written_frames
            .checked_add(1)
            .ok_or(Error::Node("recovery witness digest count overflow"))?;
        Ok(())
    }

    pub(super) fn finish(self) -> Result<SealedWitnessDigests> {
        if self.written_frames != self.expected_frames {
            return Err(Error::Node("recovery witness digest range is incomplete"));
        }
        self.file.sync_all()?;
        drop(self.file);
        let file = File::open(&self.path)?;
        Ok(SealedWitnessDigests {
            _path: self.path,
            file,
            first_sequence: self.first_sequence,
            frame_count: self.written_frames,
        })
    }
}

impl SealedWitnessDigests {
    pub(super) fn matches(&mut self, sequence: u64, digest: [u8; 32]) -> Result<()> {
        if sequence < self.first_sequence {
            return Ok(());
        }
        let offset = sequence
            .checked_sub(self.first_sequence)
            .and_then(|index| index.checked_mul(WITNESS_DIGEST_RECORD_BYTES))
            .ok_or(Error::Node("recovery witness digest offset overflow"))?;
        if sequence
            >= self
                .first_sequence
                .checked_add(self.frame_count)
                .ok_or(Error::Node("recovery witness digest range overflow"))?
        {
            return Ok(());
        }
        self.file.seek(SeekFrom::Start(offset))?;
        let mut record = [0_u8; WITNESS_DIGEST_RECORD_BYTES as usize];
        self.file.read_exact(&mut record)?;
        let stored_sequence = u64::from_le_bytes(
            record[..8]
                .try_into()
                .map_err(|_| Error::Node("recovery witness digest record is invalid"))?,
        );
        if stored_sequence != sequence || record[8..] != digest {
            return Err(Error::Node("follower witnesses disagree"));
        }
        Ok(())
    }
}

pub(super) struct WitnessReader {
    file: File,
    limits: crab_ltx::Limits,
    remaining: u64,
}

impl Iterator for WitnessReader {
    type Item = Result<crab_ltx::VerifiedNodeFrame>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let mut header = [0_u8; WITNESS_RECORD_HEADER_BYTES];
        if let Err(error) = self.file.read_exact(&mut header) {
            return Some(Err(error.into()));
        }
        let length = u64::from_le_bytes(header[..8].try_into().ok()?);
        let max_encoded = self.limits.max_capture_bytes.saturating_add(240);
        if length > max_encoded || length > usize::MAX as u64 {
            return Some(Err(Error::Node("recovery witness frame exceeds limit")));
        }
        let mut encoded = vec![0_u8; length as usize];
        if let Err(error) = self.file.read_exact(&mut encoded) {
            return Some(Err(error.into()));
        }
        if *blake3::hash(&encoded).as_bytes() != header[8..] {
            return Some(Err(Error::Node("recovery witness frame digest differs")));
        }
        self.remaining = self.remaining.saturating_sub(1);
        Some(crab_ltx::inspect_node_frame(encoded.into(), self.limits).map_err(Into::into))
    }
}

pub(super) enum WitnessCollector {
    Memory(Vec<crab_ltx::VerifiedNodeFrame>),
    File(WitnessWriter),
}

pub(super) enum WitnessMaterial {
    Memory(Vec<crab_ltx::VerifiedNodeFrame>),
    File(SealedWitness),
}

impl WitnessCollector {
    pub(super) fn file(directory: &Path) -> Result<Self> {
        Ok(Self::File(WitnessWriter::new(directory)?))
    }

    pub(super) fn push(&mut self, frames: Vec<crab_ltx::VerifiedNodeFrame>) -> Result<()> {
        match self {
            Self::Memory(existing) => existing.extend(frames),
            Self::File(writer) => {
                for frame in &frames {
                    writer.push(frame)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn matches_range(&self, first: u64, last: u64) -> bool {
        match self {
            Self::Memory(frames) => {
                frames.first().map(|frame| frame.scope().node_sequence) == Some(first)
                    && frames.last().map(|frame| frame.scope().node_sequence) == Some(last)
                    && frames.windows(2).all(|pair| {
                        pair[0].scope().node_sequence.checked_add(1)
                            == Some(pair[1].scope().node_sequence)
                    })
            }
            Self::File(writer) => writer.matches_range(first, last),
        }
    }

    pub(super) fn finish(self) -> Result<WitnessMaterial> {
        match self {
            Self::Memory(frames) => Ok(WitnessMaterial::Memory(frames)),
            Self::File(writer) => Ok(WitnessMaterial::File(writer.finish()?)),
        }
    }
}
