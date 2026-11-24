use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{CatalogDocument, CatalogError, CatalogStore};
use crate::app::MAX_NUMBER;

const MAX_TOPICS: usize = 20;

pub(crate) fn valid_topic(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 50
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Topics {
    pub names: Vec<String>,
    pub version: u64,
}

impl Topics {
    pub(super) fn is_initial(&self) -> bool {
        self.version == 0 && self.names.is_empty()
    }

    pub(super) fn valid_saved(&self) -> bool {
        self.version < MAX_NUMBER
            && (self.version != 0 || self.names.is_empty())
            && self.names.len() <= MAX_TOPICS
            && self.names.iter().all(|name| valid_topic(name))
            && self.names.windows(2).all(|pair| pair[0] < pair[1])
    }

    pub(crate) fn normalize(&mut self) -> Result<(), CatalogError> {
        const ERROR: &str = "Use up to 20 topics with 1–50 letters, digits, or hyphens each";
        if self.names.len() > MAX_TOPICS {
            return Err(CatalogError::Invalid(ERROR));
        }
        for name in &mut self.names {
            name.make_ascii_lowercase();
            if !valid_topic(name) {
                return Err(CatalogError::Invalid(ERROR));
            }
        }
        self.names.sort_unstable();
        self.names.dedup();
        Ok(())
    }
}

impl CatalogStore {
    pub(crate) async fn change_topics(
        &self,
        expected_revision: u64,
        repository: Uuid,
        input: Topics,
    ) -> Result<CatalogDocument, CatalogError> {
        self.edit_metadata(expected_revision, repository, |record| {
            if record.topics.names == input.names {
                return Ok(false);
            }
            if record.topics.version != input.version || input.version >= MAX_NUMBER - 1 {
                return Err(CatalogError::Conflict);
            }
            record.topics = Topics {
                names: input.names,
                version: record.topics.version + 1,
            };
            Ok(true)
        })
        .await
    }
}
