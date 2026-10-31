//! Bounded ref-head preparation with complete failure cleanup ownership.
use super::{PreparedRefHead, RefHeadSnapshot, write_ref_head};
use crate::{Result, WriteError};
use crab_metadata::capsule_protocol::CapsuleRefHead;
use crab_storage::{ETag, Store, StoreLayout};
use futures_util::{StreamExt, stream::FuturesUnordered};

const CONCURRENCY: usize = 32;
type WrittenHead = (RefHeadSnapshot, CapsuleRefHead, ETag);

pub(super) async fn write_heads(
    router: &StoreLayout<Store>,
    activation_id: &str,
    prepared: Vec<PreparedRefHead>,
) -> (Vec<WrittenHead>, Option<WriteError>) {
    let mut written = Vec::with_capacity(prepared.len());
    let mut pending = prepared.into_iter().enumerate();
    let mut in_flight = FuturesUnordered::new();
    let mut failure = None;
    for (index, item) in pending.by_ref().take(CONCURRENCY) {
        in_flight.push(write_one(router, activation_id, index, item));
    }
    while let Some((index, result)) = in_flight.next().await {
        match result {
            Ok(head) => written.push(head),
            Err(error) => {
                if failure.as_ref().is_none_or(|(prior, _)| index < *prior) {
                    failure = Some((index, error));
                }
            }
        }
        // Stop admitting work after any failure, but drain every started write.
        // A short-circuiting join would lose successful CAS tokens needed below.
        if failure.is_none()
            && let Some((index, item)) = pending.next()
        {
            in_flight.push(write_one(router, activation_id, index, item));
        }
    }
    (written, failure.map(|(_, error)| error))
}

async fn write_one(
    router: &StoreLayout<Store>,
    activation_id: &str,
    index: usize,
    item: PreparedRefHead,
) -> (usize, Result<WrittenHead>) {
    let result = async {
        let new_state = item
            .candidate
            .visible(&std::collections::BTreeSet::new())
            .clone();
        let candidate = item.original.head.prepare(
            item.original.visible.clone(),
            activation_id.to_owned(),
            new_state,
        )?;
        let etag = write_ref_head(router, &item.original, &candidate).await?;
        Ok((item.original, candidate, etag))
    }
    .await;
    (index, result)
}
