//! Bounded movement of settled range owners using signed fleet capacity.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use crab_cell_host::CellNodeTaskGroup;
use crab_cell_runtime::{
    cell::{actor::ACTIVE_CELL_NATIVE_BYTES, worker::ACTIVE_CELL_PAGE_CACHE_BYTES},
    fleet::placement::{CellTransferDemand, PlacementObservation, PlacementPlanner},
    identity::CellId,
    ltx::Limits,
};
use extenddb_storage::error::StorageError;

use super::{CellInitialPartitionProvisioner, lease_time_ms, provision_error};

#[derive(Default)]
struct RangeRebalance {
    ranges: HashMap<CellId, RangeObservation>,
    moved_at_ms: i64,
}

struct RangeObservation {
    generation: u64,
    first_seen_ms: i64,
    sampled_at_ms: i64,
    samples: u8,
}

impl CellInitialPartitionProvisioner {
    /// Move settled data/index owners toward available fleet capacity while serving.
    ///
    /// Requires this provisioner's peer context. Shutdown cancels planning;
    /// accepted runtime releases and receiver activations retain their own fencing.
    pub fn install_range_rebalance_loop(
        self: &Arc<Self>,
        tasks: &CellNodeTaskGroup,
    ) -> Result<(), StorageError> {
        let peers = self
            .peers
            .clone()
            .ok_or_else(|| StorageError::Validation("range rebalancing requires peers".into()))?;
        let provisioner = Arc::clone(self);
        let cancellation = tasks.cancellation_token();
        tasks
            .spawn(async move {
                let mut state = RangeRebalance::default();
                let mut ticks = tokio::time::interval(Duration::from_secs(15));
                ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        () = cancellation.cancelled() => return Ok::<(), StorageError>(()),
                        _ = ticks.tick() => {}
                    }
                    let result = tokio::select! {
                        () = cancellation.cancelled() => return Ok(()),
                        result = provisioner.rebalance_ranges(&peers, &mut state) => result,
                    };
                    if let Err(error) = result {
                        tracing::warn!(%error, "range rebalancing deferred");
                    }
                }
            })
            .map_err(provision_error)
    }

    async fn rebalance_ranges(
        &self,
        peers: &crate::BeyonddbPeers,
        state: &mut RangeRebalance,
    ) -> Result<(), StorageError> {
        let nodes = peers
            .directory()
            .live(lease_time_ms()?, 1_024)
            .await
            .map_err(provision_error)?;
        let active = self
            .runtime
            .active_cell_targets()
            .await
            .map_err(provision_error)?
            .into_iter()
            .filter(|target| {
                [crate::DATA_NAMESPACE, crate::global_index::NAMESPACE]
                    .contains(&target.namespace())
            })
            .map(|target| (target.cell_id(), target))
            .collect::<HashMap<_, _>>();
        let candidates = self
            .runtime
            .idle_transfer_candidates()
            .await
            .map_err(provision_error)?;
        let now = lease_time_ms()?;
        let observations = nodes
            .iter()
            .filter_map(|node| {
                PlacementObservation::from_signed_advertisement(node, now, false).ok()
            })
            .collect::<Vec<_>>();
        let Some(source) = observations
            .iter()
            .find(|node| node.session == self.session)
        else {
            return Ok(());
        };
        state.ranges.retain(|cell, _| active.contains_key(cell));
        let settled = candidates
            .iter()
            .map(|(cell, _, _, _)| *cell)
            .collect::<HashSet<_>>();
        for (cell, range) in &mut state.ranges {
            if !settled.contains(cell) {
                range.samples = 0;
            }
        }
        let planner = PlacementPlanner::default();
        // A partial fleet would undercount ownership and authorize excess moves.
        // Pressure shedding retains the planner's separate per-node resource gates.
        let balance = if observations.len() == nodes.len() {
            planner
                .fleet_balance(now, &observations, state.moved_at_ms)
                .map_err(provision_error)?
        } else {
            None
        };
        let demands = candidates
            .into_iter()
            .filter_map(|(cell, generation, _, _)| {
                active.get(&cell)?;
                let range = state.ranges.entry(cell).or_insert(RangeObservation {
                    generation: 0,
                    first_seen_ms: now,
                    sampled_at_ms: 0,
                    samples: 0,
                });
                if range.generation != 0 && range.generation != generation {
                    range.first_seen_ms = now;
                    range.samples = 0;
                }
                range.generation = generation;
                if range.sampled_at_ms != source.observed_at_ms {
                    range.sampled_at_ms = source.observed_at_ms;
                    range.samples = range.samples.saturating_add(1);
                }
                Some(CellTransferDemand {
                    cell,
                    source: self.session,
                    generation,
                    memory_bytes: ACTIVE_CELL_NATIVE_BYTES + ACTIVE_CELL_PAGE_CACHE_BYTES,
                    disk_bytes: Limits::default().max_database_bytes
                        + Limits::default().max_capture_bytes,
                    job_credits: 2,
                    resident_since_ms: range.first_seen_ms,
                    last_moved_at_ms: None,
                    stable_observations: range.samples,
                    settled: true,
                })
            })
            .collect::<Vec<_>>();
        let moves = planner
            .plan_transfers(now, &observations, &demands, balance.as_ref())
            .map_err(provision_error)?
            .into_iter()
            .filter_map(|intent| {
                let target = active.get(&intent.cell)?.clone();
                let node = nodes
                    .iter()
                    .find(|node| node.session() == intent.destination)?
                    .clone();
                Some((intent, target, node))
            })
            .collect::<Vec<_>>();
        for (intent, target, node) in moves {
            {
                // Serialize release with local restore and reclamation. The actor
                // rechecks generation and settled work against foreground races.
                let _admission = self.admission.lock().await;
                if let Err(error) = self
                    .runtime
                    .release_idle_cell(intent.cell, self.session, intent.generation)
                    .await
                {
                    tracing::debug!(cell = ?intent.cell, %error, "range movement lost admission");
                    continue;
                }
                state.ranges.remove(&intent.cell);
                // Wait for advertisements newer than this release before using
                // counts again, including when the destination cannot activate.
                state.moved_at_ms = lease_time_ms()?;
            }
            // A failed or lost reply leaves the published root discoverable,
            // either Idle or claimed by the receiver. Existing recovery resumes
            // that authority state without rewriting the table directory.
            if let Err(error) = peers.activate_remote_range(&target, node).await {
                tracing::warn!(cell = ?intent.cell, %error, "released range awaits activation");
            }
        }
        Ok(())
    }
}
