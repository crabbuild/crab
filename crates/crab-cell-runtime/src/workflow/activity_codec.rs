use crate::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};

use super::{
    ActivityClaim, ActivityCompletion, ActivityCompletionOutcome, ActivityLeaseOutcome,
    WorkflowActivityClaimRequest, WorkflowActivityExtendRequest, WorkflowActivityValidateRequest,
    WorkflowOutcome,
    activity::{MAX_ACTIVITY_PAYLOAD_BYTES, MAX_CLAIM_ITEMS},
};

const APPLIED_TAG: u8 = 0;
const RETRYING_TAG: u8 = 1;
const DUPLICATE_TAG: u8 = 2;
const IDENTITY_CONFLICT_TAG: u8 = 3;
const LEASE_LOST_TAG: u8 = 4;
const EXTENDED_TAG: u8 = 0;

impl WireValue for WorkflowActivityClaimRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u32(self.limit)?;
        encoder.write_u32(self.lease_ms)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            limit: decoder.read_u32()?,
            lease_ms: decoder.read_u32()?,
        })
    }
}

impl WireValue for ActivityClaim {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        validate_claim(self)?;
        encoder.write_bytes(&self.run_id)?;
        encoder.write_bytes(&self.activity_id)?;
        encoder.write_text(&self.activity_type)?;
        encoder.write_bytes(&self.input)?;
        encoder.write_bytes(self.definition_digest.as_bytes())?;
        encoder.write_u32(self.attempt)?;
        encoder.write_bytes(&self.token)?;
        encoder.write_i64(self.lease_until_ms)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let claim = Self {
            run_id: read_fixed(decoder, "activity run ID length")?,
            activity_id: read_fixed(decoder, "activity ID length")?,
            activity_type: decoder.read_text()?.to_owned(),
            input: decoder.read_bytes()?.to_vec(),
            definition_digest: crate::Digest::from_bytes(read_fixed(
                decoder,
                "activity definition digest length",
            )?),
            attempt: decoder.read_u32()?,
            token: read_fixed(decoder, "activity lease token length")?,
            lease_until_ms: decoder.read_i64()?,
        };
        validate_claim(&claim)?;
        Ok(claim)
    }
}

impl WireValue for Vec<ActivityClaim> {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        if self.len() > MAX_CLAIM_ITEMS {
            return Err(CodecError::Invalid("too many activity claims"));
        }
        encoder.write_count(self.len())?;
        for claim in self {
            claim.encode(encoder)?;
        }
        Ok(())
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let count = decoder.read_count()?;
        if count > MAX_CLAIM_ITEMS {
            return Err(CodecError::Invalid("too many activity claims"));
        }
        let mut claims = Vec::with_capacity(count);
        for _ in 0..count {
            claims.push(ActivityClaim::decode(decoder)?);
        }
        Ok(claims)
    }
}

impl WireValue for WorkflowActivityExtendRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        self.claim.encode(encoder)?;
        encoder.write_u32(self.extension_ms)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            claim: ActivityClaim::decode(decoder)?,
            extension_ms: decoder.read_u32()?,
        })
    }
}

impl WireValue for ActivityLeaseOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Extended { lease_until_ms } => {
                encoder.write_u8(EXTENDED_TAG)?;
                encoder.write_i64(*lease_until_ms)
            }
            Self::LeaseLost => encoder.write_u8(LEASE_LOST_TAG),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            EXTENDED_TAG => Ok(Self::Extended {
                lease_until_ms: decoder.read_i64()?,
            }),
            LEASE_LOST_TAG => Ok(Self::LeaseLost),
            _ => Err(CodecError::Invalid("invalid activity lease outcome tag")),
        }
    }
}

impl WireValue for ActivityCompletion {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        if self.result.len() > MAX_ACTIVITY_PAYLOAD_BYTES || (!self.failed && self.retryable) {
            return Err(CodecError::Invalid("invalid activity completion"));
        }
        encoder.write_bytes(&self.run_id)?;
        encoder.write_bytes(&self.activity_id)?;
        encoder.write_u32(self.attempt)?;
        encoder.write_bytes(&self.lease_token)?;
        encoder.write_bytes(&self.completion_token)?;
        encoder.write_bytes(&self.result)?;
        encoder.write_bool(self.failed)?;
        encoder.write_bool(self.retryable)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let completion = Self {
            run_id: read_fixed(decoder, "activity completion run ID length")?,
            activity_id: read_fixed(decoder, "activity completion ID length")?,
            attempt: decoder.read_u32()?,
            lease_token: read_fixed(decoder, "activity completion lease token length")?,
            completion_token: read_fixed(decoder, "activity completion token length")?,
            result: decoder.read_bytes()?.to_vec(),
            failed: decoder.read_bool()?,
            retryable: decoder.read_bool()?,
        };
        if completion.result.len() > MAX_ACTIVITY_PAYLOAD_BYTES
            || (!completion.failed && completion.retryable)
        {
            return Err(CodecError::Invalid("invalid activity completion"));
        }
        Ok(completion)
    }
}

