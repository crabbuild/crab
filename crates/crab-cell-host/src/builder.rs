//! Builder internals for the Cell node host.

use super::*;

/// Required inputs for one provider-neutral node.
pub struct CellNodeBuilder {
    pub(super) application: Arc<CompiledApplication>,
    pub(super) pool: Option<SqlWorkerPool>,
    pub(super) replica_host: Option<ReplicaHost>,
    pub(super) session: Option<SessionId>,
    pub(super) node_retained_bytes: Option<usize>,
    pub(super) required_components: Vec<&'static str>,
    pub(super) follower_store: Option<(PathBuf, ReplicaLimits, DiskBudget)>,
}

pub(crate) struct CellNodeParts {
    pub(super) application: Arc<CompiledApplication>,
    pub(super) pool: SqlWorkerPool,
    pub(super) replica_host: ReplicaHost,
    pub(super) session: SessionId,
    pub(super) node_retained_bytes: usize,
    pub(super) required_components: Vec<&'static str>,
    pub(super) follower_store: Option<(PathBuf, ReplicaLimits, DiskBudget)>,
}

impl CellNodeBuilder {
    /// Starts a builder for one immutable compiled application.
    #[must_use]
    pub fn new(application: Arc<CompiledApplication>) -> Self {
        Self {
            application,
            pool: None,
            replica_host: None,
            session: None,
            node_retained_bytes: None,
            required_components: Vec::new(),
            follower_store: None,
        }
    }

    /// Supplies the shared SQL worker pool and its runtime byte ceiling.
    #[must_use]
    pub fn with_runtime(mut self, pool: SqlWorkerPool, node_retained_bytes: usize) -> Self {
        self.pool = Some(pool);
        self.node_retained_bytes = Some(node_retained_bytes);
        self
    }

    /// Supplies the LTX host admission and local capacity policy.
    #[must_use]
    pub fn with_replica_host(mut self, host: ReplicaHost) -> Self {
        self.replica_host = Some(host);
        self
    }

    /// Supplies the unique node session used by runtime ownership records.
    #[must_use]
    pub fn with_session(mut self, session: SessionId) -> Self {
        self.session = Some(session);
        self
    }

    /// Declares the owned production components required before readiness.
    pub fn with_required_owned_components(
        mut self,
        names: impl IntoIterator<Item = &'static str>,
    ) -> crab_cell_runtime::Result<Self> {
        append_required_components(&mut self.required_components, names)?;
        Ok(self)
    }

    /// Supplies the durable follower store that the node must retain.
    #[must_use]
    pub fn with_follower_store(
        mut self,
        root: PathBuf,
        limits: ReplicaLimits,
        disk: DiskBudget,
    ) -> Self {
        self.follower_store = Some((root, limits, disk));
        self
    }

    /// Validates all required inputs before starting any background runtime task.
    pub fn build(self) -> crab_cell_runtime::Result<CellNode> {
        let CellNodeParts {
            application,
            pool,
            replica_host,
            session,
            node_retained_bytes,
            required_components,
            follower_store,
        } = self.required_parts()?;
        let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
            pool,
            node_retained_bytes,
            session,
            replica_host,
        )?;
        install_application_limits(&runtime, &application)?;
        let node = CellNode {
            application,
            runtime,
            session,
            state: Arc::new(Mutex::new(NodeState::Starting)),
            lease_installed: AtomicBool::new(false),
            shutdown_lock: Arc::new(tokio::sync::Mutex::new(())),
            facilities: Arc::new(Mutex::new(Vec::new())),
            required_components: Arc::new(Mutex::new(required_components)),
            task_group: Arc::new(Mutex::new(None)),
        };
        node.install_follower_store(follower_store)?;
        Ok(node)
    }

    /// Builds an unadvertised host for bounded offline maintenance.
    ///
    /// This path intentionally uses object-only runtime admission: the caller
    /// must keep the host private and may not expose serving readiness.
    pub fn build_unleased_for_maintenance(self) -> crab_cell_runtime::Result<CellNode> {
        let CellNodeParts {
            application,
            pool,
            replica_host,
            session,
            node_retained_bytes,
            required_components,
            follower_store,
        } = self.required_parts()?;
        let runtime =
            CellRuntime::new_with_replica_host(pool, node_retained_bytes, session, replica_host)?;
        install_application_limits(&runtime, &application)?;
        let node = CellNode {
            application,
            runtime,
            session,
            state: Arc::new(Mutex::new(NodeState::Starting)),
            lease_installed: AtomicBool::new(false),
            shutdown_lock: Arc::new(tokio::sync::Mutex::new(())),
            facilities: Arc::new(Mutex::new(Vec::new())),
            required_components: Arc::new(Mutex::new(required_components)),
            task_group: Arc::new(Mutex::new(None)),
        };
        node.install_follower_store(follower_store)?;
        Ok(node)
    }

    pub(super) fn required_parts(self) -> crab_cell_runtime::Result<CellNodeParts> {
        let pool = self
            .pool
            .ok_or(Error::Control("CellNode requires a SQL worker pool"))?;
        let host = self
            .replica_host
            .ok_or(Error::Control("CellNode requires an LTX replica host"))?;
        let session = self
            .session
            .ok_or(Error::Control("CellNode requires a node session"))?;
        if session.as_bytes().iter().all(|byte| *byte == 0) {
            return Err(Error::Control("CellNode node session is zero"));
        }
        let node_retained_bytes = self
            .node_retained_bytes
            .filter(|bytes| *bytes != 0)
            .ok_or(Error::Control("CellNode retained-byte ceiling is missing"))?;
        Ok(CellNodeParts {
            application: self.application,
            pool,
            replica_host: host,
            session,
            node_retained_bytes,
            required_components: self.required_components,
            follower_store: self.follower_store,
        })
    }
}

fn install_application_limits(
    runtime: &CellRuntime,
    application: &CompiledApplication,
) -> crab_cell_runtime::Result<()> {
    runtime.install_application_limits(application.cell_types().iter().map(|cell_type| {
        (
            cell_type.namespace(),
            cell_type.database_limit_bytes(),
            cell_type.capture_limit_bytes(),
        )
    }))
}

pub(crate) fn append_required_components(
    required: &mut Vec<&'static str>,
    names: impl IntoIterator<Item = &'static str>,
) -> crab_cell_runtime::Result<()> {
    let names = names.into_iter().collect::<Vec<_>>();
    if names.is_empty() || names.len() > MAX_NODE_FACILITIES {
        return Err(Error::Control(
            "CellNode required component count is out of bounds",
        ));
    }
    let mut seen = HashSet::with_capacity(names.len());
    if names
        .iter()
        .any(|name| name.is_empty() || required.contains(name) || !seen.insert(*name))
    {
        return Err(Error::Control(
            "CellNode required component names must be unique and non-empty",
        ));
    }
    if required.len().saturating_add(names.len()) > MAX_NODE_FACILITIES {
        return Err(Error::Capacity("CellNode required component limit reached"));
    }
    required.extend(names);
    Ok(())
}
