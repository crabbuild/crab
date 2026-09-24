use bytes::Bytes;
use futures_util::future::BoxFuture;

use super::*;
use crate::follower::FollowerStore;
use crate::node::log_transport::{
    AppendRequest, LocalFollowerTransport, NodeLogTransport, RetireRequest,
};

fn scope(cell: u8, sequence: u64) -> crab_ltx::NodeFrameScope {
    crab_ltx::NodeFrameScope {
        leader_session: [1; 16],
        log_epoch: 3,
        node_sequence: sequence,
        application: [4; 16],
        cell: [cell; 32],
        incarnation: [6; 16],
        cell_epoch: 7,
        commit_sequence: sequence,
    }
}

#[test]
fn scope_validation_keeps_one_generation_per_cell() {
    let scopes = unique_scopes([Ok(scope(1, 1)), Ok(scope(1, 2)), Ok(scope(2, 3))]).unwrap();
    assert_eq!(scopes.len(), 2);
    assert_eq!(scopes[0].cell, [1; 32]);
    assert_eq!(scopes[1].cell, [2; 32]);
}

#[test]
fn scope_validation_rejects_conflicting_cell_generations() {
    let mut conflicting = scope(1, 2);
    conflicting.cell_epoch = 8;
    assert!(matches!(
        unique_scopes([Ok(scope(1, 1)), Ok(conflicting)]),
        Err(Error::Control(
            "recovery Cell scope has multiple generations"
        ))
    ));
}

#[test]
fn recovery_work_summary_merge_is_checked() {
    let mut summary = RecoveryWorkSummary {
        follower_bytes: u64::MAX,
        ..RecoveryWorkSummary::default()
    };
    assert!(matches!(
        summary.merge(RecoveryWorkSummary {
            follower_bytes: 1,
            ..RecoveryWorkSummary::default()
        }),
        Err(Error::Capacity("recovery follower byte count"))
    ));
}

struct FailingFirstTransport {
    failed: NodeId,
    good: LocalFollowerTransport,
    gap: bool,
    oversized: bool,
    replacement: Option<Bytes>,
}

struct FleetTransport {
    stores: Vec<(NodeId, FollowerStore)>,
}

impl FleetTransport {
    fn store(&self, member: NodeId) -> Result<&FollowerStore> {
        self.stores
            .iter()
            .find_map(|(candidate, store)| (*candidate == member).then_some(store))
            .ok_or(Error::Node("follower is absent from test fleet"))
    }
}

impl NodeLogTransport for FleetTransport {
    fn append<'a>(
        &'a self,
        member: NodeId,
        request: AppendRequest,
    ) -> BoxFuture<'a, Result<crate::follower::FollowerReceipt>> {
        Box::pin(async move {
            self.store(member)?
                .append(
                    request.leader_session,
                    request.log_epoch,
                    request.frames,
                    request.covered_through,
                )
                .await
        })
    }

    fn seal<'a>(
        &'a self,
        member: NodeId,
        request: SealRequest,
    ) -> BoxFuture<'a, Result<crate::follower::FollowerReceipt>> {
        Box::pin(async move {
            self.store(member)?
                .seal(request.leader_session, request.log_epoch)
                .await
        })
    }

    fn retire<'a>(
        &'a self,
        member: NodeId,
        request: RetireRequest,
    ) -> BoxFuture<'a, Result<crate::follower::FollowerReceipt>> {
        Box::pin(async move {
            self.store(member)?
                .retire(
                    request.leader_session,
                    request.log_epoch,
                    request.covered_through,
                )
                .await
        })
    }

    fn tail<'a>(
        &'a self,
        member: NodeId,
        request: TailRequest,
    ) -> BoxFuture<'a, Result<Vec<Bytes>>> {
        Box::pin(async move {
            self.store(member)?
                .read_tail(
                    request.leader_session,
                    request.log_epoch,
                    request.first_sequence,
                )
                .await
        })
    }
}

