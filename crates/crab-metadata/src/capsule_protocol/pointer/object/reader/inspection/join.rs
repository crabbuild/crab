use super::*;
use crab_storage::scratch::{ScratchFile, ScratchSpace};
use std::collections::VecDeque;

const MAGIC: &[u8; 8] = b"CRBCSJ01";
const BLOCK_ROWS: usize = 4096;
const ROW_BYTES: usize = 114;
const MAX_BLOCK_BYTES: u64 = (12 + BLOCK_ROWS * ROW_BYTES) as u64;

pub(super) struct Record {
    key: [u8; 33],
    required: bool,
    defined: bool,
    expected: Option<[u8; 40]>,
    actual: Option<[u8; 40]>,
}

impl Record {
    pub(super) fn reference(kind: u8, hash: &[u8], defined: bool) -> Result<Self> {
        if hash.len() != 32 {
            return Err(corrupt("inspection reference hash length differs"));
        }
        let mut key = [0; 33];
        key[0] = kind;
        key[1..].copy_from_slice(hash);
        Ok(Self {
            key,
            required: !defined,
            defined,
            expected: None,
            actual: None,
        })
    }

    pub(super) fn chunk(hash: &[u8], placement: [u8; 40], actual: bool) -> Result<Self> {
        if hash.len() != 32 {
            return Err(corrupt("inspection chunk hash length differs"));
        }
        let mut key = [0; 33];
        key[0] = 2;
        key[1..].copy_from_slice(hash);
        Ok(Self {
            key,
            required: false,
            defined: false,
            expected: (!actual).then_some(placement),
            actual: actual.then_some(placement),
        })
    }

    fn merge(&mut self, other: Self) -> Result<()> {
        self.required |= other.required;
        self.defined |= other.defined;
        self.expected = match (self.expected, other.expected) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        };
        if self.actual.is_some() && other.actual.is_some() {
            return Err(corrupt("catalog reverse placement occurs more than once"));
        }
        self.actual = self.actual.or(other.actual);
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        match self.key[0] {
            0 | 1 if !self.required || self.defined => Ok(()),
            2 if self.expected.is_some() && self.actual == self.expected => Ok(()),
            _ => Err(corrupt(
                "catalog dependency or canonical reverse placement differs",
            )),
        }
    }

    fn encode(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&self.key);
        bytes.push(
            u8::from(self.required)
                | (u8::from(self.defined) << 1)
                | (u8::from(self.expected.is_some()) << 2)
                | (u8::from(self.actual.is_some()) << 3),
        );
        bytes.extend_from_slice(&self.expected.unwrap_or([0; 40]));
        bytes.extend_from_slice(&self.actual.unwrap_or([0; 40]));
    }
}

struct Run {
    files: Vec<ScratchFile>,
}

struct Cursor {
    files: VecDeque<ScratchFile>,
    rows: std::vec::IntoIter<Record>,
    previous: Option<[u8; 33]>,
}

impl Cursor {
    fn new(run: Run) -> Self {
        Self {
            files: run.files.into(),
            rows: Vec::new().into_iter(),
            previous: None,
        }
    }

    async fn next(&mut self, space: &ScratchSpace) -> Result<Option<Record>> {
        loop {
            if let Some(row) = self.rows.next() {
                if self.previous.is_some_and(|previous| previous >= row.key) {
                    return Err(corrupt("inspection spill rows are not strictly ordered"));
                }
                self.previous = Some(row.key);
                return Ok(Some(row));
            }
            let Some(file) = self.files.pop_front() else {
                return Ok(None);
            };
            let body = space.read(&file, MAX_BLOCK_BYTES).await?;
            self.rows = decode(&body)?.into_iter();
        }
    }
}

fn encode(rows: &[Record]) -> Result<Bytes> {
    if rows.is_empty() || rows.len() > BLOCK_ROWS {
        return Err(corrupt("inspection spill block count exceeds its bound"));
    }
    let mut bytes = Vec::with_capacity(12 + rows.len() * ROW_BYTES);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(rows.len() as u32).to_be_bytes());
    for row in rows {
        row.encode(&mut bytes);
    }
    Ok(bytes.into())
}

