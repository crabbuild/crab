use std::collections::HashSet;

use super::*;

const KEY: &str = "organization/v1/issue-types";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "IssueTypeColor"))]
pub(crate) enum Color {
    Gray,
    Blue,
    Green,
    Yellow,
    Orange,
    Red,
    Purple,
    Pink,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "IssueType"))]
pub(crate) struct Definition {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub color: Color,
    pub enabled: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(rename = "IssueTypeSchema"))]
pub(crate) struct Schema {
    pub version: u64,
    pub definitions: Vec<Definition>,
}

fn schema(context: &Transaction<'_>) -> Result<Schema> {
    Ok(context.document(KEY)?.unwrap_or_else(|| Schema {
        version: 0,
        definitions: [
            (1, "Task", "Work to be completed", Color::Blue),
            (2, "Bug", "A problem to fix", Color::Red),
            (3, "Feature", "New functionality", Color::Purple),
        ]
        .into_iter()
        .map(|(id, name, description, color)| Definition {
            id: Uuid::from_u128(id),
            name: name.into(),
            description: description.into(),
            color,
            enabled: true,
        })
        .collect(),
    }))
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 60;
    type Input = Caller;
    type Output = Schema;

    fn execute(context: &mut Transaction<'_>, _: Caller) -> Result<Schema> {
        schema(context)
    }
}

pub(crate) struct Active;
impl Read for Active {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 61;
    type Input = ();
    type Output = Schema;

    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<Self::Output> {
        let mut current = schema(context)?;
        current.definitions.retain(|definition| definition.enabled);
        Ok(current)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplaceInput {
    pub actor: Uuid,
    pub expected_version: u64,
    pub definitions: Vec<Definition>,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Outcome {
    Saved(Schema),
    Conflict,
    Invalid,
}

pub(crate) struct Replace;
impl Mutation for Replace {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 60;
    type Input = ReplaceInput;
    type Output = Outcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: ReplaceInput,
    ) -> Result<MutationResult<Outcome>> {
        let current = schema(context)?;
        // An exact retry after a lost reply can observe its committed version.
        if current.definitions == input.definitions
            && (current.version == input.expected_version
                || current.version.checked_sub(1) == Some(input.expected_version))
        {
            return Ok(MutationResult::Success(Outcome::Saved(current)));
        }
        if current.version != input.expected_version || current.version >= MAX_VERSION {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        if input.definitions.len() > 25
            || !input.definitions.iter().all(|definition| {
                !definition.id.is_nil()
                    && ids.insert(definition.id)
                    && !definition.name.is_empty()
                    && definition.name.len() <= 75
                    && definition.name.trim() == definition.name
                    && !definition.name.chars().any(char::is_control)
                    && names.insert(definition.name.to_lowercase())
                    && definition.description.len() <= 256
                    && !definition.description.chars().any(char::is_control)
            })
        {
            return Ok(MutationResult::Rejected(Outcome::Invalid));
        }
        let retired = |id| format!("{KEY}/retired/{id}");
        for definition in &input.definitions {
            if context.document::<bool>(&retired(definition.id))?.is_some() {
                return Ok(MutationResult::Rejected(Outcome::Invalid));
            }
        }
        // Deletion retires the identity so assignments cannot be revived by
        // reusing its ID. Disabling keeps the identity and its assignments.
        for definition in &current.definitions {
            if !ids.contains(&definition.id) {
                context.put_document(&retired(definition.id), &true)?;
            }
        }
        let next = Schema {
            version: current.version + 1,
            definitions: input.definitions,
        };
        context.put_document(KEY, &next)?;
        advance(
            context,
            input.actor,
            Action::IssueTypesChanged {
                version: next.version,
            },
        )?;
        Ok(MutationResult::Success(Outcome::Saved(next)))
    }
}
