//! Shared placement, admission-aware selection and bounded replica query routing.

use std::{collections::HashMap, sync::Arc, time::Duration};

use crate::control::{ControlState, authority::CellAuthority};
use crate::identity::{CellTarget, NodeId, SessionId};
use crate::node::{NodeAdvertisement, NodeDirectory};
use crate::peer::{PeerReplicaResolver, ReplicaPeerClient};
use crate::read_policy::ReadPolicyStore;
use crate::registry::Query;
use crate::{Error, Result};

use super::{CellDescription, EncodedQuery, Observed, Receipt, local::unix_time_ms};
use crate::codec::{decode_wire, encode_wire};

/// Selects read replicas from authoritative policy and signed live membership.
///
/// Clone this router across callers to share outstanding-attempt counts. These
/// counts describe this ingress only, not execution load from other ingresses.
#[derive(Clone)]
pub struct ReplicaReadRouter {
    authority: CellAuthority,
    policy: ReadPolicyStore,
    directory: NodeDirectory,
    load: Arc<ReplicaRouting>,
}

impl ReplicaReadRouter {
    /// Creates a router sharing the runtime's existing authority and directory.
    #[must_use]
    pub fn new(authority: CellAuthority, directory: NodeDirectory) -> Self {
        Self {
            policy: ReadPolicyStore::new(authority.layout().clone()),
            authority,
            directory,
            load: Arc::new(ReplicaRouting::default()),
        }
    }

    /// Returns the authority-pinned description and current selected readers.
    ///
    /// This is placement evidence; each reader must still prove its snapshot
    /// and fresh owner authority before releasing a query result.
    pub async fn selected(
        &self,
        target: &CellTarget,
    ) -> Result<(CellDescription, Vec<NodeAdvertisement>)> {
        let cell = target.cell_id();
        let control = self
            .authority
            .load(cell)
            .await?
            .ok_or(Error::ReplicaUnavailable)?;
        let control = control.value();
        if control.state != ControlState::Serving || control.recovery.is_some() {
            return Err(Error::Fenced);
        }
        let owner = control.owner.as_ref().ok_or(Error::Fenced)?;
        let expected = CellDescription {
            cell,
            incarnation: control.incarnation,
            code: control.code,
            schema: control.schema,
        };
        let Some(policy) = self.policy.load(cell).await? else {
            return Ok((expected, Vec::new()));
        };
        let policy = policy.value();
        if policy.incarnation() != control.incarnation || policy.desired_readers() == 0 {
            return Ok((expected, Vec::new()));
        }
        let selected = self
            .directory
            .select_readers(
                cell,
                owner.session,
                control.code,
                usize::from(policy.desired_readers()),
                unix_time_ms()?,
                10_000,
            )
            .await?;
        Ok((expected, selected))
    }

    /// Executes a typed read on a selected replica and returns its serving node.
    ///
    /// Selection and all attempts share one five-second deadline. A local
    /// resolver may serve this node's admitted views without a self-dial. The
    /// caller must perform its product authorization before invoking this route.
    /// There is no owner fallback when replicas are absent, behind or fenced.
    pub async fn query<Q: Query>(
        &self,
        peer: &ReplicaPeerClient,
        local: Option<(SessionId, &dyn PeerReplicaResolver)>,
        target: &CellTarget,
        minimum: Option<Receipt>,
        input: Q::Input,
    ) -> Result<(Observed<Q::Output>, NodeId)> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let query = async {
            let (expected, mut selected) = self.selected(target).await?;
            super::local::validate_minimum(expected, minimum)?;
            let operation = peer.registry().query_contract::<Q>(target.namespace())?;
            super::validate_description(peer.registry(), Q::MODULE, expected, operation)?;
            let input = encode_wire(&input, operation.input_limit)?;
            let mut behind = None;
            let mut fenced = false;
            while !selected.is_empty() {
                // Selection and reservation are atomic across this ingress's
                // Cells. The guard releases load on every exit, including timeout.
                let (index, attempt) = self
                    .load
                    .reserve(selected.iter().map(NodeAdvertisement::node))?;
                let node = selected.remove(index);
                let reader_node = attempt.node;
                let query = EncodedQuery {
                    target: target.clone(),
                    expected,
                    minimum,
                    now_ms: unix_time_ms()?,
                    module: Q::MODULE,
                    operation_id: Q::ID,
                    codec_version: Q::CODEC_VERSION,
                    input: input.clone(),
                    input_limit: operation.input_limit,
                    output_limit: operation.output_limit,
                };
                let queried = if let Some((_, resolver)) =
                    local.filter(|(session, _)| *session == node.session())
                {
                    match resolver.resolve(target.clone()).await {
                        Ok(reader) => reader.query_encoded(query).await,
                        Err(error) => Err(error),
                    }
                } else {
                    peer.query_encoded(node, query, deadline).await
                };
                drop(attempt);
                match queried {
                    Ok(result) => {
                        if result.receipt.cell != expected.cell
                            || result.receipt.incarnation != expected.incarnation
                            || minimum.is_some_and(|minimum| {
                                result.receipt.commit_sequence < minimum.commit_sequence
                            })
                        {
                            return Err(Error::Peer("read replica returned an invalid receipt"));
                        }
                        return Ok((
                            Observed {
                                output: decode_wire(&result.output, operation.output_limit)?,
                                receipt: result.receipt,
                            },
                            reader_node,
                        ));
                    }
                    Err(error @ Error::ReplicaBehind { .. }) => behind = Some(error),
                    Err(Error::Fenced) => fenced = true,
                    Err(error @ Error::PeerAuthorization(_)) => return Err(error),
                    Err(error) => {
                        tracing::debug!(cell = ?target.cell_id(), error = %error, "selected read replica unavailable")
                    }
                }
            }
            Err(behind.unwrap_or(if fenced {
                Error::Fenced
            } else {
                Error::ReplicaUnavailable
            }))
        };
        tokio::time::timeout_at(deadline, query)
            .await
            .unwrap_or(Err(Error::ReplicaUnavailable))
    }
}

