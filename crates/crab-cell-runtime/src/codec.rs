const MAX_WIRE_BYTES: usize = 1024 * 1024;

/// Canonical bounded wire-codec failure.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("invalid wire value: {0}")]
    Invalid(&'static str),
    #[error("wire value exceeds its declared byte limit")]
    Limit,
    #[error("wire text is not UTF-8")]
    Utf8(#[from] std::str::Utf8Error),
}

/// Explicit canonical codec implemented by every registered input and output.
pub trait WireValue: Sized + Send + 'static {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError>;
    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError>;
}

/// Append-only encoder enforcing one operation's declared maximum size.
pub struct BoundedEncoder {
    bytes: Vec<u8>,
    limit: usize,
}

impl BoundedEncoder {
    pub fn new(limit: u32) -> Result<Self, CodecError> {
        let limit = usize::try_from(limit).map_err(|_| CodecError::Limit)?;
        if limit == 0 || limit > MAX_WIRE_BYTES {
            return Err(CodecError::Limit);
        }
        Ok(Self {
            bytes: Vec::with_capacity(limit.min(256)),
            limit,
        })
    }

    pub fn write_bool(&mut self, value: bool) -> Result<(), CodecError> {
        self.write_u8(u8::from(value))
    }

    pub fn write_u8(&mut self, value: u8) -> Result<(), CodecError> {
        self.extend(&[value])
    }

    pub fn write_u32(&mut self, value: u32) -> Result<(), CodecError> {
        self.extend(&value.to_be_bytes())
    }

    pub fn write_u64(&mut self, value: u64) -> Result<(), CodecError> {
        self.extend(&value.to_be_bytes())
    }

    pub fn write_i64(&mut self, value: i64) -> Result<(), CodecError> {
        self.extend(&value.to_be_bytes())
    }

    pub fn write_f64(&mut self, value: f64) -> Result<(), CodecError> {
        if !value.is_finite() {
            return Err(CodecError::Invalid("non-finite f64"));
        }
        let normalized = if value == 0.0 { 0.0 } else { value };
        self.extend(&normalized.to_bits().to_be_bytes())
    }

    pub fn write_bytes(&mut self, value: &[u8]) -> Result<(), CodecError> {
        let length = u32::try_from(value.len()).map_err(|_| CodecError::Limit)?;
        self.write_u32(length)?;
        self.extend(value)
    }

    pub fn write_text(&mut self, value: &str) -> Result<(), CodecError> {
        self.write_bytes(value.as_bytes())
    }

    pub fn write_count(&mut self, count: usize) -> Result<(), CodecError> {
        self.write_u32(u32::try_from(count).map_err(|_| CodecError::Limit)?)
    }

    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn extend(&mut self, value: &[u8]) -> Result<(), CodecError> {
        let end = self
            .bytes
            .len()
            .checked_add(value.len())
            .filter(|end| *end <= self.limit)
            .ok_or(CodecError::Limit)?;
        self.bytes.reserve(end - self.bytes.len());
        self.bytes.extend_from_slice(value);
        Ok(())
    }
}

/// Forward-only decoder that rejects truncation, trailing bytes and bad tags.
pub struct BoundedDecoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> BoundedDecoder<'a> {
    pub fn new(bytes: &'a [u8], limit: u32) -> Result<Self, CodecError> {
        let limit = usize::try_from(limit).map_err(|_| CodecError::Limit)?;
        if limit == 0 || limit > MAX_WIRE_BYTES || bytes.len() > limit {
            return Err(CodecError::Limit);
        }
        Ok(Self { bytes, position: 0 })
    }

    pub fn read_bool(&mut self) -> Result<bool, CodecError> {
        match self.read_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CodecError::Invalid("invalid bool tag")),
        }
    }

    pub fn read_u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }

    pub fn read_u32(&mut self) -> Result<u32, CodecError> {
        let bytes = self
            .take(4)?
            .try_into()
            .map_err(|_| CodecError::Invalid("truncated u32"))?;
        Ok(u32::from_be_bytes(bytes))
    }

    pub fn read_u64(&mut self) -> Result<u64, CodecError> {
        let bytes = self
            .take(8)?
            .try_into()
            .map_err(|_| CodecError::Invalid("truncated u64"))?;
        Ok(u64::from_be_bytes(bytes))
    }

    pub fn read_i64(&mut self) -> Result<i64, CodecError> {
        let bytes = self
            .take(8)?
            .try_into()
            .map_err(|_| CodecError::Invalid("truncated i64"))?;
        Ok(i64::from_be_bytes(bytes))
    }

    pub fn read_f64(&mut self) -> Result<f64, CodecError> {
        let value = f64::from_bits(self.read_u64()?);
        if !value.is_finite() || value.to_bits() == (-0.0_f64).to_bits() {
            return Err(CodecError::Invalid("noncanonical f64"));
        }
        Ok(value)
    }

    pub fn read_bytes(&mut self) -> Result<&'a [u8], CodecError> {
        let length = usize::try_from(self.read_u32()?)
            .map_err(|_| CodecError::Invalid("byte length overflow"))?;
        self.take(length)
    }

    pub fn read_text(&mut self) -> Result<&'a str, CodecError> {
        Ok(std::str::from_utf8(self.read_bytes()?)?)
    }

    pub fn read_count(&mut self) -> Result<usize, CodecError> {
        usize::try_from(self.read_u32()?).map_err(|_| CodecError::Invalid("count overflow"))
    }

    pub fn finish(self) -> Result<(), CodecError> {
        if self.position != self.bytes.len() {
            return Err(CodecError::Invalid("trailing wire bytes"));
        }
        Ok(())
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], CodecError> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(CodecError::Invalid("truncated wire value"))?;
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }
}

macro_rules! fixed_wire {
    ($type:ty, $write:ident, $read:ident) => {
        impl WireValue for $type {
            fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
                encoder.$write(*self)
            }

            fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
                decoder.$read()
            }
        }
    };
}

fixed_wire!(bool, write_bool, read_bool);
fixed_wire!(u8, write_u8, read_u8);
fixed_wire!(u32, write_u32, read_u32);
fixed_wire!(u64, write_u64, read_u64);
fixed_wire!(i64, write_i64, read_i64);
fixed_wire!(f64, write_f64, read_f64);

impl WireValue for Vec<u8> {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_bytes(self)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(decoder.read_bytes()?.to_vec())
    }
}

impl WireValue for String {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_text(self)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(decoder.read_text()?.to_owned())
    }
}

impl<T: WireValue> WireValue for Option<T> {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            None => encoder.write_u8(0),
            Some(value) => {
                encoder.write_u8(1)?;
                value.encode(encoder)
            }
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(T::decode(decoder)?)),
            _ => Err(CodecError::Invalid("invalid option tag")),
        }
    }
}

impl WireValue for () {
    fn encode(&self, _encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        Ok(())
    }

    fn decode(_decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(())
    }
}

pub(crate) fn decode_wire<T: WireValue>(input: &[u8], limit: u32) -> Result<T, CodecError> {
    let mut decoder = BoundedDecoder::new(input, limit)?;
    let value = T::decode(&mut decoder)?;
    decoder.finish()?;
    Ok(value)
}

pub(crate) fn encode_wire<T: WireValue>(value: &T, limit: u32) -> Result<Vec<u8>, CodecError> {
    let mut encoder = BoundedEncoder::new(limit)?;
    value.encode(&mut encoder)?;
    Ok(encoder.finish())
}
