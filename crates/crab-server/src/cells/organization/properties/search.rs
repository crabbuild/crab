use super::*;

const REVISION: &str = "organization/v1/repository-properties/search-revision";
pub(crate) const MATCH_BATCH: usize = 128;
pub(crate) const MAX_TERMS: usize = 8;

fn revision(context: &Transaction<'_>) -> Result<u64> {
    Ok(context.document(REVISION)?.unwrap_or_default())
}

pub(super) fn advance_revision(context: &Transaction<'_>) -> Result<()> {
    let next = revision(context)?.checked_add(1).ok_or(Error::Invalid(
        "repository property search revision limit reached",
    ))?;
    context.put_document(REVISION, &next)
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Term {
    pub name: String,
    pub value: String,
    pub exclude: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    pub repositories: Vec<Repository>,
    pub terms: Vec<Term>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Matches {
    pub revision: u64,
    pub repositories: Option<Vec<Uuid>>,
}

pub(crate) struct MatchRepositories;
impl Read for MatchRepositories {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 57;
    type Input = Selection;
    type Output = Matches;

    fn execute(context: &mut Transaction<'_>, input: Selection) -> Result<Matches> {
        if input.repositories.len() > MATCH_BATCH || input.terms.len() > MAX_TERMS {
            return Err(Error::Invalid(
                "repository property search exceeds its batch limit",
            ));
        }
        let revision = revision(context)?;
        if input.terms.is_empty() && input.repositories.is_empty() {
            return Ok(Matches {
                revision,
                repositories: Some(Vec::new()),
            });
        }
        let schema = schema(context)?;
        let Some(terms) = input
            .terms
            .iter()
            .map(|term| compile(&schema, term))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(Matches {
                revision,
                repositories: None,
            });
        };
        let mut repositories = Vec::new();
        for repo in input.repositories {
            let current: Values = context.document(&repo.key()?)?.unwrap_or_default();
            if terms.iter().all(|(definition, term, exclude)| {
                let matches = current
                    .overrides
                    .get(&definition.id)
                    .or(definition.default.as_ref())
                    .is_some_and(|value| value.matches(term));
                matches != *exclude
            }) {
                repositories.push(repo.id);
            }
        }
        Ok(Matches {
            revision,
            repositories: Some(repositories),
        })
    }
}

fn compile<'a>(schema: &'a Schema, term: &Term) -> Option<(&'a Definition, PropertyValue, bool)> {
    let definition = schema
        .definitions
        .iter()
        .find(|definition| definition.name.eq_ignore_ascii_case(&term.name))?;
    let value = match definition.kind {
        Kind::Boolean if term.value.eq_ignore_ascii_case("true") => PropertyValue::Boolean(true),
        Kind::Boolean if term.value.eq_ignore_ascii_case("false") => PropertyValue::Boolean(false),
        Kind::Boolean => return None,
        _ => PropertyValue::Text(term.value.clone()),
    };
    definition
        .accepts_term(&value)
        .then_some((definition, value, term.exclude))
}

pub(crate) struct Fields;
impl Read for Fields {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 58;
    type Input = ();
    type Output = Schema;

    fn execute(context: &mut Transaction<'_>, _: ()) -> Result<Schema> {
        schema(context)
    }
}