impl WireValue for ActivityCompletionOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Applied(workflow) => {
                encoder.write_u8(APPLIED_TAG)?;
                workflow.encode(encoder)
            }
            Self::Retrying { due_at_ms } => {
                encoder.write_u8(RETRYING_TAG)?;
                encoder.write_i64(*due_at_ms)
            }
            Self::Duplicate { result } => {
                encoder.write_u8(DUPLICATE_TAG)?;
                encoder.write_bytes(result)
            }
            Self::IdentityConflict => encoder.write_u8(IDENTITY_CONFLICT_TAG),
            Self::LeaseLost => encoder.write_u8(LEASE_LOST_TAG),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            APPLIED_TAG => Ok(Self::Applied(WorkflowOutcome::decode(decoder)?)),
            RETRYING_TAG => Ok(Self::Retrying {
                due_at_ms: decoder.read_i64()?,
            }),
            DUPLICATE_TAG => {
                let result = decoder.read_bytes()?.to_vec();
                if result.len() > MAX_ACTIVITY_PAYLOAD_BYTES {
                    return Err(CodecError::Invalid("activity result exceeds 256 KiB"));
                }
                Ok(Self::Duplicate { result })
            }
            IDENTITY_CONFLICT_TAG => Ok(Self::IdentityConflict),
            LEASE_LOST_TAG => Ok(Self::LeaseLost),
            _ => Err(CodecError::Invalid(
                "invalid activity completion outcome tag",
            )),
        }
    }
}

impl WireValue for WorkflowActivityValidateRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        self.claimed.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            claimed: Vec::<ActivityClaim>::decode(decoder)?,
        })
    }
}

fn validate_claim(claim: &ActivityClaim) -> Result<(), CodecError> {
    if claim.activity_type.is_empty()
        || claim.activity_type.len() > 256
        || claim.input.len() > MAX_ACTIVITY_PAYLOAD_BYTES
        || claim
            .definition_digest
            .as_bytes()
            .iter()
            .all(|byte| *byte == 0)
        || claim.attempt == 0
        || claim.lease_until_ms < 0
    {
        return Err(CodecError::Invalid("invalid activity claim"));
    }
    Ok(())
}

fn read_fixed<const N: usize>(
    decoder: &mut BoundedDecoder<'_>,
    message: &'static str,
) -> Result<[u8; N], CodecError> {
    decoder
        .read_bytes()?
        .try_into()
        .map_err(|_| CodecError::Invalid(message))
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use super::*;

    fn roundtrip<T: WireValue + PartialEq + fmt::Debug>(value: T) {
        let mut encoder = BoundedEncoder::new(1024 * 1024).unwrap();
        value.encode(&mut encoder).unwrap();
        let bytes = encoder.finish();
        let mut decoder = BoundedDecoder::new(&bytes, 1024 * 1024).unwrap();
        assert_eq!(T::decode(&mut decoder).unwrap(), value);
        decoder.finish().unwrap();
    }

    #[test]
    fn activity_codecs_roundtrip_claim_lease_completion_and_validation() {
        let claim = ActivityClaim {
            run_id: [1; 16],
            activity_id: [2; 16],
            activity_type: "echo".into(),
            input: b"payload".to_vec(),
            definition_digest: crate::Digest::from_bytes([3; 32]),
            attempt: 1,
            token: [4; 16],
            lease_until_ms: 10_000,
        };
        roundtrip(WorkflowActivityClaimRequest {
            limit: 1,
            lease_ms: 5_000,
        });
        roundtrip(vec![claim.clone()]);
        roundtrip(WorkflowActivityExtendRequest {
            claim: claim.clone(),
            extension_ms: 5_000,
        });
        roundtrip(ActivityLeaseOutcome::Extended {
            lease_until_ms: 15_000,
        });
        roundtrip(ActivityCompletion {
            run_id: claim.run_id,
            activity_id: claim.activity_id,
            attempt: claim.attempt,
            lease_token: claim.token,
            completion_token: [5; 16],
            result: b"done".to_vec(),
            failed: false,
            retryable: false,
        });
        roundtrip(ActivityCompletionOutcome::Retrying { due_at_ms: 20_000 });
        roundtrip(WorkflowActivityValidateRequest {
            claimed: vec![claim],
        });
    }

    #[test]
    fn activity_decoder_rejects_invalid_claim_shape() {
        let claim = ActivityClaim {
            run_id: [1; 16],
            activity_id: [2; 16],
            activity_type: "echo".into(),
            input: Vec::new(),
            definition_digest: crate::Digest::from_bytes([3; 32]),
            attempt: 0,
            token: [4; 16],
            lease_until_ms: 10_000,
        };
        let mut encoder = BoundedEncoder::new(256).unwrap();
        assert!(matches!(
            claim.encode(&mut encoder),
            Err(CodecError::Invalid("invalid activity claim"))
        ));
    }
}
