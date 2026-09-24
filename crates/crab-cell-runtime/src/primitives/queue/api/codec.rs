//! Queue wire codecs for send, claim, lease, control, and info payloads.

use super::*;

const SENT_TAG: u8 = 0;
const PRODUCER_CONFLICT_TAG: u8 = 1;
const ACK_TAG: u8 = 0;
const RETRY_TAG: u8 = 1;
const EXTEND_TAG: u8 = 2;
const APPLIED_TAG: u8 = 0;
const LEASE_LOST_TAG: u8 = 1;
const PAUSE_TAG: u8 = 0;
const RESUME_TAG: u8 = 1;
const PURGE_TAG: u8 = 2;
const REDRIVE_TAG: u8 = 3;

impl WireValue for QueueSendRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.producer_id)?;
        encoder.write_bytes(&self.payload)?;
        encoder.write_i64(self.available_at_ms)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            producer_id: read_fixed(decoder, "queue producer ID length")?,
            payload: decoder.read_bytes()?.to_vec(),
            available_at_ms: decoder.read_i64()?,
        })
    }
}

impl WireValue for QueueSendOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Sent { message_id } => {
                encoder.write_u8(SENT_TAG)?;
                encoder.write_bytes(message_id)
            }
            Self::ProducerConflict => encoder.write_u8(PRODUCER_CONFLICT_TAG),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            SENT_TAG => Ok(Self::Sent {
                message_id: read_fixed(decoder, "queue message ID length")?,
            }),
            PRODUCER_CONFLICT_TAG => Ok(Self::ProducerConflict),
            _ => Err(CodecError::Invalid("invalid queue send outcome tag")),
        }
    }
}

impl WireValue for QueueClaimRequest {
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

impl WireValue for QueueMessage {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        validate_message(self)?;
        encoder.write_bytes(&self.message_id)?;
        encoder.write_bytes(&self.payload)?;
        encoder.write_bytes(&self.token)?;
        encoder.write_u32(self.attempt)?;
        encoder.write_i64(self.lease_until_ms)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let message = Self {
            message_id: read_fixed(decoder, "queue message ID length")?,
            payload: decoder.read_bytes()?.to_vec(),
            token: read_fixed(decoder, "queue lease token length")?,
            attempt: decoder.read_u32()?,
            lease_until_ms: decoder.read_i64()?,
        };
        validate_message(&message)?;
        Ok(message)
    }
}

impl WireValue for Vec<QueueMessage> {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        if self.len() > MAX_CLAIM_ITEMS {
            return Err(CodecError::Invalid("too many queue messages"));
        }
        encoder.write_count(self.len())?;
        for message in self {
            message.encode(encoder)?;
        }
        Ok(())
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let count = decoder.read_count()?;
        if count > MAX_CLAIM_ITEMS {
            return Err(CodecError::Invalid("too many queue messages"));
        }
        let mut messages = Vec::with_capacity(count);
        for _ in 0..count {
            messages.push(QueueMessage::decode(decoder)?);
        }
        Ok(messages)
    }
}

impl WireValue for QueueLeaseRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(&self.message_id)?;
        encoder.write_bytes(&self.token)?;
        match self.action {
            QueueLeaseAction::Ack => encoder.write_u8(ACK_TAG),
            QueueLeaseAction::Retry { delay_ms } => {
                encoder.write_u8(RETRY_TAG)?;
                encoder.write_u32(delay_ms)
            }
            QueueLeaseAction::Extend { extension_ms } => {
                encoder.write_u8(EXTEND_TAG)?;
                encoder.write_u32(extension_ms)
            }
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let message_id = read_fixed(decoder, "queue message ID length")?;
        let token = read_fixed(decoder, "queue lease token length")?;
        let action = match decoder.read_u8()? {
            ACK_TAG => QueueLeaseAction::Ack,
            RETRY_TAG => QueueLeaseAction::Retry {
                delay_ms: decoder.read_u32()?,
            },
            EXTEND_TAG => QueueLeaseAction::Extend {
                extension_ms: decoder.read_u32()?,
            },
            _ => return Err(CodecError::Invalid("invalid queue lease action tag")),
        };
        Ok(Self {
            message_id,
            token,
            action,
        })
    }
}

