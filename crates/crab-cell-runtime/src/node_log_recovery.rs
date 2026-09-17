use std::sync::Arc;

use futures_util::future::join_all;

use crate::{
    ApplicationId, CellAuthority, Digest, Error, FencedNodeSession, NodeDirectory, NodeLogPhase,
    NodeLogTransport, RecoveryBase, RecoveryManifestStore, Result, SealRequest, SealedNodeLog,
    SessionId, TailRequest, Transition, VersionedControl, build_recovery_overlays,
};

/// Verified uncovered suffix gathered after every reachable follower is sealed.
pub struct SealedSession {
    pub leader_session: SessionId,
    pub log_epoch: u64,
    pub tiered_through: u64,
    pub durable_through: u64,
    pub frames: Vec<crab_ltx::VerifiedNodeFrame>,
}

/// Mechanical seal-and-gather coordinator for one already claimed dead session.
///
/// Session expiry and recovery-claim CAS remain authority concerns. This type
/// never decides that a leader is dead; it only gathers the exact lane named by
/// its constructor.
pub struct NodeLogRecovery {
    transport: Arc<dyn NodeLogTransport>,
    leader_session: SessionId,
    log_epoch: u64,
    members: Vec<SessionId>,
    tiered_through: u64,
    active: bool,
    limits: crab_ltx::Limits,
}

/// One dead-session Cell control that may need a recovered tail attached.
pub struct RecoveryCell {
    pub application: ApplicationId,
    pub authority: CellAuthority,
    pub observed: VersionedControl,
}

/// Seals one claimed node log and pins every recovered Cell tail before return.
pub struct RecoveryCoordinator {
    recovery: NodeLogRecovery,
    manifests: RecoveryManifestStore,
}

/// Completed dead-session recovery with every overlay pinned before log seal.
pub struct CompletedNodeRecovery {
    pub sealed: SealedNodeLog,
    pub controls: Vec<VersionedControl>,
}

impl RecoveryCoordinator {
    #[must_use]
    pub const fn new(recovery: NodeLogRecovery, manifests: RecoveryManifestStore) -> Self {
        Self {
            recovery,
            manifests,
        }
    }

    /// Attaches immutable overlays to the exact dead-owner controls.
    ///
    /// Successful earlier attachments remain valid if a later Cell conflicts;
    /// a retry must reload every control and rebuild against its exact root.
    pub async fn recover(
        &self,
        fenced: FencedNodeSession,
        cells: Vec<RecoveryCell>,
    ) -> Result<Vec<VersionedControl>> {
        self.recovery.validate_fence(&fenced)?;
        if cells.is_empty() {
            return Err(Error::Node("recovery claim or Cell inventory differs"));
        }
        let mut bases = Vec::with_capacity(cells.len());
        for cell in &cells {
            let control = cell.observed.value();
            let root = control
                .ltx_root()
                .ok_or(Error::Control("recovery Cell has no published root"))?;
            if control.owner.as_ref().map(|owner| owner.session) != Some(fenced.session())
                || *cell.application.as_bytes() == [0; 16]
                || control.recovery.as_ref().is_some_and(|recovery| {
                    recovery.leader_session != fenced.session()
                        || recovery.log_epoch != self.recovery.log_epoch
                })
            {
                return Err(Error::Control("recovery Cell scope differs"));
            }
            bases.push(RecoveryBase {
                application: *cell.application.as_bytes(),
                cell_epoch: control.epoch,
                root,
            });
        }

        let sealed = self.recovery.ensure_sealed().await?;
        if sealed.frames.is_empty() {
            return Ok(Vec::new());
        }
        let tails = build_recovery_overlays(sealed.frames, &bases, self.recovery.limits)?;
        let pinned = self
            .manifests
            .pin(sealed.leader_session, sealed.log_epoch, tails)
            .await?;
        if pinned.len() > cells.len() {
            return Err(Error::Node("recovery manifest exceeds Cell inventory"));
        }

        let mut attached = Vec::with_capacity(pinned.len());
        for pin in pinned {
            let cell = cells
                .iter()
                .find(|candidate| {
                    candidate.application == pin.application
                        && candidate.observed.value().cell == pin.cell
                        && candidate.observed.value().incarnation == pin.incarnation
                        && candidate.observed.value().epoch == pin.cell_epoch
                })
                .ok_or(Error::Node("recovered Cell is absent from inventory"))?;
            if let Some(current) = cell.observed.value().recovery.as_ref() {
                if current == &pin.recovery {
                    attached.push(cell.observed.clone());
                    continue;
                }
                return Err(Error::Control(
                    "different recovery overlay is already pinned",
                ));
            }
            let successor = cell.observed.value().attach_recovery(pin.recovery)?;
            let versioned = match cell
                .authority
                .transition(
                    &cell.observed,
                    successor.clone(),
                    Transition::AttachRecovery,
                )
                .await
            {
                Ok(versioned) => versioned,
                Err(error) => {
                    let current = cell
                        .authority
                        .load(cell.observed.value().cell)
                        .await?
                        .ok_or(Error::Fenced)?;
                    if current.value() == &successor {
                        current
                    } else {
                        return Err(error);
                    }
                }
            };
            attached.push(versioned);
        }
        Ok(attached)
    }

