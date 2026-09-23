use crab_cell_runtime::codec::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};

use super::{
    BranchProtectionRecord, BranchProtectionSettings, ReplaceBranchProtectionsInput,
    ReplaceBranchProtectionsOutcome, ReplaceRepositoryLifecycleInput,
    ReplaceRepositoryLifecycleOutcome, RepositoryLifecycleRecord,
};

impl WireValue for BranchProtectionRecord {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_text(&self.branch)?;
        encoder.write_u8(self.required_approvals)?;
        encoder.write_count(self.required_checks.len())?;
        for check in &self.required_checks {
            encoder.write_text(check)?;
        }
        Ok(())
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        let branch = decoder.read_text()?.to_owned();
        let required_approvals = decoder.read_u8()?;
        let count = bounded_count(decoder, 50)?;
        let mut required_checks = Vec::with_capacity(count);
        for _ in 0..count {
            required_checks.push(decoder.read_text()?.to_owned());
        }
        Ok(Self {
            branch,
            required_approvals,
            required_checks,
        })
    }
}

impl WireValue for BranchProtectionSettings {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.version)?;
        encode_rules(&self.rules, encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            version: decoder.read_u64()?,
            rules: decode_rules(decoder)?,
        })
    }
}

impl WireValue for RepositoryLifecycleRecord {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.version)?;
        self.archived.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            version: decoder.read_u64()?,
            archived: bool::decode(decoder)?,
        })
    }
}

impl WireValue for ReplaceBranchProtectionsInput {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.expected_version)?;
        encode_rules(&self.rules, encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            expected_version: decoder.read_u64()?,
            rules: decode_rules(decoder)?,
        })
    }
}

fn encode_rules(
    rules: &[BranchProtectionRecord],
    encoder: &mut BoundedEncoder,
) -> Result<(), CodecError> {
    encoder.write_count(rules.len())?;
    for rule in rules {
        rule.encode(encoder)?;
    }
    Ok(())
}

fn decode_rules(
    decoder: &mut BoundedDecoder<'_>,
) -> Result<Vec<BranchProtectionRecord>, CodecError> {
    let count = bounded_count(decoder, 100)?;
    let mut rules = Vec::with_capacity(count);
    for _ in 0..count {
        rules.push(BranchProtectionRecord::decode(decoder)?);
    }
    Ok(rules)
}

fn bounded_count(decoder: &mut BoundedDecoder<'_>, maximum: usize) -> Result<usize, CodecError> {
    let count = decoder.read_count()?;
    if count > maximum {
        return Err(CodecError::Invalid("settings collection is too large"));
    }
    Ok(count)
}

impl WireValue for ReplaceBranchProtectionsOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Updated(settings) => {
                encoder.write_u8(1)?;
                settings.encode(encoder)
            }
            Self::Conflict => encoder.write_u8(2),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            1 => Ok(Self::Updated(BranchProtectionSettings::decode(decoder)?)),
            2 => Ok(Self::Conflict),
            _ => Err(CodecError::Invalid("invalid branch-protection outcome")),
        }
    }
}

impl WireValue for ReplaceRepositoryLifecycleInput {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        encoder.write_u64(self.expected_version)?;
        self.archived.encode(encoder)
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            expected_version: decoder.read_u64()?,
            archived: bool::decode(decoder)?,
        })
    }
}

impl WireValue for ReplaceRepositoryLifecycleOutcome {
    fn encode(&self, encoder: &mut BoundedEncoder) -> Result<(), CodecError> {
        match self {
            Self::Updated(lifecycle) => {
                encoder.write_u8(1)?;
                lifecycle.encode(encoder)
            }
            Self::Conflict => encoder.write_u8(2),
            Self::Unchanged => encoder.write_u8(3),
        }
    }

    fn decode(decoder: &mut BoundedDecoder<'_>) -> Result<Self, CodecError> {
        match decoder.read_u8()? {
            1 => Ok(Self::Updated(RepositoryLifecycleRecord::decode(decoder)?)),
            2 => Ok(Self::Conflict),
            3 => Ok(Self::Unchanged),
            _ => Err(CodecError::Invalid("invalid repository-lifecycle outcome")),
        }
    }
}
