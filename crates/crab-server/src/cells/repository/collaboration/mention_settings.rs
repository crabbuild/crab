use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
    repository::{CLASS, MAX_NUMBER, advance_revision, settings},
};

const KEY: &str = "collaboration/v1/team-mention-settings";
const MAX_ALIASES: usize = 24;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Alias {
    pub id: Uuid,
    pub slug: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    pub organization: Option<Uuid>,
    pub namespace: String,
    pub teams: Vec<Alias>,
    pub version: u64,
}

impl Settings {
    pub(crate) fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.version >= MAX_NUMBER {
            return Err("Mention settings version is invalid");
        }
        let Some(organization) = self.organization else {
            return if self.namespace.is_empty() && self.teams.is_empty() {
                Ok(())
            } else {
                Err("Clearing the organization requires an empty namespace and team list")
            };
        };
        if organization.is_nil() || !valid_slug(&self.namespace) {
            return Err("Select an organization and a valid mention namespace");
        }
        if self.teams.len() > MAX_ALIASES {
            return Err("Mention settings support at most 24 team aliases");
        }
        let mut ids = BTreeSet::new();
        let mut slugs = BTreeSet::new();
        for team in &self.teams {
            if team.id.is_nil()
                || !valid_slug(&team.slug)
                || !ids.insert(team.id)
                || !slugs.insert(&team.slug)
            {
                return Err("Team aliases require unique team IDs and valid, unique slugs");
            }
        }
        Ok(())
    }
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

pub(in crate::cells::repository) fn read(context: &Transaction<'_>) -> Result<Settings> {
    Ok(context.document::<Settings>(KEY)?.unwrap_or_default())
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 338;
    type Input = ();
    type Output = Settings;

    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<Self::Output> {
        read(context)
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum Outcome {
    Saved(Settings),
    Conflict,
    Archived,
}

pub(crate) struct Set;
impl Mutation for Set {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 339;
    type Input = Settings;
    type Output = Outcome;

    fn execute(
        context: &mut Transaction<'_>,
        mut input: Settings,
    ) -> Result<MutationResult<Outcome>> {
        input.validate().map_err(Error::Invalid)?;
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        let current = read(context)?;
        if current.version != input.version {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        if current == input {
            return Ok(MutationResult::Success(Outcome::Saved(current)));
        }
        let Some(version) = current.version.checked_add(1).filter(|n| *n < MAX_NUMBER) else {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        };
        input.version = version;
        // Keep the revision after clearing so a stale editor cannot restore
        // a binding that another administrator explicitly removed.
        context.put_document(KEY, &input)?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Outcome::Saved(input)))
    }
}
