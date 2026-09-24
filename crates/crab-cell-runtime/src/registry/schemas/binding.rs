//! Typed command, query, activity, and maintenance bindings.

use super::*;

pub(in crate::registry) fn typed_command<C: Command>(
    context: &mut CommandContext<'_, '_>,
    input: &[u8],
) -> Result<HandlerOutcome> {
    let input = decode_wire::<C::Input>(input, context.input_limit)?;
    let outcome = C::execute(context, input)?;
    Ok(match outcome {
        CommandResult::Success(output) => {
            HandlerOutcome::Success(encode_wire(&output, context.output_limit)?)
        }
        CommandResult::Rejected(output) => {
            HandlerOutcome::Rejected(encode_wire(&output, context.output_limit)?)
        }
    })
}

pub(in crate::registry) fn typed_query<Q: Query>(
    context: &mut QueryContext<'_>,
    input: &[u8],
) -> Result<Vec<u8>> {
    let input = decode_wire::<Q::Input>(input, context.input_limit)?;
    let output = Q::execute(context, input)?;
    Ok(encode_wire(&output, context.output_limit)?)
}

pub(in crate::registry) fn typed_activity<A: ActivityHandler>(
    context: ActivityContext,
    input: Vec<u8>,
) -> ActivityFuture {
    Box::pin(async move {
        let outcome = A::execute(context, input).await;
        let payload = match &outcome {
            ActivityExecution::Completed(result) => result,
            ActivityExecution::Failed { details, .. } => details,
        };
        if payload.len() > crate::primitives::workflow::MAX_ACTIVITY_PAYLOAD_BYTES {
            return Err(Error::Command("activity handler result exceeds 256 KiB"));
        }
        Ok(outcome)
    })
}

pub(in crate::registry) fn typed_blocking_activity<A: BlockingActivityHandler>(
    context: ActivityContext,
    input: Vec<u8>,
) -> Result<ActivityExecution> {
    let outcome = A::execute(context, input);
    let payload = match &outcome {
        ActivityExecution::Completed(result) => result,
        ActivityExecution::Failed { details, .. } => details,
    };
    if payload.len() > crate::primitives::workflow::MAX_ACTIVITY_PAYLOAD_BYTES {
        return Err(Error::Command("activity handler result exceeds 256 KiB"));
    }
    Ok(outcome)
}

pub(in crate::registry) fn typed_maintenance<M: MaintenanceModule>(
    client: CellClient,
    target: CellTarget,
    identity: MutationIdentity,
    request: MaintenanceTickRequest,
) -> MaintenanceFuture {
    Box::pin(async move {
        client
            .command::<MaintenanceTickCommand<M>>(&target, identity, request)
            .await
    })
}

pub(in crate::registry) fn typed_effect<M: EffectModule>(
    client: CellClient,
    target: CellTarget,
    peer: EffectPeerClient,
    lease_ms: u32,
) -> EffectFuture {
    Box::pin(async move {
        EffectSupervisor::<M>::new(crate::EffectSource::new(client, target), peer, lease_ms)
            .map_err(EffectSupervisorError::Runtime)?
            .run_once()
            .await
    })
}

pub(in crate::registry) fn typed_activity_runner<M: WorkflowActivityModule>(
    client: CellClient,
    tenant: TenantId,
    application: ApplicationId,
    shard: u32,
    lease_ms: u32,
    blocking: Option<BlockingActivityReservation>,
) -> ActivityRunFuture {
    Box::pin(async move {
        ActivitySupervisor::new(
            WorkflowActivities::<M>::new(client, tenant, application)
                .map_err(ActivitySupervisorError::Runtime)?,
            lease_ms,
        )
        .map_err(ActivitySupervisorError::Runtime)?
        .run_once(shard, blocking)
        .await
    })
}
