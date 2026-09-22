use serde::{Serialize, de::DeserializeOwned};

use cellule_runtime::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};

use super::pull_review_threads::{
    CreatePullReviewReplyInput, CreatePullReviewReplyOutcome, CreatePullReviewThreadInput,
    CreatePullReviewThreadOutcome, PullReviewReplyKey, PullReviewReplyListInput,
    PullReviewReplyPage, PullReviewReplyRecord, PullReviewReplySubmissionKey, PullReviewThreadKey,
    PullReviewThreadListInput, PullReviewThreadPage, PullReviewThreadRecord,
    PullReviewThreadSubmissionKey, UpdatePullReviewReplyInput, UpdatePullReviewReplyOutcome,
    UpdatePullReviewThreadInput, UpdatePullReviewThreadOutcome,
};

fn encode_json<T: Serialize>(value: &T, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| CodecError::Invalid("invalid pull review thread wire value"))?;
    encoder.write_bytes(&bytes)
}

fn decode_json<T: DeserializeOwned>(decoder: &mut BoundedDecoder<'_>) -> Result<T, CodecError> {
    serde_json::from_slice(decoder.read_bytes()?)
        .map_err(|_| CodecError::Invalid("invalid pull review thread wire value"))
}

macro_rules! json_wire {
    ($($type:ty),+ $(,)?) => {
        $(
            impl WireValue for $type {
                fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
                    encode_json(self, encoder)
                }

                fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
                    decode_json(decoder)
                }
            }
        )+
    };
}

json_wire!(
    PullReviewThreadRecord,
    PullReviewReplyRecord,
    PullReviewThreadPage,
    PullReviewReplyPage,
    PullReviewThreadKey,
    PullReviewReplyKey,
    PullReviewThreadSubmissionKey,
    PullReviewReplySubmissionKey,
    PullReviewThreadListInput,
    PullReviewReplyListInput,
    CreatePullReviewThreadInput,
    CreatePullReviewThreadOutcome,
    UpdatePullReviewThreadInput,
    UpdatePullReviewThreadOutcome,
    CreatePullReviewReplyInput,
    CreatePullReviewReplyOutcome,
    UpdatePullReviewReplyInput,
    UpdatePullReviewReplyOutcome,
);
