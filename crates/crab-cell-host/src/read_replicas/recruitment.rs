//! Owner-side recruitment for every compiled application namespace.

use super::*;
use crab_cell_runtime::node::NodeAdvertisement;
use crab_cell_runtime::{
    cell::{application::ApplicationIdentity, catalog::CatalogEntry},
    client::CellDescription,
    peer::ReplicaPeerClient,
};
use futures_util::{StreamExt, stream, stream::FuturesUnordered};
use std::collections::{HashSet, VecDeque};
use tokio::sync::broadcast;

const ACTIVATION_CONCURRENCY: usize = 16;

struct Activation {
    target: CellTarget,
    expected: CellDescription,
    node: NodeAdvertisement,
}

/// Recruits selected readers for the locally owned Cells of one application.
///
/// Activation is advisory. Each receiver independently checks current owner,
/// policy, membership, and resource admission before opening a read snapshot.
#[derive(Clone)]
pub struct ReadReplicaRecruiter {
    readers: ReadReplicaManager,
    identity: ApplicationIdentity,
    peer: ReplicaPeerClient,
    cursor: Arc<Mutex<usize>>,
}

impl ReadReplicaRecruiter {
    /// Binds owner recruitment to an existing manager and authenticated transport.
    pub fn new(
        readers: ReadReplicaManager,
        identity: ApplicationIdentity,
        peer: ReplicaPeerClient,
    ) -> Result<Self> {
        if identity.application().as_bytes() != readers.layout.application_id() {
            return Err(Error::Control(
                "read recruitment application does not match storage",
            ));
        }
        Ok(Self {
            readers,
            identity,
            peer,
            cursor: Arc::new(Mutex::new(0)),
        })
    }

    /// Reconciles one authorized target within a bounded, cancellable owner check.
    pub async fn reconcile(&self, target: CellTarget) -> Result<()> {
        tokio::select! {
            () = self.readers.closed.cancelled() => Err(Error::RuntimeClosed),
            result = tokio::time::timeout(RECONCILE_DEADLINE, async {
                let cell = target.cell_id();
                let attempts = stream::iter(self.prepare(target).await?)
                    .map(|activation| self.activate(activation))
                    .buffer_unordered(ACTIVATION_CONCURRENCY);
                tokio::pin!(attempts);
                while let Some(result) = attempts.next().await {
                    if let Err(error) = result {
                        tracing::warn!(?cell, error = %error, "read replica activation hint failed");
                    }
                }
                Ok(())
            }) => result.map_err(|_| Error::Deadline)?,
        }
    }

    async fn prepare(&self, target: CellTarget) -> Result<Vec<Activation>> {
        self.readers.ensure_open()?;
        if target.tenant() != self.identity.tenant()
            || target.application() != self.identity.application()
            || self
                .readers
                .registry
                .namespace_contract(target.namespace())
                .is_none()
        {
            return Err(Error::PeerAuthorization(
                "read recruitment target is outside the application",
            ));
        }
        let cell = target.cell_id();
        let Some(policy) = self.readers.policy.load(cell).await? else {
            return Ok(Vec::new());
        };
        let policy = policy.value();
        if policy.desired_readers() == 0 {
            return Ok(Vec::new());
        }
        let Some(control) = self.readers.authority.load(cell).await? else {
            return Ok(Vec::new());
        };
        let control = control.value();
        if control.state != ControlState::Serving
            || control.recovery.is_some()
            || control
                .owner
                .as_ref()
                .is_none_or(|owner| owner.session != self.readers.session)
            || policy.incarnation() != control.incarnation
        {
            return Ok(Vec::new());
        }
        let expected = CellDescription {
            cell,
            incarnation: control.incarnation,
            code: control.code,
            schema: control.schema,
        };
        let selected = self
            .readers
            .directory
            .select_readers(
                cell,
                self.readers.session,
                control.code,
                usize::from(policy.desired_readers()),
                now_ms()?,
                MAX_LIVE_NODES,
            )
            .await?;
        Ok(selected
            .into_iter()
            .map(|node| Activation {
                target: target.clone(),
                expected,
                node,
            })
            .collect())
    }

    async fn activate(&self, activation: Activation) -> Result<()> {
        self.peer
            .activate(
                &activation.target,
                &self.readers.directory,
                activation.node,
                activation.expected,
            )
            .await?;
        Ok(())
    }

    /// Runs one bounded pass over local owner Cells, retaining fair progress across retries.
    pub async fn reconcile_active(&self) -> Result<()> {
        self.readers.ensure_open()?;
        tokio::select! {
            () = self.readers.closed.cancelled() => Err(Error::RuntimeClosed),
            result = tokio::time::timeout(RECONCILE_DEADLINE, self.reconcile_active_open()) => {
                result.map_err(|_| Error::Deadline)?
            }
        }
    }

