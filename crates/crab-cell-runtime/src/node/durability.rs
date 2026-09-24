//! Durability gate: commit tickets and fleet or object proofs.
use std::sync::Arc;

use futures_util::future::BoxFuture;
use tokio::sync::{Mutex, OnceCell};

use crate::identity::NodeId;
use crate::identity::SessionId;
use crate::node::lease::NodeLeaseGuard;
use crate::node::log::{
    CommitTicket, DurabilityGate, DurabilityProof, DurabilitySource, NodeLogRotationBarrier,
};
use crate::node::log_shipper::{NodeLogShipper, NodeLogSubmission};
use crate::node::log_transport::NodeLogTransport;
use crate::{Error, Result};

/// Authoritative node-session mutations required by follower durability.
///
/// Implementations must serialize these mutations with heartbeat refreshes and
/// reconcile an ambiguous CAS only when the exact session and log epoch match.
pub trait NodeLogAuthority: Send + Sync {
    fn activate<'a>(&'a self, log_epoch: u64) -> BoxFuture<'a, Result<()>>;

    fn advance_coverage<'a>(
        &'a self,
        log_epoch: u64,
        tiered_through: u64,
    ) -> BoxFuture<'a, Result<()>>;

    fn close<'a>(&'a self, barrier: &'a NodeLogRotationBarrier) -> BoxFuture<'a, Result<()>>;
}

/// Provider-neutral inputs for constructing one node-log durability epoch.
///
/// Providers own enrollment and the authority/transport implementations. The
/// host owns turning these inputs into the runtime durability object so the
/// product boundary cannot accidentally create a second shipping path.
pub struct NodeDurabilityConfig {
    session: SessionId,
    node: NodeId,
    log_epoch: u64,
    members: Vec<NodeId>,
    transport: Arc<dyn NodeLogTransport>,
    authority: Arc<dyn NodeLogAuthority>,
    node_lease: NodeLeaseGuard,
    limits: crab_ltx::Limits,
    telemetry: crate::fleet::telemetry::CellTelemetryHandle,
}

impl NodeDurabilityConfig {
    /// Binds provider enrollment to one exact node session and log epoch.
    #[expect(
        clippy::too_many_arguments,
        reason = "the provider-neutral boundary keeps every enrollment contract explicit"
    )]
    pub fn new(
        session: SessionId,
        node: NodeId,
        log_epoch: u64,
        members: Vec<NodeId>,
        transport: Arc<dyn NodeLogTransport>,
        authority: Arc<dyn NodeLogAuthority>,
        node_lease: NodeLeaseGuard,
        limits: crab_ltx::Limits,
        telemetry: crate::fleet::telemetry::CellTelemetryHandle,
    ) -> Result<Self> {
        if session.as_bytes().iter().all(|byte| *byte == 0)
            || node.as_bytes().iter().all(|byte| *byte == 0)
            || log_epoch == 0
            || members.is_empty()
        {
            return Err(Error::Node("invalid node-log durability configuration"));
        }
        Ok(Self {
            session,
            node,
            log_epoch,
            members,
            transport,
            authority,
            node_lease,
            limits,
            telemetry,
        })
    }

    /// Constructs the runtime-owned durability object for this epoch.
    pub fn build(self) -> Result<Arc<NodeDurability>> {
        let gate = DurabilityGate::new(self.session, self.node, self.log_epoch, self.members)?;
        let shipper = NodeLogShipper::new_with_telemetry(
            gate.clone(),
            Arc::clone(&self.transport),
            self.limits,
            self.telemetry,
        )?;
        Ok(Arc::new(NodeDurability::new(
            gate,
            shipper,
            self.authority,
            self.transport,
            self.node_lease,
        )))
    }
}

/// One enrolled node-log epoch and its non-forgeable durability proof boundary.
///
/// The runtime can submit captured cuts immediately after enrollment, but no
/// fleet proof becomes visible until the first ticket is fsynced by every
/// member and the authoritative inactive-to-active CAS succeeds.
pub struct NodeDurability {
    gate: DurabilityGate,
    shipper: NodeLogShipper,
    authority: Arc<dyn NodeLogAuthority>,
    transport: Arc<dyn NodeLogTransport>,
    node_lease: NodeLeaseGuard,
    activated: OnceCell<()>,
    object_coverage: Mutex<()>,
    shutdown: Mutex<()>,
    closed: std::sync::atomic::AtomicBool,
}

