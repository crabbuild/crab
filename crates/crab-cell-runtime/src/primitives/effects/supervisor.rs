use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use rand::RngCore;

use crate::Error;
use crate::cell::executor::StoredOutcome;
use crate::cell::executor::{MutationIdentity, Resolution};
use crate::client::{InvocationError, PendingMutation, Receipt};
use crate::identity::RequestId;
use crate::peer::EffectPeerClient;

use super::{EffectClaim, EffectClaimRequest, EffectLeaseOutcome, EffectModule, EffectSource};

const SUPERVISOR_IDENTITY_LIFETIME_MS: i64 = 60_000;

/// Outcome of one bounded source claim, destination delivery and source transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectRunOutcome {
    /// No claimable effect remained.
    Idle {
        /// Receipt the observation is bound to.
        receipt: Receipt,
    },
    /// The destination applied the effect and the source recorded it.
    Delivered {
        /// Destination outcome the source recorded.
        destination: StoredOutcome,
        /// Receipt the source transition published.
        receipt: Receipt,
    },
    /// The destination asked for another attempt.
    Retrying {
        /// Logical time the next attempt is due.
        due_at_ms: i64,
        /// Receipt the source transition published.
        receipt: Receipt,
    },
    /// The destination rejected the effect terminally.
    Failed {
        /// Receipt the source transition published.
        receipt: Receipt,
    },
    /// The source lease was lost before the transition.
    LeaseLost {
        /// Receipt the failed resolution is bound to.
        receipt: Receipt,
    },
}

/// Failure that preserves unresolved source mutation evidence.
pub enum EffectSupervisorError {
    /// A source transition is committed but unresolved; resolve it before
    /// running another cycle.
    Pending(Box<PendingMutation>),
    /// The published source result could not be decoded.
    InvalidPublishedResult {
        /// Receipt the published result was observed at.
        receipt: Receipt,
        /// Decoding failure that produced this error.
        source: Box<Error>,
    },
    /// The supervisor failed before it could resolve the source transition.
    Runtime(Error),
}

impl fmt::Debug for EffectSupervisorError {
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

impl fmt::Display for EffectSupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pending(_) => formatter.write_str("effect supervisor mutation needs resolution"),
            Self::InvalidPublishedResult { .. } => {
                formatter.write_str("effect supervisor received an invalid published result")
            }
            Self::Runtime(error) => write!(formatter, "effect supervisor failed: {error}"),
        }
    }
}

impl std::error::Error for EffectSupervisorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidPublishedResult { source, .. } => Some(source.as_ref()),
            Self::Runtime(error) => Some(error),
            Self::Pending(_) => None,
        }
    }
}

/// Delivers published effects without holding a SQLite transaction across await.
pub struct EffectSupervisor<M> {
    source: EffectSource<M>,
    peer: EffectPeerClient,
    lease_ms: u32,
}

impl<M: EffectModule> EffectSupervisor<M> {
    /// Creates a supervisor using a 5..=300 second source lease.
    pub fn new(
        source: EffectSource<M>,
        peer: EffectPeerClient,
        lease_ms: u32,
    ) -> crate::Result<Self> {
        if !(5_000..=300_000).contains(&lease_ms) {
            return Err(Error::Command(
                "effect supervisor lease must be in 5..=300 seconds",
            ));
        }
        Ok(Self {
            source,
            peer,
            lease_ms,
        })
    }

    /// Claims and delivers at most one effect from the explicit source Cell.
    pub async fn run_once(&self) -> std::result::Result<EffectRunOutcome, EffectSupervisorError> {
        let claimed = self
            .source
            .claim(
                internal_identity()?,
                EffectClaimRequest {
                    limit: 1,
                    lease_ms: self.lease_ms,
                },
            )
            .await
            .map_err(unexpected_invocation)?;
        let Some(claim) = claimed.output.into_iter().next() else {
            return Ok(EffectRunOutcome::Idle {
                receipt: claimed.receipt,
            });
        };
        let validation = self
            .source
            .validate(vec![claim.clone()], claimed.receipt)
            .await
            .map_err(unexpected_invocation)?;
        if !validation.output {
            return Ok(EffectRunOutcome::LeaseLost {
                receipt: validation.receipt,
            });
        }

        let now_ms = system_time_ms()?;
        let destination = match self.peer.deliver(&claim, now_ms).await {
            Ok(outcome) => Some(outcome),
            Err(Error::EffectOutcomeUnknown { .. }) => {
                match self.peer.resolve(&claim, now_ms).await {
                    Ok(Resolution::Committed(outcome)) => Some(outcome),
                    Ok(Resolution::Absent | Resolution::Unknown | Resolution::Expired) => None,
                    Err(error) => return Err(EffectSupervisorError::Runtime(error)),
                }
            }
            Err(error) if retryable_delivery_error(&error) => None,
            Err(error) => return Err(EffectSupervisorError::Runtime(error)),
        };
        match destination {
            Some(destination) => self.ack(claim, destination).await,
            None => self.retry(claim).await,
        }
    }