fn decode(bytes: &[u8]) -> Result<Vec<Record>> {
    if bytes.len() < 12 || bytes.get(..8) != Some(MAGIC.as_slice()) {
        return Err(corrupt("inspection spill block framing differs"));
    }
    let count = u32::from_be_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| corrupt("spill count is truncated"))?,
    ) as usize;
    if count == 0 || count > BLOCK_ROWS || bytes.len() != 12 + count * ROW_BYTES {
        return Err(corrupt("inspection spill block count differs"));
    }
    bytes[12..]
        .chunks_exact(ROW_BYTES)
        .map(|row| {
            let key: [u8; 33] = row[..33]
                .try_into()
                .map_err(|_| corrupt("spill key is truncated"))?;
            let flags = row[33];
            if flags == 0
                || flags > 15
                || key[0] > 2
                || (key[0] < 2 && flags & 12 != 0)
                || (key[0] == 2 && flags & 3 != 0)
                || (flags & 4 == 0 && row[34..74].iter().any(|byte| *byte != 0))
                || (flags & 8 == 0 && row[74..].iter().any(|byte| *byte != 0))
            {
                return Err(corrupt("inspection spill row shape differs"));
            }
            Ok(Record {
                key,
                required: flags & 1 != 0,
                defined: flags & 2 != 0,
                expected: if flags & 4 != 0 {
                    Some(
                        row[34..74]
                            .try_into()
                            .map_err(|_| corrupt("spill placement is truncated"))?,
                    )
                } else {
                    None
                },
                actual: if flags & 8 != 0 {
                    Some(
                        row[74..]
                            .try_into()
                            .map_err(|_| corrupt("spill placement is truncated"))?,
                    )
                } else {
                    None
                },
            })
        })
        .collect()
}

pub(super) struct Join {
    space: ScratchSpace,
    buffer: Vec<Record>,
    levels: Vec<Option<Run>>,
}

impl Join {
    pub(super) async fn create(parent: &std::path::Path, maximum: u64) -> Result<Self> {
        Ok(Self {
            space: ScratchSpace::create(parent, maximum.saturating_mul(4)).await?,
            buffer: Vec::with_capacity(BLOCK_ROWS),
            levels: Vec::new(),
        })
    }