impl NodeLogTransport for FailingFirstTransport {
    fn append<'a>(
        &'a self,
        member: NodeId,
        request: AppendRequest,
    ) -> BoxFuture<'a, Result<crate::follower::FollowerReceipt>> {
        self.good.append(member, request)
    }

    fn seal<'a>(
        &'a self,
        member: NodeId,
        request: SealRequest,
    ) -> BoxFuture<'a, Result<crate::follower::FollowerReceipt>> {
        if member == self.failed {
            return Box::pin(async {
                Ok(crate::follower::FollowerReceipt {
                    base_sequence: 1,
                    durable_through: 1,
                })
            });
        }
        self.good.seal(member, request)
    }

    fn retire<'a>(
        &'a self,
        member: NodeId,
        request: RetireRequest,
    ) -> BoxFuture<'a, Result<crate::follower::FollowerReceipt>> {
        self.good.retire(member, request)
    }

    fn tail<'a>(
        &'a self,
        member: NodeId,
        request: TailRequest,
    ) -> BoxFuture<'a, Result<Vec<Bytes>>> {
        if member == self.failed {
            return Box::pin(async { Err(Error::Node("injected follower read failure")) });
        }
        self.good.tail(member, request)
    }

    fn tail_page<'a>(
        &'a self,
        member: NodeId,
        request: TailRequest,
    ) -> BoxFuture<'a, Result<crate::follower::FollowerTailPage>> {
        if member == self.failed {
            return Box::pin(async { Err(Error::Node("injected follower read failure")) });
        }
        let good = self.good.clone();
        let gap = self.gap;
        let oversized = self.oversized;
        let replacement = self.replacement.clone();
        Box::pin(async move {
            let mut page = good.tail_page(member, request).await?;
            if let Some(replacement) = replacement {
                page.frames = vec![replacement];
            }
            if oversized {
                let first = page
                    .frames
                    .first()
                    .cloned()
                    .ok_or(Error::Node("test page has no frame"))?;
                page.frames = std::iter::repeat_n(first, MAX_RECOVERY_PAGE_FRAMES + 1).collect();
                page.next_sequence = Some(
                    request
                        .first_sequence
                        .checked_add(MAX_RECOVERY_PAGE_FRAMES as u64 + 1)
                        .ok_or(Error::Node("test page sequence overflow"))?,
                );
            }
            if gap {
                let count = u64::try_from(page.frames.len())
                    .map_err(|_| Error::Node("test page frame count overflow"))?;
                page.next_sequence = Some(
                    request
                        .first_sequence
                        .checked_add(count)
                        .and_then(|next| next.checked_add(1))
                        .ok_or(Error::Node("test page sequence overflow"))?,
                );
            }
            Ok(page)
        })
    }
}

#[tokio::test]
async fn active_lane_requires_and_returns_a_complete_follower_tail() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = crab_ltx::Db::open(&source.path().join("cell.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE values_(v); INSERT INTO values_ VALUES(randomblob(2097152))",
            )
        })
        .unwrap();
    let capture = database.capture().unwrap();
    let segment = capture.segments.first().unwrap();
    let leader = SessionId::from_bytes([1; 16]);
    let member = NodeId::from_bytes([2; 16]);
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
    assert!(frame.encoded().len() > MAX_RECOVERY_PAGE_BYTES as usize);
    let root = tempfile::TempDir::new().unwrap();
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    let transport: Arc<dyn NodeLogTransport> = Arc::new(LocalFollowerTransport::new(member, store));
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
    let rejected = NodeLogRecovery::new(
        Arc::clone(&transport),
        NodeId::from_bytes([1; 16]),
        leader,
        3,
        vec![member],
        0,
        true,
        limits,
    )
    .unwrap()
    .with_recovery_disk(crab_ltx::DiskBudget::new(0));
    assert!(rejected.ensure_sealed().await.is_err());
    let budget = crab_ltx::DiskBudget::new(1 << 30);
    let recovery = NodeLogRecovery::new(
        Arc::clone(&transport),
        NodeId::from_bytes([1; 16]),
        leader,
        3,
        vec![member],
        0,
        true,
        limits,
    )
    .unwrap()
    .with_recovery_disk(budget.clone());
    let sealed = recovery.ensure_sealed().await.unwrap();
    assert_eq!(sealed.durable_through, 1);
    assert_eq!(sealed.frames.len(), 1);
    assert!(budget.used() > 0);
    drop(sealed);
    assert_eq!(budget.used(), 0);
    let scratch = tempfile::TempDir::new().unwrap();
    let bounded = NodeLogRecovery::new(
        Arc::clone(&transport),
        NodeId::from_bytes([1; 16]),
        leader,
        3,
        vec![member],
        0,
        true,
        limits,
    )
    .unwrap()
    .with_recovery_disk(crab_ltx::DiskBudget::new(1 << 30))
    .with_recovery_scratch(scratch.path().to_owned());
    let sealed = bounded.ensure_sealed_bounded().await.unwrap();
    assert!(sealed.frames.is_empty());
    assert_eq!(sealed.frame_count(), 1);
    assert_eq!(sealed.scopes(limits).unwrap().len(), 1);
    drop(sealed);
    assert!(scratch.path().read_dir().unwrap().next().is_none());
    database.close().unwrap();
}