    async fn ack(
        &self,
        claim: EffectClaim,
        destination: StoredOutcome,
    ) -> std::result::Result<EffectRunOutcome, EffectSupervisorError> {
        match self
            .source
            .ack(internal_identity()?, claim, destination.result().to_vec())
            .await
        {
            Ok(committed) if committed.output == EffectLeaseOutcome::Delivered => {
                Ok(EffectRunOutcome::Delivered {
                    destination,
                    receipt: committed.receipt,
                })
            }
            Err(InvocationError::Rejected(committed))
                if committed.output == EffectLeaseOutcome::LeaseLost =>
            {
                Ok(EffectRunOutcome::LeaseLost {
                    receipt: committed.receipt,
                })
            }
            Ok(_) | Err(InvocationError::Rejected(_)) => Err(EffectSupervisorError::Runtime(
                Error::Command("effect acknowledgement returned an invalid outcome"),
            )),
            Err(error) => Err(unexpected_invocation(error)),
        }
    }

    async fn retry(
        &self,
        claim: EffectClaim,
    ) -> std::result::Result<EffectRunOutcome, EffectSupervisorError> {
        match self.source.retry(internal_identity()?, claim).await {
            Ok(committed) => match committed.output {
                EffectLeaseOutcome::Retrying { due_at_ms } => Ok(EffectRunOutcome::Retrying {
                    due_at_ms,
                    receipt: committed.receipt,
                }),
                EffectLeaseOutcome::Failed => Ok(EffectRunOutcome::Failed {
                    receipt: committed.receipt,
                }),
                _ => Err(EffectSupervisorError::Runtime(Error::Command(
                    "effect retry returned an invalid outcome",
                ))),
            },
            Err(InvocationError::Rejected(committed))
                if committed.output == EffectLeaseOutcome::LeaseLost =>
            {
                Ok(EffectRunOutcome::LeaseLost {
                    receipt: committed.receipt,
                })
            }
            Err(InvocationError::Rejected(_)) => Err(EffectSupervisorError::Runtime(
                Error::Command("effect retry returned an invalid rejection"),
            )),
            Err(error) => Err(unexpected_invocation(error)),
        }
    }
}

fn retryable_delivery_error(error: &Error) -> bool {
    matches!(
        error,
        Error::PeerTransport { .. }
            | Error::PeerTransportUnknown { .. }
            | Error::EffectExpired
            | Error::CellNotActive
            | Error::CellDraining
            | Error::Fenced
            | Error::Capacity(_)
            | Error::Deadline
            | Error::RuntimeClosed
    )
}

fn internal_identity() -> std::result::Result<MutationIdentity, EffectSupervisorError> {
    let issued_at_ms = system_time_ms()?;
    let expires_at_ms = issued_at_ms
        .checked_add(SUPERVISOR_IDENTITY_LIFETIME_MS)
        .ok_or(EffectSupervisorError::Runtime(Error::Command(
            "effect mutation expiry overflow",
        )))?;
    let mut request_id = [0; 16];
    rand::rng().fill_bytes(&mut request_id);
    Ok(MutationIdentity {
        request_id: RequestId::from_bytes(request_id),
        issued_at_ms,
        expires_at_ms,
    })
}

fn system_time_ms() -> std::result::Result<i64, EffectSupervisorError> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| {
                EffectSupervisorError::Runtime(Error::Command("system clock is before Unix epoch"))
            })?
            .as_millis(),
    )
    .map_err(|_| {
        EffectSupervisorError::Runtime(Error::Command("system clock exceeds i64 milliseconds"))
    })
}

fn unexpected_invocation<T>(error: InvocationError<T>) -> EffectSupervisorError {
    match error {
        InvocationError::Pending(pending) => EffectSupervisorError::Pending(pending),
        InvocationError::InvalidPublishedResult { receipt, source } => {
            EffectSupervisorError::InvalidPublishedResult { receipt, source }
        }
        InvocationError::NotStarted(error) => EffectSupervisorError::Runtime(error),
        InvocationError::Rejected(_) => EffectSupervisorError::Runtime(Error::Command(
            "effect supervisor command was unexpectedly rejected",
        )),
    }
}
