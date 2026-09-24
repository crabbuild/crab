//! Running module work through a compiled registry.
//!
//! The registry decides which module owns a namespace, which runner can
//! carry its Activities, maintenance jobs, and Effects, and whether an
//! activity must run on the blocking pool.

use super::*;

impl Registry {
    pub(crate) fn activity_support(
        &self,
        module: &'static str,
        definition: Digest,
    ) -> Result<Vec<ActivitySupport>> {
        let supported = self
            .activities
            .keys()
            .filter(|key| key.module == module && key.definition == *definition.as_bytes())
            .map(|key| ActivitySupport {
                activity_type: key.activity.clone(),
                definition_digest: Digest::from_bytes(key.definition),
            })
            .collect::<Vec<_>>();
        if supported.is_empty() {
            return Err(Error::Registry("activity support is unavailable"));
        }
        Ok(supported)
    }

    pub(crate) fn execute_activity(
        &self,
        module: &'static str,
        definition: Digest,
        activity: &str,
        context: ActivityContext,
        input: Vec<u8>,
        blocking: Option<BlockingActivityReservation>,
    ) -> ActivityFuture {
        let key = match ActivityKey::new(module, definition, activity) {
            Ok(key) => key,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let Some(handler) = self.activities.get(&key).copied() else {
            return Box::pin(async { Err(Error::Registry("activity binding is unavailable")) });
        };
        match handler {
            ActivityFunction::Async(handler) => Box::pin(async move {
                let _blocking = blocking;
                handler(context, input).await
            }),
            ActivityFunction::Blocking(handler) => {
                let Some(blocking) = blocking else {
                    return Box::pin(async {
                        Err(Error::Capacity("blocking activity slot was not reserved"))
                    });
                };
                Box::pin(async move {
                    blocking
                        .execute(Box::new(move || handler(context, input)))
                        .await
                })
            }
        }
    }

    /// Runs the statically bound maintenance command for one namespace.
    pub async fn run_maintenance_once(
        &self,
        client: CellClient,
        target: CellTarget,
        identity: MutationIdentity,
        request: MaintenanceTickRequest,
    ) -> std::result::Result<
        Committed<MaintenanceTickOutcome>,
        InvocationError<MaintenanceTickOutcome>,
    > {
        let module = self
            .namespace_modules
            .get(&target.namespace())
            .map(|(module, _)| *module)
            .ok_or_else(|| InvocationError::NotStarted(Error::Registry("namespace unavailable")))?;
        let runner = self
            .maintenance_runners
            .get(module)
            .copied()
            .ok_or_else(|| {
                InvocationError::NotStarted(Error::Registry("maintenance runner unavailable"))
            })?;
        runner(client, target, identity, request).await
    }

    /// Reports whether one namespace has statically linked native activities.
    #[must_use]
    pub fn has_activity_runner(&self, namespace: NamespaceId) -> bool {
        self.activity_runners.contains_key(&namespace)
    }

    /// Reports whether this release contains any native blocking activity.
    #[must_use]
    pub fn has_blocking_activities(&self) -> bool {
        !self.blocking_activity_namespaces.is_empty()
    }

    /// Reports whether this namespace needs pre-claim blocking admission.
    #[must_use]
    pub fn requires_blocking_activity(&self, namespace: NamespaceId) -> bool {
        self.blocking_activity_namespaces.contains(&namespace)
    }

    /// Runs at most one statically bound native activity from one Workflow shard.
    pub async fn run_activity_once(
        &self,
        client: CellClient,
        target: &CellTarget,
        lease_ms: u32,
        blocking: Option<BlockingActivityReservation>,
    ) -> std::result::Result<ActivityRunOutcome, ActivitySupervisorError> {
        if self.requires_blocking_activity(target.namespace()) && blocking.is_none() {
            return Err(ActivitySupervisorError::Runtime(Error::Capacity(
                "blocking activity slot was not reserved",
            )));
        }
        let runner = self
            .activity_runners
            .get(&target.namespace())
            .copied()
            .ok_or(ActivitySupervisorError::Runtime(Error::Registry(
                "activity runner unavailable",
            )))?;
        let shard = u32::from_be_bytes(target.partition().try_into().map_err(|_| {
            ActivitySupervisorError::Runtime(Error::Identity(
                "Workflow partition is not a canonical shard",
            ))
        })?);
        runner(
            client,
            target.tenant(),
            target.application(),
            shard,
            lease_ms,
            blocking,
        )
        .await
    }

    /// Runs at most one statically bound effect from one explicit source Cell.
    pub async fn run_effect_once(
        &self,
        client: CellClient,
        target: CellTarget,
        peer: EffectPeerClient,
        lease_ms: u32,
    ) -> std::result::Result<EffectRunOutcome, EffectSupervisorError> {
        let module = self
            .namespace_modules
            .get(&target.namespace())
            .map(|(module, _)| *module)
            .ok_or(EffectSupervisorError::Runtime(Error::Registry(
                "namespace unavailable",
            )))?;
        let runner =
            self.effect_runners
                .get(module)
                .copied()
                .ok_or(EffectSupervisorError::Runtime(Error::Registry(
                    "effect runner unavailable",
                )))?;
        runner(client, target, peer, lease_ms).await
    }

    /// Reports whether one namespace's module registered effect supervision.
    #[must_use]
    pub fn has_effect_runner(&self, namespace: NamespaceId) -> bool {
        self.namespace_modules
            .get(&namespace)
            .is_some_and(|(module, _)| self.effect_runners.contains_key(module))
    }
}
