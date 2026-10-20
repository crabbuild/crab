use super::BranchProtectionRecord;
use crate::cells::records::{RecordDecoder, RecordEncoder, RecordError, StoredRecord};

impl StoredRecord for BranchProtectionRecord {
    fn encode(&self, encoder: &mut RecordEncoder) -> Result<(), RecordError> {
        encoder.write_text(&self.branch)?;
        encoder.write_u8(self.required_approvals)?;
        encoder.write_count(self.required_checks.len())?;
        for check in &self.required_checks {
            encoder.write_text(check)?;
        }
        Ok(())
    }

    fn decode(decoder: &mut RecordDecoder<'_>) -> Result<Self, RecordError> {
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

fn bounded_count(decoder: &mut RecordDecoder<'_>, maximum: usize) -> Result<usize, RecordError> {
    let count = decoder.read_count()?;
    if count > maximum {
        return Err(RecordError::Invalid("settings collection is too large"));
    }
    Ok(count)
}