    /// Pins every recovered Cell and then atomically seals the claimed node log.
    pub async fn recover_and_seal(
        &self,
        directory: &NodeDirectory,
        fenced: FencedNodeSession,
        cells: Vec<RecoveryCell>,
        now_ms: i64,
    ) -> Result<CompletedNodeRecovery> {
        let controls = self.recover(fenced.clone(), cells).await?;
        let mut manifest = None::<Digest>;
        for control in &controls {
            let recovery = control
                .value()
                .recovery
                .as_ref()
                .ok_or(Error::Control("recovered Cell has no pinned overlay"))?;
            if recovery.leader_session != fenced.session()
                || recovery.log_epoch != self.recovery.log_epoch
            {
                return Err(Error::Control("recovered Cell overlay scope differs"));
            }
            match manifest {
                None => manifest = Some(recovery.manifest_digest),
                Some(current) if current == recovery.manifest_digest => {}
                Some(_) => {
                    return Err(Error::Control(
                        "recovered session produced multiple manifests",
                    ));
                }
            }
        }
        let sealed = directory.seal_recovery(&fenced, manifest, now_ms).await?;
        Ok(CompletedNodeRecovery { sealed, controls })
    }
}

impl NodeLogRecovery {
    pub(crate) fn new(
        transport: Arc<dyn NodeLogTransport>,
        leader_session: SessionId,
        log_epoch: u64,
        members: Vec<SessionId>,
        tiered_through: u64,
        active: bool,
        limits: crab_ltx::Limits,
    ) -> Result<Self> {
        if leader_session.as_bytes().iter().all(|byte| *byte == 0)
            || log_epoch == 0
            || members.is_empty()
            || members.len() > 2
            || members.contains(&leader_session)
            || members
                .iter()
                .any(|member| member.as_bytes().iter().all(|byte| *byte == 0))
            || !members
                .windows(2)
                .all(|pair| pair[0].as_bytes() < pair[1].as_bytes())
        {
            return Err(Error::Node("invalid node-log recovery ensemble"));
        }
        Ok(Self {
            transport,
            leader_session,
            log_epoch,
            members,
            tiered_through,
            active,
            limits,
        })
    }

    /// Builds recovery only from the exact CAS-protected failed-session log.
    pub fn from_fenced(
        transport: Arc<dyn NodeLogTransport>,
        fenced: &FencedNodeSession,
        limits: crab_ltx::Limits,
    ) -> Result<Self> {
        let log = fenced
            .log()
            .ok_or(Error::Node("fenced session has no enrolled node log"))?;
        let claim = log
            .recovery()
            .ok_or(Error::Node("fenced node log has no recovery claim"))?;
        if log.phase() != NodeLogPhase::Recovering
            || claim.claimant() != fenced.claimant()
            || claim.generation() != fenced.claim_generation()
            || claim.expires_at_ms() != fenced.claim_expires_at_ms()
        {
            return Err(Error::Fenced);
        }
        Self::new(
            transport,
            fenced.session(),
            log.epoch(),
            log.members().to_vec(),
            log.tiered_through(),
            log.active(),
            limits,
        )
    }

