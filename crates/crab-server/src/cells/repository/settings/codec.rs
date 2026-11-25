use super::{BranchProtectionRecord, MAX_PROTECTION_BYTES};
use crate::cells::records::{RecordDecoder, RecordEncoder, RecordError, StoredRecord};
use uuid::Uuid;

// Existing records start with a rule count in 0..=100.
const PROVIDER_RULES_FORMAT: usize = 101;
const CODE_OWNER_RULES_FORMAT: usize = 102;
const CONVERSATION_RULES_FORMAT: usize = 103;
const UP_TO_DATE_RULES_FORMAT: usize = 104;
const QUEUE_RULES_FORMAT: usize = 105;
const LINEAR_HISTORY_RULES_FORMAT: usize = 106;
const LAST_PUSH_APPROVAL_RULES_FORMAT: usize = 107;

pub(super) fn encode(rules: &[BranchProtectionRecord]) -> Result<Vec<u8>, RecordError> {
    let mut encoder = RecordEncoder::new(MAX_PROTECTION_BYTES)?;
    let format = if rules.iter().any(|rule| rule.require_last_push_approval) {
        LAST_PUSH_APPROVAL_RULES_FORMAT
    } else if rules.iter().any(|rule| rule.require_linear_history) {
        LINEAR_HISTORY_RULES_FORMAT
    } else if rules.iter().any(|rule| rule.require_merge_queue) {
        QUEUE_RULES_FORMAT
    } else if rules.iter().any(|rule| rule.require_up_to_date) {
        UP_TO_DATE_RULES_FORMAT
    } else if rules
        .iter()
        .any(|rule| rule.require_conversation_resolution)
    {
        CONVERSATION_RULES_FORMAT
    } else if rules.iter().any(|rule| rule.required_code_owner_reviews) {
        CODE_OWNER_RULES_FORMAT
    } else if rules
        .iter()
        .any(|rule| !rule.required_integrations.is_empty())
    {
        PROVIDER_RULES_FORMAT
    } else {
        0
    };
    if format != 0 {
        encoder.write_count(format)?;
    }
    encoder.write_count(rules.len())?;
    for rule in rules {
        rule.encode(&mut encoder)?;
        if format >= PROVIDER_RULES_FORMAT {
            encoder.write_count(rule.required_integrations.len())?;
            for id in &rule.required_integrations {
                encoder.write_bytes(id.as_bytes())?;
            }
        }
        if format >= CODE_OWNER_RULES_FORMAT {
            encoder.write_u8(u8::from(rule.required_code_owner_reviews))?;
        }
        if format >= CONVERSATION_RULES_FORMAT {
            encoder.write_u8(u8::from(rule.require_conversation_resolution))?;
        }
        if format >= UP_TO_DATE_RULES_FORMAT {
            encoder.write_u8(u8::from(rule.require_up_to_date))?;
        }
        if format >= QUEUE_RULES_FORMAT {
            encoder.write_u8(u8::from(rule.require_merge_queue))?;
        }
        if format >= LINEAR_HISTORY_RULES_FORMAT {
            encoder.write_u8(u8::from(rule.require_linear_history))?;
        }
        if format >= LAST_PUSH_APPROVAL_RULES_FORMAT {
            encoder.write_u8(u8::from(rule.require_last_push_approval))?;
        }
    }
    Ok(encoder.finish())
}

pub(super) fn decode(bytes: &[u8]) -> Result<Vec<BranchProtectionRecord>, RecordError> {
    let mut decoder = RecordDecoder::new(bytes, MAX_PROTECTION_BYTES)?;
    let format = bounded_count(&mut decoder, LAST_PUSH_APPROVAL_RULES_FORMAT)?;
    let count = if format >= PROVIDER_RULES_FORMAT {
        bounded_count(&mut decoder, 100)?
    } else {
        format
    };
    let mut rules = Vec::with_capacity(count);
    for _ in 0..count {
        let mut rule = BranchProtectionRecord::decode(&mut decoder)?;
        if format >= PROVIDER_RULES_FORMAT {
            let count = bounded_count(&mut decoder, 16)?;
            for _ in 0..count {
                rule.required_integrations.push(
                    Uuid::from_slice(decoder.read_bytes()?)
                        .map_err(|_| RecordError::Invalid("invalid required integration ID"))?,
                );
            }
        }
        if format >= CODE_OWNER_RULES_FORMAT {
            rule.required_code_owner_reviews = flag(&mut decoder)?;
        }
        if format >= CONVERSATION_RULES_FORMAT {
            rule.require_conversation_resolution = flag(&mut decoder)?;
        }
        if format >= UP_TO_DATE_RULES_FORMAT {
            rule.require_up_to_date = flag(&mut decoder)?;
        }
        if format >= QUEUE_RULES_FORMAT {
            rule.require_merge_queue = flag(&mut decoder)?;
        }
        if format >= LINEAR_HISTORY_RULES_FORMAT {
            rule.require_linear_history = flag(&mut decoder)?;
        }
        if format >= LAST_PUSH_APPROVAL_RULES_FORMAT {
            rule.require_last_push_approval = flag(&mut decoder)?;
        }
        rules.push(rule);
    }
    decoder.finish()?;
    Ok(rules)
}

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
            required_code_owner_reviews: false,
            require_last_push_approval: false,
            require_conversation_resolution: false,
            require_up_to_date: false,
            require_merge_queue: false,
            require_linear_history: false,
            required_checks,
            required_integrations: Vec::new(),
        })
    }
}

fn flag(decoder: &mut RecordDecoder<'_>) -> Result<bool, RecordError> {
    match decoder.read_u8()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(RecordError::Invalid("invalid branch protection flag")),
    }
}

fn bounded_count(decoder: &mut RecordDecoder<'_>, maximum: usize) -> Result<usize, RecordError> {
    let count = decoder.read_count()?;
    if count > maximum {
        return Err(RecordError::Invalid("settings collection is too large"));
    }
    Ok(count)
}
