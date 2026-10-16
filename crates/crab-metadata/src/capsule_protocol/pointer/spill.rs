//! Bounded sorted reductions over authenticated private spill blocks.
use super::*;
use crab_storage::scratch::{ScratchFile, ScratchSpace};
use std::collections::VecDeque;

pub(super) trait Row: Sized {
    const TARGET_BYTES: usize;
    const MAX_BYTES: u64;
    fn key(&self) -> &[u8; 33];
    fn bytes(&self) -> usize;
    fn merge(&mut self, other: Self) -> Result<()>;
    fn encode(rows: &[Self]) -> Result<Bytes>;
    fn decode(bytes: Bytes) -> Result<Vec<Self>>;
}

pub(super) struct Run {
    files: Vec<ScratchFile>,
}

pub(super) struct Cursor<R> {
    files: VecDeque<ScratchFile>,
    rows: std::vec::IntoIter<R>,
    previous: Option<[u8; 33]>,
}

impl<R: Row> Cursor<R> {
    pub(super) fn new(run: Run) -> Self {
        Self {
            files: run.files.into(),
            rows: Vec::new().into_iter(),
            previous: None,
        }
    }

    pub(super) async fn next(&mut self, space: &ScratchSpace) -> Result<Option<R>> {
        loop {
            if let Some(row) = self.rows.next() {
                if self.previous.is_some_and(|previous| previous >= *row.key()) {
                    return Err(corrupt("inspection spill rows are not strictly ordered"));
                }
                self.previous = Some(*row.key());
                return Ok(Some(row));
            }
            let Some(file) = self.files.pop_front() else {
                return Ok(None);
            };
            self.rows = R::decode(space.read(&file, R::MAX_BYTES).await?)?.into_iter();
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
        for file in retired {
            space.remove(file).await?;
        }
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
        Ok(combined.unwrap_or(Run { files: Vec::new() }))
    }
}
