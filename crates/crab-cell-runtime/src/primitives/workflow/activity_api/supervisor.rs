//! Activity claim/execute/completion supervision.

use super::*;

/// Outcome of one bounded claim, execute and completion supervisor cycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivityRunOutcome {
    Idle {
        receipt: Receipt,
    },
    LeaseLost {
        receipt: Receipt,
    },
    IdentityConflict {
        receipt: Receipt,
    },
    Retrying {
        due_at_ms: i64,
        receipt: Receipt,
    },
    Completed {
        workflow: WorkflowOutcome,
        receipt: Receipt,
    },
    Duplicate {
        result: Vec<u8>,
        receipt: Receipt,
    },
}

/// Failure that preserves unresolved mutation evidence from supervisor commands.
pub enum ActivitySupervisorError {
    Pending(Box<PendingMutation>),
    InvalidPublishedResult {
        receipt: Receipt,
        source: Box<Error>,
    },
    Runtime(Error),
}

impl fmt::Debug for ActivitySupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending(pending) => formatter.debug_tuple("Pending").field(pending).finish(),
            Self::InvalidPublishedResult { receipt, source } => formatter
                .debug_struct("InvalidPublishedResult")
                .field("receipt", receipt)
                .field("source", source)
                .finish(),
            Self::Runtime(error) => formatter.debug_tuple("Runtime").field(error).finish(),
        }
    }
}

impl fmt::Display for ActivitySupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending(_) => {
                formatter.write_str("activity supervisor mutation needs resolution")
            }
            Self::InvalidPublishedResult { .. } => {
                formatter.write_str("activity supervisor received an invalid published result")
            }
            Self::Runtime(error) => write!(formatter, "activity supervisor failed: {error}"),
        }
    }
}

impl std::error::Error for ActivitySupervisorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidPublishedResult { source, .. } => Some(source.as_ref()),
            Self::Runtime(error) => Some(error),
            Self::Pending(_) => None,
        }
    }
}

/// Runs native activity attempts without holding a SQLite transaction across await.
pub struct ActivitySupervisor<M> {
    activities: WorkflowActivities<M>,
    lease_ms: u32,
}

impl<M: WorkflowActivityModule> ActivitySupervisor<M> {
    /// Creates a supervisor using a 5..=300 second heartbeat lease.
    pub fn new(activities: WorkflowActivities<M>, lease_ms: u32) -> crate::Result<Self> {
        if !(5_000..=300_000).contains(&lease_ms) {
            return Err(Error::Command(
                "activity supervisor lease must be in 5..=300 seconds",
            ));
        }
        Ok(Self {
            activities,
            lease_ms,
        })
    }