    async fn reconcile_active_open(&self) -> Result<()> {
        let mut entries = self.readers.runtime.active_catalog_entries().await?;
        entries.sort_by_key(|entry| *entry.cell().as_bytes());
        if entries.is_empty() {
            *self.cursor.lock().await = 0;
            return Ok(());
        }
        for _ in 0..entries.len().min(RECONCILE_BATCH) {
            let entry = {
                let mut cursor = self.cursor.lock().await;
                let index = *cursor % entries.len();
                // Advance before I/O and release the cursor so an explicit
                // operator pass cannot hold publication scheduling behind it.
                *cursor = (index + 1) % entries.len();
                &entries[index]
            };
            let result = match self.target(entry) {
                Ok(target) => self.reconcile(target).await,
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                tracing::warn!(cell = ?entry.cell(), error = %error, "read replica recruitment failed");
            }
        }
        Ok(())
    }

    fn target(&self, entry: &CatalogEntry) -> Result<CellTarget> {
        let target = CellTarget::new(
            self.identity.tenant(),
            self.identity.application(),
            entry.namespace(),
            entry.partition(),
        )?;
        if target.cell_id() != entry.cell() {
            return Err(Error::Control("reader catalog target differs"));
        }
        Ok(target)
    }

    pub(crate) async fn run(&self, cancellation: CancellationToken) -> Result<()> {
        let mut publications = self.readers.runtime.subscribe_publications();
        let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut dirty = VecDeque::new();
        let mut queued = VecDeque::<(Activation, tokio::time::Instant)>::new();
        let mut pending = HashSet::new();
        let mut scanning = FuturesUnordered::<BoxFuture<'_, Result<Vec<CatalogEntry>>>>::new();
        let mut preparing = FuturesUnordered::<
            BoxFuture<'_, (tokio::time::Instant, Result<Vec<Activation>>)>,
        >::new();
        let mut active =
            FuturesUnordered::<BoxFuture<'_, ((CellId, SessionId), Result<()>)>>::new();
        loop {
            while active.len() < ACTIVATION_CONCURRENCY {
                let Some((activation, deadline)) = queued.pop_front() else {
                    break;
                };
                let key = (activation.target.cell_id(), activation.node.session());
                if tokio::time::Instant::now() >= deadline {
                    pending.remove(&key);
                    continue;
                }
                active.push(Box::pin(async move {
                    let result = tokio::time::timeout_at(deadline, self.activate(activation))
                        .await
                        .unwrap_or(Err(Error::Deadline));
                    (key, result)
                }));
            }
            // Retain at most one discovered Cell's candidate list, plus a bounded
            // dirty queue. Pending activations survive later publication hints;
            // only completed (Cell, session) pairs can be scheduled again.
            if queued.is_empty()
                && preparing.is_empty()
                && let Some(entry) = dirty.pop_front()
            {
                let deadline = tokio::time::Instant::now() + RECONCILE_DEADLINE;
                preparing.push(Box::pin(async move {
                    let result = tokio::time::timeout_at(deadline, async {
                        self.prepare(self.target(&entry)?).await
                    })
                    .await
                    .unwrap_or(Err(Error::Deadline));
                    (deadline, result)
                }));
            }
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                () = self.readers.closed.cancelled() => return Ok(()),
                Some(result) = scanning.next(), if !scanning.is_empty() => {
                    let mut entries = result?;
                    entries.sort_by_key(|entry| *entry.cell().as_bytes());
                    let mut cursor = self.cursor.lock().await;
                    for _ in 0..entries.len().min(RECONCILE_BATCH) {
                        let index = *cursor % entries.len();
                        *cursor = (index + 1) % entries.len();
                        enqueue(&mut dirty, entries[index].clone());
                    }
                }
                Some((key, result)) = active.next(), if !active.is_empty() => {
                    pending.remove(&key);
                    if let Err(error) = result {
                        tracing::warn!(cell = ?key.0, session = ?key.1, error = %error, "read replica activation hint failed");
                    }
                }
                Some((deadline, result)) = preparing.next(), if !preparing.is_empty() => {
                    match result {
                        Ok(activations) => {
                            for activation in activations {
                                let key = (activation.target.cell_id(), activation.node.session());
                                if pending.insert(key) {
                                    queued.push_back((activation, deadline));
                                }
                            }
                        }
                        Err(error) => tracing::warn!(error = %error, "read replica discovery failed"),
                    }
                }
                _ = tick.tick() => {
                    // Catalog enumeration uses the runtime mailbox. Poll it with
                    // peer work and cancellation so a busy actor cannot strand
                    // healthy hints or host drain behind the scan.
                    if scanning.is_empty() {
                        scanning.push(Box::pin(self.readers.runtime.active_catalog_entries()));
                    }
                }
                published = publications.recv() => match published {
                    Ok(entry) => enqueue(&mut dirty, entry),
                    // The periodic scan repairs overflow without allowing a hot
                    // publisher to create an unbounded queue of retained work.
                    Err(broadcast::error::RecvError::Lagged(_)) => {},
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
            }
        }
    }
}

fn enqueue(dirty: &mut VecDeque<CatalogEntry>, entry: CatalogEntry) {
    if let Some(queued) = dirty
        .iter_mut()
        .find(|queued| queued.cell() == entry.cell())
    {
        *queued = entry;
    } else if dirty.len() < RECONCILE_BATCH {
        dirty.push_back(entry);
    }
}
