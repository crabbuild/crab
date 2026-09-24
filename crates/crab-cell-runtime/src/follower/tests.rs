use super::*;
use crab_ltx::{Db, NodeFrameScope, encode_node_frame};

fn frame(sequence: u64, segment: &crab_ltx::LocalSegment, limits: crab_ltx::Limits) -> Bytes {
    encode_node_frame(
        NodeFrameScope {
            leader_session: [1; 16],
            log_epoch: 2,
            node_sequence: sequence,
            application: [3; 16],
            cell: [4; 32],
            incarnation: [5; 16],
            cell_epoch: 6,
            commit_sequence: sequence,
        },
        segment.info().clone(),
        Bytes::from(std::fs::read(segment.path()).unwrap()),
        limits,
    )
    .unwrap()
    .encoded()
    .clone()
}

mod append;
mod budget;
mod scan;
