use bytes::Bytes;
use futures_util::future::BoxFuture;

use crate::{Error, FollowerReceipt, FollowerStore, Result, SessionId};

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
        member: SessionId,
        request: AppendRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>>;

    fn seal<'a>(
        &'a self,
        member: SessionId,
        request: SealRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>>;

    fn retire<'a>(
        &'a self,
        member: SessionId,
        request: RetireRequest,
    ) -> BoxFuture<'a, Result<FollowerReceipt>>;

    fn tail<'a>(
        &'a self,
        member: SessionId,
        request: TailRequest,
    ) -> BoxFuture<'a, Result<Vec<Bytes>>>;
}

/// In-process transport for deterministic tests and single-process recovery.
#[derive(Clone)]
pub struct LocalFollowerTransport {
    member: SessionId,
    store: FollowerStore,
}

impl LocalFollowerTransport {
    #[must_use]
    pub const fn new(member: SessionId, store: FollowerStore) -> Self {
        Self { member, store }
    }

    fn validate_member(&self, member: SessionId) -> Result<()> {
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
        member: SessionId,
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
        member: SessionId,
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
        member: SessionId,
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
        member: SessionId,
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
}
