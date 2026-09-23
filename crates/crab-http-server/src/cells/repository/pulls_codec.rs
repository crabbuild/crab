use serde::{Serialize, de::DeserializeOwned};

use crab_cell_runtime::codec::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};

use super::{
    CreatePullCommentInput, CreatePullCommentOutcome, CreatePullInput, CreatePullOutcome,
    CreatePullReviewInput, CreatePullReviewOutcome, PullChildKey, PullChildListInput,
    PullCommentPage, PullCommentRecord, PullListInput, PullMerge, PullPage, PullRecord,
    PullReviewPage, PullReviewRecord, PullSubmissionKey, ReservePullMergeInput,
    ReservePullMergeOutcome, TransitionPullMergeInput, TransitionPullMergeOutcome,
    UpdatePullCommentInput, UpdatePullCommentOutcome, UpdatePullInput, UpdatePullOutcome,
    UpdatePullReviewInput, UpdatePullReviewOutcome,
};

fn encode_json<T: Serialize>(value: &T, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| CodecError::Invalid("invalid pull wire value"))?;
    encoder.write_bytes(&bytes)
}

fn decode_json<T: DeserializeOwned>(decoder: &mut BoundedDecoder<'_>) -> Result<T, CodecError> {
    serde_json::from_slice(decoder.read_bytes()?)
        .map_err(|_| CodecError::Invalid("invalid pull wire value"))
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
    PullMerge,
    PullRecord,
    PullPage,
    PullCommentRecord,
    PullReviewRecord,
    PullChildKey,
    PullSubmissionKey,
    PullListInput,
    PullChildListInput,
    PullCommentPage,
    PullReviewPage,
    CreatePullInput,
    CreatePullOutcome,
    UpdatePullInput,
    UpdatePullOutcome,
    CreatePullCommentInput,
    CreatePullCommentOutcome,
    UpdatePullCommentInput,
    UpdatePullCommentOutcome,
    CreatePullReviewInput,
    CreatePullReviewOutcome,
    UpdatePullReviewInput,
    UpdatePullReviewOutcome,
    ReservePullMergeInput,
    ReservePullMergeOutcome,
    TransitionPullMergeInput,
    TransitionPullMergeOutcome,
);