impl WireValue for QueueLeaseOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Applied {
                state,
                lease_until_ms,
            } => {
                validate_lease_outcome(*state, *lease_until_ms)?;
                encoder.write_u8(APPLIED_TAG)?;
                encode_state(*state, encoder)?;
                lease_until_ms.encode(encoder)
            }
            Self::LeaseLost => encoder.write_u8(LEASE_LOST_TAG),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            APPLIED_TAG => {
                let state = decode_state(decoder)?;
                let lease_until_ms = Option::<i64>::decode(decoder)?;
                validate_lease_outcome(state, lease_until_ms)?;
                Ok(Self::Applied {
                    state,
                    lease_until_ms,
                })
            }
            LEASE_LOST_TAG => Ok(Self::LeaseLost),
            _ => Err(CodecError::Invalid("invalid queue lease outcome tag")),
        }
    }
}

impl WireValue for QueueValidateRequest {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        self.claimed.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            claimed: Vec::<QueueMessage>::decode(decoder)?,
        })
    }
}

impl WireValue for QueueControlAction {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Pause => encoder.write_u8(PAUSE_TAG),
            Self::Resume => encoder.write_u8(RESUME_TAG),
            Self::Purge { limit } => {
                encoder.write_u8(PURGE_TAG)?;
                encoder.write_u32(*limit)
            }
            Self::Redrive { limit } => {
                encoder.write_u8(REDRIVE_TAG)?;
                encoder.write_u32(*limit)
            }
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            PAUSE_TAG => Ok(Self::Pause),
            RESUME_TAG => Ok(Self::Resume),
            PURGE_TAG => Ok(Self::Purge {
                limit: decoder.read_u32()?,
            }),
            REDRIVE_TAG => Ok(Self::Redrive {
                limit: decoder.read_u32()?,
            }),
            _ => Err(CodecError::Invalid("invalid queue control action tag")),
        }
    }
}

impl WireValue for QueueControlOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Paused { generation } => {
                encoder.write_u8(PAUSE_TAG)?;
                encoder.write_u64(*generation)
            }
            Self::Resumed { generation } => {
                encoder.write_u8(RESUME_TAG)?;
                encoder.write_u64(*generation)
            }
            Self::Purged { messages } => {
                encoder.write_u8(PURGE_TAG)?;
                encoder.write_u32(*messages)
            }
            Self::Redriven { messages } => {
                encoder.write_u8(REDRIVE_TAG)?;
                encoder.write_u32(*messages)
            }
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            PAUSE_TAG => Ok(Self::Paused {
                generation: decoder.read_u64()?,
            }),
            RESUME_TAG => Ok(Self::Resumed {
                generation: decoder.read_u64()?,
            }),
            PURGE_TAG => Ok(Self::Purged {
                messages: decoder.read_u32()?,
            }),
            REDRIVE_TAG => Ok(Self::Redriven {
                messages: decoder.read_u32()?,
            }),
            _ => Err(CodecError::Invalid("invalid queue control outcome tag")),
        }
    }
}

impl WireValue for QueueInfoRequest {
    fn encode(&self, _: &mut BoundedEncoder) -> Result<(), CodecError> {
        Ok(())
    }

    fn decode(_: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self)
    }
}

impl WireValue for QueueInfo {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        self.paused.encode(encoder)?;
        encoder.write_u64(self.generation)?;
        encoder.write_u64(self.ready)?;
        encoder.write_u64(self.leased)?;
        encoder.write_u64(self.acked)?;
        encoder.write_u64(self.dead)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            paused: bool::decode(decoder)?,
            generation: decoder.read_u64()?,
            ready: decoder.read_u64()?,
            leased: decoder.read_u64()?,
            acked: decoder.read_u64()?,
            dead: decoder.read_u64()?,
        })
    }
}

fn encode_state(state: QueueState, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
    encoder.write_u8(match state {
        QueueState::Ready => 0,
        QueueState::Leased => 1,
        QueueState::Acked => 2,
        QueueState::Dead => 3,
    })
}

fn decode_state(decoder: &mut BoundedDecoder<'_>) -> Result<QueueState, CodecError> {
    match decoder.read_u8()? {
        0 => Ok(QueueState::Ready),
        1 => Ok(QueueState::Leased),
        2 => Ok(QueueState::Acked),
        3 => Ok(QueueState::Dead),
        _ => Err(CodecError::Invalid("invalid queue state tag")),
    }
}

