use crab_cell_runtime::{
    ActivityCompletion, ActivityCompletionOutcome, BlobCommand, BlobCondition, BlobMutation,
    BlobMutationOutcome, BlobQuery, BlobQueryResult, CellTarget, EffectAckRequest,
    EffectClaimRequest, EffectLease, EffectLeaseCommand, EffectLeaseOutcome, EffectLeaseRequest,
    EffectState, EffectStatus, Error, MutationIdentity, QueueClaimRequest, QueueLeaseOutcome,
    QueueSendCommand, QueueSendOutcome, QueueSendRequest, QueueState, Result,
    WorkflowActivityClaimCommand, WorkflowActivityClaimRequest, WorkflowActivityCompleteCommand,
    WorkflowActivityValidateQuery, WorkflowActivityValidateRequest, WorkflowOutcome,
    WorkflowStatus, effect_id, partition_for_shard,
};

use crate::{
    fixture,
    qualification::{fixed_id, identity},
    qualification_cancellation::now_ms,
    qualification_scheduled_retry::RetryCase,
};

impl RetryCase<'_> {
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
        let key = format!("public-retry-blob-{operation_id}-{nonce}").into_bytes();
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
            name: "public qualification Blob retry begin",
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
            name: "public qualification Blob retry part",
            source: Box::new(source),
        })?;
        let retried = self
            .command::<BlobCommand<fixture::ReferenceBlob>>(
                &target,
                mutation,
                BlobMutation::Complete {
                    key: key.clone(),
                    upload_id,
                    part_count: 1,
                },
            )
            .await?;
        let BlobMutationOutcome::Committed { etag, size } = retried.output else {
            return Err(Error::Control(
                "public qualification Blob retry not committed",
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
                name: "public qualification Blob retry observation",
                source: Box::new(source),
            })?;
        if size != payload.len() as u64
            || !matches!(observed.output, BlobQueryResult::Read(Some(ref read))
                if read.bytes == payload
                    && read.metadata.size == size
                    && read.metadata.etag == etag)
            || observed.receipt.commit_sequence < retried.receipt.commit_sequence
        {
            return Err(Error::Control(
                "public qualification Blob retry state differs",
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
                name: "public qualification Queue retry baseline",
                source: Box::new(source),
            })?;
        if baseline.output.ready != 0 || baseline.output.leased != 0 {
            return Err(Error::Control(
                "public qualification Queue retry baseline has pending work",
            ));
        }
        let payload = nonce.to_be_bytes().to_vec();
        let retried = self
            .command::<QueueSendCommand<fixture::ReferenceQueue>>(
                &target,
                mutation,
                QueueSendRequest {
                    producer_id: fixed_id(operation_id),
                    payload: payload.clone(),
                    available_at_ms: mutation.issued_at_ms,
                },
            )
            .await?;
        let QueueSendOutcome::Sent { message_id } = retried.output else {
            return Err(Error::Control("public qualification Queue retry not sent"));
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
                name: "public qualification Queue retry claim",
                source: Box::new(source),
            })?;
        let [message] = claimed.output.as_slice() else {
            return Err(Error::Control(
                "public qualification Queue retry claim differs",
            ));
        };
        if message.message_id != message_id
            || message.payload != payload
            || message.attempt != 1
            || claimed.receipt.commit_sequence <= retried.receipt.commit_sequence
        {
            return Err(Error::Control(
                "public qualification Queue retry message differs",
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
                name: "public qualification Queue retry settlement",
                source: Box::new(source),
            })?;
        let info = queue
            .info(0, Some(acked.receipt))
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Queue retry observation",
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
                "public qualification Queue retry settlement differs",
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
        let workflow_id = format!("public-activity-retry-{operation_id}-{nonce}").into_bytes();
        let mutation_index = operation_id.saturating_mul(100);
        let started = workflow
            .start(
                identity(mutation_index.saturating_add(1), mutation.issued_at_ms),
                workflow_id.clone(),
                b"activity".to_vec(),
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Activity retry start",
                source: Box::new(source),
            })?;
        let WorkflowOutcome::Applied {
            run_id,
            status: WorkflowStatus::Running,
            event_sequence: 1,
        } = started.output
        else {
            return Err(Error::Control(
                "public qualification Activity retry not scheduled",
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
                name: "public qualification Activity retry claim",
                source: Box::new(source),
            })?;
        let [claim] = claimed.output.as_slice() else {
            return Err(Error::Control(
                "public qualification Activity retry claim differs",
            ));
        };
        if claim.run_id != run_id || claim.attempt != 1 || claim.input != b"activity-result" {
            return Err(Error::Control(
                "public qualification Activity retry lease differs",
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
        let retried = self
            .command::<WorkflowActivityCompleteCommand<fixture::ReferenceWorkflow>>(
                &target, mutation, completion,
            )
            .await?;
        if !matches!(retried.output, ActivityCompletionOutcome::Applied(WorkflowOutcome::Applied {
            run_id: completed_run, status: WorkflowStatus::Completed, event_sequence: 2,
        }) if completed_run == run_id)
        {
            return Err(Error::Control(
                "public qualification Activity retry completion differs",
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
                name: "public qualification Activity retry observation",
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
                name: "public qualification Activity retry lease observation",
                source: Box::new(source),
            })?;
        if !matches!(observed.output, Some(ref run)
            if run.run_id == run_id
                && run.workflow_id == workflow_id
                && run.status == WorkflowStatus::Completed
                && run.event_sequence == 2
                && run.result.as_deref() == Some(expected_result.as_slice()))
            || observed.receipt.commit_sequence < retried.receipt.commit_sequence
            || lease.output
        {
            return Err(Error::Control(
                "public qualification Activity retry state differs",
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
        let workflow_id = format!("public-effect-retry-{operation_id}-{nonce}").into_bytes();
        let mutation_index = operation_id.saturating_mul(100);
        let started = workflow
            .start(
                identity(mutation_index.saturating_add(1), mutation.issued_at_ms),
                workflow_id,
                b"effect".to_vec(),
            )
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Effect retry start",
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
                "public qualification Effect retry not scheduled",
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
                name: "public qualification Effect retry claim",
                source: Box::new(source),
            })?;
        let [claim] = claimed.output.as_slice() else {
            return Err(Error::Control(
                "public qualification Effect retry claim differs",
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
                "public qualification Effect retry source differs",
            ));
        }
        let result = nonce.to_be_bytes().to_vec();
        let retried = self
            .command::<EffectLeaseCommand<fixture::ReferenceWorkflow>>(
                &target,
                mutation,
                EffectLeaseRequest::Ack(EffectAckRequest {
                    lease: EffectLease::from(claim),
                    result: result.clone(),
                }),
            )
            .await?;
        let status = effects
            .status(claim.effect_id, None)
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Effect retry status",
                source: Box::new(source),
            })?;
        let lease = effects
            .validate(vec![claim.clone()], claimed.receipt)
            .await
            .map_err(|source| Error::Facility {
                name: "public qualification Effect retry lease observation",
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
                name: "public qualification Effect retry queue observation",
                source: Box::new(source),
            })?;
        if retried.output != EffectLeaseOutcome::Delivered
            || status.output
                != Some(EffectStatus {
                    state: EffectState::Delivered,
                    attempt: 1,
                    token_present: false,
                    lease_until_ms: None,
                    expires_at_ms: claim.expires_at_ms,
                    result: Some(result),
                })
            || status.receipt.commit_sequence < retried.receipt.commit_sequence
            || lease.output
            || !remaining.output.is_empty()
        {
            return Err(Error::Control(
                "public qualification Effect retry state differs",
            ));
        }
        Ok(())
    }
}
