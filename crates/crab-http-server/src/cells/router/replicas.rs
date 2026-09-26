//! Explicit replica routing, bounded readiness, and advisory successor selection.

use super::*;
use futures_util::{StreamExt, stream};

const READ_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
const READ_RECONCILE_BATCH: usize = 64;

#[derive(Default)]
pub(super) struct ReplicaRouting {
    state: std::sync::Mutex<ReplicaRoutingState>,
}

#[derive(Default)]
struct ReplicaRoutingState {
    cursor: usize,
    in_flight: HashMap<NodeId, usize>,
}

struct ReplicaAttempt<'a> {
    routing: &'a ReplicaRouting,
    node: NodeId,
}

impl ReplicaRouting {
    fn reserve(
        &self,
        candidates: impl ExactSizeIterator<Item = NodeId>,
    ) -> crab_cell_runtime::Result<(usize, ReplicaAttempt<'_>)> {
        let count = candidates.len();
        if count == 0 {
            return Err(crab_cell_runtime::Error::ReplicaUnavailable);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| crab_cell_runtime::Error::Control("replica routing load lock poisoned"))?;
        let start = state.cursor % count;
        let (index, node) = candidates
            .enumerate()
            .min_by_key(|(index, node)| {
                let distance = if *index >= start {
                    *index - start
                } else {
                    count - (start - *index)
                };
                (state.in_flight.get(node).copied().unwrap_or(0), distance)
            })
            .ok_or(crab_cell_runtime::Error::ReplicaUnavailable)?;
        state.cursor = index + 1;
        *state.in_flight.entry(node).or_default() += 1;
        Ok((
            index,
            ReplicaAttempt {
                routing: self,
                node,
            },
        ))
    }
}

impl Drop for ReplicaAttempt<'_> {
    fn drop(&mut self) {
        let mut state = self
            .routing
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(active) = state.in_flight.get_mut(&self.node) {
            *active -= 1;
            if *active == 0 {
                state.in_flight.remove(&self.node);
            }
        }
    }
}

#[derive(serde::Serialize)]
pub(crate) struct ReadReplicaStatus {
    pub(crate) owner_serving: bool,
    pub(crate) selected_readers: usize,
    pub(crate) ready_readers: usize,
    pub(crate) unverified_readers: usize,
    pub(crate) minimum_sequence: Option<u64>,
}

impl RepositoryCellRouter {
    pub(crate) async fn query_replica<Q: Query>(
        &self,
        repository: Uuid,
        principal: &Identity,
        local: &crate::cells::ReadReplicaManager,
        minimum: Option<Receipt>,
        input: Q::Input,
    ) -> crab_cell_runtime::Result<(Observed<Q::Output>, NodeId)>
    where
        Q::Input: Clone,
    {
        let target = self.repository_target(repository)?;
        let cell = target.cell_id();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let (expected, mut selected) =
            tokio::time::timeout_at(deadline, self.selected_readers(&target))
                .await
                .map_err(|_| crab_cell_runtime::Error::ReplicaUnavailable)??;
        if selected.is_empty() {
            return Err(crab_cell_runtime::Error::ReplicaUnavailable);
        }
        let client = ReplicaPeerClient::new(
            Arc::clone(&self.registry),
            Arc::clone(&self.peer.signer),
            PeerPrincipal {
                issuer: principal.issuer.clone(),
                subject: principal.subject.clone(),
                actions: vec!["repository.read".into()],
            },
            Arc::clone(&self.peer.round_trip),
        );
        let mut behind = None;
        let mut fenced = false;
        let attempts = async {
            while !selected.is_empty() {
                // This ingress counts outstanding attempts across Cells. Selection
                // and increment share a lock; cancellation drops the count. Only
                // active attempts retain entries, so membership churn cannot leak them.
                let (index, attempt) = self
                    .replica_routing
                    .reserve(selected.iter().map(NodeAdvertisement::node))?;
                let node = selected.remove(index);
                let reader_node = attempt.node;
                // Peer transport refuses self-dials; the admitted local view
                // runs the same post-query authority gate as a remote reader.
                let queried = if node.session() == self.peer.owner.session {
                    match local.resolve(target.clone()).await {
                        Ok(reader) => reader.query::<Q>(minimum, input.clone()).await,
                        Err(error) => Err(error),
                    }
                } else {
                    client
                        .query::<Q>(&target, node, expected, minimum, input.clone())
                        .await
                };
                drop(attempt);
                match queried {
                    Ok(result) => return Ok((result, reader_node)),
                    Err(error @ crab_cell_runtime::Error::ReplicaBehind { .. }) => {
                        behind = Some(error)
                    }
                    Err(crab_cell_runtime::Error::Fenced) => fenced = true,
                    Err(error @ crab_cell_runtime::Error::PeerAuthorization(_)) => {
                        return Err(error);
                    }
                    Err(error) => {
                        tracing::debug!(?cell, error = %error, "selected read replica unavailable")
                    }
                }
            }
            Err(behind.unwrap_or(if fenced {
                crab_cell_runtime::Error::Fenced
            } else {
                crab_cell_runtime::Error::ReplicaUnavailable
            }))
        };
        tokio::time::timeout_at(deadline, attempts)
            .await
            .unwrap_or(Err(crab_cell_runtime::Error::ReplicaUnavailable))
    }

