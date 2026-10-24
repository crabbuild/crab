use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Transaction;
use crate::cells::{Error, Result};

pub(in crate::cells::repository) mod migration;
pub(crate) use migration::{
    AssignmentBatch, AssignmentMapping, AssignmentState, GetAssignmentBatch, MigrateAssignments,
    MigrateAssignmentsInput, MigrateAssignmentsOutcome, RecordMapping,
};
pub(super) use migration::{ensure_ready, initialize};

// Names are explicit snapshots, so removing repository access cannot erase
// historical attribution. A public profile may supply newer presentation.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Assignee {
    pub id: Uuid,
    pub name: String,
}

pub(super) fn validate(assignees: &[Assignee]) -> Result<()> {
    if assignees.len() > 10
        || assignees.iter().any(|assignee| {
            assignee.id.is_nil()
                || assignee.name.trim() != assignee.name
                || assignee.name.is_empty()
                || assignee.name.chars().count() > 160
                || assignee.name.chars().any(char::is_control)
        })
        || assignees.windows(2).any(|pair| pair[0].id >= pair[1].id)
    {
        return Err(Error::Invalid("Repository assignees are invalid"));
    }
    Ok(())
}

pub(super) fn encode(assignees: &[Assignee]) -> Result<Vec<u8>> {
    use crate::cells::records::RecordEncoder;
    validate(assignees)?;
    let mut encoder = RecordEncoder::new(16 * 1024).map_err(Error::sdk)?;
    encoder.write_count(assignees.len()).map_err(Error::sdk)?;
    for assignee in assignees {
        encoder
            .write_bytes(assignee.id.as_bytes())
            .map_err(Error::sdk)?;
        encoder.write_text(&assignee.name).map_err(Error::sdk)?;
    }
    Ok(encoder.finish())
}

pub(super) fn decode(bytes: &[u8]) -> Result<Vec<Assignee>> {
    use crate::cells::records::RecordDecoder;
    let mut decoder = RecordDecoder::new(bytes, 16 * 1024).map_err(Error::sdk)?;
    let count = decoder.read_count().map_err(Error::sdk)?;
    if count > 10 {
        return Err(Error::Invalid("Repository assignment count is invalid"));
    }
    let mut assignees = Vec::with_capacity(count);
    for _ in 0..count {
        let id = Uuid::from_slice(decoder.read_bytes().map_err(Error::sdk)?).map_err(Error::sdk)?;
        let name = decoder.read_text().map_err(Error::sdk)?.to_owned();
        assignees.push(Assignee { id, name });
    }
    decoder.finish().map_err(Error::sdk)?;
    validate(&assignees)?;
    Ok(assignees)
}

#[cfg(test)]
mod tests;
