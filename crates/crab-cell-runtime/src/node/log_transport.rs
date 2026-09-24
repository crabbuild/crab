//! Node-log transport requests: append, retire, seal, and tail.
use bytes::Bytes;
use futures_util::future::BoxFuture;

use crate::follower::FollowerStore;
use crate::follower::{FollowerReceipt, FollowerTailPage};
use crate::identity::NodeId;
use crate::identity::SessionId;
use crate::{Error, Result};

/// One ordered follower append with the leader's safe truncation watermark.
pub struct AppendRequest {
    /// Leader issuing the append.
    pub leader_session: SessionId,
    /// Node-log epoch the append belongs to.
    pub log_epoch: u64,
    /// Ordered frames to append.
    pub frames: Vec<Bytes>,
    /// Highest sequence the leader knows is durable elsewhere, so the follower
    /// may truncate below it.
    pub covered_through: u64,
}

/// Recovery request that atomically closes one follower lane to new appends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SealRequest {
    /// Leader whose lane is being sealed.
    pub leader_session: SessionId,
    /// Epoch of the lane to seal.
    pub log_epoch: u64,
}

/// Leader-authorized deletion of one fully object-covered follower lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetireRequest {
    /// Leader authorizing the retirement.
    pub leader_session: SessionId,
    /// Epoch of the lane to retire.
    pub log_epoch: u64,
    /// Highest sequence object storage covers.
    pub covered_through: u64,
}

/// Bounded read of one already sealed follower tail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TailRequest {
    /// Leader whose sealed tail is read.
    pub leader_session: SessionId,
    /// Epoch of the sealed lane.
    pub log_epoch: u64,
    /// First sequence the follower should return.
    pub first_sequence: u64,
}

/// Authenticated peer transport used by node-log shipping and recovery.
///
/// Implementations own mTLS, peer enrollment, request deadlines, and response
/// size limits. Follower storage and LTX verification remain runtime concerns.
pub trait NodeLogTransport: Send + Sync {
    /// Appends one ordered batch to a member's lane.
    fn append<'a>(
        &'a self,
        member: NodeId,
        request: AppendRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>>;

    /// Closes the member's lane to new appends.
    fn seal<'a>(
        &'a self,
        member: NodeId,
        request: SealRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>>;

    /// Deletes a fully object-covered member lane.
    fn retire<'a>(
        &'a self,
        member: NodeId,
        request: RetireRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>>;

    /// Reads the sealed tail starting at the requested sequence.
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
    /// Binds the local transport to one member and follower store.
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
