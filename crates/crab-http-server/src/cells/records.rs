use bytes::{Buf, BufMut};
use serde::{Serialize, de::DeserializeOwned};

// These bytes are stored in repository SQLite rows and submission digests.
// Preserve the existing big-endian record format when changing the runtime.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RecordError {
    #[error("repository record exceeds its limit")]
    Limit,
    #[error("invalid repository record: {0}")]
    Invalid(&'static str),
    #[error("repository record JSON is invalid")]
    Json(#[from] serde_json::Error),
    #[error("repository record text is not UTF-8")]
    Utf8(#[from] std::str::Utf8Error),
}

pub(crate) trait StoredRecord: Sized {
    fn encode(&self, encoder: &mut RecordEncoder) -> Result<(), RecordError>;
    fn decode(decoder: &mut RecordDecoder<'_>) -> Result<Self, RecordError>;
}

pub(crate) struct RecordEncoder {
    bytes: Vec<u8>,
    limit: usize,
}

impl RecordEncoder {
    pub(crate) fn new(limit: u32) -> Result<Self, RecordError> {
        if limit == 0 {
            return Err(RecordError::Limit);
        }
        Ok(Self {
            bytes: Vec::new(),
            limit: limit as usize,
        })
    }

    fn reserve(&mut self, bytes: usize) -> Result<(), RecordError> {
        if bytes > self.limit.saturating_sub(self.bytes.len()) {
            return Err(RecordError::Limit);
        }
        self.bytes.reserve(bytes);
        Ok(())
    }

    pub(crate) fn write_u8(&mut self, value: u8) -> Result<(), RecordError> {
        self.reserve(1)?;
        self.bytes.put_u8(value);
        Ok(())
    }

    pub(crate) fn write_u64(&mut self, value: u64) -> Result<(), RecordError> {
        self.reserve(8)?;
        self.bytes.put_u64(value);
        Ok(())
    }

    pub(crate) fn write_count(&mut self, value: usize) -> Result<(), RecordError> {
        let value = u32::try_from(value).map_err(|_| RecordError::Limit)?;
        self.reserve(4)?;
        self.bytes.put_u32(value);
        Ok(())
    }

    pub(crate) fn write_bytes(&mut self, value: &[u8]) -> Result<(), RecordError> {
        self.write_count(value.len())?;
        self.reserve(value.len())?;
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    pub(crate) fn write_text(&mut self, value: &str) -> Result<(), RecordError> {
        self.write_bytes(value.as_bytes())
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

pub(crate) struct RecordDecoder<'a> {
    bytes: &'a [u8],
}

impl<'a> RecordDecoder<'a> {
    pub(crate) fn new(bytes: &'a [u8], limit: u32) -> Result<Self, RecordError> {
        if bytes.len() > limit as usize {
            return Err(RecordError::Limit);
        }
        Ok(Self { bytes })
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], RecordError> {
        let (head, tail) = self
            .bytes
            .split_at_checked(count)
            .ok_or(RecordError::Invalid("truncated value"))?;
        self.bytes = tail;
        Ok(head)
    }

    pub(crate) fn read_u8(&mut self) -> Result<u8, RecordError> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn read_u64(&mut self) -> Result<u64, RecordError> {
        Ok(self.take(8)?.get_u64())
    }
    pub(crate) fn read_count(&mut self) -> Result<usize, RecordError> {
        Ok(self.take(4)?.get_u32() as usize)
    }
    pub(crate) fn read_bytes(&mut self) -> Result<&'a [u8], RecordError> {
        let count = self.read_count()?;
        self.take(count)
    }
    pub(crate) fn read_text(&mut self) -> Result<&'a str, RecordError> {
        Ok(std::str::from_utf8(self.read_bytes()?)?)
    }
    pub(crate) fn finish(self) -> Result<(), RecordError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(RecordError::Invalid("trailing bytes"))
        }
    }
}

impl StoredRecord for u8 {
    fn encode(&self, encoder: &mut RecordEncoder) -> Result<(), RecordError> {
        encoder.write_u8(*self)
    }
    fn decode(decoder: &mut RecordDecoder<'_>) -> Result<Self, RecordError> {
        decoder.read_u8()
    }
}
impl StoredRecord for String {
    fn encode(&self, encoder: &mut RecordEncoder) -> Result<(), RecordError> {
        encoder.write_text(self)
    }
    fn decode(decoder: &mut RecordDecoder<'_>) -> Result<Self, RecordError> {
        Ok(decoder.read_text()?.to_owned())
    }
}
impl<T: StoredRecord> StoredRecord for Option<T> {
    fn encode(&self, encoder: &mut RecordEncoder) -> Result<(), RecordError> {
        match self {
            None => encoder.write_u8(0),
            Some(value) => {
                encoder.write_u8(1)?;
                value.encode(encoder)
            }
        }
    }
    fn decode(decoder: &mut RecordDecoder<'_>) -> Result<Self, RecordError> {
        match decoder.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(T::decode(decoder)?)),
            _ => Err(RecordError::Invalid("invalid optional value tag")),
        }
    }
}

fn encode_json(value: &impl Serialize, encoder: &mut RecordEncoder) -> Result<(), RecordError> {
    let bytes = serde_json::to_vec(value)?;
    encoder.write_bytes(&bytes)
}
fn decode_json<T: DeserializeOwned>(decoder: &mut RecordDecoder<'_>) -> Result<T, RecordError> {
    Ok(serde_json::from_slice(decoder.read_bytes()?)?)
}

impl StoredRecord for super::repository::PullMerge {
    fn encode(&self, encoder: &mut RecordEncoder) -> Result<(), RecordError> {
        encode_json(self, encoder)
    }
    fn decode(decoder: &mut RecordDecoder<'_>) -> Result<Self, RecordError> {
        decode_json(decoder)
    }
}
impl StoredRecord for super::repository::ReleasePublication {
    fn encode(&self, encoder: &mut RecordEncoder) -> Result<(), RecordError> {
        encode_json(self, encoder)
    }
    fn decode(decoder: &mut RecordDecoder<'_>) -> Result<Self, RecordError> {
        decode_json(decoder)
    }
}
