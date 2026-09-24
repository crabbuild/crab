//! Rebuilding a recovered Cell tail from published node-log witnesses.
//!
//! The overlays here are what a restart replays: each one names the exact
//! published base, the retained witness frames above it, and the cell tail
//! they prove, so a recovery cannot invent frames it never verified.

use super::*;

/// Exact published Cell state used to validate a recovered node-log witness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryBase {
    pub application: [u8; 16],
    pub cell_epoch: u64,
    pub root: crab_ltx::RootRef,
}

/// One Cell's verified tail selected from a complete node-log witness.
pub struct RecoveredCellTail {
    pub application: [u8; 16],
    pub cell_epoch: u64,
    pub first_node_sequence: u64,
    pub last_node_sequence: u64,
    pub overlay: crab_ltx::RecoveryOverlay,
}

/// Splits one complete, contiguous witness into exact per-Cell overlays.
pub fn build_recovery_overlays(
    frames: Vec<crab_ltx::VerifiedNodeFrame>,
    bases: &[RecoveryBase],
    limits: crab_ltx::Limits,
) -> Result<Vec<RecoveredCellTail>> {
    let first = frames
        .first()
        .ok_or(Error::Node("recovery witness is empty"))?;
    let leader = first.scope().leader_session;
    let log_epoch = first.scope().log_epoch;
    if frames
        .iter()
        .any(|frame| frame.scope().leader_session != leader || frame.scope().log_epoch != log_epoch)
        || !frames.windows(2).all(|pair| {
            pair[0].scope().node_sequence.checked_add(1) == Some(pair[1].scope().node_sequence)
        })
    {
        return Err(Error::Node("recovery witness is not contiguous"));
    }

    type Key = ([u8; 16], [u8; 32], [u8; 16], u64);
    let base_by_key = bases
        .iter()
        .map(|base| {
            (
                (
                    base.application,
                    base.root.cell,
                    base.root.incarnation,
                    base.cell_epoch,
                ),
                *base,
            )
        })
        .collect::<BTreeMap<Key, RecoveryBase>>();
    if base_by_key.len() != bases.len() {
        return Err(Error::Node("recovery bases contain duplicate Cell scope"));
    }

    let mut grouped = BTreeMap::<Key, Vec<crab_ltx::VerifiedNodeFrame>>::new();
    for frame in frames {
        let scope = frame.scope();
        let key = (
            scope.application,
            scope.cell,
            scope.incarnation,
            scope.cell_epoch,
        );
        if !base_by_key.contains_key(&key) {
            return Err(Error::Node("recovery frame has no exact published base"));
        }
        grouped.entry(key).or_default().push(frame);
    }

    let mut recovered = Vec::with_capacity(grouped.len());
    for (key, mut cell_frames) in grouped {
        let base = base_by_key
            .get(&key)
            .ok_or(Error::Node("recovery frame base disappeared"))?;
        // A different Cell can hold back the global coverage watermark even
        // after this Cell's root CAS. Discard only this exact root's covered
        // prefix; a commit/position mismatch must never hide an uncovered cut.
        for frame in &cell_frames {
            let covered = frame.scope().commit_sequence <= base.root.commit_sequence;
            let position = frame.segment().position();
            if covered != (position.txid <= base.root.position.txid)
                || (position.txid == base.root.position.txid && position != base.root.position)
            {
                return Err(Error::Node("recovery frame disagrees with published base"));
            }
        }
        cell_frames.retain(|frame| frame.scope().commit_sequence > base.root.commit_sequence);
        if cell_frames.is_empty() {
            continue;
        }
        let first_frame = cell_frames
            .first()
            .ok_or(Error::Node("recovery Cell tail is empty"))?;
        let last = cell_frames
            .last()
            .ok_or(Error::Node("recovery Cell tail is empty"))?;
        let first_commit = base
            .root
            .commit_sequence
            .checked_add(1)
            .ok_or(Error::Node("recovery commit sequence overflow"))?;
        if first_frame.scope().commit_sequence != first_commit
            || !cell_frames.windows(2).all(|pair| {
                let left = pair[0].scope().commit_sequence;
                let right = pair[1].scope().commit_sequence;
                right == left || left.checked_add(1) == Some(right)
            })
        {
            return Err(Error::Node("recovery Cell commit sequence has a gap"));
        }
        let entries = cell_frames
            .iter()
            .map(|frame| {
                crab_ltx::bundle::BundleEntry::for_cell(
                    base.root.cell,
                    base.root.incarnation,
                    frame.segment().clone(),
                    frame.body().to_vec(),
                )
            })
            .collect();
        let bundle = crab_ltx::bundle::Bundle::encode(entries, limits)?;
        recovered.push(RecoveredCellTail {
            application: base.application,
            cell_epoch: base.cell_epoch,
            first_node_sequence: first_frame.scope().node_sequence,
            last_node_sequence: last.scope().node_sequence,
            overlay: crab_ltx::RecoveryOverlay::new(
                base.root,
                bundle,
                last.segment().position(),
                last.scope().commit_sequence,
            ),
        });
    }
    Ok(recovered)
}

