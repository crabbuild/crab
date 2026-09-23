use crab_cell_app::ApplicationHandle;
use crab_cell_runtime::identity::{ApplicationId, CellTarget, TenantId, partition_for_shard};
use crab_cell_runtime::primitives::workflow::{
    ActivityCompletion, ActivityCompletionOutcome, WorkflowActivityClaimCommand,
    WorkflowActivityClaimRequest, WorkflowActivityCompleteCommand, WorkflowActivityValidateQuery,
    WorkflowActivityValidateRequest, WorkflowOutcome, WorkflowStatus,
};
use crab_cell_runtime::{Error, Result};

use crate::{
    fixture,
    qualification::{fixed_id, identity},
};

pub async fn run(
    handle: &ApplicationHandle<fixture::ReferenceApplication>,
    observer: &ApplicationHandle<fixture::ReferenceApplication>,
    tenant: TenantId,
    application: ApplicationId,
    operation_id: u64,
    nonce: u64,
    now_ms: i64,
) -> Result<()> {
    let workflow = handle.workflow::<fixture::ReferenceWorkflow>()?;
    let workflow_id = format!("public-activity-duplicate-{operation_id}-{nonce}").into_bytes();
    let mutation_index = operation_id.saturating_mul(100);
    let started = workflow
        .start(
            identity(mutation_index, now_ms),
            workflow_id.clone(),
            b"activity".to_vec(),
        )
        .await
        .map_err(|source| Error::Facility {
            name: "public qualification Activity start",
            source: Box::new(source),
        })?;
    let WorkflowOutcome::Applied {
        run_id,
        status: WorkflowStatus::Running,
        event_sequence: 1,
    } = started.output
    else {
        return Err(Error::Control(
            "public qualification Activity not scheduled",
        ));
    };
    let target = CellTarget::new(
        tenant,
        application,
        fixture::WORKFLOW_NAMESPACE,
        &partition_for_shard(0),
    )?;
    let claimed = handle
        .command::<WorkflowActivityClaimCommand<fixture::ReferenceWorkflow>>(
            &target,
            identity(mutation_index.saturating_add(1), now_ms),
            WorkflowActivityClaimRequest {
                limit: 1,
                lease_ms: 60_000,
            },
        )
        .await
        .map_err(|source| Error::Facility {
            name: "public qualification Activity claim",
            source: Box::new(source),
        })?;
    let [claim] = claimed.output.as_slice() else {
        return Err(Error::Control(
            "public qualification Activity claim differs",
        ));
    };
    if claim.run_id != run_id || claim.attempt != 1 || claim.input != b"activity-result" {
        return Err(Error::Control(
            "public qualification Activity lease differs",
        ));
    }
    let completion = ActivityCompletion {
        run_id,
        activity_id: claim.activity_id,
        attempt: claim.attempt,
        lease_token: claim.token,
        completion_token: fixed_id(operation_id),
        result: claim.input.clone(),
        failed: false,
        retryable: false,
    };
    let complete_identity = identity(mutation_index.saturating_add(2), now_ms);
    let completed = handle
        .command::<WorkflowActivityCompleteCommand<fixture::ReferenceWorkflow>>(
            &target,
            complete_identity,
            completion.clone(),
        )
        .await
        .map_err(|source| Error::Facility {
            name: "public qualification Activity completion",
            source: Box::new(source),
        })?;
    if !matches!(
        completed.output,
        ActivityCompletionOutcome::Applied(WorkflowOutcome::Applied {
            run_id: completed_run,
            status: WorkflowStatus::Completed,
            event_sequence: 2,
        }) if completed_run == run_id
    ) {
        return Err(Error::Control(
            "public qualification Activity not completed",
        ));
    }
    let duplicate = handle
        .command::<WorkflowActivityCompleteCommand<fixture::ReferenceWorkflow>>(
            &target,
            identity(mutation_index.saturating_add(3), now_ms),
            completion,
        )
        .await
        .map_err(|source| Error::Facility {
            name: "public qualification Activity duplicate",
            source: Box::new(source),
        })?;
    if duplicate.output
        != (ActivityCompletionOutcome::Duplicate {
            result: claim.input.clone(),
        })
    {
        return Err(Error::Control(
            "public qualification Activity duplicate differs",
        ));
    }
    let lease = observer
        .query::<WorkflowActivityValidateQuery<fixture::ReferenceWorkflow>>(
            &target,
            Some(duplicate.receipt),
            WorkflowActivityValidateRequest {
                claimed: claimed.output.clone(),
            },
        )
        .await
        .map_err(|source| Error::Facility {
            name: "public qualification Activity validation",
            source: Box::new(source),
        })?;
    if lease.output {
        return Err(Error::Control(
            "public qualification Activity lease remains",
        ));
    }
    let observed = observer
        .workflow::<fixture::ReferenceWorkflow>()?
        .state(workflow_id.clone(), Some(duplicate.receipt))
        .await
        .map_err(|source| Error::Facility {
            name: "public qualification Activity verification",
            source: Box::new(source),
        })?;
    let mut expected_result = b"activity\0\0".to_vec();
    expected_result.extend_from_slice(&claim.activity_id);
    expected_result.extend_from_slice(&(claim.input.len() as u32).to_be_bytes());
    expected_result.extend_from_slice(&claim.input);
    if !matches!(observed.output, Some(ref run)
        if run.run_id == run_id
            && run.workflow_id == workflow_id
            && run.status == WorkflowStatus::Completed
            && run.event_sequence == 2
            && run.result.as_deref() == Some(expected_result.as_slice()))
    {
        return Err(Error::Control(
            "public qualification Activity state differs",
        ));
    }
    Ok(())
}