    fn validate_fence(&self, fenced: &FencedNodeSession) -> Result<()> {
        let log = fenced.log().ok_or(Error::Fenced)?;
        let claim = log.recovery().ok_or(Error::Fenced)?;
        if fenced.session() != self.leader_session
            || log.phase() != NodeLogPhase::Recovering
            || log.epoch() != self.log_epoch
            || log.members() != self.members
            || log.tiered_through() != self.tiered_through
            || log.active() != self.active
            || claim.claimant() != fenced.claimant()
            || claim.generation() != fenced.claim_generation()
            || claim.expires_at_ms() != fenced.claim_expires_at_ms()
        {
            return Err(Error::Fenced);
        }
        Ok(())
    }

    /// Seals all reachable members and returns one complete verified witness.
    pub async fn ensure_sealed(&self) -> Result<SealedSession> {
        let receipts = join_all(self.members.iter().map(|member| {
            let transport = Arc::clone(&self.transport);
            let member = *member;
            async move {
                (
                    member,
                    transport
                        .seal(
                            member,
                            SealRequest {
                                leader_session: self.leader_session,
                                log_epoch: self.log_epoch,
                            },
                        )
                        .await,
                )
            }
        }))
        .await;
        let successful_seals = receipts
            .iter()
            .filter(|(_, receipt)| receipt.is_ok())
            .count();
        let durable_through = receipts
            .iter()
            .filter_map(|(_, receipt)| receipt.as_ref().ok())
            .map(|receipt| receipt.durable_through)
            .max()
            .unwrap_or(self.tiered_through);
        if durable_through <= self.tiered_through {
            if self.active && successful_seals == 0 {
                return Err(Error::Node(
                    "active node log has no complete follower witness",
                ));
            }
            return Ok(SealedSession {
                leader_session: self.leader_session,
                log_epoch: self.log_epoch,
                tiered_through: self.tiered_through,
                durable_through: self.tiered_through,
                frames: Vec::new(),
            });
        }

        let required_first = self
            .tiered_through
            .checked_add(1)
            .ok_or(Error::Node("node-log recovery sequence overflow"))?;
        for (member, receipt) in receipts {
            let Ok(receipt) = receipt else {
                continue;
            };
            if receipt.base_sequence > required_first || receipt.durable_through < durable_through {
                continue;
            }
            let Ok(encoded) = self
                .transport
                .tail(
                    member,
                    TailRequest {
                        leader_session: self.leader_session,
                        log_epoch: self.log_epoch,
                        first_sequence: required_first,
                    },
                )
                .await
            else {
                continue;
            };
            let Ok(frames) = encoded
                .into_iter()
                .map(|bytes| crab_ltx::inspect_node_frame(bytes, self.limits))
                .collect::<crab_ltx::Result<Vec<_>>>()
            else {
                continue;
            };
            if frames.first().map(|frame| frame.scope().node_sequence) != Some(required_first)
                || frames.last().map(|frame| frame.scope().node_sequence) != Some(durable_through)
                || !frames.windows(2).all(|pair| {
                    pair[0].scope().node_sequence.checked_add(1)
                        == Some(pair[1].scope().node_sequence)
                })
            {
                continue;
            }
            return Ok(SealedSession {
                leader_session: self.leader_session,
                log_epoch: self.log_epoch,
                tiered_through: self.tiered_through,
                durable_through,
                frames,
            });
        }
        Err(Error::Node(
            "active node log has no complete follower witness",
        ))
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures_util::future::BoxFuture;

    use super::*;
    use crate::{
        AppendRequest, FollowerStore, LocalFollowerTransport, NodeLogTransport, RetireRequest,
    };

    struct FailingFirstTransport {
        failed: SessionId,
        good: LocalFollowerTransport,
    }

    impl NodeLogTransport for FailingFirstTransport {
        fn append<'a>(
            &'a self,
            member: SessionId,
            request: AppendRequest,
        ) -> BoxFuture<'a, Result<crate::FollowerReceipt>> {
            self.good.append(member, request)
        }