    async fn selected_readers(
        &self,
        target: &CellTarget,
    ) -> crab_cell_runtime::Result<(CellDescription, Vec<NodeAdvertisement>)> {
        let cell = target.cell_id();
        let control = self
            .authority
            .load(cell)
            .await?
            .ok_or(crab_cell_runtime::Error::ReplicaUnavailable)?;
        let control = control.value();
        if control.state != ControlState::Serving || control.recovery.is_some() {
            return Err(crab_cell_runtime::Error::Fenced);
        }
        let owner = control
            .owner
            .as_ref()
            .ok_or(crab_cell_runtime::Error::Fenced)?;
        let expected = CellDescription {
            cell,
            incarnation: control.incarnation,
            code: control.code,
            schema: control.schema,
        };
        let Some(policy) = ReadPolicyStore::new(self.layout.clone()).load(cell).await? else {
            return Ok((expected, Vec::new()));
        };
        let policy = policy.value();
        if policy.incarnation() != control.incarnation || policy.desired_readers() == 0 {
            return Ok((expected, Vec::new()));
        }
        let selected = self
            .peer
            .directory
            .select_readers(
                cell,
                owner.session,
                control.code,
                usize::from(policy.desired_readers()),
                crate::cells::unix_now_ms()
                    .map_err(|_| crab_cell_runtime::Error::Command("clock failed"))?,
                10_000,
            )
            .await?;
        Ok((expected, selected))
    }