fn validate_message(message: &QueueMessage) -> Result<(), CodecError> {
    if message.payload.len() > MAX_PAYLOAD_BYTES
        || !(1..=MAX_ATTEMPTS).contains(&message.attempt)
        || message.lease_until_ms < 0
    {
        return Err(CodecError::Invalid("invalid queue claim"));
    }
    Ok(())
}

fn validate_lease_outcome(
    state: QueueState,
    lease_until_ms: Option<i64>,
) -> Result<(), CodecError> {
    if (state == QueueState::Leased) != lease_until_ms.is_some()
        || lease_until_ms.is_some_and(|deadline| deadline < 0)
    {
        return Err(CodecError::Invalid("inconsistent queue lease outcome"));
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
    use super::*;

    fn roundtrip<T: WireValue + PartialEq + std::fmt::Debug>(value: T) {
        let mut encoder = BoundedEncoder::new(1024 * 1024).unwrap();
        value.encode(&mut encoder).unwrap();
        let bytes = encoder.finish();
        let mut decoder = BoundedDecoder::new(&bytes, 1024 * 1024).unwrap();
        assert_eq!(T::decode(&mut decoder).unwrap(), value);
        decoder.finish().unwrap();
    }

    #[test]
    fn queue_codecs_roundtrip_every_operation_shape() {
        roundtrip(QueueSendRequest {
            producer_id: [1; 16],
            payload: vec![0, 255],
            available_at_ms: i64::MAX,
        });
        roundtrip(QueueSendOutcome::Sent {
            message_id: [2; 16],
        });
        roundtrip(QueueSendOutcome::ProducerConflict);
        roundtrip(QueueClaimRequest {
            limit: 32,
            lease_ms: 300_000,
        });
        let message = QueueMessage {
            message_id: [3; 16],
            payload: b"job".to_vec(),
            token: [4; 16],
            attempt: 20,
            lease_until_ms: i64::MAX,
        };
        roundtrip(vec![message.clone()]);
        for action in [
            QueueLeaseAction::Ack,
            QueueLeaseAction::Retry { delay_ms: 100 },
            QueueLeaseAction::Extend {
                extension_ms: 5_000,
            },
        ] {
            roundtrip(QueueLeaseRequest {
                message_id: message.message_id,
                token: message.token,
                action,
            });
        }
        roundtrip(QueueLeaseOutcome::Applied {
            state: QueueState::Leased,
            lease_until_ms: Some(10),
        });
        roundtrip(QueueLeaseOutcome::LeaseLost);
        roundtrip(QueueValidateRequest {
            claimed: vec![message],
        });
        roundtrip(QueueControlAction::Purge { limit: 128 });
        roundtrip(QueueControlOutcome::Paused { generation: 7 });
        roundtrip(QueueInfo {
            paused: true,
            generation: 7,
            ready: 1,
            leased: 2,
            acked: 3,
            dead: 4,
        });
    }

    #[test]
    fn queue_decoder_rejects_invalid_fixed_ids_and_unbounded_claims() {
        let mut bad_id = BoundedEncoder::new(32).unwrap();
        bad_id.write_bytes(&[1; 15]).unwrap();
        bad_id.write_bytes(&[]).unwrap();
        bad_id.write_i64(0).unwrap();
        let bytes = bad_id.finish();
        let mut decoder = BoundedDecoder::new(&bytes, 32).unwrap();
        assert!(matches!(
            QueueSendRequest::decode(&mut decoder),
            Err(CodecError::Invalid("queue producer ID length"))
        ));

        let mut too_many = BoundedEncoder::new(16).unwrap();
        too_many.write_count(MAX_CLAIM_ITEMS + 1).unwrap();
        let bytes = too_many.finish();
        let mut decoder = BoundedDecoder::new(&bytes, 16).unwrap();
        assert!(matches!(
            Vec::<QueueMessage>::decode(&mut decoder),
            Err(CodecError::Invalid("too many queue messages"))
        ));

        let mut inconsistent = BoundedEncoder::new(16).unwrap();
        inconsistent.write_u8(APPLIED_TAG).unwrap();
        encode_state(QueueState::Acked, &mut inconsistent).unwrap();
        Some(10_i64).encode(&mut inconsistent).unwrap();
        let bytes = inconsistent.finish();
        let mut decoder = BoundedDecoder::new(&bytes, 16).unwrap();
        assert!(matches!(
            QueueLeaseOutcome::decode(&mut decoder),
            Err(CodecError::Invalid("inconsistent queue lease outcome"))
        ));
    }
}
