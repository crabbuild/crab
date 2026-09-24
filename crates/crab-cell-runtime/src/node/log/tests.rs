use super::*;
use bytes::Bytes;

fn verified_frame(
    sequence: u64,
    commit_sequence: u64,
    cell: [u8; 32],
    incarnation: [u8; 16],
    segment: &crab_ltx::LocalSegment,
) -> crab_ltx::VerifiedNodeFrame {
    crab_ltx::encode_node_frame(
        crab_ltx::NodeFrameScope {
            leader_session: [1; 16],
            log_epoch: 2,
            node_sequence: sequence,
            application: [9; 16],
            cell,
            incarnation,
            cell_epoch: 3,
            commit_sequence,
        },
        segment.info().clone(),
        Bytes::from(std::fs::read(segment.path()).unwrap()),
        crab_ltx::Limits::default(),
    )
    .unwrap()
}

fn session(byte: u8) -> SessionId {
    SessionId::from_bytes([byte; 16])
}

fn node(byte: u8) -> NodeId {
    NodeId::from_bytes([byte; 16])
}

#[tokio::test]
async fn fleet_requires_activation_and_every_follower() {
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(3), node(4)]).unwrap();
    let ticket = gate.issue(2).unwrap();
    gate.acknowledge(node(3), 2).unwrap();
    gate.acknowledge(node(4), 2).unwrap();
    assert!(gate.proof(ticket).unwrap().is_none());
    gate.activate_fleet().unwrap();
    assert_eq!(
        gate.prove(ticket).await.unwrap().source(),
        DurabilitySource::Fleet
    );
}

#[tokio::test]
async fn follower_wait_does_not_activate_fleet_proof() {
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(3), node(4)]).unwrap();
    let ticket = gate.issue(2).unwrap();
    gate.acknowledge(node(3), 2).unwrap();
    gate.acknowledge(node(4), 2).unwrap();

    gate.wait_followers(ticket).await.unwrap();

    assert!(gate.proof(ticket).unwrap().is_none());
}

#[tokio::test]
async fn follower_wait_wakes_after_ack_arrives() {
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(3)]).unwrap();
    let ticket = gate.issue(1).unwrap();
    let waiter = {
        let gate = gate.clone();
        tokio::spawn(async move { gate.wait_followers(ticket).await })
    };
    tokio::task::yield_now().await;
    gate.acknowledge(node(3), 1).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn object_proof_wins_independently_and_watermark_stays_contiguous() {
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(3)]).unwrap();
    let first = gate.issue(1).unwrap();
    let second = gate.issue(1).unwrap();
    assert_eq!(gate.prove_object(second).unwrap(), 0);
    assert_eq!(
        gate.prove(second).await.unwrap().source(),
        DurabilitySource::Object
    );
    assert_eq!(gate.prove_object(first).unwrap(), 2);
    assert_eq!(gate.tiered_through(), 2);
}

#[tokio::test]
async fn proof_wakes_after_object_coverage_arrives() {
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(3)]).unwrap();
    let ticket = gate.issue(1).unwrap();
    let waiter = {
        let gate = gate.clone();
        tokio::spawn(async move { gate.prove(ticket).await })
    };
    tokio::task::yield_now().await;
    gate.prove_object(ticket).unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .source(),
        DurabilitySource::Object
    );
}

#[tokio::test]
async fn rotation_waits_for_object_coverage_and_closes_the_old_gate() {
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(4), node(3)]).unwrap();
    let ticket = gate.issue(2).unwrap();

    assert!(matches!(
        gate.begin_rotation(),
        Err(Error::PendingPublication)
    ));
    gate.prove_object(ticket).unwrap();
    let barrier = gate.begin_rotation().unwrap();
    assert_eq!(barrier.leader_session(), session(1));
    assert_eq!(barrier.log_epoch(), 2);
    assert_eq!(barrier.members(), [node(3), node(4)]);
    assert_eq!(barrier.covered_through(), 2);
    assert_eq!(gate.begin_rotation().unwrap(), barrier);
    assert!(gate.issue(1).is_err());
    assert!(gate.activate_fleet().is_err());
    assert!(gate.acknowledge(node(3), 2).is_err());
    assert_eq!(
        gate.prove(ticket).await.unwrap().source(),
        DurabilitySource::Object
    );
}