        fn seal<'a>(
            &'a self,
            member: SessionId,
            request: SealRequest,
        ) -> BoxFuture<'a, Result<crate::FollowerReceipt>> {
            if member == self.failed {
                return Box::pin(async {
                    Ok(crate::FollowerReceipt {
                        base_sequence: 1,
                        durable_through: 1,
                    })
                });
            }
            self.good.seal(member, request)
        }

        fn retire<'a>(
            &'a self,
            member: SessionId,
            request: RetireRequest,
        ) -> BoxFuture<'a, Result<crate::FollowerReceipt>> {
            self.good.retire(member, request)
        }

        fn tail<'a>(
            &'a self,
            member: SessionId,
            request: TailRequest,
        ) -> BoxFuture<'a, Result<Vec<Bytes>>> {
            if member == self.failed {
                return Box::pin(async { Err(Error::Node("injected follower read failure")) });
            }
            self.good.tail(member, request)
        }
    }

    #[tokio::test]
    async fn active_lane_requires_and_returns_a_complete_follower_tail() {
        let limits = crab_ltx::Limits::default();
        let source = tempfile::TempDir::new().unwrap();
        let mut database =
            crab_ltx::ManagedDb::open(&source.path().join("cell.sqlite"), limits).unwrap();
        database
            .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
            .unwrap();
        let capture = database.capture().unwrap();
        let segment = capture.segments.first().unwrap();
        let leader = SessionId::from_bytes([1; 16]);
        let member = SessionId::from_bytes([2; 16]);
        let frame = crab_ltx::encode_node_frame(
            crab_ltx::NodeFrameScope {
                leader_session: *leader.as_bytes(),
                log_epoch: 3,
                node_sequence: 1,
                application: [4; 16],
                cell: [5; 32],
                incarnation: [6; 16],
                cell_epoch: 7,
                commit_sequence: 1,
            },
            segment.info().clone(),
            Bytes::from(std::fs::read(segment.path()).unwrap()),
            limits,
        )
        .unwrap();
        let root = tempfile::TempDir::new().unwrap();
        let store = FollowerStore::open(
            root.path().to_owned(),
            limits,
            crab_ltx::DiskBudget::new(1 << 30),
        )
        .unwrap();
        let transport: Arc<dyn NodeLogTransport> =
            Arc::new(LocalFollowerTransport::new(member, store));
        transport
            .append(
                member,
                AppendRequest {
                    leader_session: leader,
                    log_epoch: 3,
                    frames: vec![frame.encoded().clone()],
                    covered_through: 0,
                },
            )
            .await
            .unwrap();
        let recovery =
            NodeLogRecovery::new(transport, leader, 3, vec![member], 0, true, limits).unwrap();
        let sealed = recovery.ensure_sealed().await.unwrap();
        assert_eq!(sealed.durable_through, 1);
        assert_eq!(sealed.frames.len(), 1);
        database.close().unwrap();
    }

    #[tokio::test]
    async fn recovery_uses_the_next_complete_witness_after_a_read_failure() {
        let limits = crab_ltx::Limits::default();
        let source = tempfile::TempDir::new().unwrap();
        let mut database =
            crab_ltx::ManagedDb::open(&source.path().join("cell.sqlite"), limits).unwrap();
        database
            .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
            .unwrap();
        let segment = database.capture().unwrap().segments.remove(0);
        let leader = SessionId::from_bytes([1; 16]);
        let failed = SessionId::from_bytes([2; 16]);
        let good = SessionId::from_bytes([3; 16]);
        let frame = crab_ltx::encode_node_frame(
            crab_ltx::NodeFrameScope {
                leader_session: *leader.as_bytes(),
                log_epoch: 3,
                node_sequence: 1,
                application: [4; 16],
                cell: [5; 32],
                incarnation: [6; 16],
                cell_epoch: 7,
                commit_sequence: 1,
            },
            segment.info().clone(),
            Bytes::from(std::fs::read(segment.path()).unwrap()),
            limits,
        )
        .unwrap();
        let root = tempfile::TempDir::new().unwrap();
        let local = LocalFollowerTransport::new(
            good,
            FollowerStore::open(
                root.path().to_owned(),
                limits,
                crab_ltx::DiskBudget::new(1 << 30),
            )
            .unwrap(),
        );
        local
            .append(
                good,
                AppendRequest {
                    leader_session: leader,
                    log_epoch: 3,
                    frames: vec![frame.encoded().clone()],
                    covered_through: 0,
                },
            )
            .await
            .unwrap();
        let transport: Arc<dyn NodeLogTransport> = Arc::new(FailingFirstTransport {
            failed,
            good: local,
        });
        let recovery =
            NodeLogRecovery::new(transport, leader, 3, vec![failed, good], 0, true, limits)
                .unwrap();
        assert_eq!(recovery.ensure_sealed().await.unwrap().frames.len(), 1);
        database.close().unwrap();
    }
}
