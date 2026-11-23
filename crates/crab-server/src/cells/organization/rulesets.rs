use std::collections::{BTreeMap, HashSet};

use super::properties::{self, PropertyValue, Schema};
use super::*;
use crate::cells::repository::rulesets::{
    BypassActor, Enforcement, MAX_RULESETS, Pattern, Ruleset, Rulesets, Target,
};

const KEY: &str = "organization/v1/rulesets";
const MAX_BYTES: usize = 96 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Repositories {
    All,
    Selected {
        ids: Vec<Uuid>,
    },
    Name {
        include: Vec<String>,
        exclude: Vec<String>,
    },
    Properties {
        conditions: Vec<PropertyCondition>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum PropertyCondition {
    Custom(CustomCondition),
    System(SystemCondition),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustomCondition {
    pub property: Uuid,
    pub value: PropertyValue,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exclude: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SystemProperty {
    Fork,
    Name,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SystemCondition {
    system: SystemProperty,
    value: PropertyValue,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    exclude: bool,
}

fn name_pattern(value: &str) -> Option<Pattern> {
    if value.contains('/') {
        return None;
    }
    Pattern::parse(value, Target::Tag)
}

fn matches_name(pattern: &str, name: &str) -> Result<bool> {
    name_pattern(pattern)
        .map(|pattern| pattern.matches(name, false))
        .ok_or(Error::Invalid("invalid saved repository pattern"))
}

impl PropertyCondition {
    fn valid(&self) -> bool {
        match self {
            Self::Custom(term) => !term.property.is_nil(),
            Self::System(term) => match (term.system, &term.value) {
                (SystemProperty::Fork, PropertyValue::Boolean(_)) => true,
                (SystemProperty::Name, PropertyValue::Text(pattern)) => {
                    name_pattern(pattern).is_some()
                }
                _ => false,
            },
        }
    }

    fn matches(&self, repo: &Repository, values: &BTreeMap<Uuid, PropertyValue>) -> Result<bool> {
        match self {
            Self::Custom(term) => Ok(values
                .get(&term.property)
                .is_some_and(|value| value.matches(&term.value))
                != term.exclude),
            Self::System(term) => {
                let matched = match (term.system, &term.value) {
                    (SystemProperty::Fork, PropertyValue::Boolean(expected)) => {
                        repo.fork == *expected
                    }
                    (SystemProperty::Name, PropertyValue::Text(pattern)) => {
                        matches_name(pattern, &repo.name)?
                    }
                    _ => return Err(Error::Invalid("invalid saved system property")),
                };
                Ok(matched != term.exclude)
            }
        }
    }
}

impl Repositories {
    fn valid(&self) -> bool {
        match self {
            Self::All => true,
            Self::Selected { ids } => {
                let mut unique = HashSet::new();
                !ids.is_empty()
                    && ids.len() <= 100
                    && ids.iter().all(|id| !id.is_nil() && unique.insert(*id))
            }
            Self::Name { include, exclude } => {
                !include.is_empty()
                    && [include, exclude].into_iter().all(|patterns| {
                        let mut unique = HashSet::new();
                        patterns.len() <= 20
                            && patterns.iter().all(|pattern| {
                                unique.insert(pattern) && name_pattern(pattern).is_some()
                            })
                    })
            }
            Self::Properties { conditions } => {
                !conditions.is_empty()
                    && conditions.len() <= 20
                    && conditions.iter().enumerate().all(|(index, condition)| {
                        condition.valid() && !conditions[..index].contains(condition)
                    })
            }
        }
    }

    fn matches(&self, repo: &Repository, values: &BTreeMap<Uuid, PropertyValue>) -> Result<bool> {
        match self {
            Self::All => Ok(true),
            Self::Selected { ids } => Ok(ids.contains(&repo.id)),
            Self::Name { include, exclude } => {
                let matches = |patterns: &[String]| -> Result<bool> {
                    for pattern in patterns {
                        if matches_name(pattern, &repo.name)? {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                };
                Ok(matches(include)? && !matches(exclude)?)
            }
            Self::Properties { conditions } => {
                for condition in conditions {
                    if !condition.matches(repo, values)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }

    fn uses_custom_properties(&self) -> bool {
        matches!(self, Self::Properties { conditions } if conditions.iter().any(|term| matches!(term, PropertyCondition::Custom(_))))
    }

    fn defined_by(&self, schema: &Schema) -> bool {
        let Self::Properties { conditions } = self else {
            return true;
        };
        conditions.iter().all(|term| match term {
            PropertyCondition::System(_) => true,
            PropertyCondition::Custom(term) => schema
                .definition(term.property)
                .is_some_and(|definition| definition.accepts_term(&term.value)),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    pub ruleset: Ruleset,
    pub repositories: Repositories,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    pub version: u64,
    pub rulesets: Vec<Entry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplaceInput {
    pub actor: Uuid,
    pub expected_version: u64,
    pub rulesets: Vec<Entry>,
}

impl ReplaceInput {
    pub(crate) fn valid(&self) -> bool {
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        self.expected_version < MAX_VERSION
            && self.rulesets.len() <= MAX_RULESETS
            && serde_json::to_vec(&self.rulesets).is_ok_and(|value| value.len() <= MAX_BYTES)
            && self.rulesets.iter().all(|entry| {
                entry.ruleset.valid()
                    && entry.repositories.valid()
                    && entry.ruleset.required_integrations.is_empty()
                    && ids.insert(entry.ruleset.id)
                    && names.insert(entry.ruleset.name.to_lowercase())
            })
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) enum ReplaceOutcome {
    Saved(Settings),
    Conflict,
    UnavailableTeam,
    UnavailableProperty,
}

pub(crate) struct Replace;
impl Mutation for Replace {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 46;
    type Input = ReplaceInput;
    type Output = ReplaceOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<ReplaceOutcome>> {
        if !input.valid() {
            return Err(Error::Invalid("invalid organization rulesets"));
        }
        let current = load(context)?;
        if current.version != input.expected_version || current.version >= MAX_VERSION - 1 {
            return Ok(MutationResult::Rejected(ReplaceOutcome::Conflict));
        }
        if input
            .rulesets
            .iter()
            .any(|entry| entry.repositories.uses_custom_properties())
        {
            let schema = properties::schema(context)?;
            if !input
                .rulesets
                .iter()
                .all(|entry| entry.repositories.defined_by(&schema))
            {
                return Ok(MutationResult::Rejected(
                    ReplaceOutcome::UnavailableProperty,
                ));
            }
        }
        let old_teams = team_ids(&current.rulesets);
        for id in team_ids(&input.rulesets).difference(&old_teams) {
            if !teams::team(context, *id)?
                .is_some_and(|team| team.visibility == teams::Visibility::Closed)
            {
                return Ok(MutationResult::Rejected(ReplaceOutcome::UnavailableTeam));
            }
        }
        if current.rulesets == input.rulesets {
            return Ok(MutationResult::Success(ReplaceOutcome::Saved(current)));
        }
        let saved = Settings {
            version: current.version + 1,
            rulesets: input.rulesets,
        };
        context.put_document(KEY, &saved)?;
        advance(
            context,
            input.actor,
            Action::RulesetsChanged {
                version: saved.version,
            },
        )?;
        Ok(MutationResult::Success(ReplaceOutcome::Saved(saved)))
    }
}

fn team_ids(entries: &[Entry]) -> HashSet<Uuid> {
    entries
        .iter()
        .flat_map(|entry| &entry.ruleset.bypass_actors)
        .filter_map(|grant| match grant.actor {
            BypassActor::Team { id } => Some(id),
            _ => None,
        })
        .collect()
}

fn load(context: &Transaction<'_>) -> Result<Settings> {
    Ok(context.document(KEY)?.unwrap_or_default())
}

pub(super) fn properties_valid(context: &Transaction<'_>, schema: &Schema) -> Result<bool> {
    Ok(load(context)?
        .rulesets
        .iter()
        .all(|entry| entry.repositories.defined_by(schema)))
}

pub(super) fn version(context: &Transaction<'_>) -> Result<u64> {
    Ok(load(context)?.version)
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 46;
    type Input = Caller;
    type Output = Settings;

    fn execute(context: &mut Transaction<'_>, _: Caller) -> Result<Settings> {
        load(context)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Repository {
    pub id: Uuid,
    pub name: String,
    pub ownership_epoch: u32,
    pub fork: bool,
}

pub(crate) struct Applicable;
impl Read for Applicable {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 47;
    type Input = Repository;
    type Output = Rulesets;

    fn execute(context: &mut Transaction<'_>, repository: Repository) -> Result<Rulesets> {
        if repository.id.is_nil() || !crate::config::valid_repository_component(&repository.name) {
            return Err(Error::Invalid("invalid ruleset repository"));
        }
        let settings = load(context)?;
        let values = if settings.rulesets.iter().any(|entry| {
            entry.ruleset.enforcement == Enforcement::Active
                && entry.repositories.uses_custom_properties()
        }) {
            properties::effective(
                context,
                properties::Repository {
                    id: repository.id,
                    ownership_epoch: repository.ownership_epoch,
                },
            )?
        } else {
            BTreeMap::new()
        };
        let mut rulesets = Vec::new();
        for entry in settings.rulesets {
            if entry.ruleset.enforcement == Enforcement::Active
                && entry.repositories.matches(&repository, &values)?
            {
                rulesets.push(entry.ruleset);
            }
        }
        Ok(Rulesets {
            version: settings.version,
            rulesets,
        })
    }
}
