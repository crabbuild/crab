//! Explicit replica routing, bounded readiness, and advisory successor selection.

use super::*;
use futures_util::{StreamExt, stream};

#[cfg(test)]
mod tests;

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
        local: &crab_cell_host::read_replicas::ReadReplicaManager,
        minimum: Option<Receipt>,
        input: Q::Input,
    ) -> crab_cell_runtime::Result<(Observed<Q::Output>, NodeId)> {
        let target = self.repository_target(repository)?;
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
        let local = Some((self.peer.owner.session, local as &dyn PeerReplicaResolver));
        self.replica_routing
            .query::<Q>(&client, local, &target, minimum, input)
            .await
    }

    pub(crate) async fn read_replica_status(
        &self,
        target: &CellTarget,
        local: &crab_cell_host::read_replicas::ReadReplicaManager,
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
        let (expected, selected) =
            tokio::time::timeout_at(deadline, self.replica_routing.selected(target))
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
        local: &crab_cell_host::read_replicas::ReadReplicaManager,
    ) -> crab_cell_runtime::Result<(Receipt, bool)> {
        if node.session() == self.peer.owner.session {
            return local.status(target.clone()).await;
        }
        ReplicaPeerClient::new(
            Arc::clone(&self.registry),
            Arc::clone(&self.peer.signer),
            self.runtime_principal(&["cell.replica.status"]),
            Arc::clone(&self.peer.round_trip),
        )
        .status(target, node, expected)
        .await
    }

    pub(crate) fn read_replica_peer(&self) -> ReplicaPeerClient {
        ReplicaPeerClient::new(
            Arc::clone(&self.registry),
            Arc::clone(&self.peer.signer),
            self.runtime_principal(&["cell.replica.activate"]),
            Arc::clone(&self.peer.round_trip),
        )
    }

    fn read_recruiter(&self) -> crate::Result<crab_cell_host::read_replicas::ReadReplicaRecruiter> {
        let readers = self
            .read_replicas
            .clone()
            .ok_or(crab_cell_runtime::Error::ReplicaUnavailable)?;
        Ok(crab_cell_host::read_replicas::ReadReplicaRecruiter::new(
            readers,
            self.identity,
            self.read_replica_peer(),
        )?)
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
        self.read_recruiter()?.reconcile(target).await?;
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
                expected: None,
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
        readers: Option<crab_cell_host::read_replicas::ReadReplicaManager>,
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
            let (expected, selected) = self.replica_routing.selected(target).await?;
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