#[tokio::test]
async fn recovery_uses_the_next_complete_witness_after_a_read_failure() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = crab_ltx::Db::open(&source.path().join("cell.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
        .unwrap();
    let segment = database.capture().unwrap().segments.remove(0);
    let leader = SessionId::from_bytes([1; 16]);
    let failed = NodeId::from_bytes([2; 16]);
    let good = NodeId::from_bytes([3; 16]);
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
        good: local.clone(),
        gap: false,
        oversized: false,
        replacement: None,
    });
    let recovery = NodeLogRecovery::new(
        transport,
        NodeId::from_bytes([1; 16]),
        leader,
        3,
        vec![failed, good],
        0,
        true,
        limits,
    )
    .unwrap();
    assert_eq!(recovery.ensure_sealed().await.unwrap().frames.len(), 1);

    let gapped: Arc<dyn NodeLogTransport> = Arc::new(FailingFirstTransport {
        failed,
        good: local.clone(),
        gap: true,
        oversized: false,
        replacement: None,
    });
    let recovery = NodeLogRecovery::new(
        gapped,
        NodeId::from_bytes([1; 16]),
        leader,
        3,
        vec![good],
        0,
        true,
        limits,
    )
    .unwrap();
    assert!(recovery.ensure_sealed().await.is_err());

    let oversized: Arc<dyn NodeLogTransport> = Arc::new(FailingFirstTransport {
        failed,
        good: local.clone(),
        gap: false,
        oversized: true,
        replacement: None,
    });
    let recovery = NodeLogRecovery::new(
        oversized,
        NodeId::from_bytes([1; 16]),
        leader,
        3,
        vec![good],
        0,
        true,
        limits,
    )
    .unwrap();
    assert!(recovery.ensure_sealed().await.is_err());
    for wrong_epoch in [false, true] {
        let mut scope = frame.scope();
        if wrong_epoch {
            scope.log_epoch += 1;
        } else {
            scope.leader_session = [99; 16];
        }
        let replacement = crab_ltx::encode_node_frame(
            scope,
            frame.segment().clone(),
            frame.body().clone(),
            limits,
        )
        .unwrap();
        let transport = Arc::new(FailingFirstTransport {
            failed,
            good: local.clone(),
            gap: false,
            oversized: false,
            replacement: Some(replacement.encoded().clone()),
        });
        let recovery = NodeLogRecovery::new(
            transport,
            NodeId::from_bytes([1; 16]),
            leader,
            3,
            vec![good],
            0,
            true,
            limits,
        )
        .unwrap();
        assert!(recovery.ensure_sealed().await.is_err());
    }
    database.close().unwrap();
}

#[tokio::test]
async fn recovery_survives_a_simultaneous_follower_fleet_restart() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = crab_ltx::Db::open(&source.path().join("cell.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
        .unwrap();
    let segment = database.capture().unwrap().segments.remove(0);
    let leader = SessionId::from_bytes([1; 16]);
    let leader_node = NodeId::from_bytes([1; 16]);
    let members = [NodeId::from_bytes([2; 16]), NodeId::from_bytes([3; 16])];
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
    for conflict in [false, true] {
        let roots = [
            tempfile::TempDir::new().unwrap(),
            tempfile::TempDir::new().unwrap(),
        ];
        for (index, (member, root)) in members.iter().zip(&roots).enumerate() {
            let store = FollowerStore::open(
                root.path().to_owned(),
                limits,
                crab_ltx::DiskBudget::new(1 << 30),
            )
            .unwrap();
            let mut scope = frame.scope();
            if conflict && index == 1 {
                scope.cell = [99; 32];
            }
            let frame = crab_ltx::encode_node_frame(
                scope,
                frame.segment().clone(),
                frame.body().clone(),
                limits,
            )
            .unwrap();
            store
                .append(leader, 3, vec![frame.encoded().clone()], 0)
                .await
                .unwrap();
            drop(store);
            assert!(root.path().join("followers").exists(), "{member:?}");
        }

        let transport: Arc<dyn NodeLogTransport> = Arc::new(FleetTransport {
            stores: members
                .iter()
                .zip(&roots)
                .map(|(member, root)| {
                    (
                        *member,
                        FollowerStore::open(
                            root.path().to_owned(),
                            limits,
                            crab_ltx::DiskBudget::new(1 << 30),
                        )
                        .unwrap(),
                    )
                })
                .collect(),
        });
        let recovery = NodeLogRecovery::new(
            transport,
            leader_node,
            leader,
            3,
            members.to_vec(),
            0,
            true,
            limits,
        )
        .unwrap();

        let result = recovery.ensure_sealed().await;
        if conflict {
            assert!(matches!(
                result,
                Err(Error::Node("follower witnesses disagree"))
            ));
            continue;
        }
        let sealed = result.unwrap();
        assert_eq!(sealed.durable_through, 1);
        assert_eq!(sealed.frames.len(), 1);
        assert_eq!(sealed.frames[0].scope().node_sequence, 1);
    }
    database.close().unwrap();
}
