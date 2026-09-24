use std::sync::Arc;

use bytes::Bytes;
use crab_ltx::{CaptureBatch, CellReplica, CellStorageLayout, Db, Limits};
use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};

use super::{
    CellDurabilitySubmitter, CellPublisher, DurabilitySubmissionOutcome, NodeDurabilitySlot,
};
use crate::Error;
use crate::control::authority::CellAuthority;
use crate::control::{Control, Owner};
use crate::identity::IncarnationId;
use crate::identity::{CellId, Digest, SessionId};

#[tokio::test]
async fn quiet_compaction_publishes_exact_root_after_eight_appends() {
    let directory = tempfile::tempdir().unwrap();
    let mut database = Db::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
    let cell = CellId::from_bytes([41; 32]);
    let incarnation = IncarnationId::from_bytes([42; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("quiet-compaction"),
        [43; 16],
    );
    let replica = CellReplica::new(
        layout.clone(),
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let control = Control::initial(
        cell,
        incarnation,
        Owner {
            session: SessionId::from_bytes([44; 16]),
            endpoint: "https://node.internal:8081".into(),
        },
        Digest::from_bytes([45; 32]),
        1,
    )
    .unwrap();
    layout
        .store()
        .create_strict(
            &layout.control_path(cell.as_bytes()),
            Bytes::from(control.encode().unwrap()),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout);
    let observed = authority.load(cell).await.unwrap().unwrap();
    let mut publisher = CellPublisher::new(
        replica.clone(),
        authority,
        observed,
        directory.path().to_owned(),
    );
    for sequence in 1..=8_u64 {
        database
            .transaction(|transaction| {
                if sequence == 1 {
                    transaction
                        .execute_batch("CREATE TABLE events(sequence INTEGER PRIMARY KEY)")?;
                }
                transaction.execute("INSERT INTO events VALUES (?1)", [sequence])?;
                Ok(())
            })
            .unwrap();
        let cuts = database.capture_deferred().unwrap();
        let prepared = publisher.prepare_append(&cuts, sequence, 1).await.unwrap();
        publisher.publish_prepared(&prepared, None).await.unwrap();
    }
    assert!(publisher.compaction_due());
    let before = publisher.control().value().ltx_root().unwrap();
    assert_eq!(replica.open_root(&before).await.unwrap().segment_count(), 8);
    assert_eq!(publisher.compact_one_quiet().await.unwrap(), Some(true));
    let after = publisher.control().value().ltx_root().unwrap();
    assert_eq!(after.position, before.position);
    assert_eq!(after.commit_sequence, before.commit_sequence);
    assert_eq!(replica.open_root(&after).await.unwrap().segment_count(), 1);
    assert_eq!(publisher.compact_one_quiet().await.unwrap(), Some(false));
    assert!(!publisher.compaction_due());

    let mut segments = Vec::new();
    let mut position = after.position;
    for sequence in 9..=10_u64 {
        database
            .transaction(|transaction| {
                transaction.execute("INSERT INTO events VALUES (?1)", [sequence])?;
                Ok(())
            })
            .unwrap();
        let captured = database.capture_deferred().unwrap();
        segments.extend(captured.segments);
        position = captured.position;
    }
    let cuts = CaptureBatch {
        segments,
        position,
        timing: Default::default(),
    };
    assert_eq!(cuts.segments.len(), 2);
    let prepared = publisher.prepare_append(&cuts, 9, 1).await.unwrap();
    publisher.publish_prepared(&prepared, None).await.unwrap();
    let extended = publisher.control().value().ltx_root().unwrap();
    assert_eq!(extended.position, position);
    assert_eq!(
        replica.open_root(&extended).await.unwrap().segment_count(),
        3
    );

    for sequence in 10..=37_u64 {
        database
            .transaction(|transaction| {
                transaction.execute("INSERT INTO events VALUES (?1)", [sequence + 1])?;
                Ok(())
            })
            .unwrap();
        let cuts = database.capture_deferred().unwrap();
        let prepared = publisher.prepare_append(&cuts, sequence, 1).await.unwrap();
        publisher.publish_prepared(&prepared, None).await.unwrap();
    }
    let at_ceiling = publisher.control().value().ltx_root().unwrap();
    assert_eq!(
        replica
            .open_root(&at_ceiling)
            .await
            .unwrap()
            .segment_count(),
        31
    );
    database
        .transaction(|transaction| {
            transaction.execute("INSERT INTO events VALUES (39)", [])?;
            Ok(())
        })
        .unwrap();
    let cuts = database.capture_deferred().unwrap();
    let prepared = publisher.prepare_append(&cuts, 38, 1).await.unwrap();
    publisher.publish_prepared(&prepared, None).await.unwrap();
    let forced = publisher.control().value().ltx_root().unwrap();
    assert!(replica.open_root(&forced).await.unwrap().segment_count() < 32);
    database.close().unwrap();
}

#[derive(Default)]
struct RecordingSubmissions {
    outcomes: std::sync::Mutex<Vec<DurabilitySubmissionOutcome>>,
}

impl crate::fleet::telemetry::CellTelemetry for RecordingSubmissions {
    fn durability_submission(&self, outcome: DurabilitySubmissionOutcome) {
        self.outcomes.lock().unwrap().push(outcome);
    }
}

#[tokio::test]
async fn commits_report_when_no_enrolled_lane_can_carry_them() {
    let telemetry = crate::fleet::telemetry::CellTelemetryHandle::default();
    let recording = Arc::new(RecordingSubmissions::default());
    telemetry.install(recording.clone()).unwrap();
    let cuts = CaptureBatch {
        segments: Vec::new(),
        position: Default::default(),
        timing: Default::default(),
    };
    let submitter = CellDurabilitySubmitter {
        cell: CellId::from_bytes([71; 32]),
        incarnation: IncarnationId::from_bytes([72; 16]),
        epoch: 1,
        node_lease: None,
        node_durability: None,
        telemetry: telemetry.clone(),
    };
    assert!(submitter.submit(1, &cuts).await.unwrap().is_none());

    let lane: NodeDurabilitySlot = Arc::new(std::sync::RwLock::new(None));
    let submitter = CellDurabilitySubmitter {
        node_durability: Some(lane),
        telemetry,
        ..submitter
    };
    assert!(submitter.submit(1, &cuts).await.unwrap().is_none());

    assert_eq!(
        *recording.outcomes.lock().unwrap(),
        vec![
            DurabilitySubmissionOutcome::Unsupported,
            DurabilitySubmissionOutcome::Unavailable,
        ]
    );
}

/// Transport that refuses every follower request; the gate is fenced first,
/// so no frame reaches it in this test.
struct RefusingTransport;

impl crate::node::log_transport::NodeLogTransport for RefusingTransport {
    fn append<'a>(
        &'a self,
        _member: crate::identity::NodeId,
        _request: crate::node::log_transport::AppendRequest,
    ) -> futures_util::future::BoxFuture<'a, crate::Result<crate::follower::FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("test transport refuses appends")) })
    }

    fn seal<'a>(
        &'a self,
        _member: crate::identity::NodeId,
        _request: crate::node::log_transport::SealRequest,
    ) -> futures_util::future::BoxFuture<'a, crate::Result<crate::follower::FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("test transport refuses seals")) })
    }

    fn retire<'a>(
        &'a self,
        _member: crate::identity::NodeId,
        _request: crate::node::log_transport::RetireRequest,
    ) -> futures_util::future::BoxFuture<'a, crate::Result<crate::follower::FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("test transport refuses retirements")) })
    }

    fn tail<'a>(
        &'a self,
        _member: crate::identity::NodeId,
        _request: crate::node::log_transport::TailRequest,
    ) -> futures_util::future::BoxFuture<'a, crate::Result<Vec<bytes::Bytes>>> {
        Box::pin(async { Err(Error::Node("test transport refuses tails")) })
    }
}

