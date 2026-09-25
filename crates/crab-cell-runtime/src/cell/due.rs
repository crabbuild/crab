//! Bounded due hints: one small key per released Cell deadline.
//!
//! A hint is an accelerator, never authority. The owner writes one key when it
//! releases a Cell that still has a due time, and the scheduler lists the
//! buckets up to now and confirms each candidate against its own control before
//! ticking it. A missing, stale, or unreadable hint costs a slower discovery,
//! never a missed or duplicated deadline: the full catalog scan remains the
//! backstop, and the Cell's own SQLite state decides what is due.

use crab_ltx::CellStorageLayout;
use object_store::path::Path;

use crate::identity::{CellId, decode_hex};
use crate::{Error, Result};

/// Milliseconds covered by one hint bucket.
pub const HINT_BUCKET_MS: i64 = 60_000;
/// Buckets a listing walks back from the current bucket.
pub const HINT_LOOKBACK_BUCKETS: u32 = 5;
/// Hints one listing returns.
const MAX_HINT_BATCH: usize = 128;

/// Records one released Cell's next due time.
///
/// The write is best effort by contract: the caller may ignore an error and
/// still be correct, because the backstop scan covers a missing hint.
///
/// A deadline already older than the listing window is not written at all: no
/// listing would ever see that key, so writing it would only leave an object
/// behind that nothing consumes. The backstop covers that deadline instead.
pub async fn publish(
    layout: &CellStorageLayout,
    cell: CellId,
    due_ms: i64,
    now_ms: i64,
) -> Result<()> {
    let bucket = bucket_for(due_ms)?;
    let current = bucket_for(now_ms)?;
    if current.saturating_sub(bucket) > u64::from(HINT_LOOKBACK_BUCKETS) {
        return Ok(());
    }
    layout
        .store()
        .put(
            &layout.due_hint_path(bucket, cell.as_bytes()),
            bytes::Bytes::new(),
        )
        .await?;
    Ok(())
}

/// Lists and consumes up to `limit` hints whose bucket has arrived.
///
/// A returned hint is deleted: the scheduler holds the candidate list, the
/// backstop covers anything the caller loses, and a stale hint would otherwise
/// be listed on every cycle.
pub async fn take(layout: &CellStorageLayout, now_ms: i64, limit: usize) -> Result<Vec<CellId>> {
    let bucket = bucket_for(now_ms)?;
    let mut cells = Vec::new();
    for offset in 0..=HINT_LOOKBACK_BUCKETS {
        if cells.len() >= limit || bucket < u64::from(offset) {
            break;
        }
        let bucket = bucket - u64::from(offset);
        list_bucket(layout, bucket, limit - cells.len(), &mut cells).await?;
    }
    Ok(cells)
}

/// Returns the bucket one due time belongs to.
pub fn bucket_for(due_ms: i64) -> Result<u64> {
    if due_ms < 0 {
        return Err(Error::Control("due hint time is negative"));
    }
    Ok(due_ms as u64 / HINT_BUCKET_MS as u64)
}

async fn list_bucket(
    layout: &CellStorageLayout,
    bucket: u64,
    limit: usize,
    cells: &mut Vec<CellId>,
) -> Result<()> {
    let prefix = layout.due_hint_prefix(bucket);
    let mut stream = layout.store().list_stream(&prefix);
    let max_cells = cells.len().saturating_add(limit.min(MAX_HINT_BATCH));
    while cells.len() < max_cells {
        let Some(item) = futures_util::StreamExt::next(&mut stream).await else {
            break;
        };
        let meta = item?;
        let Some(cell) = hint_cell(layout, bucket, &meta.location) else {
            // A key this module did not write must not be listed forever.
            let _ = layout.store().delete(&meta.location).await;
            continue;
        };
        let _ = layout.store().delete(&meta.location).await;
        cells.push(cell);
    }
    Ok(())
}

/// Accepts only the canonical key for a Cell in this bucket.
fn hint_cell(layout: &CellStorageLayout, bucket: u64, location: &Path) -> Option<CellId> {
    let stem = location.filename()?.strip_suffix(".json")?;
    let bytes = decode_hex::<32>(stem).ok()?;
    (layout.due_hint_path(bucket, &bytes) == *location).then_some(CellId::from_bytes(bytes))
}
