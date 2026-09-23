//! Node log transport tests extracted from `src/node_log_transport.rs`.
use crab_cell_runtime::follower::FollowerReceipt;
use crab_cell_runtime::identity::NodeId;
use crab_cell_runtime::node::log_transport::{
    AppendRequest, NodeLogTransport, RetireRequest, SealRequest, TailRequest,
};

use bytes::Bytes;
use futures_util::future::BoxFuture;

use crab_cell_runtime::*;

struct LegacyTransport {
    frames: Vec<Bytes>,
}

impl NodeLogTransport for LegacyTransport {
    fn append<'a>(
        &'a self,
        _member: NodeId,
        _request: AppendRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("append is not used by this test")) })
    }

    fn seal<'a>(
        &'a self,
        _member: NodeId,
        _request: SealRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("seal is not used by this test")) })
    }

    fn retire<'a>(
        &'a self,
        _member: NodeId,
        _request: RetireRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("retire is not used by this test")) })
    }

    fn tail<'a>(
        &'a self,
        _member: NodeId,
        _request: TailRequest,
    ) -> BoxFuture<'a, Result<Vec<Bytes>>> {
        let frames = self.frames.clone();
        Box::pin(async move { Ok(frames) })
    }
}

#[tokio::test]
async fn legacy_tail_fallback_preserves_page_boundaries() {
    let transport = LegacyTransport {
        frames: (0..4_097).map(|_| Bytes::from_static(b"frame")).collect(),
    };
    let page = transport
        .tail_page(
            NodeId::from_bytes([1; 16]),
            TailRequest {
                leader_session: SessionId::from_bytes([2; 16]),
                log_epoch: 1,
                first_sequence: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.frames.len(), 4_096);
    assert_eq!(page.next_sequence, Some(4_106));
}