impl NodeDurability {
    #[must_use]
    pub fn new(
        gate: DurabilityGate,
        shipper: NodeLogShipper,
        authority: Arc<dyn NodeLogAuthority>,
        transport: Arc<dyn NodeLogTransport>,
        node_lease: NodeLeaseGuard,
    ) -> Self {
        Self {
            gate,
            shipper,
            authority,
            transport,
            node_lease,
            activated: OnceCell::new(),
            object_coverage: Mutex::new(()),
            shutdown: Mutex::new(()),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Returns whether shipping stopped or the epoch reached its rotation threshold.
    #[must_use]
    pub fn needs_rotation(&self, max_issued_frames: u64) -> bool {
        self.gate.shipping_scope().is_err() || self.gate.issued_through() >= max_issued_frames
    }

    /// Assigns and asynchronously ships one captured commit to every member.
    pub async fn submit(&self, submission: NodeLogSubmission) -> Result<CommitTicket> {
        self.node_lease.check()?;
        let ticket = tokio::select! {
            result = self.shipper.submit(submission) => result?,
            () = self.node_lease.wait_fenced() => return Err(Error::Fenced),
        };
        self.node_lease.check()?;
        Ok(ticket)
    }

    /// Returns fleet proof only after follower fsync and authoritative activation.
    pub async fn prove_fleet(&self, ticket: CommitTicket) -> Result<DurabilityProof> {
        self.node_lease.check()?;
        tokio::select! {
            result = self.gate.wait_followers(ticket) => result?,
            () = self.node_lease.wait_fenced() => return Err(Error::Fenced),
        }
        self.activated
            .get_or_try_init(|| async {
                self.node_lease.check()?;
                tokio::select! {
                    result = self.authority.activate(ticket.log_epoch()) => result?,
                    () = self.node_lease.wait_fenced() => return Err(Error::Fenced),
                }
                self.node_lease.check()?;
                self.gate.activate_fleet()?;
                Ok::<(), Error>(())
            })
            .await?;
        let proof = tokio::select! {
            result = self.gate.prove(ticket) => result?,
            () = self.node_lease.wait_fenced() => return Err(Error::Fenced),
        };
        self.node_lease.check()?;
        if proof.source() != DurabilitySource::Fleet {
            return Err(Error::Node(
                "fleet proof was superseded by object durability",
            ));
        }
        Ok(proof)
    }

    /// Returns the first valid follower or object proof for one ticket.
    pub async fn prove(&self, ticket: CommitTicket) -> Result<DurabilityProof> {
        self.node_lease.check()?;
        let activate = async {
            self.gate.wait_followers(ticket).await?;
            self.activated
                .get_or_try_init(|| async {
                    self.node_lease.check()?;
                    tokio::select! {
                        result = self.authority.activate(ticket.log_epoch()) => result?,
                        () = self.node_lease.wait_fenced() => return Err(Error::Fenced),
                    }
                    self.node_lease.check()?;
                    self.gate.activate_fleet()?;
                    Ok::<(), Error>(())
                })
                .await?;
            Ok::<(), Error>(())
        };
        tokio::pin!(activate);
        let proof = tokio::select! {
            proof = self.gate.prove(ticket) => proof?,
            activated = &mut activate => {
                activated?;
                self.gate.prove(ticket).await?
            }
            () = self.node_lease.wait_fenced() => return Err(Error::Fenced),
        };
        self.node_lease.check()?;
        Ok(proof)
    }

    /// Records an already-published object root and persists its contiguous watermark.
    ///
    /// Callers must complete the exact Cell root CAS before invoking this method.
    pub async fn prove_object(&self, ticket: CommitTicket) -> Result<DurabilityProof> {
        // Publishers from different Cells share this watermark. Serialize the
        // preview, authority CAS and local confirmation so concurrent completions
        // cannot leave the persisted prefix behind the local truncation proof.
        let _coverage = self.object_coverage.lock().await;
        self.node_lease.check()?;
        let tiered_through = self.gate.preview_object(ticket)?;
        tokio::select! {
            result = self.authority.advance_coverage(ticket.log_epoch(), tiered_through) => result?,
            () = self.node_lease.wait_fenced() => return Err(Error::Fenced),
        }
        self.node_lease.check()?;
        self.gate.prove_object(ticket)?;
        let proof = self.gate.prove(ticket).await?;
        if proof.source() != DurabilitySource::Object {
            return Err(Error::Node("object proof lost its durability race"));
        }
        Ok(proof)
    }

    /// Drains accepted frames and permanently closes fleet issuance for this epoch.
    pub async fn shutdown(&self) -> Result<()> {
        let _shutdown = self.shutdown.lock().await;
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }
        self.shipper.shutdown().await?;
        let barrier = self.gate.begin_rotation()?;
        crate::node::log::retire_node_log(Arc::clone(&self.transport), &barrier).await?;
        self.authority.close(&barrier).await?;
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