    /// Claims and executes at most one activity from an explicit Workflow shard.
    pub async fn run_once(
        &self,
        shard: u32,
        blocking: Option<BlockingActivityReservation>,
    ) -> std::result::Result<ActivityRunOutcome, ActivitySupervisorError> {
        let claimed = self
            .activities
            .claim(internal_identity()?, shard, self.lease_ms)
            .await
            .map_err(unexpected_invocation)?;
        let Some(mut claim) = claimed.output.into_iter().next() else {
            return Ok(ActivityRunOutcome::Idle {
                receipt: claimed.receipt,
            });
        };
        let validation = self
            .activities
            .validate(shard, claim.clone(), claimed.receipt)
            .await
            .map_err(unexpected_invocation)?;
        if !validation.output {
            return Ok(ActivityRunOutcome::LeaseLost {
                receipt: validation.receipt,
            });
        }

        let context = ActivityContext::new(&claim);
        let cancellation = context.cancellation();
        let _cancellation_guard = CancellationGuard(cancellation.clone());
        let lease_deadline = context.lease_until_ms.clone();
        let execution = self.activities.execute(
            claim.definition_digest,
            claim.activity_type.clone(),
            claim.input.clone(),
            context,
            blocking,
        );
        tokio::pin!(execution);
        let heartbeat_period = Duration::from_millis(u64::from(self.lease_ms / 3));
        let heartbeat = tokio::time::sleep(heartbeat_period);
        tokio::pin!(heartbeat);
        let execution = loop {
            tokio::select! {
                result = &mut execution => break result.map_err(ActivitySupervisorError::Runtime)?,
                () = &mut heartbeat => {
                    let extension = self.activities
                        .extend(internal_identity()?, shard, claim.clone(), self.lease_ms)
                        .await;
                    match extension {
                        Ok(committed) => match committed.output {
                            ActivityLeaseOutcome::Extended { lease_until_ms } => {
                                claim.lease_until_ms = lease_until_ms;
                                lease_deadline.store(lease_until_ms, Ordering::Release);
                            }
                            ActivityLeaseOutcome::LeaseLost => {
                                cancellation.cancel();
                                return Ok(ActivityRunOutcome::LeaseLost { receipt: committed.receipt });
                            }
                        },
                        Err(InvocationError::Rejected(committed))
                            if committed.output == ActivityLeaseOutcome::LeaseLost =>
                        {
                            cancellation.cancel();
                            return Ok(ActivityRunOutcome::LeaseLost { receipt: committed.receipt });
                        }
                        Err(error) => {
                            cancellation.cancel();
                            return Err(unexpected_invocation(error));
                        }
                    }
                    heartbeat.as_mut().reset(tokio::time::Instant::now() + heartbeat_period);
                }
            }
        };

        let (result, failed, retryable) = execution.payload();
        if result.len() > MAX_ACTIVITY_PAYLOAD_BYTES {
            return Err(ActivitySupervisorError::Runtime(Error::Command(
                "activity handler result exceeds 256 KiB",
            )));
        }

        if self.lease_ms < MAX_LEASE_MS {
            // Provider-backed completion can take longer than the handler. Reserve a fresh
            // bounded lease before submitting the terminal mutation, or the result can be
            // rejected as expired while the owner is still durably completing it.
            let extended = self
                .activities
                .extend(internal_identity()?, shard, claim.clone(), MAX_LEASE_MS)
                .await;
            let extended = match extended {
                Ok(committed) => committed,
                Err(InvocationError::Rejected(committed))
                    if committed.output == ActivityLeaseOutcome::LeaseLost =>
                {
                    return Ok(ActivityRunOutcome::LeaseLost {
                        receipt: committed.receipt,
                    });
                }
                Err(error) => return Err(unexpected_invocation(error)),
            };
            match extended.output {
                ActivityLeaseOutcome::Extended { lease_until_ms } => {
                    claim.lease_until_ms = lease_until_ms;
                    lease_deadline.store(lease_until_ms, Ordering::Release);
                }
                ActivityLeaseOutcome::LeaseLost => {
                    return Ok(ActivityRunOutcome::LeaseLost {
                        receipt: extended.receipt,
                    });
                }
            }
        }

        let completion = ActivityCompletion {
            run_id: claim.run_id,
            activity_id: claim.activity_id,
            attempt: claim.attempt,
            lease_token: claim.token,
            completion_token: completion_token(&claim),
            result: result.to_vec(),
            failed,
            retryable,
        };
        let completion_identity = internal_identity()?;
        let result = self
            .activities
            .complete(completion_identity, shard, completion.clone())
            .await;
        let result = match result {
            Err(InvocationError::Pending(pending)) => {
                // Resolve the exact completion before returning or retrying: rerunning the
                // handler could repeat an external side effect after its result committed.
                let resolution = match self.activities.client.resolve(&pending).await {
                    Ok(resolution) => resolution,
                    Err(_) => return Err(ActivitySupervisorError::Pending(pending)),
                };
                match resolution {
                    Resolution::Committed(outcome) => crate::client::decode_pending::<
                        ActivityCompletionOutcome,
                    >(&pending, outcome),
                    Resolution::Absent => {
                        self.activities
                            .complete(completion_identity, shard, completion)
                            .await
                    }
                    Resolution::Unknown | Resolution::Expired => {
                        return Err(ActivitySupervisorError::Pending(pending));
                    }
                }
            }
            result => result,
        };
        let committed = match result {
            Ok(committed) => committed,
            Err(InvocationError::Rejected(committed)) => {
                return match committed.output {
                    ActivityCompletionOutcome::LeaseLost => Ok(ActivityRunOutcome::LeaseLost {
                        receipt: committed.receipt,
                    }),
                    ActivityCompletionOutcome::IdentityConflict => {
                        Ok(ActivityRunOutcome::IdentityConflict {
                            receipt: committed.receipt,
                        })
                    }
                    _ => Err(ActivitySupervisorError::Runtime(Error::Command(
                        "activity completion returned an invalid rejection",
                    ))),
                };
            }
            Err(error) => return Err(unexpected_invocation(error)),
        };
        Ok(match committed.output {
            ActivityCompletionOutcome::Applied(workflow) => ActivityRunOutcome::Completed {
                workflow,
                receipt: committed.receipt,
            },
            ActivityCompletionOutcome::Retrying { due_at_ms } => ActivityRunOutcome::Retrying {
                due_at_ms,
                receipt: committed.receipt,
            },
            ActivityCompletionOutcome::Duplicate { result } => ActivityRunOutcome::Duplicate {
                result,
                receipt: committed.receipt,
            },
            ActivityCompletionOutcome::IdentityConflict => ActivityRunOutcome::IdentityConflict {
                receipt: committed.receipt,
            },
            ActivityCompletionOutcome::LeaseLost => ActivityRunOutcome::LeaseLost {
                receipt: committed.receipt,
            },
        })
    }
}

