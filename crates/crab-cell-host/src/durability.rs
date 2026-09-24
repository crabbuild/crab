//! Durability internals for the Cell node host.

use super::*;

/// Error returned by a provider-owned node facility during drain.
pub type FacilityResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Node-log rotation events emitted by the host supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeDurabilityRotation {
    /// The supervisor is retiring the current node-log generation.
    Started,
    /// Shutdown is waiting for pending publications to settle.
    Pending,
    /// A rotation step failed and this rotation is being abandoned.
    Failed,
    /// The replacement generation is installed and serving.
    Completed,
}

/// Provider-owned enrollment adapter used by the host durability supervisor.
///
/// The provider is responsible for authority and transport enrollment. The
/// host consumes the resulting provider-neutral configuration and is the only
/// owner that constructs and installs [`crab_cell_runtime::node::durability::NodeDurability`].
pub trait NodeDurabilityProvider: Send + Sync + 'static {
    /// Recruits one enrollment round for the replacement generation.
    ///
    /// `Ok(None)` means the provider is not ready and the supervisor should
    /// ask again after its recruit interval.
    fn recruit(
        self: Arc<Self>,
        limits: ReplicaLimits,
        required_follower_bytes: u64,
        live_node_limit: usize,
    ) -> Pin<Box<dyn Future<Output = FacilityResult<Option<NodeDurabilityConfig>>> + Send>>;

    /// Reports one rotation event to the provider; the default ignores it.
    fn rotation_event(&self, _event: NodeDurabilityRotation) {}
}

/// Fixed host-owned bounds and identity for the node-log supervisor.
#[derive(Clone, Copy, Debug)]
pub struct NodeDurabilitySupervisorConfig {
    pub(super) application: ApplicationId,
    pub(super) limits: ReplicaLimits,
    pub(super) required_follower_bytes: u64,
    pub(super) live_node_limit: usize,
    pub(super) recruit_interval: std::time::Duration,
    pub(super) rotation_interval: std::time::Duration,
    pub(super) max_issued_frames: u64,
}

impl NodeDurabilitySupervisorConfig {
    /// Creates a bounded supervisor configuration.
    pub fn new(
        application: ApplicationId,
        limits: ReplicaLimits,
        required_follower_bytes: u64,
        live_node_limit: usize,
        recruit_interval: std::time::Duration,
        rotation_interval: std::time::Duration,
        max_issued_frames: u64,
    ) -> crab_cell_runtime::Result<Self> {
        if application.as_bytes().iter().all(|byte| *byte == 0)
            || required_follower_bytes == 0
            || live_node_limit == 0
            || recruit_interval.is_zero()
            || rotation_interval.is_zero()
            || max_issued_frames == 0
        {
            return Err(Error::Control(
                "invalid CellNode durability supervisor configuration",
            ));
        }
        Ok(Self {
            application,
            limits,
            required_follower_bytes,
            live_node_limit,
            recruit_interval,
            rotation_interval,
            max_issued_frames,
        })
    }
}

pub(crate) async fn run_node_durability_supervisor<P>(
    provider: Arc<P>,
    runtime: CellRuntime,
    configuration: NodeDurabilitySupervisorConfig,
    cancellation: CancellationToken,
) -> FacilityResult
where
    P: NodeDurabilityProvider,
{
    let mut recruit = tokio::time::interval(configuration.recruit_interval);
    let mut rotation = tokio::time::interval(configuration.rotation_interval);
    loop {
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            _ = recruit.tick(), if runtime.node_durability().is_none() => {
                match provider.clone().recruit(
                    configuration.limits,
                    configuration.required_follower_bytes,
                    configuration.live_node_limit,
                ).await {
                    Ok(Some(config)) => {
                        match config.build() {
                            Ok(durability) => {
                                if let Err(error) = runtime.install_node_durability(
                                    configuration.application,
                                    durability,
                                ) {
                                    provider.rotation_event(NodeDurabilityRotation::Failed);
                                    return Err(Box::new(error));
                                }
                            }
                            Err(_error) => {
                                provider.rotation_event(NodeDurabilityRotation::Failed);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(_) => provider.rotation_event(NodeDurabilityRotation::Failed),
                }
            }
            _ = rotation.tick(), if runtime.node_durability().is_some() => {
                rotate_node_durability(
                    Arc::clone(&provider),
                    runtime.clone(),
                    configuration,
                    cancellation.clone(),
                ).await?;
            }
        }
    }
}

pub(crate) async fn rotate_node_durability<P>(
    provider: Arc<P>,
    runtime: CellRuntime,
    configuration: NodeDurabilitySupervisorConfig,
    cancellation: CancellationToken,
) -> FacilityResult
where
    P: NodeDurabilityProvider,
{
    let Some((application, durability)) = runtime.node_durability() else {
        return Ok(());
    };
    if application != configuration.application {
        return Err(Box::new(Error::Control(
            "CellNode node durability application changed during rotation",
        )));
    }
    if !durability.needs_rotation(configuration.max_issued_frames) {
        return Ok(());
    }
    provider.rotation_event(NodeDurabilityRotation::Started);
    loop {
        match durability.shutdown().await {
            Ok(()) => break,
            Err(Error::PendingPublication) => {
                provider.rotation_event(NodeDurabilityRotation::Pending);
                tokio::select! {
                    () = cancellation.cancelled() => return Ok(()),
                    () = tokio::time::sleep(configuration.recruit_interval) => {}
                }
            }
            Err(error) => {
                provider.rotation_event(NodeDurabilityRotation::Failed);
                return Err(Box::new(error));
            }
        }
    }
    if cancellation.is_cancelled() {
        return Ok(());
    }
    let replacement = loop {
        match provider
            .clone()
            .recruit(
                configuration.limits,
                configuration.required_follower_bytes,
                configuration.live_node_limit,
            )
            .await
        {
            Ok(Some(config)) => match config.build() {
                Ok(durability) => break durability,
                Err(_) => provider.rotation_event(NodeDurabilityRotation::Failed),
            },
            Ok(None) => {}
            Err(_) => provider.rotation_event(NodeDurabilityRotation::Failed),
        }
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            () = tokio::time::sleep(configuration.recruit_interval) => {}
        }
    };
    if cancellation.is_cancelled() {
        replacement
            .shutdown()
            .await
            .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
        return Ok(());
    }
    match runtime.replace_node_durability(configuration.application, Arc::clone(&replacement)) {
        Ok(_) => {
            provider.rotation_event(NodeDurabilityRotation::Completed);
            Ok(())
        }
        Err(error) => {
            provider.rotation_event(NodeDurabilityRotation::Failed);
            replacement.shutdown().await.map_err(|shutdown_error| {
                Box::new(shutdown_error) as Box<dyn std::error::Error + Send + Sync>
            })?;
            Err(Box::new(error))
        }
    }
}
