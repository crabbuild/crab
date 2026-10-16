//! Bounded sorted reductions over authenticated private spill blocks.
use super::*;
use crab_storage::scratch::{ScratchFile, ScratchSpace};

pub(super) trait Row: Sized {
    type Key: Ord + Clone;
    const TARGET_BYTES: usize;
    const MAX_BYTES: u64;
    fn key(&self) -> &Self::Key;
    fn bytes(&self) -> usize;
    fn merge(&mut self, other: Self) -> Result<()>;
    fn encode(rows: &[Self]) -> Result<Bytes>;
    fn decode(bytes: Bytes) -> Result<Vec<Self>>;
}

#[derive(Clone, Default)]
pub(super) struct Run {
    files: Vec<ScratchFile>,
}

impl Run {
    pub(super) async fn remove(self, space: &ScratchSpace) -> Result<()> {
        for file in self.files {
            space.remove(file).await?;
        }
        Ok(())
    }
}

pub(super) struct Cursor<R: Row> {
    files: Vec<ScratchFile>,
    next_file: usize,
    rows: std::vec::IntoIter<R>,
    previous: Option<R::Key>,
}

impl<R: Row> Cursor<R> {
    pub(super) fn new(run: Run) -> Self {
        Self {
            files: run.files,
            next_file: 0,
            rows: Vec::new().into_iter(),
            previous: None,
        }
    }

    pub(super) fn restart(mut self) -> Self {
        self.next_file = 0;
        self.rows = Vec::new().into_iter();
        self.previous = None;
        self
    }

    pub(super) fn into_run(self) -> Run {
        Run { files: self.files }
    }

    pub(super) async fn next(&mut self, space: &ScratchSpace) -> Result<Option<R>> {
        loop {
            if let Some(row) = self.rows.as_slice().first() {
                if self
                    .previous
                    .as_ref()
                    .is_some_and(|previous| previous >= row.key())
                {
                    return Err(corrupt("inspection spill rows are not strictly ordered"));
                }
                self.previous = Some(row.key().clone());
                return Ok(self.rows.next());
            }
            let Some(file) = self.files.get(self.next_file) else {
                return Ok(None);
            };
            // A caller can cancel and resume this read. Commit progression only
            // after verified I/O and decoding, or the unread block is lost.
            let rows = R::decode(space.read(file, R::MAX_BYTES).await?)?;
            self.next_file += 1;
            self.rows = rows.into_iter();
        }
    }
}

pub(super) struct Reducer<R> {
    buffer: Vec<R>,
    bytes: usize,
    levels: Vec<Option<Run>>,
}

impl<R: Row> Reducer<R> {
    pub(super) fn new() -> Self {
        Self {
            buffer: Vec::new(),
            bytes: 0,
            levels: Vec::new(),
        }
    }

    pub(super) async fn add(&mut self, space: &ScratchSpace, row: R) -> Result<()> {
        if !self.buffer.is_empty() && self.bytes.saturating_add(row.bytes()) > R::TARGET_BYTES {
            self.flush(space).await?;
        }
        self.bytes = self.bytes.saturating_add(row.bytes());
        self.buffer.push(row);
        if self.buffer.len() == 4096 || self.bytes >= R::TARGET_BYTES {
            self.flush(space).await?;
        }
        Ok(())
    }

    async fn flush(&mut self, space: &ScratchSpace) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let mut rows = std::mem::take(&mut self.buffer);
        self.bytes = 0;
        rows.sort_unstable_by(|a, b| a.key().cmp(b.key()));
        let mut grouped: Vec<R> = Vec::with_capacity(rows.len());
        for row in rows {
            match grouped.last_mut().filter(|prior| prior.key() == row.key()) {
                Some(prior) => prior.merge(row)?,
                None => grouped.push(row),
            }
        }
        let mut run = Run {
            files: vec![space.write(R::encode(&grouped)?).await?],
        };
        let mut level = 0;
        loop {
            if level == self.levels.len() {
                self.levels.push(None);
            }
            match self.levels[level].take() {
                Some(prior) => run = Self::merge(space, prior, run).await?,
                None => {
                    self.levels[level] = Some(run);
                    return Ok(());
                }
            }
            level += 1;
        }
    }

    async fn merge(space: &ScratchSpace, left: Run, right: Run) -> Result<Run> {
        let retired = left
            .files
            .iter()
            .chain(&right.files)
            .cloned()
            .collect::<Vec<_>>();
        let mut left = Cursor::<R>::new(left);
        let mut right = Cursor::<R>::new(right);
        let mut a = left.next(space).await?;
        let mut b = right.next(space).await?;
        let mut rows = Vec::new();
        let mut bytes = 0usize;
        let mut output = Run { files: Vec::new() };
        while a.is_some() || b.is_some() {
            let take_left = match (&a, &b) {
                (Some(a), Some(b)) => a.key() <= b.key(),
                (Some(_), None) => true,
                _ => false,
            };
            let row = if take_left {
                let mut row = a
                    .take()
                    .ok_or_else(|| corrupt("spill merge lost its left row"))?;
                a = left.next(space).await?;
                if b.as_ref().is_some_and(|other| other.key() == row.key()) {
                    row.merge(
                        b.take()
                            .ok_or_else(|| corrupt("spill merge lost its right row"))?,
                    )?;
                    b = right.next(space).await?;
                }
                row
            } else {
                let row = b
                    .take()
                    .ok_or_else(|| corrupt("spill merge lost its right row"))?;
                b = right.next(space).await?;
                row
            };
            if !rows.is_empty() && bytes.saturating_add(row.bytes()) > R::TARGET_BYTES {
                output.files.push(space.write(R::encode(&rows)?).await?);
                rows.clear();
                bytes = 0;
            }
            bytes = bytes.saturating_add(row.bytes());
            rows.push(row);
            if rows.len() == 4096 || bytes >= R::TARGET_BYTES {
                output.files.push(space.write(R::encode(&rows)?).await?);
                rows.clear();
                bytes = 0;
            }
        }
        if !rows.is_empty() {
            output.files.push(space.write(R::encode(&rows)?).await?);
        }
        Run { files: retired }.remove(space).await?;
        Ok(output)
    }

    pub(super) async fn finish(mut self, space: &ScratchSpace) -> Result<Run> {
        self.flush(space).await?;
        let mut combined = None;
        for run in self.levels.into_iter().flatten() {
            combined = Some(match combined.take() {
                Some(prior) => Self::merge(space, prior, run).await?,
                None => run,
            });
        }
        Ok(combined.unwrap_or_default())
    }
}