/// Splits a complete witness into per-Cell overlays without retaining segment
/// bodies in the heap. Each Cell gets one scratch-backed bundle builder and
/// every frame body is released after its row is appended.
pub fn build_recovery_overlays_file_backed(
    frames: Vec<crab_ltx::VerifiedNodeFrame>,
    bases: &[RecoveryBase],
    limits: crab_ltx::Limits,
    scratch: &Path,
) -> Result<Vec<RecoveredCellTail>> {
    build_recovery_overlays_file_backed_stream(frames.into_iter().map(Ok), bases, limits, scratch)
}

/// Streaming variant used by the bounded witness reader. The iterator may
/// yield one verified frame at a time; no complete witness is retained.
pub fn build_recovery_overlays_file_backed_stream<I>(
    frames: I,
    bases: &[RecoveryBase],
    limits: crab_ltx::Limits,
    scratch: &Path,
) -> Result<Vec<RecoveredCellTail>>
where
    I: IntoIterator<Item = Result<crab_ltx::VerifiedNodeFrame>>,
{
    let mut frames = frames.into_iter();
    let first = frames
        .next()
        .ok_or(Error::Node("recovery witness is empty"))??;
    let leader = first.scope().leader_session;
    let log_epoch = first.scope().log_epoch;
    let frames = std::iter::once(Ok(first)).chain(frames);

    type Key = ([u8; 16], [u8; 32], [u8; 16], u64);
    let base_by_key = bases
        .iter()
        .map(|base| {
            (
                (
                    base.application,
                    base.root.cell,
                    base.root.incarnation,
                    base.cell_epoch,
                ),
                *base,
            )
        })
        .collect::<BTreeMap<Key, RecoveryBase>>();
    if base_by_key.len() != bases.len() {
        return Err(Error::Node("recovery bases contain duplicate Cell scope"));
    }

    struct CellBuilder {
        base: RecoveryBase,
        bundle: crab_ltx::bundle::BundleBuilder,
        first_node_sequence: u64,
        last_node_sequence: u64,
        last_commit_sequence: u64,
        final_position: crab_ltx::Position,
    }

    let mut previous_node_sequence = None::<u64>;
    let mut grouped = BTreeMap::<Key, CellBuilder>::new();
    for frame in frames {
        let frame = frame?;
        let scope = frame.scope();
        if scope.leader_session != leader || scope.log_epoch != log_epoch {
            return Err(Error::Node("recovery witness has mixed sessions"));
        }
        if previous_node_sequence
            .is_some_and(|previous| previous.checked_add(1) != Some(scope.node_sequence))
        {
            return Err(Error::Node("recovery witness is not contiguous"));
        }
        previous_node_sequence = Some(scope.node_sequence);
        let key = (
            scope.application,
            scope.cell,
            scope.incarnation,
            scope.cell_epoch,
        );
        let base = base_by_key
            .get(&key)
            .copied()
            .ok_or(Error::Node("recovery frame has no exact published base"))?;
        let covered = scope.commit_sequence <= base.root.commit_sequence;
        let position = frame.segment().position();
        if covered != (position.txid <= base.root.position.txid)
            || (position.txid == base.root.position.txid && position != base.root.position)
        {
            return Err(Error::Node("recovery frame disagrees with published base"));
        }
        if covered {
            if grouped.contains_key(&key) {
                return Err(Error::Node(
                    "recovery Cell covered prefix follows uncovered tail",
                ));
            }
            continue;
        }

        if let Some(cell) = grouped.get_mut(&key) {
            if scope.commit_sequence != cell.last_commit_sequence
                && cell.last_commit_sequence.checked_add(1) != Some(scope.commit_sequence)
            {
                return Err(Error::Node("recovery Cell commit sequence has a gap"));
            }
            cell.bundle.push(crab_ltx::bundle::BundleEntry::for_cell(
                base.root.cell,
                base.root.incarnation,
                frame.segment().clone(),
                frame.body().to_vec(),
            ))?;
            cell.last_node_sequence = scope.node_sequence;
            cell.last_commit_sequence = scope.commit_sequence;
            cell.final_position = position;
            continue;
        }

        let first_commit = base
            .root
            .commit_sequence
            .checked_add(1)
            .ok_or(Error::Node("recovery commit sequence overflow"))?;
        if scope.commit_sequence != first_commit {
            return Err(Error::Node("recovery Cell commit sequence has a gap"));
        }
        let mut bundle = crab_ltx::bundle::BundleBuilder::new_temp(scratch, limits)?;
        bundle.push(crab_ltx::bundle::BundleEntry::for_cell(
            base.root.cell,
            base.root.incarnation,
            frame.segment().clone(),
            frame.body().to_vec(),
        ))?;
        grouped.insert(
            key,
            CellBuilder {
                base,
                bundle,
                first_node_sequence: scope.node_sequence,
                last_node_sequence: scope.node_sequence,
                last_commit_sequence: scope.commit_sequence,
                final_position: position,
            },
        );
    }

    grouped
        .into_values()
        .map(|cell| {
            let bundle = cell.bundle.finish()?;
            Ok(RecoveredCellTail {
                application: cell.base.application,
                cell_epoch: cell.base.cell_epoch,
                first_node_sequence: cell.first_node_sequence,
                last_node_sequence: cell.last_node_sequence,
                overlay: crab_ltx::RecoveryOverlay::new(
                    cell.base.root,
                    bundle,
                    cell.final_position,
                    cell.last_commit_sequence,
                ),
            })
        })
        .collect()
}