    pub(super) async fn add(&mut self, row: Record) -> Result<()> {
        self.buffer.push(row);
        if self.buffer.len() == BLOCK_ROWS {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let mut rows = std::mem::replace(&mut self.buffer, Vec::with_capacity(BLOCK_ROWS));
        rows.sort_unstable_by_key(|row| row.key);
        let mut grouped: Vec<Record> = Vec::with_capacity(rows.len());
        for row in rows {
            match grouped.last_mut().filter(|prior| prior.key == row.key) {
                Some(prior) => prior.merge(row)?,
                None => grouped.push(row),
            }
        }
        let mut run = Run {
            files: vec![self.space.write(encode(&grouped)?).await?],
        };
        let mut level = 0;
        loop {
            if level == self.levels.len() {
                self.levels.push(None);
            }
            match self.levels[level].take() {
                Some(prior) => run = self.merge(prior, run).await?,
                None => {
                    self.levels[level] = Some(run);
                    return Ok(());
                }
            }
            level += 1;
        }
    }

    async fn merge(&self, left: Run, right: Run) -> Result<Run> {
        let retired = left
            .files
            .iter()
            .chain(&right.files)
            .cloned()
            .collect::<Vec<_>>();
        let mut left = Cursor::new(left);
        let mut right = Cursor::new(right);
        let mut a = left.next(&self.space).await?;
        let mut b = right.next(&self.space).await?;
        let mut rows = Vec::with_capacity(BLOCK_ROWS);
        let mut output = Run { files: Vec::new() };
        while a.is_some() || b.is_some() {
            let take_left = match (&a, &b) {
                (Some(a), Some(b)) => a.key <= b.key,
                (Some(_), None) => true,
                _ => false,
            };
            let row = if take_left {
                let mut row = a
                    .take()
                    .ok_or_else(|| corrupt("spill merge lost its left row"))?;
                a = left.next(&self.space).await?;
                if b.as_ref().is_some_and(|other| other.key == row.key) {
                    row.merge(
                        b.take()
                            .ok_or_else(|| corrupt("spill merge lost its right row"))?,
                    )?;
                    b = right.next(&self.space).await?;
                }
                row
            } else {
                let row = b
                    .take()
                    .ok_or_else(|| corrupt("spill merge lost its right row"))?;
                b = right.next(&self.space).await?;
                row
            };
            rows.push(row);
            if rows.len() == BLOCK_ROWS {
                output.files.push(self.space.write(encode(&rows)?).await?);
                rows.clear();
            }
        }
        if !rows.is_empty() {
            output.files.push(self.space.write(encode(&rows)?).await?);
        }
        for file in retired {
            self.space.remove(file).await?;
        }
        Ok(output)
    }

    pub(super) async fn finish(mut self, prior: Result<()>) -> Result<()> {
        let result: Result<()> = async {
            prior?;
            self.flush().await?;
            let mut combined = None;
            for run in std::mem::take(&mut self.levels).into_iter().flatten() {
                combined = Some(match combined.take() {
                    Some(prior) => self.merge(prior, run).await?,
                    None => run,
                });
            }
            if let Some(run) = combined {
                let mut cursor = Cursor::new(run);
                while let Some(row) = cursor.next(&self.space).await? {
                    row.validate()?;
                }
            }
            Ok(())
        }
        .await;
        let cleanup = self.space.close().await.map_err(MetadataError::from);
        if let (Err(_), Err(source)) = (&result, &cleanup) {
            tracing::warn!(%source, "inspection spill cleanup failed after validation error");
        }
        result?;
        cleanup
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spilled_join_uses_the_lowest_owner_across_separate_runs() {
        for correct in [false, true] {
            let parent = tempfile::tempdir().unwrap();
            let mut join = Join::create(parent.path(), 8 * 1024 * 1024).await.unwrap();
            let mut high = [9; 40];
            let mut low = high;
            low[..32].fill(1);
            high[32..36].copy_from_slice(&0u32.to_be_bytes());
            low[32..36].copy_from_slice(&0u32.to_be_bytes());
            join.add(Record::chunk(&[3; 32], high, false).unwrap())
                .await
                .unwrap();
            join.add(Record::chunk(&[3; 32], if correct { low } else { high }, true).unwrap())
                .await
                .unwrap();
            for index in 0..10_000u32 {
                let mut key = [0; 32];
                key[28..].copy_from_slice(&index.to_be_bytes());
                join.add(Record::reference(0, &key, true).unwrap())
                    .await
                    .unwrap();
            }
            join.add(Record::chunk(&[3; 32], low, false).unwrap())
                .await
                .unwrap();
            assert_eq!(join.finish(Ok(())).await.is_ok(), correct);
            assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn prior_scan_failure_drains_and_removes_written_runs() {
        let parent = tempfile::tempdir().unwrap();
        let mut join = Join::create(parent.path(), 8 * 1024 * 1024).await.unwrap();
        for index in 0..5000u32 {
            let mut key = [0; 32];
            key[28..].copy_from_slice(&index.to_be_bytes());
            join.add(Record::reference(0, &key, true).unwrap())
                .await
                .unwrap();
        }
        assert!(
            join.finish(Err(corrupt("source page missing")))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
    }

    #[test]
    fn spill_decoder_rejects_unbounded_counts_and_noncanonical_rows() {
        let row = Record::reference(0, &[1; 32], true).unwrap();
        let canonical = encode(&[row]).unwrap();
        for scenario in ["count", "truncated", "flags", "absent-value"] {
            let mut bytes = canonical.to_vec();
            match scenario {
                "count" => bytes[8..12].copy_from_slice(&u32::MAX.to_be_bytes()),
                "truncated" => {
                    bytes.pop();
                }
                "flags" => bytes[12 + 33] = 255,
                "absent-value" => bytes[12 + 34] = 1,
                _ => unreachable!(),
            }
            assert!(decode(&bytes).is_err(), "{scenario}");
        }
    }
}
