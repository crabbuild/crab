use std::sync::atomic::Ordering;

use crab_cell_runtime::cell::executor::MutationIdentity;
use crab_cell_runtime::cell::executor::Resolution;
use crab_cell_runtime::identity::{CellTarget, partition_for_shard};
use crab_cell_runtime::primitives::blob::{
    BlobCommand, BlobCondition, BlobMutation, BlobMutationOutcome, BlobQuery, BlobQueryResult,
};
use crab_cell_runtime::primitives::effects::{
    EffectAckRequest, EffectClaimRequest, EffectLease, EffectLeaseCommand, EffectLeaseOutcome,
    EffectLeaseRequest, EffectState, EffectStatus, effect_id,
};
use crab_cell_runtime::primitives::queue::{
    QueueClaimRequest, QueueLeaseOutcome, QueueSendCommand, QueueSendOutcome, QueueSendRequest,
    QueueState,
};
use crab_cell_runtime::primitives::workflow::{
    ActivityCompletion, ActivityCompletionOutcome, WorkflowActivityClaimCommand,
    WorkflowActivityClaimRequest, WorkflowActivityCompleteCommand, WorkflowActivityValidateQuery,
    WorkflowActivityValidateRequest, WorkflowOutcome, WorkflowStatus,
};
use crab_cell_runtime::{Error, Result};

use crate::{
    fixture,
    qualification::{fixed_id, identity},
    qualification_cancellation::{cancel_prepared, committed_output, now_ms},
    qualification_scheduled_cancellation::CancellationCase,
};

