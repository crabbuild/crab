use serde::{Serialize, de::DeserializeOwned};

use crab_cell_runtime::codec::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};

use super::{
    AttachReleaseAssetInput, AttachReleaseAssetOutcome, CompleteReleasePublicationInput,
    CompleteReleasePublicationOutcome, CreateReleaseInput, CreateReleaseOutcome,
    DeleteReleaseAssetInput, DeleteReleaseAssetOutcome, DeleteReleaseInput, DeleteReleaseOutcome,
    ReleaseAssetRecord, ReleaseAssetReservation, ReleaseListInput, ReleasePage, ReleasePublication,
    ReleaseRecord, ReleaseSubmissionKey, ReserveReleaseAssetInput, ReserveReleaseAssetOutcome,
    UpdateReleaseInput, UpdateReleaseOutcome,
};

fn encode_json<T: Serialize>(value: &T, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| CodecError::Invalid("invalid release wire value"))?;
    encoder.write_bytes(&bytes)
}

fn decode_json<T: DeserializeOwned>(decoder: &mut BoundedDecoder<'_>) -> Result<T, CodecError> {
    serde_json::from_slice(decoder.read_bytes()?)
        .map_err(|_| CodecError::Invalid("invalid release wire value"))
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
    ReleasePublication,
    ReleaseAssetRecord,
    ReleaseRecord,
    CreateReleaseInput,
    CreateReleaseOutcome,
    UpdateReleaseInput,
    UpdateReleaseOutcome,
    CompleteReleasePublicationInput,
    CompleteReleasePublicationOutcome,
    DeleteReleaseInput,
    DeleteReleaseOutcome,
    ReleaseAssetReservation,
    ReserveReleaseAssetInput,
    ReserveReleaseAssetOutcome,
    AttachReleaseAssetInput,
    AttachReleaseAssetOutcome,
    DeleteReleaseAssetInput,
    DeleteReleaseAssetOutcome,
    ReleaseListInput,
    ReleasePage,
    ReleaseSubmissionKey,
);
