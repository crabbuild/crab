use bytes::Bytes;
use futures_util::future::BoxFuture;

use crate::{Error, FollowerReceipt, FollowerStore, FollowerTailPage, NodeId, Result, SessionId};

/// One ordered follower append with the leader's safe truncation watermark.
pub struct AppendRequest {
    pub leader_session: SessionId,
    pub log_epoch: u64,
    pub frames: Vec<Bytes>,
    pub covered_through: u64,
}

/// Recovery request that atomically closes one follower lane to new appends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SealRequest {
    pub leader_session: SessionId,
    pub log_epoch: u64,
}

/// Leader-authorized deletion of one fully object-covered follower lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetireRequest {
    pub leader_session: SessionId,
    pub log_epoch: u64,
    pub covered_through: u64,
}

/// Bounded read of one already sealed follower tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TailRequest {
    pub leader_session: SessionId,
    pub log_epoch: u64,
    pub first_sequence: u64,
}

/// Authenticated peer transport used by node-log shipping and recovery.
///
/// Implementations own mTLS, peer enrollment, request deadlines, and response
/// size limits. Follower storage and LTX verification remain runtime concerns.
pub trait NodeLogTransport: Send + Sync {
    fn append<'a>(
        &'a self,
        member: NodeId,
        request: AppendRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>>;

    fn seal<'a>(
        &'a self,
        member: NodeId,
        request: SealRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>>;

    fn retire<'a>(
        &'a self,
        member: NodeId,
        request: RetireRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>>;

    fn tail<'a>(
        &'a self,
        member: NodeId,
        request: TailRequest,
    ) -> BoxFuture<'a, Result<Vec<Bytes>>>;

    /// Reads one bounded page of a sealed follower tail.
    ///
    /// Implementations with a paged transport should override this method.
    /// The default keeps older transports source-compatible while applying the
    /// same one-megabyte/4096-frame page boundary in memory.
    fn tail_page<'a>(
        &'a self,
        member: NodeId,
        request: TailRequest,
    ) -> BoxFuture<'a, Result<FollowerTailPage>> {
        Box::pin(async move {
            let frames = self.tail(member, request).await?;
            let mut bytes = 0_usize;
            let mut count = 0_usize;
            for frame in &frames {
                let next_bytes = bytes
                    .checked_add(frame.len())
                    .ok_or(Error::Node("follower tail page byte count overflow"))?;
                if count == 4096 || (count != 0 && next_bytes > 1 << 20) {
                    break;
                }
                bytes = next_bytes;
                count += 1;
            }
            let next_sequence = if count < frames.len() {
                let count = u64::try_from(count)
                    .map_err(|_| Error::Node("follower tail page frame count overflow"))?;
                Some(
                    request
                        .first_sequence
                        .checked_add(count)
                        .ok_or(Error::Node("follower tail page sequence overflow"))?,
                )
            } else {
                None
            };
            Ok(FollowerTailPage {
                frames: frames.into_iter().take(count).collect(),
                next_sequence,
            })
        })
    }
}

/// In-process transport for deterministic tests and single-process recovery.
#[derive(Clone)]
pub struct LocalFollowerTransport {
    member: NodeId,
    store: FollowerStore,
}

impl LocalFollowerTransport {
    #[must_use]
    pub const fn new(member: NodeId, store: FollowerStore) -> Self {
        Self { member, store }
    }

    fn validate_member(&self, member: NodeId) -> Result<()> {
        if member != self.member {
            return Err(Error::PeerAuthorization(
                "node-log transport selected a different follower",
            ));
        }
        Ok(())
    }
}

impl NodeLogTransport for LocalFollowerTransport {
    fn append<'a>(
        &'a self,
        member: NodeId,
        request: AppendRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>> {
        Box::pin(async move {
            self.validate_member(member)?;
            self.store
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
    ) -> BoxFuture<'a, Result<FollowerReceipt>> {
        Box::pin(async move {
            self.validate_member(member)?;
            self.store
                .seal(request.leader_session, request.log_epoch)
                .await
        })
    }

    fn retire<'a>(
        &'a self,
        member: NodeId,
        request: RetireRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>> {
        Box::pin(async move {
            self.validate_member(member)?;
            self.store
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
            self.validate_member(member)?;
            self.store
                .read_tail(
                    request.leader_session,
                    request.log_epoch,
                    request.first_sequence,
                )
                .await
        })
    }

    fn tail_page<'a>(
        &'a self,
        member: NodeId,
        request: TailRequest,
    ) -> BoxFuture<'a, Result<FollowerTailPage>> {
        Box::pin(async move {
            self.validate_member(member)?;
            self.store
                .read_tail_page(
                    request.leader_session,
                    request.log_epoch,
                    request.first_sequence,
                )
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