struct CancellationGuard(ActivityCancellation);

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn completion_token(claim: &ActivityClaim) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.activity-completion.v1\0");
    hasher.update(&claim.run_id);
    hasher.update(&claim.activity_id);
    hasher.update(&claim.attempt.to_be_bytes());
    let mut token = [0; 16];
    token.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    token
}

fn internal_identity() -> std::result::Result<MutationIdentity, ActivitySupervisorError> {
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| {
                ActivitySupervisorError::Runtime(Error::Command(
                    "system clock is before Unix epoch",
                ))
            })?
            .as_millis(),
    )
    .map_err(|_| {
        ActivitySupervisorError::Runtime(Error::Command("system clock exceeds i64 milliseconds"))
    })?;
    let expires_at_ms = now_ms.checked_add(HANDLER_IDENTITY_LIFETIME_MS).ok_or(
        ActivitySupervisorError::Runtime(Error::Command("activity mutation expiry overflow")),
    )?;
    let mut request_id = [0; 16];
    rand::rng().fill_bytes(&mut request_id);
    Ok(MutationIdentity {
        request_id: RequestId::from_bytes(request_id),
        issued_at_ms: now_ms,
        expires_at_ms,
    })
}

fn unexpected_invocation<T>(error: InvocationError<T>) -> ActivitySupervisorError {
    match error {
        InvocationError::Pending(pending) => ActivitySupervisorError::Pending(pending),
        InvocationError::InvalidPublishedResult { receipt, source } => {
            ActivitySupervisorError::InvalidPublishedResult { receipt, source }
        }
        InvocationError::NotStarted(error) => ActivitySupervisorError::Runtime(error),
        InvocationError::Rejected(_) => ActivitySupervisorError::Runtime(Error::Command(
            "activity supervisor command was unexpectedly rejected",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_an_attempt_guard_signals_cooperative_cancellation() {
        let cancellation = ActivityCancellation::default();
        {
            let _guard = CancellationGuard(cancellation.clone());
            assert!(!cancellation.is_cancelled());
        }
        assert!(cancellation.is_cancelled());
    }
}
