//! Local ordering of a verified capture for one-shard-at-a-time integrity checks.
use super::*;

struct Grouped {
    key: [u8; 66],
    entry: Entry,
}

impl Grouped {
    fn new(entry: Entry) -> Result<Self> {
        let mut key = [0; 66];
        match entry.key[0] {
            0 => {
                key[0] = 1;
                key[1..33].copy_from_slice(&entry.value[8..]);
                key[33] = 1;
                key[34..].copy_from_slice(&entry.key[1..]);
            }
            1 => {
                key[0] = 1;
                key[1..33].copy_from_slice(&entry.key[1..]);
            }
            2 => key[1..33].copy_from_slice(&entry.key[1..]),
            _ => return Err(corrupt("grouped catalog row kind differs")),
        }
        Ok(Self { key, entry })
    }
}

impl Row for Grouped {
    type Key = [u8; 66];
    const TARGET_BYTES: usize = Entry::TARGET_BYTES;
    const MAX_BYTES: u64 = Entry::MAX_BYTES;
    fn key(&self) -> &Self::Key {
        &self.key
    }
    fn bytes(&self) -> usize {
        self.entry.bytes().saturating_add(self.key.len())
    }
    fn merge(&mut self, _: Self) -> Result<()> {
        Err(corrupt("normalized catalog grouping repeated an entry"))
    }
    fn encode(rows: &[Self]) -> Result<Bytes> {
        Entry::encode_rows(rows.iter().map(|row| &row.entry))
    }
    fn decode(bytes: Bytes) -> Result<Vec<Self>> {
        Entry::decode(bytes)?.into_iter().map(Self::new).collect()
    }
}

/// One record in a verified catalog ordered for shard integrity inspection.
#[derive(Debug, PartialEq, Eq)]
pub enum PointerCatalogEntry {
    Object(PointerCatalogObject),
    File {
        hash: String,
        entry: FileCatalogEntry,
    },
}

/// A verified capture ordered as xorbs followed by each shard and its file bindings.
///
/// File bindings within a shard follow file-hash order. Await `finish` on all
/// returned paths; dropping retains cleanup behind blocking workers.
#[must_use]
pub struct PointerCatalogShardInspection {
    space: Option<ScratchSpace>,
    rows: Cursor<Grouped>,
    counts: PointerCatalogCounts,
}

impl PointerCatalogInspection {
    /// Reorder the complete verified capture locally, grouping all final file bindings by shard.
    ///
    /// Prior cursor consumption does not omit records. The same private disk
    /// ceiling applies; input spill files retire before this method returns.
    /// No origin object is read or written. One recipe may reach its admitted bound.
    pub async fn group_by_shard(self) -> Result<PointerCatalogShardInspection> {
        let Self {
            space,
            rows,
            counts,
        } = self;
        let Some(space) = space else {
            return Ok(PointerCatalogShardInspection {
                space: None,
                rows: Cursor::new(Run::default()),
                counts,
            });
        };
        let result: Result<Run> = async {
            let mut rows = rows.restart();
            let mut grouped = Reducer::<Grouped>::new();
            while let Some(entry) = rows.next(&space).await? {
                grouped.add(&space, Grouped::new(entry)?).await?;
            }
            let run = grouped.finish(&space).await?;
            rows.into_run().remove(&space).await?;
            Ok(run)
        }
        .await;
        match result {
            Ok(run) => Ok(PointerCatalogShardInspection {
                space: Some(space),
                rows: Cursor::new(run),
                counts,
            }),
            Err(error) => finish(Some(space), Err(error)).await,
        }
    }
}

impl PointerCatalogShardInspection {
    /// Return complete cardinalities, independent of iteration progress.
    #[must_use]
    pub fn counts(&self) -> PointerCatalogCounts {
        self.counts
    }

    /// Return the next external object or final file binding in shard-grouped order.
    ///
    /// A pending read can be cancelled and resumed without losing its next entry.
    pub async fn next_entry(&mut self) -> Result<Option<PointerCatalogEntry>> {
        let row = match &self.space {
            Some(space) => self.rows.next(space).await?,
            None => None,
        };
        let Some(row) = row else {
            return Ok(None);
        };
        let entry = row.entry;
        if entry.key[0] != 0 {
            return entry
                .external_object()
                .map(PointerCatalogEntry::Object)
                .map(Some);
        }
        let hash = hex(&entry.key[1..])?;
        let size = u64::from_be_bytes(
            entry.value[..8]
                .try_into()
                .map_err(|_| corrupt("grouped file size is truncated"))?,
        );
        Ok(Some(PointerCatalogEntry::File {
            hash,
            entry: FileCatalogEntry::new(size, hex(&entry.value[8..])?),
        }))
    }

    /// Drain and remove the private spill, preserving the operation's primary error.
    pub async fn finish<T, E: From<MetadataError>>(
        self,
        result: std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        finish(self.space, result).await
    }
}
