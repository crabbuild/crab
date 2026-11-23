use std::collections::{BTreeMap, HashSet};

use super::*;

const KEY: &str = "organization/v1/repository-properties";
const MAX_PROPERTIES: usize = 50;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Text,
    Boolean,
    SingleSelect,
    MultiSelect,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum PropertyValue {
    Boolean(bool),
    Text(String),
    Options(Vec<String>),
}

impl PropertyValue {
    pub(super) fn matches(&self, term: &Self) -> bool {
        match (self, term) {
            (Self::Options(options), Self::Text(value)) => options.contains(value),
            (actual, expected) => actual == expected,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Definition {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub kind: Kind,
    pub options: Vec<String>,
    pub default: Option<PropertyValue>,
}

fn valid_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 75
        && value.trim() == value
        && value
            .bytes()
            .all(|byte| (b' '..=b'~').contains(&byte) && byte != b'"')
}

impl Definition {
    fn valid(&self) -> bool {
        let choices = matches!(self.kind, Kind::SingleSelect | Kind::MultiSelect);
        let mut unique = HashSet::new();
        !self.id.is_nil()
            && !self.name.is_empty()
            && self.name.len() <= 75
            && self
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-#$".contains(&byte))
            && self.description.len() <= 256
            && !self.description.chars().any(char::is_control)
            && if choices {
                !self.options.is_empty()
                    && self.options.len() <= 50
                    && self
                        .options
                        .iter()
                        .all(|value| valid_text(value) && unique.insert(value))
            } else {
                self.options.is_empty()
            }
            && self
                .default
                .as_ref()
                .is_none_or(|value| self.accepts(value))
    }

    fn accepts(&self, value: &PropertyValue) -> bool {
        match (self.kind, value) {
            (Kind::Boolean, PropertyValue::Boolean(_)) => true,
            (Kind::Text, PropertyValue::Text(value)) => valid_text(value),
            (Kind::SingleSelect, PropertyValue::Text(value)) => self.options.contains(value),
            (Kind::MultiSelect, PropertyValue::Options(values)) => {
                let mut unique = HashSet::new();
                !values.is_empty()
                    && values.len() <= 20
                    && values
                        .iter()
                        .all(|value| self.options.contains(value) && unique.insert(value))
            }
            _ => false,
        }
    }

    pub(super) fn accepts_term(&self, value: &PropertyValue) -> bool {
        if self.kind == Kind::MultiSelect {
            return matches!(value, PropertyValue::Text(value) if self.options.contains(value));
        }
        self.accepts(value)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Schema {
    pub version: u64,
    pub definitions: Vec<Definition>,
}

impl Schema {
    pub(super) fn definition(&self, id: Uuid) -> Option<&Definition> {
        self.definitions
            .iter()
            .find(|definition| definition.id == id)
    }
}

pub(super) fn schema(context: &Transaction<'_>) -> Result<Schema> {
    Ok(context.document(KEY)?.unwrap_or_default())
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Repository {
    pub id: Uuid,
    pub ownership_epoch: u32,
}

impl Repository {
    fn key(self) -> Result<String> {
        if self.id.is_nil() {
            return Err(Error::Invalid("invalid property repository"));
        }
        Ok(format!(
            "{KEY}/repository/{}/{:08x}",
            self.id, self.ownership_epoch
        ))
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Values {
    pub version: u64,
    pub overrides: BTreeMap<Uuid, PropertyValue>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct View {
    pub schema: Schema,
    pub values: Values,
}

fn view(context: &Transaction<'_>, repository: Repository) -> Result<View> {
    let schema = schema(context)?;
    let mut values: Values = context.document(&repository.key()?)?.unwrap_or_default();
    values
        .overrides
        .retain(|id, _| schema.definition(*id).is_some());
    Ok(View { schema, values })
}

pub(super) fn effective(
    context: &Transaction<'_>,
    repository: Repository,
) -> Result<BTreeMap<Uuid, PropertyValue>> {
    let View { schema, mut values } = view(context, repository)?;
    Ok(schema
        .definitions
        .into_iter()
        .filter_map(|definition| {
            values
                .overrides
                .remove(&definition.id)
                .or(definition.default)
                .map(|value| (definition.id, value))
        })
        .collect())
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Outcome<T> {
    Saved(T),
    Conflict,
    Invalid,
    UsedByRuleset,
    IncompatibleDefinition,
}

pub(crate) struct GetSchema;
impl Read for GetSchema {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 54;
    type Input = Caller;
    type Output = Schema;

    fn execute(context: &mut Transaction<'_>, _: Caller) -> Result<Schema> {
        schema(context)
    }
}

pub(crate) struct GetValues;
impl Read for GetValues {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 55;
    type Input = Repository;
    type Output = View;

    fn execute(context: &mut Transaction<'_>, repository: Repository) -> Result<View> {
        view(context, repository)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetSchemaInput {
    pub actor: Uuid,
    pub expected_version: u64,
    pub definitions: Vec<Definition>,
}

pub(crate) struct SetSchema;
impl Mutation for SetSchema {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 54;
    type Input = SetSchemaInput;
    type Output = Outcome<Schema>;

    fn execute(
        context: &mut Transaction<'_>,
        input: SetSchemaInput,
    ) -> Result<MutationResult<Self::Output>> {
        let current = schema(context)?;
        if current.version != input.expected_version || current.version >= MAX_VERSION - 1 {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        if input.definitions.len() > MAX_PROPERTIES
            || serde_json::to_vec(&input.definitions)?.len() > 96 * 1024
            || !input.definitions.iter().all(|definition| {
                definition.valid()
                    && ids.insert(definition.id)
                    && names.insert(definition.name.to_ascii_lowercase())
            })
        {
            return Ok(MutationResult::Rejected(Outcome::Invalid));
        }
        let next = Schema {
            version: current.version + 1,
            definitions: input.definitions,
        };
        if !super::rulesets::properties_valid(context, &next)? {
            return Ok(MutationResult::Rejected(Outcome::UsedByRuleset));
        }
        if current.definitions == next.definitions {
            return Ok(MutationResult::Success(Outcome::Saved(current)));
        }
        // A retained definition keeps all existing explicit values valid.
        // Defaults can change independently and apply to repositories with no override.
        if next.definitions.iter().any(|definition| {
            current.definition(definition.id).is_some_and(|old| {
                old.kind != definition.kind
                    || !old
                        .options
                        .iter()
                        .all(|value| definition.options.contains(value))
            })
        }) {
            return Ok(MutationResult::Rejected(Outcome::IncompatibleDefinition));
        }
        // Retired IDs cannot expose their old overrides if an API caller
        // submits a definition using the same ID again.
        let retired = |id| format!("{KEY}/retired/{id}");
        for definition in &next.definitions {
            if current.definition(definition.id).is_none()
                && context.document::<bool>(&retired(definition.id))?.is_some()
            {
                return Ok(MutationResult::Rejected(Outcome::Invalid));
            }
        }
        for definition in &current.definitions {
            if next.definition(definition.id).is_none() {
                context.put_document(&retired(definition.id), &true)?;
            }
        }
        context.put_document(KEY, &next)?;
        advance(
            context,
            input.actor,
            Action::RepositoryPropertiesChanged {
                repository: None,
                version: next.version,
            },
        )?;
        Ok(MutationResult::Success(Outcome::Saved(next)))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetValuesInput {
    pub actor: Uuid,
    pub repository: Repository,
    pub expected_version: u64,
    pub schema_version: u64,
    pub overrides: BTreeMap<Uuid, PropertyValue>,
}

pub(crate) struct SetValues;
impl Mutation for SetValues {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 55;
    type Input = SetValuesInput;
    type Output = Outcome<View>;

    fn execute(
        context: &mut Transaction<'_>,
        input: SetValuesInput,
    ) -> Result<MutationResult<Self::Output>> {
        let mut current = view(context, input.repository)?;
        if current.schema.version != input.schema_version
            || current.values.version != input.expected_version
            || current.values.version >= MAX_VERSION - 1
        {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        if !input.overrides.iter().all(|(id, value)| {
            current
                .schema
                .definition(*id)
                .is_some_and(|definition| definition.accepts(value))
        }) {
            return Ok(MutationResult::Rejected(Outcome::Invalid));
        }
        if current.values.overrides == input.overrides {
            return Ok(MutationResult::Success(Outcome::Saved(current)));
        }
        current.values = Values {
            version: current.values.version + 1,
            overrides: input.overrides,
        };
        context.put_document(&input.repository.key()?, &current.values)?;
        advance(
            context,
            input.actor,
            Action::RepositoryPropertiesChanged {
                repository: Some(input.repository.id),
                version: current.values.version,
            },
        )?;
        Ok(MutationResult::Success(Outcome::Saved(current)))
    }
}