    pub(crate) async fn read_replica_status(
        &self,
        target: &CellTarget,
        local: &crate::cells::ReadReplicaManager,
    ) -> crab_cell_runtime::Result<ReadReplicaStatus> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let initial = tokio::time::timeout_at(deadline, self.authority.load(target.cell_id()))
            .await
            .map_err(|_| crab_cell_runtime::Error::Deadline)??
            .ok_or(crab_cell_runtime::Error::Fenced)?;
        if initial.value().state != ControlState::Serving
            || initial.value().recovery.is_some()
            || initial.value().owner.is_none()
        {
            return Ok(ReadReplicaStatus {
                owner_serving: false,
                selected_readers: 0,
                ready_readers: 0,
                unverified_readers: 0,
                minimum_sequence: None,
            });
        }
        let (expected, selected) = tokio::time::timeout_at(deadline, self.selected_readers(target))
            .await
            .map_err(|_| crab_cell_runtime::Error::Deadline)??;
        let mut status = ReadReplicaStatus {
            owner_serving: true,
            selected_readers: selected.len(),
            ready_readers: 0,
            unverified_readers: selected.len(),
            minimum_sequence: None,
        };
        {
            let probes = stream::iter(selected)
                .map(|node| self.replica_status_on_node(target, node, expected, local))
                .buffer_unordered(16);
            tokio::pin!(probes);
            let probe_deadline = deadline - Duration::from_secs(1);
            while let Ok(Some(result)) =
                tokio::time::timeout_at(probe_deadline, probes.next()).await
            {
                if let Ok((receipt, true)) = result {
                    status.ready_readers += 1;
                    status.unverified_readers -= 1;
                    status.minimum_sequence = Some(
                        status
                            .minimum_sequence
                            .map_or(receipt.commit_sequence, |sequence| {
                                sequence.min(receipt.commit_sequence)
                            }),
                    );
                }
            }
        }
        let current = tokio::time::timeout_at(deadline, self.authority.load(target.cell_id()))
            .await
            .map_err(|_| crab_cell_runtime::Error::Deadline)??
            .ok_or(crab_cell_runtime::Error::Fenced)?;
        let initial = initial.value();
        let current = current.value();
        if current.state != ControlState::Serving
            || current.recovery.is_some()
            || current.epoch != initial.epoch
            || current.owner != initial.owner
            || current.incarnation != expected.incarnation
            || current.code != expected.code
            || current.schema != expected.schema
        {
            return Err(crab_cell_runtime::Error::Fenced);
        }
        Ok(status)
    }

    async fn replica_status_on_node(
        &self,
        target: &CellTarget,
        node: NodeAdvertisement,
        expected: CellDescription,
        local: &crate::cells::ReadReplicaManager,
    ) -> crab_cell_runtime::Result<(Receipt, bool)> {
        if node.session() == self.peer.owner.session {
            return local.status(target.clone()).await;
        }
        let now_ms = crate::cells::unix_now_ms()
            .map_err(|_| crab_cell_runtime::Error::Command("clock failed"))?;
        let request = self.peer.signer.sign(
            self.runtime_principal(&["cell.replica.status"]),
            now_ms,
            now_ms.saturating_add(10_000),
            5_000,
            PeerOperation::Read(peer_wire::ReadRequest {
                target: Some(peer_target(target)),
                timeout_ms: 5_000,
                minimum: None,
                operation: Some(peer_wire::read_request::Operation::ReplicaStatus(true)),
            }),
        )?;
        let bytes = self
            .peer
            .round_trip
            .send_to_node(target.clone(), node, request, 5_000)
            .await?;
        let reply = crab_cell_runtime::peer::decode_peer_reply(&bytes)?;
        match reply.outcome {
            Some(peer_wire::peer_reply::Outcome::Read(peer_wire::ReadReply {
                receipt: Some(receipt),
                result: Some(peer_wire::read_reply::Result::ReplicaReady(ready)),
            })) if receipt.cell_id == expected.cell.as_bytes()
                && receipt.incarnation == expected.incarnation.as_bytes() =>
            {
                Ok((
                    Receipt {
                        cell: expected.cell,
                        incarnation: expected.incarnation,
                        commit_sequence: receipt.commit_sequence,
                    },
                    ready,
                ))
            }
            _ => Err(crab_cell_runtime::Error::ReplicaUnavailable),
        }
    }

    pub(crate) async fn run_read_replica_reconciliation(
        &self,
        cancellation: CancellationToken,
    ) -> crate::Result<()> {
        let mut tick = tokio::time::interval(READ_RECONCILE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut cursor = 0_usize;
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                _ = tick.tick() => {
                    if let Err(error) = self.reconcile_readers_once(&mut cursor).await {
                        tracing::warn!(error = %error, "Cell read replica reconciliation failed");
                    }
                }
            }
        }
    }

    async fn reconcile_readers_once(&self, cursor: &mut usize) -> crate::Result<()> {
        let mut entries = self.runtime.active_catalog_entries().await?;
        entries.retain(|entry| entry.role() == CatalogRole::Repository);
        entries.sort_by_key(|entry| entry.cell().as_bytes().to_owned());
        if entries.is_empty() {
            *cursor = 0;
            return Ok(());
        }
        let count = entries.len().min(READ_RECONCILE_BATCH);
        for offset in 0..count {
            let entry = &entries[(*cursor + offset) % entries.len()];
            let target = CellTarget::new(
                self.identity.tenant(),
                self.identity.application(),
                entry.namespace(),
                entry.partition(),
            )?;
            self.reconcile_reader_target(target).await?;
        }
        *cursor = (*cursor + count) % entries.len();
        Ok(())
    }

    pub(crate) async fn reconcile_reader_target(&self, target: CellTarget) -> crate::Result<()> {
        if target.tenant() != self.identity.tenant()
            || target.application() != self.identity.application()
            || target.namespace() != REPOSITORY_NAMESPACE
        {
            return Err(crab_cell_runtime::Error::PeerAuthorization(
                "read-replica target is outside the repository application",
            )
            .into());
        }
        let cell = target.cell_id();
        let policy = ReadPolicyStore::new(self.layout.clone());
        let Some(target_policy) = policy.load(cell).await? else {
            return Ok(());
        };
        let target_policy = target_policy.value();
        if target_policy.desired_readers() == 0 {
            return Ok(());
        }
        let Some(control) = self.authority.load(cell).await? else {
            return Ok(());
        };
        let control = control.value();
        if control.state != ControlState::Serving
            || control.owner.as_ref() != Some(&self.peer.owner)
            || target_policy.incarnation() != control.incarnation
        {
            return Ok(());
        }
        let now_ms = crate::cells::unix_now_ms()?;
        let selected = self
            .peer
            .directory
            .select_readers(
                cell,
                self.peer.owner.session,
                control.code,
                usize::from(target_policy.desired_readers()),
                now_ms,
                10_000,
            )
            .await?;
        for node in selected {
            if let Err(error) = self
                .peer
                .activate_read_replica(target.clone(), node, now_ms)
                .await
            {
                tracing::warn!(?cell, error = %error, "Cell read replica activation hint failed");
            }
        }
        Ok(())
    }

    pub(crate) async fn hint_read_replica_target(&self, target: CellTarget) -> crate::Result<()> {
        let Some(control) = self.authority.load(target.cell_id()).await? else {
            return Ok(());
        };
        let Some(owner) = control.value().owner.as_ref() else {
            return Ok(());
        };
        if owner.session == self.peer.owner.session {
            return self.reconcile_reader_target(target).await;
        }
        let now_ms = crate::cells::unix_now_ms()?;
        let request = self.peer.signer.sign(
            self.runtime_principal(&["cell.replica.reconcile"]),
            now_ms,
            now_ms.saturating_add(60_000),
            30_000,
            PeerOperation::Read(peer_wire::ReadRequest {
                target: Some(peer_target(&target)),
                timeout_ms: 30_000,
                minimum: None,
                operation: Some(peer_wire::read_request::Operation::ReplicaReconcile(true)),
            }),
        )?;
        let reply = self.peer.round_trip.send(target, request, 30_000).await?;
        let reply = crab_cell_runtime::peer::decode_peer_reply(&reply)?;
        match reply.outcome {
            Some(peer_wire::peer_reply::Outcome::Read(peer_wire::ReadReply {
                receipt: None,
                result: Some(peer_wire::read_reply::Result::ReplicaReconciled(true)),
            })) => Ok(()),
            _ => Err(
                crab_cell_runtime::Error::Peer("owner rejected read-replica reconciliation").into(),
            ),
        }
    }

    pub(crate) fn with_read_replicas(
        mut self,
        readers: Option<crate::cells::ReadReplicaManager>,
    ) -> Self {
        self.read_replicas = readers;
        self
    }

    pub(super) async fn preferred_warm_reader(
        &self,
        target: &CellTarget,
        observed: &VersionedControl,
    ) -> crate::Result<Option<NodeAdvertisement>> {
        let Some(local) = &self.read_replicas else {
            return Ok(None);
        };
        let Some(owner) = &observed.value().owner else {
            return Ok(None);
        };
        if self.remote_owner_is_live(owner).await? {
            return Ok(None);
        }
        // Readiness only biases placement. The selected node must still prove
        // predecessor death, recover any old log, and win the normal epoch CAS.
        let probe = async {
            let (expected, selected) = self.selected_readers(target).await?;
            let probes = stream::iter(selected)
                .map(|node| async move {
                    self.replica_status_on_node(target, node.clone(), expected, local)
                        .await
                        .map(|_| node)
                })
                .buffered(16);
            tokio::pin!(probes);
            while let Some(result) = probes.next().await {
                if let Ok(node) = result {
                    return Ok(Some(node));
                }
            }
            Ok::<_, crab_cell_runtime::Error>(None)
        };
        match tokio::time::timeout(Duration::from_secs(5), probe).await {
            Ok(Ok(node)) => Ok(node),
            // Warm placement is advisory; ordinary placement still proves
            // authority independently if snapshots cannot be verified in time.
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes() -> [NodeId; 3] {
        [1, 2, 3].map(|byte| NodeId::from_bytes([byte; 16]))
    }

    #[test]
    fn idle_readers_share_ties_and_a_busy_reader_is_skipped() {
        let routing = ReplicaRouting::default();
        let nodes = nodes();
        let mut counts = [0; 3];
        for _ in 0..12 {
            let (index, _attempt) = routing.reserve(nodes.into_iter()).unwrap();
            counts[index] += 1;
        }
        assert_eq!(counts, [4, 4, 4]);

        let (_, busy) = routing.reserve(nodes.into_iter()).unwrap();
        assert_eq!(busy.node, nodes[0]);
        counts = [0; 3];
        for _ in 0..12 {
            let (index, _attempt) = routing.reserve(nodes.into_iter()).unwrap();
            counts[index] += 1;
        }
        assert_eq!(counts, [0, 6, 6]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_attempt_releases_load_for_overlapping_candidate_sets() {
        let routing = Arc::new(ReplicaRouting::default());
        let nodes = nodes();
        let blocked_routing = Arc::clone(&routing);
        let (started, ready) = tokio::sync::oneshot::channel();
        let blocked = tokio::spawn(async move {
            let (_, attempt) = blocked_routing.reserve(nodes.into_iter()).unwrap();
            started.send(attempt.node).unwrap();
            std::future::pending::<()>().await;
            drop(attempt);
        });
        assert_eq!(ready.await.unwrap(), nodes[0]);
        // A different Cell can share the busy physical reader. Its local load
        // must carry across the two candidate sets without pinning membership.
        let (index, attempt) = routing.reserve([nodes[0], nodes[2]].into_iter()).unwrap();
        assert_eq!(index, 1);
        drop(attempt);
        blocked.abort();
        assert!(blocked.await.unwrap_err().is_cancelled());
        assert!(routing.state.lock().unwrap().in_flight.is_empty());
    }

    #[tokio::test]
    async fn expired_route_attempt_releases_its_load() {
        let routing = ReplicaRouting::default();
        let nodes = nodes();
        let expired = tokio::time::timeout(Duration::from_millis(10), async {
            let (_, _attempt) = routing.reserve(nodes.into_iter()).unwrap();
            std::future::pending::<()>().await;
        })
        .await;
        assert!(expired.is_err());
        assert!(routing.state.lock().unwrap().in_flight.is_empty());
    }
}