/// Authority that refuses activation; the fenced gate never asks it anything.
struct RefusingAuthority;

impl crate::node::durability::NodeLogAuthority for RefusingAuthority {
    fn activate<'a>(
        &'a self,
        _log_epoch: u64,
    ) -> futures_util::future::BoxFuture<'a, crate::Result<()>> {
        Box::pin(async { Err(Error::Node("test authority refuses activation")) })
    }

    fn advance_coverage<'a>(
        &'a self,
        _log_epoch: u64,
        _tiered_through: u64,
    ) -> futures_util::future::BoxFuture<'a, crate::Result<()>> {
        Box::pin(async { Err(Error::Node("test authority refuses coverage")) })
    }

    fn close<'a>(
        &'a self,
        _barrier: &'a crate::node::log::NodeLogRotationBarrier,
    ) -> futures_util::future::BoxFuture<'a, crate::Result<()>> {
        Box::pin(async { Err(Error::Node("test authority refuses closing")) })
    }
}

#[tokio::test]
async fn commits_report_a_fenced_lane_instead_of_failing() {
    let telemetry = crate::fleet::telemetry::CellTelemetryHandle::default();
    let recording = Arc::new(RecordingSubmissions::default());
    telemetry.install(recording.clone()).unwrap();

    let gate = crate::node::log::DurabilityGate::new(
        SessionId::from_bytes([81; 16]),
        crate::identity::NodeId::from_bytes([82; 16]),
        9,
        [crate::identity::NodeId::from_bytes([83; 16])],
    )
    .unwrap();
    let transport: Arc<dyn crate::node::log_transport::NodeLogTransport> =
        Arc::new(RefusingTransport);
    let shipper = crate::node::log_shipper::NodeLogShipper::new_with_telemetry(
        gate.clone(),
        Arc::clone(&transport),
        Limits::default(),
        telemetry.clone(),
    )
    .unwrap();
    let lease = crate::node::lease::NodeLeaseGuard::new(0, 60_000).unwrap();
    let durability = Arc::new(crate::node::durability::NodeDurability::new(
        gate.clone(),
        shipper,
        Arc::new(RefusingAuthority),
        transport,
        lease,
    ));
    gate.stop_shipping();

    let directory = tempfile::tempdir().unwrap();
    let mut database = Db::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
    database
        .transaction(|transaction| {
            transaction.execute_batch("CREATE TABLE events(sequence INTEGER PRIMARY KEY)")?;
            transaction.execute("INSERT INTO events VALUES (1)", [])?;
            Ok(())
        })
        .unwrap();
    let cuts = database.capture_deferred().unwrap();

    let submitter = CellDurabilitySubmitter {
        cell: CellId::from_bytes([84; 32]),
        incarnation: IncarnationId::from_bytes([85; 16]),
        epoch: 1,
        node_lease: None,
        node_durability: Some(Arc::new(std::sync::RwLock::new(Some((
            crate::identity::ApplicationId::from_bytes([86; 16]),
            durability,
        ))))),
        telemetry,
    };
    assert!(submitter.submit(1, &cuts).await.unwrap().is_none());
    assert_eq!(
        *recording.outcomes.lock().unwrap(),
        vec![DurabilitySubmissionOutcome::Rejected]
    );
    database.close().unwrap();
}
