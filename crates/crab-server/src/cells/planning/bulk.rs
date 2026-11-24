use std::collections::BTreeSet;

use super::{MAX_VERSION, Uuid};
use crate::cells::repository::projects::Item;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Edit {
    pub item: Uuid,
    pub version: u64,
    pub change: Field,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "field", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Field {
    Status { value: String },
    Priority { value: u8 },
    Start { value: Option<String> },
    End { value: Option<String> },
    Custom { id: Uuid, value: Option<String> },
}

impl Field {
    pub(crate) fn update(
        &self,
        item: &mut Item,
        definition: &super::super::repository::projects::Definition,
    ) -> bool {
        match self {
            Self::Status { value } => item.status.clone_from(value),
            Self::Priority { value } => item.priority = *value,
            Self::Start { value } => item.start.clone_from(value),
            Self::End { value } => item.end.clone_from(value),
            Self::Custom { id, value } => {
                if !definition.fields.iter().any(|field| field.id == *id) {
                    return false;
                }
                if let Some(value) = value {
                    item.values.insert(*id, value.clone());
                } else {
                    item.values.remove(id);
                }
            }
        }
        true
    }
}

pub(crate) const LIMIT: usize = 50;

pub(crate) fn valid(edits: &[Edit]) -> bool {
    let mut ids = BTreeSet::new();
    (1..=LIMIT).contains(&edits.len())
        && edits.iter().all(|edit| {
            !edit.item.is_nil()
                && (1..=MAX_VERSION).contains(&edit.version)
                && ids.insert(edit.item)
        })
}