pub(super) struct ReplicaClient {
    pub(super) router: ReplicaReadRouter,
    pub(super) peer: ReplicaPeerClient,
    pub(super) local: Option<(SessionId, Arc<dyn PeerReplicaResolver>)>,
}

#[derive(Default)]
struct ReplicaRouting {
    state: std::sync::Mutex<ReplicaRoutingState>,
}

#[derive(Default)]
struct ReplicaRoutingState {
    cursor: usize,
    in_flight: HashMap<NodeId, usize>,
}

struct ReplicaAttempt<'a> {
    routing: &'a ReplicaRouting,
    node: NodeId,
}

impl ReplicaRouting {
    fn reserve(
        &self,
        candidates: impl ExactSizeIterator<Item = NodeId>,
    ) -> Result<(usize, ReplicaAttempt<'_>)> {
        let count = candidates.len();
        if count == 0 {
            return Err(Error::ReplicaUnavailable);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::Control("replica routing load lock poisoned"))?;
        let start = state.cursor % count;
        let (index, node) = candidates
            .enumerate()
            .min_by_key(|(index, node)| {
                let distance = if *index >= start {
                    *index - start
                } else {
                    count - (start - *index)
                };
                (state.in_flight.get(node).copied().unwrap_or(0), distance)
            })
            .ok_or(Error::ReplicaUnavailable)?;
        state.cursor = index + 1;
        *state.in_flight.entry(node).or_default() += 1;
        Ok((
            index,
            ReplicaAttempt {
                routing: self,
                node,
            },
        ))
    }
}

impl Drop for ReplicaAttempt<'_> {
    fn drop(&mut self) {
        let mut state = self
            .routing
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(active) = state.in_flight.get_mut(&self.node) {
            *active -= 1;
            if *active == 0 {
                state.in_flight.remove(&self.node);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes() -> [NodeId; 3] {
        [1, 2, 3].map(|byte| NodeId::from_bytes([byte; 16]))
    }

    #[test]
    fn idle_readers_share_ties_and_a_busy_reader_is_skipped() {
        let routing = ReplicaRouting::default();
        let nodes = nodes();
        let mut counts = [0; 3];
        for _ in 0..12 {
            let (index, _attempt) = routing.reserve(nodes.into_iter()).unwrap();
            counts[index] += 1;
        }
        assert_eq!(counts, [4, 4, 4]);

        let (_, busy) = routing.reserve(nodes.into_iter()).unwrap();
        assert_eq!(busy.node, nodes[0]);
        counts = [0; 3];
        for _ in 0..12 {
            let (index, _attempt) = routing.reserve(nodes.into_iter()).unwrap();
            counts[index] += 1;
        }
        assert_eq!(counts, [0, 6, 6]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_attempt_releases_load_for_overlapping_candidate_sets() {
        let routing = Arc::new(ReplicaRouting::default());
        let nodes = nodes();
        let blocked_routing = Arc::clone(&routing);
        let (started, ready) = tokio::sync::oneshot::channel();
        let blocked = tokio::spawn(async move {
            let (_, attempt) = blocked_routing.reserve(nodes.into_iter()).unwrap();
            started.send(attempt.node).unwrap();
            std::future::pending::<()>().await;
            drop(attempt);
        });
        assert_eq!(ready.await.unwrap(), nodes[0]);
        // A different Cell can share the busy physical reader. Its local load
        // must carry across the two candidate sets without pinning membership.
        let (index, attempt) = routing.reserve([nodes[0], nodes[2]].into_iter()).unwrap();
        assert_eq!(index, 1);
        drop(attempt);
        blocked.abort();
        assert!(blocked.await.unwrap_err().is_cancelled());
        assert!(routing.state.lock().unwrap().in_flight.is_empty());
    }

    #[tokio::test]
    async fn expired_route_attempt_releases_its_load() {
        let routing = ReplicaRouting::default();
        let nodes = nodes();
        let expired = tokio::time::timeout(Duration::from_millis(10), async {
            let (_, _attempt) = routing.reserve(nodes.into_iter()).unwrap();
            std::future::pending::<()>().await;
        })
        .await;
        assert!(expired.is_err());
        assert!(routing.state.lock().unwrap().in_flight.is_empty());
    }
}
