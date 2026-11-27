use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{CatalogDocument, CatalogError, CatalogStore};
use crate::app::MAX_NUMBER;

const MAX_HOMEPAGE_CHARS: usize = 256;
const MAX_LICENSE_CHARS: usize = 64;

pub(crate) fn valid_homepage(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let valid_scheme = value.starts_with("https://") || value.starts_with("http://");
    valid_scheme
        && value.chars().count() <= MAX_HOMEPAGE_CHARS
        && !value
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
}

pub(crate) fn valid_license(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    value.trim() == value
        && value.chars().count() <= MAX_LICENSE_CHARS
        && value
            .bytes()
            .all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'+' | b' '))
}

#[derive(Clone, Default, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Attributes {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub homepage: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub license: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_template: bool,
    #[serde(default)]
    pub version: u64,
}

impl Attributes {
    pub(super) fn is_initial(&self) -> bool {
        self.version == 0
            && self.homepage.is_empty()
            && self.license.is_empty()
            && !self.is_template
    }

    pub(super) fn valid_saved(&self) -> bool {
        self.version < MAX_NUMBER
            && (self.version != 0
                || (self.homepage.is_empty() && self.license.is_empty() && !self.is_template))
            && valid_homepage(&self.homepage)
            && valid_license(&self.license)
    }

    pub(crate) fn normalize(&mut self) -> Result<(), CatalogError> {
        if self.homepage.chars().any(char::is_control) {
            return Err(CatalogError::Invalid(
                "Website URL must start with http:// or https:// without whitespace",
            ));
        }
        self.homepage = self.homepage.trim().to_owned();
        if !valid_homepage(&self.homepage) {
            return Err(CatalogError::Invalid(
                "Website URL must start with http:// or https:// without whitespace",
            ));
        }
        if self.license.chars().any(char::is_control) {
            return Err(CatalogError::Invalid(
                "License must be at most 64 characters using letters, digits, spaces, '.', '-', '_', or '+'",
            ));
        }
        self.license = self.license.trim().to_owned();
        if !valid_license(&self.license) {
            return Err(CatalogError::Invalid(
                "License must be at most 64 characters using letters, digits, spaces, '.', '-', '_', or '+'",
            ));
        }
        Ok(())
    }
}

impl CatalogStore {
    pub(crate) async fn change_attributes(
        &self,
        expected_revision: u64,
        repository: Uuid,
        mut input: Attributes,
    ) -> Result<CatalogDocument, CatalogError> {
        input.normalize()?;
        self.edit_metadata(expected_revision, repository, |record| {
            let current = &record.attributes;
            if current.homepage == input.homepage
                && current.license == input.license
                && current.is_template == input.is_template
            {
                return Ok(false);
            }
            if current.version != input.version || input.version >= MAX_NUMBER - 1 {
                return Err(CatalogError::Conflict);
            }
            record.attributes = Attributes {
                homepage: input.homepage,
                license: input.license,
                is_template: input.is_template,
                version: current.version + 1,
            };
            Ok(true)
        })
        .await
    }
}