#[tokio::test]
async fn fencing_wakes_waiters_and_rejects_late_acks() {
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(3)]).unwrap();
    let ticket = gate.issue(1).unwrap();
    let waiter = {
        let gate = gate.clone();
        tokio::spawn(async move { gate.prove(ticket).await })
    };
    gate.fence();
    assert!(matches!(waiter.await.unwrap(), Err(Error::Fenced)));
    assert!(matches!(gate.acknowledge(node(3), 1), Err(Error::Fenced)));
}

#[test]
fn fencing_rejects_late_object_coverage() {
    let gate = DurabilityGate::new(session(1), node(1), 2, [node(3)]).unwrap();
    let ticket = gate.issue(1).unwrap();
    gate.fence();

    assert!(matches!(gate.prove_object(ticket), Err(Error::Fenced)));
    assert_eq!(gate.tiered_through(), 0);
}

#[test]
fn recovery_witness_splits_interleaved_cells_against_exact_bases() {
    let limits = crab_ltx::Limits::default();
    let directory = tempfile::TempDir::new().unwrap();
    let mut left = crab_ltx::Db::open(&directory.path().join("left.sqlite"), limits).unwrap();
    let mut right = crab_ltx::Db::open(&directory.path().join("right.sqlite"), limits).unwrap();
    for database in [&mut left, &mut right] {
        database
            .transaction(|transaction| {
                transaction.execute_batch(
                    "CREATE TABLE events(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                     INSERT INTO events(body) VALUES ('base')",
                )
            })
            .unwrap();
    }
    let left_base = left.capture().unwrap();
    let right_base = right.capture().unwrap();
    for database in [&mut left, &mut right] {
        database
            .transaction(|transaction| {
                transaction.execute("INSERT INTO events(body) VALUES ('tail')", [])?;
                Ok(())
            })
            .unwrap();
    }
    let left_tail = left.capture().unwrap();
    let right_tail = right.capture().unwrap();
    let left_cell = [4; 32];
    let left_incarnation = [5; 16];
    let right_cell = [6; 32];
    let right_incarnation = [7; 16];
    let frames = vec![
        verified_frame(
            1,
            2,
            left_cell,
            left_incarnation,
            left_tail.segments.first().unwrap(),
        ),
        verified_frame(
            2,
            2,
            right_cell,
            right_incarnation,
            right_tail.segments.first().unwrap(),
        ),
    ];
    let bases = [
        RecoveryBase {
            application: [9; 16],
            cell_epoch: 3,
            root: crab_ltx::RootRef {
                cell: left_cell,
                incarnation: left_incarnation,
                digest: [10; 32],
                position: left_base.position,
                commit_sequence: 1,
            },
        },
        RecoveryBase {
            application: [9; 16],
            cell_epoch: 3,
            root: crab_ltx::RootRef {
                cell: right_cell,
                incarnation: right_incarnation,
                digest: [11; 32],
                position: right_base.position,
                commit_sequence: 1,
            },
        },
    ];

    let recovered = build_recovery_overlays(frames.clone(), &bases, limits).unwrap();
    assert_eq!(recovered.len(), 2);
    assert_eq!(recovered[0].first_node_sequence, 1);
    assert_eq!(recovered[0].overlay.final_position(), left_tail.position);
    assert_eq!(recovered[1].last_node_sequence, 2);
    assert_eq!(recovered[1].overlay.final_position(), right_tail.position);
    // The second Cell can reach object storage before the first, leaving
    // its already-published commit above the shared node watermark.
    let mut advanced_bases = bases;
    advanced_bases[1].root.position = right_tail.position;
    advanced_bases[1].root.commit_sequence = 2;
    let recovered = build_recovery_overlays(frames.clone(), &advanced_bases, limits).unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].overlay.predecessor().cell, left_cell);
    advanced_bases[1].root.position.checksum ^= 1;
    assert!(build_recovery_overlays(frames.clone(), &advanced_bases, limits).is_err());
    advanced_bases[1].root.position = right_base.position;
    assert!(build_recovery_overlays(frames, &advanced_bases, limits).is_err());
    left.close().unwrap();
    right.close().unwrap();
}

