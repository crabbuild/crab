//! Node-owned read-only snapshots selected by current Cell policy.

use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crab_cell_runtime::{
    Error, Result,
    cell::actor::CellRuntime,
    client::{CellReadReplica, Receipt},
    control::{Control, ControlState, authority::CellAuthority},
    identity::{CellId, CellTarget, SessionId},
    ltx::{CellReplica, CellStorageLayout},
    node::NodeDirectory,
    peer::PeerReplicaResolver,
    read_policy::ReadPolicyStore,
    registry::Registry,
};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::repository_replica_limits;

const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
const RECONCILE_BATCH: usize = 64;
const MAX_LIVE_NODES: usize = 10_000;

#[derive(Clone)]
pub(crate) struct ReadReplicaManager {
    runtime: CellRuntime,
    registry: Arc<Registry>,
    layout: CellStorageLayout,
    authority: CellAuthority,
    directory: NodeDirectory,
    policy: ReadPolicyStore,
    session: SessionId,
    root: PathBuf,
    activation: Arc<Mutex<()>>,
    active: Arc<RwLock<HashMap<CellId, CellReadReplica>>>,
}

impl ReadReplicaManager {
    pub(crate) fn new(
        runtime: CellRuntime,
        registry: Arc<Registry>,
        layout: CellStorageLayout,
        directory: NodeDirectory,
        session: SessionId,
        root: PathBuf,
    ) -> Self {
        Self {
            runtime,
            registry,
            authority: CellAuthority::new(layout.clone()),
            policy: ReadPolicyStore::new(layout.clone()),
            layout,
            directory,
            session,
            root,
            activation: Arc::new(Mutex::new(())),
            active: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub(crate) async fn activate(&self, target: CellTarget, origin: SessionId) -> Result<Receipt> {
        let _activation = self.activation.lock().await;
        let cell = target.cell_id();
        let control = self
            .authority
            .load(cell)
            .await?
            .ok_or(Error::CellNotActive)?;
        let control = control.value();
        let owner = control.owner.as_ref().ok_or(Error::Fenced)?;
        if control.state != ControlState::Serving
            || control.recovery.is_some()
            || owner.session != origin
        {
            return Err(Error::Fenced);
        }
        if !self.selected(control, origin).await? {
            return Err(Error::Fenced);
        }
        let path = self.destination(cell).await?;
        let existing = { self.active.read().await.get(&cell).cloned() };
        if let Some(existing) = existing {
            match existing.refresh(&path).await {
                Ok(receipt) if self.still_selected(cell).await? => return Ok(receipt),
                Ok(_) => {
                    self.active.write().await.remove(&cell);
                    return Err(Error::Fenced);
                }
                Err(Error::Fenced) => {
                    self.active.write().await.remove(&cell);
                }
                Err(error) => return Err(error),
            }
        }
        let replica = CellReplica::new(
            self.layout.clone(),
            *cell.as_bytes(),
            *control.incarnation.as_bytes(),
            repository_replica_limits(),
        )?;
        let reader = CellReadReplica::open(
            self.runtime.clone(),
            Arc::clone(&self.registry),
            self.authority.clone(),
            self.directory.clone(),
            replica,
            target,
            &path,
        )
        .await?;
        let receipt = reader.receipt().await;
        if !self.still_selected(cell).await? {
            return Err(Error::Fenced);
        }
        self.active.write().await.insert(cell, reader);
        Ok(receipt)
    }

    async fn destination(&self, cell: CellId) -> Result<PathBuf> {
        let directory = self.root.join(format!("{cell:?}"));
        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(|source| Error::Facility {
                name: "Cell read replica directory",
                source: Box::new(source),
            })?;
        Ok(directory.join(format!("{}.sqlite", Uuid::now_v7())))
    }

    async fn selected(&self, control: &Control, origin: SessionId) -> Result<bool> {
        let Some(policy) = self.policy.load(control.cell).await? else {
            return Ok(false);
        };
        let policy = policy.value();
        if policy.incarnation() != control.incarnation || policy.desired_readers() == 0 {
            return Ok(false);
        }
        let candidates = self
            .directory
            .select_readers(
                control.cell,
                origin,
                control.code,
                usize::from(policy.desired_readers()),
                now_ms()?,
                MAX_LIVE_NODES,
            )
            .await?;
        Ok(candidates
            .iter()
            .any(|candidate| candidate.session() == self.session))
    }

    async fn still_selected(&self, cell: CellId) -> Result<bool> {
        let Some(control) = self.authority.load(cell).await? else {
            return Ok(false);
        };
        let control = control.value();
        if control.state != ControlState::Serving || control.recovery.is_some() {
            return Ok(false);
        }
        let Some(owner) = control.owner.as_ref() else {
            return Ok(false);
        };
        self.selected(control, owner.session).await
    }

    pub(crate) async fn run(&self, cancellation: CancellationToken) -> Result<()> {
        let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut cursor = 0_usize;
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                _ = tick.tick() => {
                    let mut readers = self.active.read().await.keys().copied().collect::<Vec<_>>();
                    readers.sort_by_key(|cell| cell.as_bytes().to_owned());
                    let count = readers.len().min(RECONCILE_BATCH);
                    for offset in 0..count {
                        if cancellation.is_cancelled() { return Ok(()); }
                        let cell = readers[(cursor + offset) % readers.len()];
                        let current = self.active.read().await.get(&cell).cloned();
                        if let Some(reader) = current {
                            match self.still_selected(cell).await {
                                Ok(true) => {},
                                Ok(false) => {
                                    self.active.write().await.remove(&cell);
                                    continue;
                                },
                                Err(error) => {
                                    tracing::warn!(?cell, error = %error, "read replica selection check failed");
                                    continue;
                                },
                            }
                            let path = self.destination(cell).await?;
                            match reader.refresh(&path).await {
                                Ok(_) => {},
                                Err(Error::Fenced) => { self.active.write().await.remove(&cell); },
                                Err(error) => tracing::warn!(?cell, error = %error, "read replica refresh failed"),
                            }
                        }
                    }
                    cursor = if readers.is_empty() { 0 } else { (cursor + count) % readers.len() };
                }
            }
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.active.write().await.clear();
    }
}

fn now_ms() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Command("system clock precedes Unix epoch"))?;
    i64::try_from(elapsed.as_millis()).map_err(|_| Error::Command("system clock overflow"))
}

impl PeerReplicaResolver for ReadReplicaManager {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellReadReplica>> + Send + 'static>> {
        let active = Arc::clone(&self.active);
        Box::pin(async move {
            active
                .read()
                .await
                .get(&target.cell_id())
                .cloned()
                .ok_or(Error::CellNotActive)
        })
    }
}