impl CancellationCase<'_> {
    pub async fn blob(
        self,
        operation_id: u64,
        nonce: u64,
        mutation: MutationIdentity,
    ) -> Result<()> {
        let target = CellTarget::new(
            self.tenant,
            self.application,
            fixture::BLOB_NAMESPACE,
            &partition_for_shard(0),
        )?;
        let blob = self.observer.blob::<fixture::ReferenceBlob>()?;
        let key = format!("public-cancel-blob-{operation_id}-{nonce}").into_bytes();
        let upload_id = fixed_id(operation_id);
        let payload = nonce.to_be_bytes().to_vec();
        let mutation_index = operation_id.saturating_mul(100);
        blob.mutate(
            identity(mutation_index.saturating_add(1), mutation.issued_at_ms),
            BlobMutation::Begin {
                key: key.clone(),
                upload_id,
                condition: BlobCondition::Missing,
                content_type: None,
                metadata: Vec::new(),
                expires_at_ms: mutation.issued_at_ms.saturating_add(120_000),
            },
        )
        .await
        .map_err(|source| Error::Facility {
            name: "public qualification Blob cancellation begin",
            source: Box::new(source),
        })?;
        blob.mutate(
            identity(mutation_index.saturating_add(2), mutation.issued_at_ms),
            BlobMutation::PutPart {
                key: key.clone(),
                upload_id,
                part_number: 1,
                payload: payload.clone(),
            },
        )
        .await
        .map_err(|source| Error::Facility {
            name: "public qualification Blob cancellation part",
            source: Box::new(source),
        })?;
        let prepared = self
            .peer
            .prepare_command::<BlobCommand<fixture::ReferenceBlob>>(
                &target,
                mutation,
                BlobMutation::Complete {
                    key: key.clone(),
                    upload_id,
                    part_count: 1,
                },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Blob cancellation prepare",
                source: Box::new(source),
            })?;
        let (_retained_attempt, resolution) =
            cancel_prepared(prepared, self.observer, self.entered, self.dispatched, true).await;
        let Resolution::Committed(outcome) = resolution else {
            return Err(Error::Control(
                "public qualification Blob cancelled acknowledgement unresolved",
            ));
        };
        let (output, commit_sequence) = committed_output::<BlobMutationOutcome>(outcome);
        let BlobMutationOutcome::Committed { etag, size } = output else {
            return Err(Error::Control(
                "public qualification Blob cancelled completion differs",
            ));
        };
        let observed = blob
            .query(
                BlobQuery::Read {
                    key,
                    offset: 0,
                    limit: 128,
                },
                None,
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Blob cancellation observation",
                source: Box::new(source),
            })?;
        if self.dispatched.load(Ordering::Acquire) != 1
            || size != payload.len() as u64
            || !matches!(observed.output, BlobQueryResult::Read(Some(ref read))
                if read.bytes == payload
                    && read.metadata.size == size
                    && read.metadata.etag == etag)
            || observed.receipt.commit_sequence < commit_sequence
        {
            return Err(Error::Control(
                "public qualification Blob cancelled value differs",
            ));
        }
        Ok(())
    }

    pub async fn queue(
        self,
        operation_id: u64,
        nonce: u64,
        mutation: MutationIdentity,
    ) -> Result<()> {
        let target = CellTarget::new(
            self.tenant,
            self.application,
            fixture::QUEUE_NAMESPACE,
            &partition_for_shard(0),
        )?;
        let queue = self.observer.queue::<fixture::ReferenceQueue>()?;
        let baseline = queue
            .info(0, None)
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Queue cancellation baseline",
                source: Box::new(source),
            })?;
        if baseline.output.ready != 0 || baseline.output.leased != 0 {
            return Err(Error::Control(
                "public qualification Queue baseline has pending work",
            ));
        }
        let payload = nonce.to_be_bytes().to_vec();
        let prepared = self
            .peer
            .prepare_command::<QueueSendCommand<fixture::ReferenceQueue>>(
                &target,
                mutation,
                QueueSendRequest {
                    producer_id: fixed_id(operation_id),
                    payload: payload.clone(),
                    available_at_ms: mutation.issued_at_ms,
                },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Queue cancellation prepare",
                source: Box::new(source),
            })?;
        let (_retained_attempt, resolution) =
            cancel_prepared(prepared, self.observer, self.entered, self.dispatched, true).await;
        let Resolution::Committed(outcome) = resolution else {
            return Err(Error::Control(
                "public qualification Queue cancelled acknowledgement unresolved",
            ));
        };
        let (output, commit_sequence) = committed_output::<QueueSendOutcome>(outcome);
        let QueueSendOutcome::Sent { message_id } = output else {
            return Err(Error::Control(
                "public qualification Queue cancelled send differs",
            ));
        };
        let mutation_index = operation_id.saturating_mul(100);
        let claimed = queue
            .claim(
                identity(mutation_index.saturating_add(1), now_ms()),
                0,
                QueueClaimRequest {
                    limit: 2,
                    lease_ms: 60_000,
                },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Queue cancellation claim",
                source: Box::new(source),
            })?;
        let [message] = claimed.output.as_slice() else {
            return Err(Error::Control(
                "public qualification Queue cancelled claim differs",
            ));
        };
        if self.dispatched.load(Ordering::Acquire) != 1
            || message.payload != payload
            || message.attempt != 1
            || message.message_id != message_id
            || claimed.receipt.commit_sequence <= commit_sequence
        {
            return Err(Error::Control(
                "public qualification Queue cancelled message differs",
            ));
        }
        let acked = queue
            .ack(
                identity(mutation_index.saturating_add(2), now_ms()),
                0,
                message.message_id,
                message.token,
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Queue cancellation settlement",
                source: Box::new(source),
            })?;
        let info = queue
            .info(0, Some(acked.receipt))
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Queue cancellation observation",
                source: Box::new(source),
            })?;
        if !matches!(
            acked.output,
            QueueLeaseOutcome::Applied {
                state: QueueState::Acked,
                ..
            }
        ) || info.output.acked != baseline.output.acked.saturating_add(1)
            || info.output.ready != 0
            || info.output.leased != 0
        {
            return Err(Error::Control(
                "public qualification Queue cancelled settlement differs",
            ));
        }
        Ok(())
    }

    pub async fn activity(
        self,
        operation_id: u64,
        nonce: u64,
        mutation: MutationIdentity,
    ) -> Result<()> {
        let target = CellTarget::new(
            self.tenant,
            self.application,
            fixture::WORKFLOW_NAMESPACE,
            &partition_for_shard(0),
        )?;
        let workflow = self.observer.workflow::<fixture::ReferenceWorkflow>()?;
        let workflow_id =
            format!("public-activity-cancellation-{operation_id}-{nonce}").into_bytes();
        let mutation_index = operation_id.saturating_mul(100);
        let started = workflow
            .start(
                identity(mutation_index.saturating_add(1), mutation.issued_at_ms),
                workflow_id.clone(),
                b"activity".to_vec(),
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Activity cancellation start",
                source: Box::new(source),
            })?;
        let WorkflowOutcome::Applied {
            run_id,
            status: WorkflowStatus::Running,
            event_sequence: 1,
        } = started.output
        else {
            return Err(Error::Control(
                "public qualification Activity cancellation not scheduled",
            ));
        };
        let claimed = self
            .observer
            .command::<WorkflowActivityClaimCommand<fixture::ReferenceWorkflow>>(
                &target,
                identity(mutation_index.saturating_add(2), now_ms()),
                WorkflowActivityClaimRequest {
                    limit: 1,
                    lease_ms: 60_000,
                },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Activity cancellation claim",
                source: Box::new(source),
            })?;
        let [claim] = claimed.output.as_slice() else {
            return Err(Error::Control(
                "public qualification Activity cancellation claim differs",
            ));
        };
        if claim.run_id != run_id || claim.attempt != 1 || claim.input != b"activity-result" {
            return Err(Error::Control(
                "public qualification Activity cancellation lease differs",
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
        let prepared = self
            .peer
            .prepare_command::<WorkflowActivityCompleteCommand<fixture::ReferenceWorkflow>>(
                &target,
                mutation,
                completion.clone(),
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Activity cancellation prepare",
                source: Box::new(source),
            })?;
        let (_retained_attempt, resolution) =
            cancel_prepared(prepared, self.observer, self.entered, self.dispatched, true).await;
        let Resolution::Committed(outcome) = resolution else {
            return Err(Error::Control(
                "public qualification Activity cancelled acknowledgement unresolved",
            ));
        };
        let (output, commit_sequence) = committed_output::<ActivityCompletionOutcome>(outcome);
        if !matches!(output, ActivityCompletionOutcome::Applied(WorkflowOutcome::Applied {
            run_id: completed_run,
            status: WorkflowStatus::Completed,
            event_sequence: 2,
        }) if completed_run == run_id)
        {
            return Err(Error::Control(
                "public qualification Activity cancelled completion differs",
            ));
        }
        let mut expected_result = b"activity\0\0".to_vec();
        expected_result.extend_from_slice(&claim.activity_id);
        expected_result.extend_from_slice(&(claim.input.len() as u32).to_be_bytes());
        expected_result.extend_from_slice(&claim.input);
        let observed = workflow
            .state(workflow_id.clone(), None)
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Activity cancellation observation",
                source: Box::new(source),
            })?;
        let lease = self
            .observer
            .query::<WorkflowActivityValidateQuery<fixture::ReferenceWorkflow>>(
                &target,
                None,
                WorkflowActivityValidateRequest {
                    claimed: claimed.output.clone(),
                },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Activity cancellation lease observation",
                source: Box::new(source),
            })?;
        if self.dispatched.load(Ordering::Acquire) != 1
            || !matches!(observed.output, Some(ref run)
                if run.run_id == run_id
                    && run.workflow_id == workflow_id
                    && run.status == WorkflowStatus::Completed
                    && run.event_sequence == 2
                    && run.result.as_deref() == Some(expected_result.as_slice()))
            || observed.receipt.commit_sequence < commit_sequence
            || lease.output
        {
            return Err(Error::Control(
                "public qualification Activity cancelled state differs",
            ));
        }
        let duplicate = self
            .observer
            .command::<WorkflowActivityCompleteCommand<fixture::ReferenceWorkflow>>(
                &target,
                identity(mutation_index.saturating_add(3), now_ms()),
                completion,
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Activity cancellation replay",
                source: Box::new(source),
            })?;
        let final_run = workflow
            .state(workflow_id, Some(duplicate.receipt))
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Activity cancellation final observation",
                source: Box::new(source),
            })?;
        if duplicate.output
            != (ActivityCompletionOutcome::Duplicate {
                result: claim.input.clone(),
            })
            || !matches!(final_run.output, Some(ref run) if run.event_sequence == 2)
        {
            return Err(Error::Control(
                "public qualification Activity cancelled replay differs",
            ));
        }
        Ok(())
    }

    pub async fn effects(
        self,
        operation_id: u64,
        nonce: u64,
        mutation: MutationIdentity,
    ) -> Result<()> {
        let target = CellTarget::new(
            self.tenant,
            self.application,
            fixture::WORKFLOW_NAMESPACE,
            &partition_for_shard(0),
        )?;
        let effects = self
            .observer
            .effects::<fixture::ReferenceWorkflow>(target.clone())?;
        let workflow = self.observer.workflow::<fixture::ReferenceWorkflow>()?;
        let workflow_id = format!("public-effect-cancellation-{operation_id}-{nonce}").into_bytes();
        let mutation_index = operation_id.saturating_mul(100);
        let started = workflow
            .start(
                identity(mutation_index.saturating_add(1), mutation.issued_at_ms),
                workflow_id,
                b"effect".to_vec(),
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Effect cancellation start",
                source: Box::new(source),
            })?;
        if !matches!(
            started.output,
            WorkflowOutcome::Applied {
                status: WorkflowStatus::Completed,
                event_sequence: 1,
                ..
            }
        ) {
            return Err(Error::Control(
                "public qualification Effect cancellation not scheduled",
            ));
        }
        let claimed = effects
            .claim(
                identity(mutation_index.saturating_add(2), now_ms()),
                EffectClaimRequest {
                    limit: 1,
                    lease_ms: 60_000,
                },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Effect cancellation claim",
                source: Box::new(source),
            })?;
        let [claim] = claimed.output.as_slice() else {
            return Err(Error::Control(
                "public qualification Effect cancellation claim differs",
            ));
        };
        if claim.attempt != 1
            || claim.created_sequence != started.receipt.commit_sequence
            || claim.effect_id
                != effect_id(
                    started.receipt.cell,
                    started.receipt.incarnation,
                    started.receipt.commit_sequence,
                    0,
                )
        {
            return Err(Error::Control(
                "public qualification Effect cancellation source differs",
            ));
        }
        let claim = claim.clone();
        let result = nonce.to_be_bytes().to_vec();
        let prepared = self
            .peer
            .prepare_command::<EffectLeaseCommand<fixture::ReferenceWorkflow>>(
                &target,
                mutation,
                EffectLeaseRequest::Ack(EffectAckRequest {
                    lease: EffectLease::from(&claim),
                    result: result.clone(),
                }),
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Effect cancellation prepare",
                source: Box::new(source),
            })?;
        let (_retained_attempt, resolution) =
            cancel_prepared(prepared, self.observer, self.entered, self.dispatched, true).await;
        let Resolution::Committed(outcome) = resolution else {
            return Err(Error::Control(
                "public qualification Effect cancelled acknowledgement unresolved",
            ));
        };
        let (output, commit_sequence) = committed_output::<EffectLeaseOutcome>(outcome);
        let status = effects
            .status(claim.effect_id, None)
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Effect cancellation status",
                source: Box::new(source),
            })?;
        let lease = effects
            .validate(vec![claim.clone()], claimed.receipt)
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Effect cancellation lease observation",
                source: Box::new(source),
            })?;
        let remaining = effects
            .claim(
                identity(mutation_index.saturating_add(3), now_ms()),
                EffectClaimRequest {
                    limit: 1,
                    lease_ms: 5_000,
                },
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Effect cancellation queue observation",
                source: Box::new(source),
            })?;
        if self.dispatched.load(Ordering::Acquire) != 1
            || output != EffectLeaseOutcome::Delivered
            || status.output
                != Some(EffectStatus {
                    state: EffectState::Delivered,
                    attempt: 1,
                    token_present: false,
                    lease_until_ms: None,
                    expires_at_ms: claim.expires_at_ms,
                    result: Some(result),
                })
            || status.receipt.commit_sequence < commit_sequence
            || lease.output
            || !remaining.output.is_empty()
        {
            return Err(Error::Control(
                "public qualification Effect cancelled status differs",
            ));
        }
        Ok(())
    }
}