#[test]
fn recovery_witness_splits_two_interleaved_cuts_from_one_thousand_cells() {
    const CELLS: usize = 1_000;

    let limits = crab_ltx::Limits::default();
    let directory = tempfile::TempDir::new().unwrap();
    let mut database = crab_ltx::Db::open(&directory.path().join("source.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE events(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO events(body) VALUES ('base')",
            )
        })
        .unwrap();
    let base = database.capture().unwrap();
    database
        .transaction(|transaction| {
            transaction.execute("INSERT INTO events(body) VALUES ('first')", [])?;
            Ok(())
        })
        .unwrap();
    let first = database.capture().unwrap();
    database
        .transaction(|transaction| {
            transaction.execute("INSERT INTO events(body) VALUES ('second')", [])?;
            Ok(())
        })
        .unwrap();
    let second = database.capture().unwrap();
    let incarnation = [5; 16];
    let mut bases = Vec::with_capacity(CELLS);
    let mut cells = Vec::with_capacity(CELLS);
    for index in 0..CELLS {
        let mut cell = [0_u8; 32];
        cell[..8].copy_from_slice(&(index as u64 + 1).to_be_bytes());
        cells.push(cell);
        bases.push(RecoveryBase {
            application: [9; 16],
            cell_epoch: 3,
            root: crab_ltx::RootRef {
                cell,
                incarnation,
                digest: *blake3::hash(&cell).as_bytes(),
                position: base.position,
                commit_sequence: 1,
            },
        });
    }
    let mut frames = Vec::with_capacity(CELLS * 2);
    for (index, cell) in cells.iter().copied().enumerate() {
        frames.push(verified_frame(
            index as u64 + 1,
            2,
            cell,
            incarnation,
            first.segments.first().unwrap(),
        ));
    }
    for (index, cell) in cells.iter().copied().enumerate() {
        frames.push(verified_frame(
            CELLS as u64 + index as u64 + 1,
            3,
            cell,
            incarnation,
            second.segments.first().unwrap(),
        ));
    }

    let recovered = build_recovery_overlays(frames.clone(), &bases, limits).unwrap();

    assert_eq!(recovered.len(), CELLS);
    let file_backed =
        build_recovery_overlays_file_backed(frames.clone(), &bases, limits, directory.path())
            .unwrap();
    assert_eq!(file_backed.len(), recovered.len());
    for (memory, disk) in recovered.iter().zip(&file_backed) {
        assert_eq!(memory.first_node_sequence, disk.first_node_sequence);
        assert_eq!(memory.last_node_sequence, disk.last_node_sequence);
        assert_eq!(
            memory.overlay.final_position(),
            disk.overlay.final_position()
        );
        assert_eq!(
            memory.overlay.final_commit_sequence(),
            disk.overlay.final_commit_sequence()
        );
        assert_eq!(
            memory.overlay.bundle().digest(),
            disk.overlay.bundle().digest()
        );
    }
    for (index, tail) in recovered.into_iter().enumerate() {
        assert_eq!(tail.overlay.predecessor().cell, cells[index]);
        assert_eq!(tail.first_node_sequence, index as u64 + 1);
        assert_eq!(tail.last_node_sequence, CELLS as u64 + index as u64 + 1);
        assert_eq!(tail.overlay.final_position(), second.position);
        assert_eq!(tail.overlay.final_commit_sequence(), 3);
    }
    for base in &mut bases {
        base.root.position = first.position;
        base.root.commit_sequence = 2;
    }
    let recovered = build_recovery_overlays(frames, &bases, limits).unwrap();
    assert_eq!(recovered.len(), CELLS);
    for (index, tail) in recovered.iter().enumerate() {
        assert_eq!(tail.first_node_sequence, CELLS as u64 + index as u64 + 1);
        assert_eq!(tail.overlay.predecessor().position, first.position);
        assert_eq!(tail.overlay.final_position(), second.position);
    }
    database.close().unwrap();
}
