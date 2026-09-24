//! Idle-cell movement for operator-driven scale down.

use super::*;

impl CellNode {
    /// Lists settled local Cells as advisory candidates for the fleet planner.
    pub async fn idle_transfer_candidates(
        &self,
    ) -> crab_cell_runtime::Result<Vec<(CellId, u64, i64, crab_cell_runtime::CatalogRole)>> {
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        self.runtime.idle_transfer_candidates().await
    }
    /// Releases one exact settled Cell generation and waits for owner release.
    pub async fn release_idle_cell(
        &self,
        cell: CellId,
        source: SessionId,
        generation: u64,
    ) -> crab_cell_runtime::Result<()> {
        if !self.is_ready() {
            return Err(Error::CellDraining);
        }
        self.runtime
            .release_idle_cell(cell, source, generation)
            .await
    }
    /// Stops new Cell acquisition while retaining the lease and current owners.
    pub fn begin_scale_down(&self) -> crab_cell_runtime::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::Control("CellNode lifecycle lock poisoned"))?;
        match *state {
            NodeState::Ready => {
                self.runtime.stop_acquiring()?;
                *state = NodeState::ScalingDown;
                Ok(())
            }
            NodeState::ScalingDown => Ok(()),
            _ => Err(Error::CellDraining),
        }
    }
    /// Waits for confirmed releases. An incomplete result leaves the node
    /// serving and its facilities alive for the next fleet planning window.
    pub async fn drain_for_scale_down(
        &self,
        deadline: Instant,
    ) -> crab_cell_runtime::Result<ScaleDownStatus> {
        let _shutdown = self.shutdown_lock.lock().await;
        if self.state() == NodeState::Stopped {
            return Ok(ScaleDownStatus {
                remaining_cells: 0,
                settled_candidates: 0,
                released_cells: 0,
                blocked_cells: 0,
            });
        }
        self.begin_scale_down()?;
        let mut released_cells = 0_usize;
        let mut blocked = HashSet::new();
        loop {
            let candidates = self.runtime.idle_transfer_candidates().await?;
            for (cell, generation, _, _) in candidates.iter().copied() {
                if Instant::now() >= deadline {
                    break;
                }
                let result = tokio::time::timeout_at(
                    deadline.into(),
                    self.runtime
                        .release_idle_cell(cell, self.session, generation),
                )
                .await;
                match result {
                    Ok(Ok(())) => {
                        released_cells = released_cells.saturating_add(1);
                        blocked.remove(&cell);
                    }
                    Ok(Err(_)) => {
                        blocked.insert(cell);
                    }
                    Err(_) => break,
                }
            }
            let remaining_cells = self.runtime.unreleased_cell_count().await?;
            let current_candidates = self.runtime.idle_transfer_candidates().await?;
            let settled_candidates = current_candidates.len();
            let candidate_ids = current_candidates
                .iter()
                .map(|(cell, _, _, _)| *cell)
                .collect::<HashSet<_>>();
            blocked.retain(|cell| candidate_ids.contains(cell));
            let status = ScaleDownStatus {
                remaining_cells,
                settled_candidates,
                released_cells,
                blocked_cells: blocked
                    .len()
                    .saturating_add(remaining_cells.saturating_sub(settled_candidates)),
            };
            if status.ready_to_stop() {
                if Instant::now() >= deadline {
                    return Ok(status);
                }
                self.drain_until_locked(Some(deadline)).await?;
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Ok(status);
            }
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(1));
            tokio::time::sleep(wait).await;
        }
    }
}
